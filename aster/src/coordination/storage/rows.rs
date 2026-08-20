//! The `state.redb` row layer: typed I/O over the §11.3 tables, the whole-state
//! load, and the independent validator.
//!
//! Separate from [`machine`](super::machine) because the two answer different
//! questions. This module knows how a row is stored and what makes a stored
//! state coherent; the machine knows when to read and write one. Splitting them
//! is also what lets the validator be written from §11.3 and §12.1 rather than
//! from the transition — the check that recovers a state has to be able to
//! disagree with the code that produced it, or it is a restatement.
//!
//! ## What the validator is, and is not
//!
//! It is **derived from the normative rules** — §12.1's caps and charge
//! formulas, §11.3's audit floor and tip, §7.1's profile — and implemented
//! independently of the transition. It recomputes every derived value from the
//! rows and *compares* the persisted one against it: a persisted counter is
//! evidence to be checked, never an input to trust.
//!
//! It is not a second copy of the charge arithmetic. `State::recompute_growth`
//! is the *definition* of the growth counter — the function apply's incremental
//! updates are required to agree with — so recomputing through it is the check.
//! A hand-written second implementation here would only prove that two copies
//! of the same formula agree.
//!
//! A mismatch is data loss, not a repair opportunity: §11.3 makes rows,
//! counters and `last_applied` one transaction, so a state where they disagree
//! is a state no sequence of commits could produce.

// See the `storage` module docs: nothing in a shipping build reaches
// `VoterStorage::open` until Slice 3 supplies a verified identity.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use openraft::StoredMembership;
use redb::{Database, ReadableDatabase, ReadableTable};

use super::codec::{decode_persisted_bounded, encode_committed};
use super::error::{Db, StorageError, StorageResult};
use super::schema::{
    durable_write, ACL, ADMINS, AUDIT, INTENTS, KEY_META, MAX_COORDINATION_META_BYTES, RESOURCES,
    ROUTES, SESSIONS, STATE_META,
};
use crate::coordination::contract::LogIdV1;
use crate::coordination::ids::{CoordinationNodeId, ResourceKey};
use crate::coordination::model::limits;
use crate::coordination::model::shape;
use crate::coordination::model::state::{
    AuditRow, ChangeSet, CommittedMembership, GroupAuthority, LogId, ResourceRow, Route, State,
};
use crate::coordination::model::wire::{
    intent_from_wire, intent_to_wire, session_from_wire, session_to_wire,
};
use crate::coordination::raft::{
    ClientSessionV1, CoordinationMetaV1, MembershipIntentV1, NodeRouteV1, RecoveredFromV1,
    ResourceRowV1, StoredMembershipV1,
};
use crate::coordination::rowstream::AuditRowV1;
use crate::coordination::typeconfig::CoordinationNode;

// ── Value ceilings (§4.2) ────────────────────────────────────────────────────

/// Framing allowance over a row's declared field maxima.
///
/// Fory writes a type header and a per-field tag around the values it carries,
/// and that overhead is a property of the codec rather than of §12.1, so it
/// cannot be derived from the limits. One shared allowance rather than a
/// per-row fudge factor, and
/// [`every_row_ceiling_admits_its_maximal_legal_row`] measures the real
/// maximal encoding of every row type against it — in both directions, so a
/// ceiling that stopped admitting legal rows and one that drifted far above
/// them both fail.
const ROW_FRAMING_ALLOWANCE: usize = 1024;

/// A 32-byte node id or hash, as a term in the sums below.
const ID32: usize = 32;
/// A 16-byte client or operation id.
const ID16: usize = 16;
/// The largest `(namespace, key)` a row can embed.
const ROW_KEY: usize = limits::MAX_NAMESPACE + limits::MAX_KEY;

/// `ResourceRowV1`: a value, a holder, and counters.
const MAX_RESOURCE_ROW_BYTES: usize = limits::MAX_VALUE + ID32 + ROW_FRAMING_ALLOWANCE;

/// `ClientSessionV1`: a request hash and a saved reply, whose snapshot carries
/// the group id, a resource key and a holder.
const MAX_SESSION_ROW_BYTES: usize =
    ID32 + ID32 + ROW_KEY + ID32 + ID16 + ID32 + ROW_FRAMING_ALLOWANCE;

/// `MembershipIntentV1`: the encoded plan dominates, and §12.1 bounds that on
/// its encoded form rather than field by field.
const MAX_INTENT_ROW_BYTES: usize =
    limits::MAX_ENCODED_COMMAND + ID32 + ID32 + 4 * ID32 + ROW_FRAMING_ALLOWANCE;

/// `NodeRouteV1`: a ticket, an operation id, and a log id.
const MAX_ROUTE_ROW_BYTES: usize = limits::MAX_TICKET + ID16 + ID32 + ROW_FRAMING_ALLOWANCE;

/// `AuditRowV1`: two node ids and a resource key.
const MAX_AUDIT_ROW_BYTES: usize = ID32 + ID32 + ROW_KEY + ROW_FRAMING_ALLOWANCE;

/// Every redb failure in this module names `state.redb`.
///
/// A free function rather than a closure because redb's fallible operations
/// return four different error types, and one closure can only take one.
fn state_err(source: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::redb(Db::State, source)
}

// ── The durable state ────────────────────────────────────────────────────────

/// Everything `state.redb` holds, as one value.
///
/// The model plus the three things `CoordinationMetaV1` carries that the model
/// does not:
///
/// * the OpenRaft `StoredMembership`, which `applied_state` must return
///   verbatim. The model keeps only a lossy projection of it — the member set
///   and its log id — because those are what §9's rules read, and a projection
///   cannot reconstruct a joint configuration.
/// * §13's recovery provenance, which no transition reads.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DurableState {
    pub(crate) model: State,
    pub(crate) membership: StoredMembership<CoordinationNodeId, CoordinationNode>,
    pub(crate) recovery_export_hash: Option<Vec<u8>>,
    pub(crate) recovered_from: Option<RecoveredFromV1>,
}

impl DurableState {
    /// The group id every group-bound stored value must name.
    fn group_id(&self) -> [u8; 32] {
        self.model.authority.group_id
    }
}

/// Project OpenRaft's membership onto the roles §9 and §7.3 read.
///
/// The single derivation, shared by the loader and by the state machine's
/// native-entry path. §11.3 persists only the `StoredMembership`, so this runs
/// again at every open; a second implementation would mean a restart could
/// disagree with the running store about who the voters are.
pub(crate) fn project_membership(
    membership: &openraft::Membership<CoordinationNodeId, CoordinationNode>,
) -> CommittedMembership {
    let configs = membership.get_joint_config();
    // The **union** of every configuration, which is what OpenRaft means by a
    // voter: during a joint change `{A,B,C} -> {A,B,D}`, `C` still votes, and
    // taking only the target configuration dropped it. That made §9's route
    // protection let go of a current voter's route, and made the member set the
    // membership checks read disagree with the one OpenRaft uses for quorum.
    //
    // Completion is unaffected: it refuses a joint membership outright, and
    // with one configuration the union is that configuration.
    let voters: BTreeSet<[u8; 32]> = membership.voter_ids().map(|id| *id.as_bytes()).collect();
    // A learner is a node the map names that no configuration does -- which is
    // how OpenRaft represents one, and the reason the node map is not simply
    // the union of the configs.
    let learners: BTreeSet<[u8; 32]> = membership
        .nodes()
        .map(|(id, _)| *id.as_bytes())
        .filter(|id| {
            !configs
                .iter()
                .any(|set| set.iter().any(|c| c.as_bytes() == id))
        })
        .collect();
    CommittedMembership {
        voters,
        learners,
        joint: configs.len() > 1,
    }
}

// ── Meta ↔ CoordinationMetaV1 ────────────────────────────────────────────────

pub(crate) fn meta_to_wire(state: &DurableState) -> CoordinationMetaV1 {
    let authority = &state.model.authority;
    CoordinationMetaV1 {
        schema_version: super::identity::LOGICAL_SCHEMA_VERSION,
        group_id: authority.group_id.to_vec(),
        genesis_hash: authority.genesis_hash.to_vec(),
        authority_public_key: authority.public_key.to_vec(),
        authority_domain: authority.domain.clone(),
        deployment_profile_code: state.model.deployment_profile_code,
        last_applied: state.model.last_applied.as_ref().map(Into::into),
        membership: StoredMembershipV1::from(&state.membership),
        admin_revision: state.model.admin_revision,
        recovery_export_hash: state.recovery_export_hash.clone(),
        recovered_from: state.recovered_from.clone(),
        aggregate_growth_bytes: state.model.growth_bytes,
        aggregate_reserve_bytes: state.model.reserve_bytes,
        audit_floor: state.model.audit_floor.as_ref().map(Into::into),
    }
}

/// §7.1's authority domain: 1 to 128 ASCII bytes.
///
/// Not merely a maximum. An empty domain names no authority at all, and a
/// non-ASCII one is not a domain any signed document can be verified against --
/// the permit's own domain is compared bytewise, so a value that is merely
/// short enough is not a value.
fn check_authority_domain(what: &'static str, domain: &str) -> StorageResult<()> {
    let max = crate::coordination::model::authority::MAX_AUTHORITY_DOMAIN_BYTES;
    if domain.is_empty() {
        return Err(StorageError::corrupt(Db::State, what, "empty"));
    }
    if domain.len() > max {
        return Err(StorageError::corrupt(
            Db::State,
            what,
            format!("{} bytes, over the {max}-byte limit", domain.len()),
        ));
    }
    if !domain.is_ascii() {
        return Err(StorageError::corrupt(
            Db::State,
            what,
            "not ASCII; a domain is compared bytewise against signed documents",
        ));
    }
    Ok(())
}

/// Everything the **raw** stored metadata must satisfy.
///
/// Runs before the mirror conversions, because those are total in the wrong
/// direction for storage. `MembershipV1 -> Membership` deduplicates ids and
/// inserts a default node for any id a config names but the node map omits --
/// deliberately, so a peer's malformed message cannot panic inside OpenRaft --
/// and the same totality turns a corrupt row into a repaired one. §11.3 has no
/// repair path: a row that is not what this group wrote is data loss.
///
/// The membership check is a **canonical round trip** rather than a list of
/// field rules. Converting in and back out again and requiring equality catches
/// duplicates, manufactured nodes, unsorted configs and unsorted node maps in
/// one statement, and keeps catching whatever the conversion is taught to
/// normalise next. Everything this store wrote went out through the same
/// `From`, so a legitimate row round-trips by construction.
fn validate_meta_wire(meta: &CoordinationMetaV1) -> StorageResult<()> {
    let bad = |what: &'static str, detail: String| StorageError::corrupt(Db::State, what, detail);
    let width = |what: &'static str, bytes: &[u8], expected: usize| -> StorageResult<()> {
        if bytes.len() != expected {
            return Err(bad(
                what,
                format!("expected {expected} bytes, got {}", bytes.len()),
            ));
        }
        Ok(())
    };

    // The schema version is a compiled protocol constant, and a stored row
    // naming another one describes a logical schema this build does not
    // implement. It was decoded and then discarded, which is worse than not
    // storing it: the field read as checked.
    if meta.schema_version != super::identity::LOGICAL_SCHEMA_VERSION {
        return Err(StorageError::SchemaVersion {
            db: Db::State,
            found: meta.schema_version,
            expected: super::identity::LOGICAL_SCHEMA_VERSION,
        });
    }

    width("CoordinationMetaV1.group_id", &meta.group_id, 32)?;
    width("CoordinationMetaV1.genesis_hash", &meta.genesis_hash, 32)?;
    width(
        "CoordinationMetaV1.authority_public_key",
        &meta.authority_public_key,
        32,
    )?;
    check_authority_domain(
        "CoordinationMetaV1.authority_domain",
        &meta.authority_domain,
    )?;

    if let Some(hash) = &meta.recovery_export_hash {
        width("CoordinationMetaV1.recovery_export_hash", hash, 32)?;
    }
    if let Some(source) = &meta.recovered_from {
        width(
            "RecoveredFromV1.source_group_id",
            &source.source_group_id,
            32,
        )?;
        width(
            "RecoveredFromV1.source_genesis_hash",
            &source.source_genesis_hash,
            32,
        )?;
        width(
            "RecoveredFromV1.source_authority_public_key",
            &source.source_authority_public_key,
            32,
        )?;
        width("RecoveredFromV1.export_hash", &source.export_hash, 32)?;
        check_authority_domain(
            "RecoveredFromV1.source_authority_domain",
            &source.source_authority_domain,
        )?;
        if let Some(id) = &source.source_last_applied {
            width(
                "RecoveredFromV1.source_last_applied",
                &id.leader_node_id,
                32,
            )?;
        }
        // §13 ties the two together: the new genesis binds the BLAKE3 over the
        // imported body, and that is the same hash the provenance records. Two
        // fields naming one hash have exactly one coherent reading, and a row
        // where they disagree cannot say which export it came from.
        //
        // Only this direction is required. A group may hold an export hash
        // without full provenance -- §13 writes the provenance pre-initialize,
        // and a build that has the hash but not the record is behind, not
        // corrupt -- so the converse is not enforced.
        match &meta.recovery_export_hash {
            None => {
                return Err(bad(
                    "CoordinationMetaV1.recovery_export_hash",
                    "recovery provenance is recorded but the export hash it binds is absent".into(),
                ))
            }
            Some(hash) if *hash != source.export_hash => {
                return Err(bad(
                    "CoordinationMetaV1.recovery_export_hash",
                    "the recorded export hash and the provenance disagree".into(),
                ))
            }
            Some(_) => {}
        }
    }

    for (what, id) in [
        ("CoordinationMetaV1.last_applied", &meta.last_applied),
        ("CoordinationMetaV1.audit_floor", &meta.audit_floor),
        ("StoredMembershipV1.log_id", &meta.membership.log_id),
    ] {
        if let Some(id) = id {
            width(what, &id.leader_node_id, 32)?;
        }
    }

    // The canonical round trip. Skipped only for the one shape that is legally
    // empty -- a group that has committed no membership -- which the mirror
    // refuses by design.
    let wire = &meta.membership.membership;
    if !(wire.configs.is_empty() && wire.nodes.is_empty()) {
        let converted =
            openraft::Membership::<CoordinationNodeId, CoordinationNode>::try_from(wire)
                .map_err(|err| bad("StoredMembershipV1.membership", format!("{err:?}")))?;
        let canonical = crate::coordination::raft::MembershipV1::from(&converted);
        if canonical != *wire {
            return Err(bad(
                "StoredMembershipV1.membership",
                "the stored membership is not the canonical encoding of itself: it holds \
                 duplicate ids, an id no node map entry covers, or an unsorted config or \
                 node map, and converting it would silently repair it"
                    .into(),
            ));
        }
    }

    Ok(())
}

/// Narrow a stored meta row into the state it describes, with no rows yet.
///
/// The row tables are loaded separately and then validated against this; that
/// order is deliberate, since the ceilings and caps the loader applies come
/// from constants rather than from anything the meta claims.
pub(crate) fn meta_from_wire(meta: &CoordinationMetaV1) -> StorageResult<DurableState> {
    let bad = |what: &'static str, detail: String| StorageError::corrupt(Db::State, what, detail);

    let array32 = |what: &'static str, bytes: &[u8]| -> StorageResult<[u8; 32]> {
        bytes
            .try_into()
            .map_err(|_| bad(what, format!("expected 32 bytes, got {}", bytes.len())))
    };

    // Everything the raw wire record must satisfy, checked **before** any
    // conversion runs. The conversions are lossy in ways that are correct for a
    // peer's message and wrong for a stored row: the membership mirror
    // deduplicates node ids and manufactures node-map entries a config
    // references, which for storage means a malformed row is silently repaired
    // into a plausible one. A repair is a rewrite of replicated state that no
    // other voter performed.
    validate_meta_wire(meta)?;

    // An empty config set is "no membership has been committed yet", which is
    // exactly what OpenRaft's own `StoredMembership::default()` is and what a
    // freshly bound store holds. The mirror conversion refuses it -- correctly,
    // since an empty *joint config* on a running group is a corrupt record --
    // so the two cases are separated here rather than by weakening the mirror.
    let membership = if meta.membership.log_id.is_none()
        && meta.membership.membership.configs.is_empty()
        && meta.membership.membership.nodes.is_empty()
    {
        StoredMembership::<CoordinationNodeId, CoordinationNode>::default()
    } else {
        StoredMembership::<CoordinationNodeId, CoordinationNode>::try_from(&meta.membership)
            .map_err(|err| bad("CoordinationMetaV1.membership", format!("{err:?}")))?
    };

    // The model's view is *derived* from the stored membership, never stored
    // beside it: two copies of the same fact are two things that can disagree,
    // and §9 protects a route by asking whether its node is a member.
    let projection = project_membership(membership.membership());
    let membership_log_id = membership.log_id().as_ref().map(LogId::from);

    let log_id = |what: &'static str, wire: &Option<LogIdV1>| -> StorageResult<Option<LogId>> {
        wire.as_ref()
            .map(|id| LogId::try_from(id).map_err(|err| bad(what, format!("{err:?}"))))
            .transpose()
    };

    Ok(DurableState {
        model: State {
            authority: GroupAuthority {
                public_key: array32(
                    "CoordinationMetaV1.authority_public_key",
                    &meta.authority_public_key,
                )?,
                domain: meta.authority_domain.clone(),
                group_id: array32("CoordinationMetaV1.group_id", &meta.group_id)?,
                genesis_hash: array32("CoordinationMetaV1.genesis_hash", &meta.genesis_hash)?,
            },
            deployment_profile_code: meta.deployment_profile_code,
            last_applied: log_id("CoordinationMetaV1.last_applied", &meta.last_applied)?,
            admin_revision: meta.admin_revision,
            growth_bytes: meta.aggregate_growth_bytes,
            reserve_bytes: meta.aggregate_reserve_bytes,
            audit_floor: log_id("CoordinationMetaV1.audit_floor", &meta.audit_floor)?,
            membership: projection,
            membership_log_id,
            // Filled by `load`, and `audit_tip` is rebuilt from the rows
            // rather than persisted at all.
            ..State::default()
        },
        membership,
        recovery_export_hash: meta.recovery_export_hash.clone(),
        recovered_from: meta.recovered_from.clone(),
    })
}

// ── Loading ──────────────────────────────────────────────────────────────────

/// Read and validate the complete state.
///
/// Every row is narrowed through its declared root with that table's ceiling
/// applied before allocation, and the result is then checked as a whole. A
/// loader that validated rows individually and stopped there would accept a
/// state whose every row is well-formed and whose counters describe a different
/// state entirely.
pub(crate) fn load(database: &Database) -> StorageResult<DurableState> {
    let read = database.begin_read().map_err(state_err)?;

    let meta_bytes = {
        let table = read.open_table(STATE_META).map_err(state_err)?;
        let value =
            table
                .get(KEY_META)
                .map_err(state_err)?
                .ok_or(StorageError::MissingMetaKey {
                    db: Db::State,
                    key: KEY_META,
                })?;
        super::codec::copy_stored_bounded(
            Db::State,
            KEY_META,
            value.value(),
            MAX_COORDINATION_META_BYTES,
        )?
    };
    let meta: CoordinationMetaV1 = decode_persisted_bounded(
        Db::State,
        "CoordinationMetaV1",
        &meta_bytes,
        MAX_COORDINATION_META_BYTES,
    )?;
    let mut state = meta_from_wire(&meta)?;
    let group = state.group_id();

    let corrupt =
        |what: &'static str, detail: String| StorageError::corrupt(Db::State, what, detail);

    // Resources.
    {
        let table = read.open_table(RESOURCES).map_err(state_err)?;
        for entry in table.iter().map_err(state_err)? {
            let (key, value) = entry.map_err(state_err)?;
            let (namespace, raw) = key.value();
            let resource = ResourceKey::new(namespace, raw.to_vec())
                .map_err(|err| corrupt("resources.key", format!("{err:?}")))?;
            let wire: ResourceRowV1 = decode_persisted_bounded(
                Db::State,
                "ResourceRowV1",
                value.value(),
                MAX_RESOURCE_ROW_BYTES,
            )?;
            let row = ResourceRow::try_from(&wire)
                .map_err(|err| corrupt("ResourceRowV1", format!("{err:?}")))?;
            state.model.resources.insert(resource, row);
        }
    }

    // Grants. The value is a redb `u64`, so there is no Fory root here -- but
    // §8.1's rights bits still have to be checked: an undefined bit is a grant
    // whose meaning this build cannot evaluate, not forward compatibility.
    {
        let table = read.open_table(ACL).map_err(state_err)?;
        for entry in table.iter().map_err(state_err)? {
            let (key, value) = entry.map_err(state_err)?;
            let (namespace, raw, subject) = key.value();
            let resource = ResourceKey::new(namespace, raw.to_vec())
                .map_err(|err| corrupt("resource_acl.key", format!("{err:?}")))?;
            let rights = value.value();
            if !limits::rights_are_known(rights) {
                return Err(corrupt(
                    "resource_acl.rights",
                    format!("rights {rights:#x} include a bit v1 does not define"),
                ));
            }
            state.model.acl.insert((resource, subject), rights);
        }
    }

    // Administrators.
    {
        let table = read.open_table(ADMINS).map_err(state_err)?;
        for entry in table.iter().map_err(state_err)? {
            let (key, _) = entry.map_err(state_err)?;
            state.model.admins.insert(key.value());
        }
    }

    // Sessions.
    {
        let table = read.open_table(SESSIONS).map_err(state_err)?;
        for entry in table.iter().map_err(state_err)? {
            let (key, value) = entry.map_err(state_err)?;
            let wire: ClientSessionV1 = decode_persisted_bounded(
                Db::State,
                "ClientSessionV1",
                value.value(),
                MAX_SESSION_ROW_BYTES,
            )?;
            let row = session_from_wire(&group, &wire)
                .map_err(|err| corrupt("ClientSessionV1", format!("{err:?}")))?;
            state.model.sessions.insert(key.value(), row);
        }
    }

    // Membership intents.
    {
        let table = read.open_table(INTENTS).map_err(state_err)?;
        for entry in table.iter().map_err(state_err)? {
            let (key, value) = entry.map_err(state_err)?;
            let operation_id = key.value();
            let wire: MembershipIntentV1 = decode_persisted_bounded(
                Db::State,
                "MembershipIntentV1",
                value.value(),
                MAX_INTENT_ROW_BYTES,
            )?;
            let row = intent_from_wire(&operation_id, &wire)
                .map_err(|err| corrupt("MembershipIntentV1", format!("{err:?}")))?;
            state.model.intents.insert(operation_id, row);
        }
    }

    // Routes.
    {
        let table = read.open_table(ROUTES).map_err(state_err)?;
        for entry in table.iter().map_err(state_err)? {
            let (key, value) = entry.map_err(state_err)?;
            let wire: NodeRouteV1 = decode_persisted_bounded(
                Db::State,
                "NodeRouteV1",
                value.value(),
                MAX_ROUTE_ROW_BYTES,
            )?;
            let row =
                Route::try_from(&wire).map_err(|err| corrupt("NodeRouteV1", format!("{err:?}")))?;
            state.model.routes.insert(key.value(), row);
        }
    }

    // Audit.
    {
        let table = read.open_table(AUDIT).map_err(state_err)?;
        for entry in table.iter().map_err(state_err)? {
            let (key, value) = entry.map_err(state_err)?;
            let wire: AuditRowV1 = decode_persisted_bounded(
                Db::State,
                "AuditRowV1",
                value.value(),
                MAX_AUDIT_ROW_BYTES,
            )?;
            let row = AuditRow::try_from(&wire)
                .map_err(|err| corrupt("AuditRowV1", format!("{err:?}")))?;
            state.model.audit.insert(key.value(), row);
        }
    }

    // The tip is **rebuilt**, never persisted: it is a summary of the rows, and
    // a summary read back from disk beside the rows it summarises is a second
    // copy that can disagree with them.
    state.model.audit_tip = rebuild_audit_tip(&state.model.audit);

    validate(&state)?;
    Ok(state)
}

/// The full log id of the newest retained audit row.
///
/// By **index**, not by table order. The table is keyed `(term, index)`, so
/// redb sorts it by term first, and `next_back()` returns the highest-term row
/// -- which is the newest entry in any real history and is not the newest row
/// in a corrupt one. A row at a higher term and a lower index would become the
/// reported tip and hide the genuinely newest entry, which is exactly the kind
/// of quiet substitution the tip exists to prevent.
///
/// `check_audit_key_order` separately refuses a table where the two orderings
/// disagree, so on a valid state this and `next_back()` agree; taking the max
/// by index means the tip is right even while the disagreement is being
/// reported.
fn rebuild_audit_tip(audit: &BTreeMap<(u64, u64), AuditRow>) -> Option<LogId> {
    audit
        .iter()
        .max_by_key(|((_, index), _)| *index)
        .map(|((term, index), row)| LogId::new(*term, row.leader_node_id, *index))
}

// ── Validation ───────────────────────────────────────────────────────────────

/// Check a loaded state against §11.3 and §12.1.
///
/// Every derived value is recomputed from the rows and compared with what was
/// persisted. Nothing here reads the transition, and nothing here repairs: a
/// state that fails is a state no sequence of §11.3 transactions could have
/// written, so continuing would mean serving replicated state that disagrees
/// with a peer's.
pub(crate) fn validate(state: &DurableState) -> StorageResult<()> {
    validate_meta(state)?;
    validate_rows(state)
}

/// The half of the check that reads only the metadata row.
///
/// Separate because it is answerable about a `CoordinationMetaV1` on its own --
/// a snapshot header carries one, and so does a store whose tables have not
/// been read yet -- and because the two halves have different subjects. These
/// are constraints on the *binding*: the versions and widths `meta_from_wire`
/// has already narrowed, an authority key that is a key, a membership its
/// profile permits, counters within their budgets, and an audit floor that does
/// not run past what was applied.
///
/// It deliberately says nothing about rows. A maximal legal metadata row
/// carries budget-sized counters and cannot pass [`validate_rows`] without the
/// hundred thousand rows that would charge them -- correctly, and that is why
/// the two are not one function.
pub(crate) fn validate_meta(state: &DurableState) -> StorageResult<()> {
    let bad = |what: &'static str, detail: String| StorageError::corrupt(Db::State, what, detail);
    let model = &state.model;

    // An authority key is a curve point, not 32 arbitrary bytes. Checked here
    // rather than at first use: a group whose replicated key cannot verify
    // anything can never accept a join permit, and discovering that when the
    // first membership change arrives is discovering it too late.
    if ed25519_dalek::VerifyingKey::from_bytes(&model.authority.public_key).is_err() {
        return Err(bad(
            "authority_public_key",
            "the stored key does not decompress to an Ed25519 public key".into(),
        ));
    }
    check_authority_domain("authority_domain", &model.authority.domain)?;

    // §12.1's budgets bound the counters whatever the rows say. A counter above
    // its budget describes a state apply would never have admitted.
    for (what, value, budget) in [
        (
            "aggregate_growth_bytes",
            model.growth_bytes,
            limits::GROWTH_BUDGET,
        ),
        (
            "aggregate_reserve_bytes",
            model.reserve_bytes,
            limits::RESERVE_BUDGET,
        ),
    ] {
        if value > budget {
            return Err(bad(
                what,
                format!("{value} bytes, over the {budget}-byte budget"),
            ));
        }
    }

    // ── §7.1 profile and §9 membership ──────────────────────────────────────
    //
    // The profile is checked whatever the membership looks like. An earlier
    // version only reached this when the last config was non-empty, so a group
    // whose genesis named a profile this build does not implement opened
    // cleanly for as long as it had committed no membership -- and then decided
    // its first change against a rule it had never been admitted under.
    if !shape::PROFILES.contains(&model.deployment_profile_code) {
        return Err(bad(
            "deployment_profile_code",
            format!(
                "profile {} is not one §7.1 declares",
                model.deployment_profile_code
            ),
        ));
    }

    let configs = state.membership.membership().get_joint_config();
    // §9 has exactly two shapes: uniform, and joint during one change. Three
    // configurations is not a membership OpenRaft produces or Aster can reason
    // about.
    if configs.len() > 2 {
        return Err(bad(
            "membership",
            format!(
                "{} joint configurations; §9 allows at most two",
                configs.len()
            ),
        ));
    }
    // **Every** configuration, not only the last. A joint membership is legal
    // only if both halves are, and checking the target alone would admit a
    // change out of a configuration this profile never permitted -- which is
    // the half a voter has to be able to reach quorum in while the change is in
    // flight.
    for (n, config) in configs.iter().enumerate() {
        if shape::profile_permits_voters(model.deployment_profile_code, config.len()) {
            return Err(bad(
                "membership",
                format!(
                    "configuration {n} holds {} voters, which profile {} does not permit",
                    config.len(),
                    model.deployment_profile_code
                ),
            ));
        }
    }
    // §9's learner envelope: the departing retained learners, the new voters
    // the controller has added as learners and not yet promoted, and the
    // arriving retained learners. See `shape::MAX_LEARNER_ENVELOPE` for why the
    // sum is the bound and not the retained cap.
    //
    // There is no tighter settled-state rule. `Failed` is reachable from any
    // active phase and changes no membership, so a group can legally sit at the
    // envelope with nothing in flight.
    if model.membership.learners.len() > shape::MAX_LEARNER_ENVELOPE {
        return Err(bad(
            "membership",
            format!(
                "{} learners, over the {}-learner envelope",
                model.membership.learners.len(),
                shape::MAX_LEARNER_ENVELOPE
            ),
        ));
    }

    // A membership that names anyone was committed by an entry, and an entry
    // has a log id. The two move together or the record is not one this group
    // wrote.
    let membership_log_id = state.membership.log_id().as_ref().map(LogId::from);
    let named = !configs.is_empty() || state.membership.membership().nodes().next().is_some();
    if named != membership_log_id.is_some() {
        return Err(bad(
            "membership",
            if named {
                "a committed membership with no log id".into()
            } else {
                "a membership log id with no membership".into()
            },
        ));
    }

    // A membership entry is an applied entry, so its id cannot sit above
    // `last_applied` -- and at the same index it must *be* that entry, not a
    // different one from another term or leader. The second case is what a
    // maximum-index comparison cannot see.
    if let Some(id) = membership_log_id {
        match model.last_applied {
            None => {
                return Err(bad(
                    "membership",
                    format!("a membership at {id:?} with nothing applied"),
                ))
            }
            Some(last) if id.index > last.index => {
                return Err(bad(
                    "membership",
                    format!("{id:?} runs ahead of last_applied {last:?}"),
                ))
            }
            Some(last) if id.index == last.index && id != last => {
                return Err(bad(
                    "membership",
                    format!("{id:?} and last_applied {last:?} are different entries at one index"),
                ))
            }
            _ => {}
        }
    }

    // The model's projection is derived from the stored membership at load, so
    // this compares a derivation with its source rather than two persisted
    // copies. It earns its place on the paths that do *not* go through
    // `meta_from_wire` -- a snapshot install, and any hand-built state -- and
    // as a guard against the derivation itself drifting.
    let projection = project_membership(state.membership.membership());
    if model.membership != projection {
        return Err(bad(
            "membership",
            "the membership projection does not match the stored membership".into(),
        ));
    }
    if model.membership_log_id != membership_log_id {
        return Err(bad(
            "membership",
            "the membership log id does not match the stored membership".into(),
        ));
    }

    // An audit floor names a row that was retained, so it cannot sit above the
    // last entry applied -- with or without any rows to compare it against.
    if let Some(floor) = model.audit_floor {
        match model.last_applied {
            None => {
                return Err(bad(
                    "audit_floor",
                    format!("a floor at {floor:?} with nothing applied"),
                ))
            }
            Some(last) if floor.index > last.index => {
                return Err(bad(
                    "audit_floor",
                    format!("{floor:?} runs ahead of last_applied {last:?}"),
                ))
            }
            _ => {}
        }
    }

    Ok(())
}

/// One §11.3 or §12.1 rule a stored state must satisfy, named so a failure says
/// which one and so the negative tests can be checked against the list.
///
/// A table rather than a run of `if`s because the previous shape ended after
/// four checks and nobody could see that it had: an impossible state -- an
/// oversized value, a zero-right grant, a route whose ticket names another
/// node -- loaded cleanly. A list makes the coverage countable, and
/// `every_invariant_has_an_independent_negative` fails when a rule is added
/// without one.
struct Invariant {
    name: &'static str,
    check: fn(&DurableState) -> Result<(), String>,
}

/// Every rule, in the order a load applies them.
///
/// Deliberately **not** derived from the transition. These are read off §11.3,
/// §12.1, §9 and §5.2 and implemented here, so a state the transition produced
/// wrongly is refused rather than blessed by the same code that made it.
static INVARIANTS: &[Invariant] = &[
    Invariant {
        name: "table-row-caps",
        check: check_table_caps,
    },
    Invariant {
        name: "grants-per-resource",
        check: check_grants_per_resource,
    },
    Invariant {
        name: "sessions-per-actor",
        check: check_sessions_per_actor,
    },
    Invariant {
        name: "active-clients",
        check: check_active_clients,
    },
    Invariant {
        name: "aggregate-counters",
        check: check_aggregate_counters,
    },
    Invariant {
        name: "audit-tip",
        check: check_audit_tip,
    },
    Invariant {
        name: "audit-floor",
        check: check_audit_floor,
    },
    Invariant {
        name: "audit-key-order",
        check: check_audit_key_order,
    },
    Invariant {
        name: "audit-row-shape",
        check: check_audit_rows,
    },
    Invariant {
        name: "resource-row-shape",
        check: check_resource_rows,
    },
    Invariant {
        name: "grant-row-shape",
        check: check_grant_rows,
    },
    Invariant {
        name: "admin-revision",
        check: check_admin_revision,
    },
    Invariant {
        name: "session-row-shape",
        check: check_session_rows,
    },
    Invariant {
        name: "intent-row-shape",
        check: check_intent_rows,
    },
    Invariant {
        name: "route-row-shape",
        check: check_route_rows,
    },
    Invariant {
        name: "stamped-log-ids",
        check: check_stamped_log_ids,
    },
];

/// The half of the check that reads the rows.
///
/// Every derived value is recomputed and compared, and every row is checked
/// against the shape §12.1 declares for it. Nothing here repairs: §11.3 makes
/// rows, counters and `last_applied` one transaction, so a state where they
/// disagree is one no sequence of commits could have produced.
fn validate_rows(state: &DurableState) -> StorageResult<()> {
    for invariant in INVARIANTS {
        (invariant.check)(state)
            .map_err(|detail| StorageError::corrupt(Db::State, invariant.name, detail))?;
    }
    Ok(())
}

// ── §12.1 slot caps ─────────────────────────────────────────────────────────

fn check_table_caps(state: &DurableState) -> Result<(), String> {
    let model = &state.model;
    let caps: [(&str, u64, u64); 7] = [
        (
            "resources",
            model.resources.len() as u64,
            limits::MAX_RESOURCES_PER_GROUP,
        ),
        (
            "resource_acl",
            model.acl.len() as u64,
            limits::MAX_ACL_ROWS_PER_GROUP,
        ),
        (
            "group_admins",
            model.admins.len() as u64,
            limits::MAX_ADMINS_PER_GROUP,
        ),
        (
            "client_sessions",
            model.sessions.len() as u64,
            limits::MAX_RETAINED_SESSIONS_PER_GROUP,
        ),
        (
            "membership_intents",
            model.intents.len() as u64,
            limits::MAX_INTENT_ROWS,
        ),
        (
            "node_routes",
            model.routes.len() as u64,
            limits::MAX_RETAINED_ROUTES,
        ),
        ("audit", model.audit.len() as u64, limits::MAX_AUDIT_ROWS),
    ];
    for (table, count, cap) in caps {
        if count > cap {
            return Err(format!(
                "{table} holds {count} rows, over the {cap}-row cap"
            ));
        }
    }
    Ok(())
}

/// A group total in range does not imply the per-resource one: one resource may
/// hold every grant.
fn check_grants_per_resource(state: &DurableState) -> Result<(), String> {
    let mut per_resource: BTreeMap<&ResourceKey, u64> = BTreeMap::new();
    for (resource, _) in state.model.acl.keys() {
        *per_resource.entry(resource).or_default() += 1;
    }
    match per_resource
        .into_iter()
        .find(|(_, n)| *n > limits::MAX_GRANTS_PER_RESOURCE)
    {
        Some((resource, count)) => Err(format!(
            "{resource:?} holds {count} grants, over the {}-grant cap",
            limits::MAX_GRANTS_PER_RESOURCE
        )),
        None => Ok(()),
    }
}

fn check_sessions_per_actor(state: &DurableState) -> Result<(), String> {
    let mut retained: BTreeMap<[u8; 32], u64> = BTreeMap::new();
    for (actor, _) in state.model.sessions.keys() {
        *retained.entry(*actor).or_default() += 1;
    }
    match retained
        .into_iter()
        .find(|(_, n)| *n > limits::MAX_RETAINED_SESSIONS_PER_ACTOR)
    {
        Some((actor, count)) => Err(format!(
            "actor {actor:02x?} holds {count} retained sessions, over the {} cap",
            limits::MAX_RETAINED_SESSIONS_PER_ACTOR
        )),
        None => Ok(()),
    }
}

fn check_active_clients(state: &DurableState) -> Result<(), String> {
    let mut per_actor: BTreeMap<[u8; 32], u64> = BTreeMap::new();
    let mut group = 0u64;
    for ((actor, _), session) in &state.model.sessions {
        if !session.retired {
            *per_actor.entry(*actor).or_default() += 1;
            group += 1;
        }
    }
    if let Some((actor, count)) = per_actor
        .into_iter()
        .find(|(_, n)| *n > limits::MAX_ACTIVE_CLIENTS_PER_ACTOR)
    {
        return Err(format!(
            "actor {actor:02x?} holds {count} active clients, over the {} cap",
            limits::MAX_ACTIVE_CLIENTS_PER_ACTOR
        ));
    }
    if group > limits::MAX_ACTIVE_CLIENTS_PER_GROUP {
        return Err(format!(
            "{group} active clients, over the {}-client cap",
            limits::MAX_ACTIVE_CLIENTS_PER_GROUP
        ));
    }
    Ok(())
}

// ── §12.1 counters ──────────────────────────────────────────────────────────

/// Recomputed from the rows and compared. The persisted figure is evidence,
/// never an input: apply maintains it incrementally, so the only thing that can
/// catch an incremental update that went wrong is the full sum.
fn check_aggregate_counters(state: &DurableState) -> Result<(), String> {
    let model = &state.model;
    let growth = model.recompute_growth();
    if model.growth_bytes != growth {
        return Err(format!(
            "growth persisted {} but the rows charge {growth}",
            model.growth_bytes
        ));
    }
    let reserve = model.recompute_reserve();
    if model.reserve_bytes != reserve {
        return Err(format!(
            "reserve persisted {} but the rows charge {reserve}",
            model.reserve_bytes
        ));
    }
    Ok(())
}

// ── §11.3 audit identity ────────────────────────────────────────────────────

fn check_audit_tip(state: &DurableState) -> Result<(), String> {
    let model = &state.model;
    let tip = rebuild_audit_tip(&model.audit);
    if model.audit_tip != tip {
        return Err(format!(
            "state holds {:?} but the rows give {tip:?}",
            model.audit_tip
        ));
    }
    match (tip, model.last_applied) {
        (Some(tip), None) => Err(format!("an audit row at {tip:?} with nothing applied")),
        (Some(tip), Some(last)) if tip.index > last.index => {
            Err(format!("{tip:?} runs ahead of last_applied {last:?}"))
        }
        // The case a maximum-index comparison cannot see: the same index under
        // a different term or leader is a *different* entry.
        (Some(tip), Some(last)) if tip.index == last.index && tip != last => Err(format!(
            "{tip:?} and last_applied {last:?} are different entries at one index"
        )),
        _ => Ok(()),
    }
}

/// The floor is the whole log id of the oldest retained row whenever anything
/// has been pruned. With rows present it is checkable exactly; with none, the
/// floor names a row that is gone and there is nothing to compare.
fn check_audit_floor(state: &DurableState) -> Result<(), String> {
    let model = &state.model;
    let Some(floor) = model.audit_floor else {
        return Ok(());
    };
    let Some(((term, index), oldest)) = model.audit.iter().next() else {
        return Ok(());
    };
    let expected = LogId::new(*term, oldest.leader_node_id, *index);
    if floor != expected {
        return Err(format!(
            "persisted {floor:?} but the oldest retained row is {expected:?}"
        ));
    }
    Ok(())
}

/// The audit table is keyed `(term, index)`, so redb orders it by **term
/// first**. Every rule that treats the last key as the newest row -- the tip
/// rebuild, the floor, the range `audit_stream` walks -- is only sound while
/// the two orderings agree, which they do in any real history because term
/// never decreases as index grows.
///
/// A corrupt row at a higher term and a lower index breaks that silently: it
/// sorts last, becomes the reported tip, and hides the genuinely newest row.
/// So the agreement is checked rather than assumed.
fn check_audit_key_order(state: &DurableState) -> Result<(), String> {
    let mut previous: Option<(u64, u64)> = None;
    for (term, index) in state.model.audit.keys() {
        if let Some((prev_term, prev_index)) = previous {
            if *index <= prev_index {
                return Err(format!(
                    "row ({term}, {index}) sorts after ({prev_term}, {prev_index}) by term \
                     while its index is not greater, so table order and log order disagree"
                ));
            }
        }
        previous = Some((*term, *index));
    }
    Ok(())
}

/// §5.2 declares eleven client method codes and two system ones. A row naming
/// anything else describes an operation this build cannot interpret, and its
/// result code cannot be placed in either code space.
fn check_audit_rows(state: &DurableState) -> Result<(), String> {
    use crate::coordination::contract::{
        SYS_EXPIRED, SYS_EXPIRE_MALFORMED, SYS_EXPIRY_STALE, SYS_MEMBERSHIP_ADVANCED,
        SYS_MEMBERSHIP_MALFORMED, SYS_MEMBERSHIP_PHASE_MISMATCH,
    };
    use crate::coordination::model::apply::{OPERATION_EXPIRE, OPERATION_MEMBERSHIP_PROGRESS};
    use crate::coordination::reason::Outcome;

    for (at, row) in &state.model.audit {
        // The exact pairing, not merely the code space. `Expire` reporting
        // `SYS_MEMBERSHIP_MALFORMED`, or `Acquire` reporting `Released`, are
        // both in the right space and describe results no apply path produces.
        let allowed: &[i32] = match row.operation_code {
            1 => &[Outcome::Granted as i32, Outcome::Rejected as i32],
            2 => &[Outcome::Renewed as i32, Outcome::Rejected as i32],
            3 => &[Outcome::Mutated as i32, Outcome::Rejected as i32],
            // Release and release-with-value share an outcome; they differ in
            // what they write, not in what they report.
            4 | 5 => &[Outcome::Released as i32, Outcome::Rejected as i32],
            6 => &[Outcome::Revoked as i32, Outcome::Rejected as i32],
            7 => &[Outcome::GrantUpdated as i32, Outcome::Rejected as i32],
            8 => &[Outcome::AdminUpdated as i32, Outcome::Rejected as i32],
            9 => &[Outcome::MembershipAccepted as i32, Outcome::Rejected as i32],
            10 => &[Outcome::ClientRetired as i32, Outcome::Rejected as i32],
            11 => &[
                Outcome::MembershipCancelled as i32,
                Outcome::Rejected as i32,
            ],
            OPERATION_EXPIRE => &[SYS_EXPIRED, SYS_EXPIRY_STALE, SYS_EXPIRE_MALFORMED],
            OPERATION_MEMBERSHIP_PROGRESS => &[
                SYS_MEMBERSHIP_ADVANCED,
                SYS_MEMBERSHIP_PHASE_MISMATCH,
                SYS_MEMBERSHIP_MALFORMED,
            ],
            other => {
                return Err(format!(
                    "row {at:?} names operation code {other}, which §5.2 does not declare"
                ))
            }
        };

        // Which code space the row is in is decided by its operation, and the
        // decoder used the same rule to build it -- so a mismatch here means
        // the stored code was reconstructed into the wrong space.
        let system = matches!(
            row.operation_code,
            OPERATION_EXPIRE | OPERATION_MEMBERSHIP_PROGRESS
        );
        if system == row.result.is_client() {
            return Err(format!(
                "row {at:?} pairs operation code {} with a {} result",
                row.operation_code,
                if row.result.is_client() {
                    "client"
                } else {
                    "system"
                }
            ));
        }
        if !allowed.contains(&row.result.code()) {
            return Err(format!(
                "row {at:?} records result {} for operation {}, which that operation cannot \
                 produce",
                row.result.code(),
                row.operation_code
            ));
        }

        for (what, value) in [("epoch", row.epoch), ("value_revision", row.value_revision)] {
            if value > limits::MAX_COUNTER {
                return Err(format!(
                    "row {at:?} records {what} {value}, over the {} ceiling",
                    limits::MAX_COUNTER
                ));
            }
        }
    }
    Ok(())
}

// ── §12.1 row shapes ────────────────────────────────────────────────────────

/// The TTLs a stored row can legally carry.
///
/// A **held** row's TTL is the lease it is holding, and §12.1 bounds every
/// `acquire` that could have set one -- so zero is not among them. A held lease
/// with no duration is a lease that expired the moment it was granted, and the
/// expiry path would never fire for it.
///
/// A **free** row may additionally be zero: a resource created by `set_grant`
/// never had a lease, and a released one keeps whatever its last lease had,
/// which was bounded when it was set. Anything else is a duration no command
/// could have produced.
fn ttl_is_legal(ttl_ms: u64, held: bool) -> bool {
    if (limits::MIN_TTL_MS..=limits::MAX_TTL_MS).contains(&ttl_ms) {
        return true;
    }
    !held && ttl_ms == 0
}

fn check_resource_rows(state: &DurableState) -> Result<(), String> {
    for (key, row) in &state.model.resources {
        if row.value.len() > limits::MAX_VALUE {
            return Err(format!(
                "{key:?} holds a {}-byte value, over the {}-byte limit",
                row.value.len(),
                limits::MAX_VALUE
            ));
        }
        // A **free** row is bounded too. It keeps whatever TTL its last lease
        // had, and that value was bounded when it was set -- so the legal set
        // is the same one plus zero, not "anything at all". Exempting free rows
        // let an arbitrary duration sit in state until the next acquire
        // overwrote it, and be reported by every read in between.
        if !ttl_is_legal(row.ttl_ms, row.held) {
            return Err(format!(
                "{key:?} is {} with a {}ms TTL, which is not a duration §12.1 admits",
                if row.held { "held" } else { "free" },
                row.ttl_ms
            ));
        }
        for (what, value) in [
            ("epoch", row.epoch),
            ("lease_revision", row.lease_revision),
            ("value_revision", row.value_revision),
            ("acl_revision", row.acl_revision),
        ] {
            if value > limits::MAX_COUNTER {
                return Err(format!(
                    "{key:?} holds {what} {value}, over the {} ceiling",
                    limits::MAX_COUNTER
                ));
            }
        }
    }
    Ok(())
}

/// A grant row is created and removed with its resource row in one step
/// (§8.1), and rights of zero *is* the removal, so neither an orphan nor an
/// empty grant is a state apply can produce.
fn check_grant_rows(state: &DurableState) -> Result<(), String> {
    for ((resource, subject), rights) in &state.model.acl {
        if *rights == 0 {
            return Err(format!(
                "{resource:?} grants {subject:02x?} no rights; zero rights is how a grant is \
                 removed, so the row should not exist"
            ));
        }
        if !limits::rights_are_known(*rights) {
            return Err(format!(
                "{resource:?} grants {subject:02x?} rights {rights:#x}, which include a bit v1 \
                 does not define"
            ));
        }
        if !state.model.resources.contains_key(resource) {
            return Err(format!(
                "{resource:?} has a grant for {subject:02x?} but no resource row; the two move \
                 in one step"
            ));
        }
    }
    Ok(())
}

fn check_admin_revision(state: &DurableState) -> Result<(), String> {
    if state.model.admin_revision > limits::MAX_COUNTER {
        return Err(format!(
            "admin_revision {} is over the {} ceiling",
            state.model.admin_revision,
            limits::MAX_COUNTER
        ));
    }
    Ok(())
}

fn check_session_rows(state: &DurableState) -> Result<(), String> {
    for ((actor, client), session) in &state.model.sessions {
        // §5.2 stops client counters at `i64::MAX`. The field is a `u64` read
        // back from storage, so a corrupt `u64::MAX` wraps to 0 in release and
        // a stale sequence then looks like the exact next one.
        if session.last_sequence > limits::MAX_COUNTER {
            return Err(format!(
                "session {actor:02x?}/{client:02x?} holds sequence {}, over the {} ceiling",
                session.last_sequence,
                limits::MAX_COUNTER
            ));
        }
        // §11.3: `last_result` is `None` **only** for a sequence-zero
        // tombstone. Any other missing result is a consumed sequence whose
        // answer was lost, which is the one thing a client's journal cannot
        // survive.
        if session.last_result.is_none() && session.last_sequence != 0 {
            return Err(format!(
                "session {actor:02x?}/{client:02x?} consumed sequence {} but saved no result",
                session.last_sequence
            ));
        }
        let Some(saved) = &session.last_result else {
            continue;
        };
        // §5.2's pairings, which the model asserts after every applied step and
        // which a stored row must therefore also satisfy.
        if saved.reason.is_reserved() {
            return Err(format!(
                "session {actor:02x?}/{client:02x?} saved reserved reason {:?}",
                saved.reason
            ));
        }
        if saved.outcome != crate::coordination::reason::Outcome::Rejected
            && saved.reason != crate::coordination::reason::Reason::None
        {
            return Err(format!(
                "session {actor:02x?}/{client:02x?} saved {:?} with reason {:?}; only a \
                 rejection carries one",
                saved.outcome, saved.reason
            ));
        }
        // The saved snapshot is what a replay reports, so its counters are as
        // client-visible as the row's own -- and they came from a resource row
        // that was itself bounded when it was written.
        for (what, value) in [
            ("acl_revision", saved.acl_revision),
            ("admin_revision", saved.admin_revision),
        ] {
            if value > limits::MAX_COUNTER {
                return Err(format!(
                    "session {actor:02x?}/{client:02x?} saved {what} {value}, over the {} \
                     ceiling",
                    limits::MAX_COUNTER
                ));
            }
        }
        let Some(snapshot) = &saved.snapshot else {
            continue;
        };
        for (what, value) in [
            ("epoch", snapshot.epoch),
            ("lease_revision", snapshot.lease_revision),
            ("value_revision", snapshot.value_revision),
            ("acl_revision", snapshot.acl_revision),
        ] {
            if value > limits::MAX_COUNTER {
                return Err(format!(
                    "session {actor:02x?}/{client:02x?} saved snapshot {what} {value}, over \
                     the {} ceiling",
                    limits::MAX_COUNTER
                ));
            }
        }
        if !ttl_is_legal(snapshot.ttl_ms, snapshot.held) {
            return Err(format!(
                "session {actor:02x?}/{client:02x?} saved a {}ms TTL on a {} resource, which \
                 is not a duration §12.1 admits",
                snapshot.ttl_ms,
                if snapshot.held { "held" } else { "free" }
            ));
        }
        if snapshot.held != snapshot.holder.is_some() {
            return Err(format!(
                "session {actor:02x?}/{client:02x?} saved a snapshot whose held flag and \
                 holder disagree"
            ));
        }
    }
    Ok(())
}

/// Retained intents, checked for **static** shape and for every signature they
/// carry.
///
/// Not re-run through current-membership admission: whether a seed was a join
/// or an existing member was decided against the membership at acceptance, and
/// that classification is not reconstructible from the state this row survived
/// into. What is reconstructible is every bound §12.1 states outright, and
/// whether the permits a seed carries verify under the group's own authority.
fn check_intent_rows(state: &DurableState) -> Result<(), String> {
    use crate::coordination::model::membership::{check_seed_ticket, is_terminal_phase};

    // The two bounds §9 states, enforced separately rather than as one row
    // count. "If all 64 slots hold terminal intents *and* one is active, a new
    // intent is still accepted; the cap only bounds terminal history" -- so 65
    // rows is a legal group, and a single 64-row ceiling refused a state the
    // transition is required to produce.
    let active = state
        .model
        .intents
        .values()
        .filter(|intent| !is_terminal_phase(intent.status_code))
        .count();
    if active > 1 {
        return Err(format!(
            "{active} intents are active; §9 allows one at a time"
        ));
    }
    let terminal = state.model.intents.len() - active;
    if terminal as u64 > limits::MAX_RETAINED_INTENTS {
        return Err(format!(
            "{terminal} terminal intents, over the {}-intent retention cap",
            limits::MAX_RETAINED_INTENTS
        ));
    }

    // **No settled-state learner rule.** An earlier revision required at most
    // the retained two once nothing was in flight, and that made a reachable
    // state unloadable: §9 lets any active phase transition to `Failed`, and
    // `Failed` changes no membership. An intent that fails during
    // `AddingLearners` therefore leaves its transient learners in the node map
    // with no intent active, and the group could not be restarted.
    //
    // Cancellation is sequenced -- the controller removes the learners it
    // introduced and only then records `Cancelled` -- but failure is not, and
    // §9 reserves `Failed` for a deterministic outcome that cannot succeed
    // unchanged. Convergence back to the retained set is the controller's job
    // and takes further committed entries; it is not a property every stored
    // state satisfies, so it is not something a validator can require. The
    // envelope in `validate_meta` is the enforceable bound.

    for (operation_id, intent) in &state.model.intents {
        let plan = &intent.plan;
        if shape::profile_permits_voters(
            state.model.deployment_profile_code,
            plan.desired_voters.len(),
        ) {
            return Err(format!(
                "intent {operation_id:02x?} plans {} voters, which profile {} does not permit",
                plan.desired_voters.len(),
                state.model.deployment_profile_code
            ));
        }
        if plan.retained_learners.len() > shape::MAX_RETAINED_LEARNERS {
            return Err(format!(
                "intent {operation_id:02x?} retains {} learners, over the {} cap",
                plan.retained_learners.len(),
                shape::MAX_RETAINED_LEARNERS
            ));
        }
        // Sorted and duplicate-free: §9 requires it so two voters encode one
        // plan identically, which is what makes the request hash comparable.
        for (what, ids) in [
            ("desired_voters", &plan.desired_voters),
            ("retained_learners", &plan.retained_learners),
        ] {
            if !shape::sorted_unique(ids) {
                return Err(format!(
                    "intent {operation_id:02x?} holds an unsorted or duplicated {what}"
                ));
            }
        }
        let seed_ids: Vec<[u8; 32]> = plan.member_seeds.iter().map(|s| s.node_id).collect();
        if !shape::sorted_unique(&seed_ids) {
            return Err(format!(
                "intent {operation_id:02x?} holds unsorted or duplicated member seeds"
            ));
        }
        for seed in &plan.member_seeds {
            if let Some(limit) =
                shape::check_member_seed(seed.ticket.len(), seed.join_permit.as_ref().map(Vec::len))
            {
                return Err(format!(
                    "intent {operation_id:02x?} seed {:02x?} violates {limit:?}",
                    seed.node_id
                ));
            }
            if let Err(limit) = check_seed_ticket(&seed.ticket, &seed.node_id) {
                return Err(format!(
                    "intent {operation_id:02x?} seed {:02x?} carries a ticket that is not a \
                     canonical ticket for it: {limit:?}",
                    seed.node_id
                ));
            }
            // A permit that no longer verifies is a stored credential this
            // group never issued, whatever it looked like when it arrived.
            if let Some(permit) = &seed.join_permit {
                if let Err(err) = crate::coordination::model::authority::verify_join_permit(
                    &state.model.authority,
                    permit,
                    &seed.node_id,
                    &seed.ticket,
                ) {
                    return Err(format!(
                        "intent {operation_id:02x?} seed {:02x?} carries a join permit that \
                         does not verify: {err:?}",
                        seed.node_id
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Every log id apply stamps onto a row names an entry that has been applied.
///
/// Intent acceptance and completion, and route introduction, are stamped with
/// the log id of the entry doing the stamping, so none of them can sit above
/// `last_applied` -- and an intent cannot complete before it was accepted. A
/// row that says otherwise describes an ordering no log produced, which matters
/// because §9 evicts by exactly these ids: a stamp from the future is an
/// eviction order that never comes round.
fn check_stamped_log_ids(state: &DurableState) -> Result<(), String> {
    let model = &state.model;
    let within = |what: &str, id: LogId| -> Result<(), String> {
        match model.last_applied {
            None => Err(format!("{what} is stamped {id:?} with nothing applied")),
            Some(last) if id.index > last.index => Err(format!(
                "{what} is stamped {id:?}, above last_applied {last:?}"
            )),
            _ => Ok(()),
        }
    };

    for (operation_id, intent) in &model.intents {
        within(
            &format!("intent {operation_id:02x?} acceptance"),
            intent.accepted_log_id,
        )?;
        if let Some(completed) = intent.completed_log_id {
            within(&format!("intent {operation_id:02x?} completion"), completed)?;
            if completed.index < intent.accepted_log_id.index {
                return Err(format!(
                    "intent {operation_id:02x?} completed at {completed:?} before it was \
                     accepted at {:?}",
                    intent.accepted_log_id
                ));
            }
        }
        if let Some(observed) = intent.observed_membership_log_id {
            within(&format!("intent {operation_id:02x?} observation"), observed)?;
        }
    }
    for (node, route) in &model.routes {
        within(&format!("route for {node:02x?}"), route.introduced_log_id)?;
    }
    Ok(())
}

/// A route is dialling material **for the node it is filed under**. A ticket
/// naming another identity would send a dialler to the wrong peer, and the
/// Iroh handshake would reject it -- after the connection attempt, on every
/// voter, forever.
fn check_route_rows(state: &DurableState) -> Result<(), String> {
    for (node, route) in &state.model.routes {
        if let Err(limit) =
            crate::coordination::model::membership::check_seed_ticket(&route.ticket, node)
        {
            return Err(format!(
                "route for {node:02x?} holds a ticket that is not a canonical ticket for it: \
                 {limit:?}"
            ));
        }
    }
    Ok(())
}

// ── Writing ──────────────────────────────────────────────────────────────────

/// Persist an apply batch: every recorded row, plus the meta row, in **one**
/// transaction.
///
/// The caller has already mutated its private working state; nothing here
/// decides anything. Each recorded key is looked up in that state and inserted
/// or deleted accordingly, so a change set names touched rows without having to
/// say which way they went.
///
/// Every encode goes through [`encode_committed`]: these values are the result
/// of entries the group has already decided apply, so a value that will not
/// encode has no retry available and must not report one (§4.4).
///
/// ## Why it takes the state by value and hands it back
///
/// So that publishing a state that was not committed is not expressible. The
/// caller gives up its working copy to call this and does not get one back
/// until the commit has returned, which makes "memory never moves ahead of
/// disk" a property of the signature rather than of the order two statements
/// happen to be written in.
///
/// That ordering cannot be tested from outside. A commit failure always latches
/// the store fatal *before* it replies, and a fatal store answers nothing
/// further, so no reader can ever observe a state published early — verified by
/// mutation: moving the publication one line earlier passed all 576 tests. An
/// untestable rule needs a form that cannot be got wrong rather than a test
/// whose name implies it was checked.
pub(crate) fn commit(
    database: &Database,
    state: DurableState,
    changes: &ChangeSet,
) -> StorageResult<DurableState> {
    let group = state.group_id();
    let write = durable_write(database, Db::State)?;

    {
        let mut table = write.open_table(RESOURCES).map_err(state_err)?;
        for key in &changes.resources {
            let redb_key = (key.namespace(), key.key());
            match state.model.resources.get(key) {
                Some(row) => {
                    let bytes = encode_committed(
                        Db::State,
                        "ResourceRowV1",
                        &ResourceRowV1::from(row),
                        MAX_RESOURCE_ROW_BYTES,
                    )?;
                    table.insert(redb_key, bytes.as_slice())
                }
                None => table.remove(redb_key).map(|_| None),
            }
            .map_err(state_err)?;
        }
    }

    {
        let mut table = write.open_table(ACL).map_err(state_err)?;
        for key @ (resource, subject) in &changes.grants {
            let redb_key = (resource.namespace(), resource.key(), *subject);
            match state.model.acl.get(key) {
                Some(rights) => table.insert(redb_key, *rights),
                None => table.remove(redb_key).map(|_| None),
            }
            .map_err(state_err)?;
        }
    }

    {
        let mut table = write.open_table(ADMINS).map_err(state_err)?;
        for subject in &changes.admins {
            if state.model.admins.contains(subject) {
                table.insert(*subject, ())
            } else {
                table.remove(*subject).map(|_| None)
            }
            .map_err(state_err)?;
        }
    }

    {
        let mut table = write.open_table(SESSIONS).map_err(state_err)?;
        for key in &changes.sessions {
            match state.model.sessions.get(key) {
                Some(row) => {
                    let bytes = encode_committed(
                        Db::State,
                        "ClientSessionV1",
                        &session_to_wire(&group, row),
                        MAX_SESSION_ROW_BYTES,
                    )?;
                    table.insert(*key, bytes.as_slice())
                }
                None => table.remove(*key).map(|_| None),
            }
            .map_err(state_err)?;
        }
    }

    {
        let mut table = write.open_table(INTENTS).map_err(state_err)?;
        for key in &changes.intents {
            match state.model.intents.get(key) {
                Some(row) => {
                    let wire = intent_to_wire(key, row).map_err(|err| {
                        StorageError::corrupt(
                            Db::State,
                            "MembershipIntentV1",
                            format!(
                                "failed to encode the result of an already-committed entry: \
                                 {err:?}"
                            ),
                        )
                    })?;
                    let bytes = encode_committed(
                        Db::State,
                        "MembershipIntentV1",
                        &wire,
                        MAX_INTENT_ROW_BYTES,
                    )?;
                    table.insert(*key, bytes.as_slice())
                }
                None => table.remove(*key).map(|_| None),
            }
            .map_err(state_err)?;
        }
    }

    {
        let mut table = write.open_table(ROUTES).map_err(state_err)?;
        for key in &changes.routes {
            match state.model.routes.get(key) {
                Some(row) => {
                    let bytes = encode_committed(
                        Db::State,
                        "NodeRouteV1",
                        &NodeRouteV1::from(row),
                        MAX_ROUTE_ROW_BYTES,
                    )?;
                    table.insert(*key, bytes.as_slice())
                }
                None => table.remove(*key).map(|_| None),
            }
            .map_err(state_err)?;
        }
    }

    {
        let mut table = write.open_table(AUDIT).map_err(state_err)?;
        for key in &changes.audit {
            match state.model.audit.get(key) {
                Some(row) => {
                    let bytes = encode_committed(
                        Db::State,
                        "AuditRowV1",
                        &AuditRowV1::from(row),
                        MAX_AUDIT_ROW_BYTES,
                    )?;
                    table.insert(*key, bytes.as_slice())
                }
                None => table.remove(*key).map(|_| None),
            }
            .map_err(state_err)?;
        }
    }

    // The meta row goes in the same transaction as the rows it summarises.
    // §11.3 is explicit that `last_applied`, the counters and the audit floor
    // move with the effects, and it is what makes `last_applied` the
    // authoritative answer to "has this entry been applied?".
    {
        let mut table = write.open_table(STATE_META).map_err(state_err)?;
        let bytes = encode_committed(
            Db::State,
            "CoordinationMetaV1",
            &meta_to_wire(&state),
            MAX_COORDINATION_META_BYTES,
        )?;
        table
            .insert(KEY_META, bytes.as_slice())
            .map_err(state_err)?;
    }

    write.commit().map_err(state_err)?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::history::sample_applied_state;
    use crate::coordination::model::state::{
        Intent, MembershipPlan, SavedReply, SavedSnapshot, Session,
    };
    use crate::coordination::raft::ClientSessionV1;
    use crate::coordination::reason::{Outcome, Reason};
    use crate::coordination::storage::fixture::maximal_legal_meta;
    use openraft::Membership;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A bare `state.redb` with the §11.3 tables and no identity binding.
    ///
    /// Deliberately not a bound store: these tests write states whose group is
    /// the model history's, and binding would tie the file to a different one.
    /// Identity binding is `schema`'s subject and is tested there.
    fn scratch_database() -> (PathBuf, Database) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aster-coordination-rows-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let path = dir.join("state.redb");
        let database = Database::create(&path).expect("create state.redb");
        let write = durable_write(&database, Db::State).expect("write");
        super::super::schema::create_state_tables(&write).expect("create tables");
        write.commit().expect("commit");
        (dir, database)
    }

    /// The three voters this fixture's group committed.
    ///
    /// Declared here rather than read off the model. An earlier version built
    /// the stored membership *from* `model.members`, which made the rule that
    /// compares the two a tautology: any member set would have matched the
    /// membership synthesised from it. The membership is the source now, and
    /// the model's projection is derived from it exactly as `meta_from_wire`
    /// does at load.
    const VOTERS: [[u8; 32]; 3] = [[1u8; 32], [2u8; 32], [3u8; 32]];

    fn committed_membership() -> StoredMembership<CoordinationNodeId, CoordinationNode> {
        let ids: BTreeSet<CoordinationNodeId> = VOTERS
            .iter()
            .map(|id| CoordinationNodeId::from_array(*id))
            .collect();
        let nodes: BTreeMap<CoordinationNodeId, CoordinationNode> =
            ids.iter().map(|id| (*id, CoordinationNode)).collect();
        StoredMembership::new(
            Some(openraft::LogId {
                leader_id: openraft::CommittedLeaderId::new(
                    1,
                    CoordinationNodeId::from_array([0x11u8; 32]),
                ),
                index: 400,
            }),
            Membership::new(vec![ids], nodes),
        )
    }

    /// A coherent complete state: every row, count, charge, audit tip and
    /// metadata value agrees, because apply produced it.
    ///
    /// The intent and route are added on top, because the generated history
    /// retires membership operations faster than it retains them and those two
    /// tables would otherwise never be exercised by the validator at all. Both
    /// are built from the real helpers -- a canonical ticket, a permit signed
    /// by this group's own authority -- so the positive case runs the signature
    /// path rather than skipping it.
    fn coherent_state() -> DurableState {
        use crate::coordination::model::authority::testing as auth;
        use crate::coordination::model::membership::{test_ticket, PHASE_COMPLETED};
        use crate::coordination::model::state::{Intent, MemberSeed, MembershipPlan, Route};

        let mut model = sample_applied_state(400);
        assert!(
            model.counters_match_recomputation(),
            "the history fixture must itself be coherent"
        );

        let membership = committed_membership();
        // Derived from the membership, which is the only thing §11.3 persists.
        model.membership = project_membership(membership.membership());
        model.membership_log_id = membership.log_id().as_ref().map(LogId::from);

        let seed_node = [4u8; 32];
        let ticket = test_ticket(&seed_node);
        model.intents.insert(
            [0x55u8; 16],
            Intent {
                actor: [1u8; 32],
                request_hash: [0x56u8; 32],
                expected_membership: None,
                plan: MembershipPlan {
                    desired_voters: VOTERS.to_vec(),
                    retained_learners: Vec::new(),
                    member_seeds: vec![MemberSeed {
                        node_id: seed_node,
                        ticket: ticket.clone(),
                        join_permit: Some(auth::valid(&seed_node, &ticket)),
                    }],
                },
                status_code: PHASE_COMPLETED,
                result_code: crate::coordination::contract::SYS_MEMBERSHIP_ADVANCED,
                accepted_log_id: LogId::new(1, [0x11u8; 32], 390),
                completed_log_id: Some(LogId::new(1, [0x11u8; 32], 395)),
                observed_membership_log_id: Some(LogId::new(1, [0x11u8; 32], 400)),
            },
        );
        model.routes.insert(
            seed_node,
            Route {
                ticket,
                source_operation_id: [0x55u8; 16],
                introduced_log_id: LogId::new(1, [0x11u8; 32], 390),
            },
        );
        model.growth_bytes = model.recompute_growth();
        model.reserve_bytes = model.recompute_reserve();

        DurableState {
            model,
            membership,
            recovery_export_hash: None,
            recovered_from: None,
        }
    }

    /// Name every row a state holds, as the writer takes them.
    fn every_row(state: &DurableState) -> ChangeSet {
        ChangeSet {
            resources: state.model.resources.keys().cloned().collect(),
            grants: state.model.acl.keys().cloned().collect(),
            admins: state.model.admins.iter().copied().collect(),
            sessions: state.model.sessions.keys().copied().collect(),
            intents: state.model.intents.keys().copied().collect(),
            routes: state.model.routes.keys().copied().collect(),
            audit: state.model.audit.keys().copied().collect(),
        }
    }

    // ── Ceilings ────────────────────────────────────────────────────────────

    /// Each row ceiling admits its maximal legal row, and does not sit
    /// arbitrarily far above it.
    ///
    /// Both directions, and both matter. Too small and a legal row becomes
    /// unwritable at exactly the moment a group grows into it; too large and
    /// the bound stops bounding a corrupt row's allocation, which is the whole
    /// point of §4.2. The upper assertion is deliberately tight enough that a
    /// field added to any of these types forces this test to be revisited
    /// rather than silently absorbed.
    #[test]
    fn every_row_ceiling_admits_its_maximal_legal_row() {
        use crate::coordination::contract::payload_fory;

        let max_key = ResourceKey::new(
            "n".repeat(limits::MAX_NAMESPACE),
            vec![0xFFu8; limits::MAX_KEY],
        )
        .expect("the declared maximum is a valid identity");

        let resource = ResourceRowV1::from(&ResourceRow {
            epoch: u64::MAX,
            held: true,
            holder: Some([0xFF; 32]),
            ttl_ms: limits::MAX_TTL_MS,
            lease_revision: u64::MAX,
            value_revision: u64::MAX,
            value: vec![0xFF; limits::MAX_VALUE],
            acl_revision: u64::MAX,
        });

        let session = session_to_wire(
            &[0xFF; 32],
            &Session {
                last_sequence: limits::MAX_COUNTER,
                request_hash: [0xFF; 32],
                last_result: Some(SavedReply {
                    outcome: Outcome::Mutated,
                    reason: Reason::None,
                    snapshot: Some(SavedSnapshot {
                        resource: max_key.clone(),
                        epoch: u64::MAX,
                        holder: Some([0xFF; 32]),
                        held: true,
                        ttl_ms: limits::MAX_TTL_MS,
                        lease_revision: u64::MAX,
                        value_revision: u64::MAX,
                        acl_revision: u64::MAX,
                    }),
                    acl_revision: u64::MAX,
                    admin_revision: u64::MAX,
                    operation_id: Some([0xFF; 16]),
                    membership_log_id: Some(LogId::new(u64::MAX, [0xFF; 32], u64::MAX)),
                }),
                retired: true,
            },
        );

        // The intent is measured differently below: §12.1 bounds its plan on
        // the plan's *encoded* form, not field by field, so the thing that has
        // to be measured is the wrapper around that bound rather than a
        // constructed maximal plan. A plan built to fill 256 KiB would need
        // twenty-odd member seeds, and a legal plan has at most seven.
        let small_plan = MembershipPlan {
            desired_voters: (0..shape::MAX_VOTERS as u8).map(|b| [b; 32]).collect(),
            retained_learners: (100..102u8).map(|b| [b; 32]).collect(),
            member_seeds: Vec::new(),
        };
        let intent_model = Intent {
            actor: [0xFF; 32],
            request_hash: [0xFF; 32],
            expected_membership: Some(LogId::new(u64::MAX, [0xFF; 32], u64::MAX)),
            plan: small_plan.clone(),
            status_code: crate::coordination::model::membership::PHASE_COMPLETED,
            result_code: i32::MAX,
            accepted_log_id: LogId::new(u64::MAX, [0xFF; 32], u64::MAX),
            completed_log_id: Some(LogId::new(u64::MAX, [0xFF; 32], u64::MAX)),
            observed_membership_log_id: Some(LogId::new(u64::MAX, [0xFF; 32], u64::MAX)),
        };
        let intent_wire = intent_to_wire(&[0xFF; 16], &intent_model).expect("the intent encodes");
        let intent_encoded = payload_fory::<MembershipIntentV1>()
            .serialize(&intent_wire)
            .expect("encode")
            .len();
        let plan_encoded = small_plan.encoded_len(&[0xFF; 16], intent_model.expected_membership);
        let wrapper = intent_encoded - plan_encoded;
        assert!(
            MAX_INTENT_ROW_BYTES >= limits::MAX_ENCODED_COMMAND + wrapper,
            "an intent whose plan sits at the {}-byte §12.1 bound encodes to at least \
             {} bytes, over the {MAX_INTENT_ROW_BYTES}-byte ceiling",
            limits::MAX_ENCODED_COMMAND,
            limits::MAX_ENCODED_COMMAND + wrapper
        );
        assert!(
            MAX_INTENT_ROW_BYTES <= limits::MAX_ENCODED_COMMAND + wrapper + ROW_FRAMING_ALLOWANCE,
            "the {MAX_INTENT_ROW_BYTES}-byte ceiling is more than one framing allowance \
             above the {}-byte maximum an intent can reach; recompute it",
            limits::MAX_ENCODED_COMMAND + wrapper
        );

        let route = NodeRouteV1::from(&Route {
            ticket: vec![0xFF; limits::MAX_TICKET],
            source_operation_id: [0xFF; 16],
            introduced_log_id: LogId::new(u64::MAX, [0xFF; 32], u64::MAX),
        });

        let audit = AuditRowV1::from(&AuditRow {
            leader_node_id: [0xFF; 32],
            actor: Some([0xFF; 32]),
            operation_code: i32::MAX,
            resource: Some(max_key),
            result: crate::coordination::model::state::AuditResult::System(i32::MAX),
            epoch: u64::MAX,
            value_revision: u64::MAX,
        });

        let measured: Vec<(&str, usize, usize)> = vec![
            (
                "ResourceRowV1",
                payload_fory::<ResourceRowV1>()
                    .serialize(&resource)
                    .expect("encode")
                    .len(),
                MAX_RESOURCE_ROW_BYTES,
            ),
            (
                "ClientSessionV1",
                payload_fory::<ClientSessionV1>()
                    .serialize(&session)
                    .expect("encode")
                    .len(),
                MAX_SESSION_ROW_BYTES,
            ),
            (
                "NodeRouteV1",
                payload_fory::<NodeRouteV1>()
                    .serialize(&route)
                    .expect("encode")
                    .len(),
                MAX_ROUTE_ROW_BYTES,
            ),
            (
                "AuditRowV1",
                payload_fory::<AuditRowV1>()
                    .serialize(&audit)
                    .expect("encode")
                    .len(),
                MAX_AUDIT_ROW_BYTES,
            ),
        ];

        for (name, encoded, ceiling) in measured {
            assert!(
                encoded <= ceiling,
                "{name} encodes to {encoded} bytes at its declared maximum, over the \
                 {ceiling}-byte ceiling"
            );
            assert!(
                ceiling <= encoded + 2 * ROW_FRAMING_ALLOWANCE,
                "{name}'s {ceiling}-byte ceiling is more than two framing allowances \
                 above its {encoded}-byte maximum, so it no longer bounds a corrupt row \
                 tightly; recompute it from the fields the type now carries"
            );
        }
    }

    // ── Metadata-local validation ───────────────────────────────────────────

    /// The widest legal binding a production group can hold passes the
    /// metadata-local check.
    ///
    /// `maximal_legal_meta` is built from the declared caps -- five voters
    /// under a joint config with two retained learners, a 128-byte authority
    /// domain, a real Ed25519 key, and both counters at exactly their budgets.
    /// If this rejects it, the fixture or the validator is wrong, not the
    /// fixture's ambition.
    #[test]
    fn the_maximal_legal_binding_passes_metadata_local_validation() {
        let state = meta_from_wire(&maximal_legal_meta()).expect("the maximal meta narrows");
        validate_meta(&state).expect("the maximal legal binding must validate");

        // And it really is at the limits, rather than passing because it is
        // small: a check that accepted anything would accept this too.
        assert_eq!(state.model.growth_bytes, limits::GROWTH_BUDGET);
        assert_eq!(state.model.reserve_bytes, limits::RESERVE_BUDGET);
        // The **union** of both configurations: a joint change's old voters
        // still vote, so the widest legal binding names the retained three plus
        // two leaving and two joining.
        assert_eq!(
            state.model.membership.voters.len(),
            shape::MAX_VOTERS + shape::MAX_NEW_VOTERS,
            "every voter of both configurations"
        );
        assert_eq!(
            state.model.membership.learners.len(),
            shape::MAX_LEARNER_ENVELOPE,
            "the whole envelope, which is what the validator admits"
        );
        assert!(
            state.model.membership.joint,
            "the widest legal shape is joint"
        );
    }

    /// Each **raw wire** rule refuses on its own, before any conversion runs.
    ///
    /// These are separate from the validator's rules because they are about the
    /// stored record rather than about the state it describes, and because the
    /// conversions below them are lossy in ways that would hide several: the
    /// membership mirror deduplicates ids and manufactures node-map entries, so
    /// a malformed row would arrive at the validator already repaired.
    #[test]
    fn each_raw_wire_rule_refuses_before_any_conversion() {
        use crate::coordination::raft::{NodeEntryV1, NodeIdSetV1, NodeIdV1, RecoveredFromV1};

        /// A perturbation of a legal metadata row, and the field its refusal
        /// must name.
        type WireCase = (
            &'static str,
            Box<dyn Fn(&mut CoordinationMetaV1)>,
            &'static str,
        );

        let cases: Vec<WireCase> = vec![
            (
                "schema version",
                Box::new(|m: &mut CoordinationMetaV1| m.schema_version = 99),
                "SchemaVersion",
            ),
            (
                "group id width",
                Box::new(|m: &mut CoordinationMetaV1| m.group_id = vec![0xFF; 31]),
                "group_id",
            ),
            (
                "authority key width",
                Box::new(|m: &mut CoordinationMetaV1| m.authority_public_key = vec![0xFF; 33]),
                "authority_public_key",
            ),
            (
                "authority domain length",
                Box::new(|m: &mut CoordinationMetaV1| {
                    m.authority_domain = "d".repeat(
                        crate::coordination::model::authority::MAX_AUTHORITY_DOMAIN_BYTES + 1,
                    );
                }),
                "authority_domain",
            ),
            (
                "an empty authority domain",
                Box::new(|m: &mut CoordinationMetaV1| m.authority_domain = String::new()),
                "authority_domain",
            ),
            (
                // A domain is compared bytewise against the domain inside a
                // signed permit, so "short enough" is not the rule.
                "a non-ASCII authority domain",
                Box::new(|m: &mut CoordinationMetaV1| m.authority_domain = "aster0.nét".into()),
                "authority_domain",
            ),
            (
                "an empty source authority domain",
                Box::new(|m: &mut CoordinationMetaV1| {
                    m.recovered_from
                        .as_mut()
                        .expect("provenance")
                        .source_authority_domain = String::new();
                }),
                "source_authority_domain",
            ),
            (
                "last-applied leader width",
                Box::new(|m: &mut CoordinationMetaV1| {
                    m.last_applied.as_mut().expect("an id").leader_node_id = vec![0u8; 8];
                }),
                "last_applied",
            ),
            (
                "recovery export hash width",
                Box::new(|m: &mut CoordinationMetaV1| {
                    m.recovery_export_hash = Some(vec![0xFF; 16]);
                }),
                "recovery_export_hash",
            ),
            (
                "provenance without the hash it binds",
                Box::new(|m: &mut CoordinationMetaV1| m.recovery_export_hash = None),
                "recovery_export_hash",
            ),
            (
                "provenance disagreeing with the hash",
                Box::new(|m: &mut CoordinationMetaV1| {
                    m.recovery_export_hash = Some(vec![0xAB; 32]);
                }),
                "recovery_export_hash",
            ),
            (
                "source authority key width",
                Box::new(|m: &mut CoordinationMetaV1| {
                    m.recovered_from
                        .as_mut()
                        .expect("provenance")
                        .source_authority_public_key = vec![0xFF; 31];
                }),
                "source_authority_public_key",
            ),
            (
                "source last-applied leader width",
                Box::new(|m: &mut CoordinationMetaV1| {
                    m.recovered_from
                        .as_mut()
                        .expect("provenance")
                        .source_last_applied
                        .as_mut()
                        .expect("an id")
                        .leader_node_id = vec![0u8; 8];
                }),
                "source_last_applied",
            ),
            (
                // The repair the mirror would perform silently: a config names
                // an id the node map omits, and the conversion inserts a
                // default entry for it.
                "a config id with no node-map entry",
                Box::new(|m: &mut CoordinationMetaV1| {
                    m.membership
                        .membership
                        .nodes
                        .retain(|n| n.node_id != vec![0u8; 32]);
                    m.membership
                        .membership
                        .configs
                        .last_mut()
                        .expect("a config")
                        .ids
                        .push(NodeIdV1 {
                            node_id: vec![0u8; 32],
                        });
                }),
                "membership",
            ),
            (
                // The other repair: a duplicate id collapses into a set.
                "a duplicated config id",
                Box::new(|m: &mut CoordinationMetaV1| {
                    let ids = &mut m
                        .membership
                        .membership
                        .configs
                        .last_mut()
                        .expect("a config")
                        .ids;
                    let first = ids.first().expect("an id").clone();
                    ids.push(first);
                }),
                "membership",
            ),
            (
                "an unsorted node map",
                Box::new(|m: &mut CoordinationMetaV1| {
                    m.membership.membership.nodes.reverse();
                }),
                "membership",
            ),
            (
                "a node entry from an unknown format version",
                Box::new(|m: &mut CoordinationMetaV1| {
                    m.membership.membership.nodes.push(NodeEntryV1 {
                        node_id: vec![0xC7; 32],
                        format_version: 99,
                    });
                }),
                "membership",
            ),
        ];

        for (label, break_it, expected) in cases {
            let mut meta = maximal_legal_meta();
            break_it(&mut meta);
            let err = meta_from_wire(&meta)
                .expect_err(&format!("{label} must be refused before conversion"));
            assert!(
                format!("{err:?}").contains(expected),
                "{label}: expected a failure naming {expected}, got {err:?}"
            );
        }

        // And an empty config set is still the one legal empty shape: a group
        // that has committed no membership yet, which is what a freshly bound
        // store holds.
        let mut fresh = maximal_legal_meta();
        fresh.membership = crate::coordination::raft::StoredMembershipV1 {
            log_id: None,
            membership: crate::coordination::raft::MembershipV1 {
                configs: Vec::new(),
                nodes: Vec::new(),
            },
        };
        let state = meta_from_wire(&fresh).expect("an uncommitted membership is legal");
        assert!(state.model.membership.members().is_empty());
        let _ = NodeIdSetV1 { ids: Vec::new() };
        let _: Option<RecoveredFromV1> = None;
    }

    /// Each metadata-local validator rule refuses on its own.
    ///
    /// These read the narrowed state rather than the wire record: they are
    /// about whether the binding describes a group that could exist, not about
    /// whether the row was well formed.
    #[test]
    fn each_metadata_local_constraint_refuses_on_its_own() {
        let narrow = |meta: CoordinationMetaV1| meta_from_wire(&meta).expect("narrows");

        // An authority key that is not a curve point.
        //
        // Found by scanning rather than hard-coded. Which 32-byte strings
        // decompress is a property of the curve encoding, not something to
        // guess: `[0x01; 32]` -- the obvious choice, and the one this test
        // first used -- is a perfectly valid point, so the assertion proved
        // nothing until it was replaced.
        let not_a_key = (0u16..=255)
            .map(|n| [n as u8; 32])
            .find(|bytes| ed25519_dalek::VerifyingKey::from_bytes(bytes).is_err())
            .expect("some repeated byte is not a curve point");
        let mut meta = maximal_legal_meta();
        meta.authority_public_key = not_a_key.to_vec();
        let err = validate_meta(&narrow(meta)).expect_err("not a key");
        assert!(err.to_string().contains("decompress"), "got: {err}");

        // A counter past its budget. One byte over, not a round number: the
        // check is `>` and an off-by-one would pass a looser fixture.
        for (field, budget) in [
            ("growth", limits::GROWTH_BUDGET),
            ("reserve", limits::RESERVE_BUDGET),
        ] {
            let mut meta = maximal_legal_meta();
            if field == "growth" {
                meta.aggregate_growth_bytes = budget + 1;
            } else {
                meta.aggregate_reserve_bytes = budget + 1;
            }
            let err = validate_meta(&narrow(meta)).expect_err("over budget");
            assert!(err.to_string().contains("budget"), "{field}: {err}");
        }

        // A voter count the profile does not permit. Four voters is legal in
        // OpenRaft and illegal in Aster, which is exactly the distinction
        // §7.1 draws.
        let mut meta = maximal_legal_meta();
        {
            let config = meta
                .membership
                .membership
                .configs
                .last_mut()
                .expect("a config");
            let dropped = config.ids.pop().expect("an id");
            // The node map keeps its entry, so this stays canonical and the
            // wire rules pass: the subject here is the voter count.
            assert!(meta
                .membership
                .membership
                .nodes
                .iter()
                .any(|n| n.node_id == dropped.node_id));
        }
        let err = validate_meta(&narrow(meta)).expect_err("four voters");
        assert!(err.to_string().contains("does not permit"), "got: {err}");

        // An unknown profile permits nothing rather than falling back.
        let mut meta = maximal_legal_meta();
        meta.deployment_profile_code = 99;
        assert!(
            validate_meta(&narrow(meta)).is_err(),
            "an unknown profile permits nothing"
        );

        // An audit floor above what was applied.
        let mut meta = maximal_legal_meta();
        meta.audit_floor = Some(LogIdV1 {
            term: 9,
            leader_node_id: vec![3u8; 32],
            index: 9_000_001,
        });
        let err = validate_meta(&narrow(meta)).expect_err("floor ahead");
        assert!(err.to_string().contains("audit_floor"), "got: {err}");
    }

    /// Every way a stored membership can be unreachable, refused on its own.
    ///
    /// Each of these was accepted before. The validator checked only the last
    /// joint configuration, so an illegal *first* half passed; it never asked
    /// whether the membership had a log id at all, nor whether that id named an
    /// entry this store had applied.
    #[test]
    fn each_unreachable_membership_is_refused_on_its_own() {
        use crate::coordination::raft::{MembershipV1, NodeEntryV1, NodeIdSetV1, NodeIdV1};

        let node = |b: u8| NodeIdV1 {
            node_id: vec![b; 32],
        };
        let set = |bytes: &[u8]| NodeIdSetV1 {
            ids: bytes.iter().map(|b| node(*b)).collect(),
        };
        let nodes = |bytes: &[u8]| -> Vec<NodeEntryV1> {
            let mut out: Vec<NodeEntryV1> = bytes
                .iter()
                .map(|b| NodeEntryV1 {
                    node_id: vec![*b; 32],
                    format_version: crate::coordination::convert::NODE_FORMAT_VERSION,
                })
                .collect();
            out.sort_by(|a, b| a.node_id.cmp(&b.node_id));
            out
        };
        let stored = |membership: MembershipV1, log_id: Option<LogIdV1>| {
            crate::coordination::raft::StoredMembershipV1 { log_id, membership }
        };
        let applied = |index: u64| {
            Some(LogIdV1 {
                term: 9,
                leader_node_id: vec![3u8; 32],
                index,
            })
        };

        // A legal joint change under PROFILE_PROD: three voters to three, with
        // one swapped. Both halves are permitted counts.
        let legal_joint = MembershipV1 {
            configs: vec![set(&[1, 2, 3]), set(&[1, 2, 4])],
            nodes: nodes(&[1, 2, 3, 4]),
        };

        let cases: Vec<(
            &str,
            crate::coordination::raft::StoredMembershipV1,
            Option<LogIdV1>,
            &str,
        )> = vec![
            (
                // The case the previous check could not see: the configuration
                // being moved *out of* is one this profile never permitted, and
                // it is the half a voter needs quorum in while the change is in
                // flight.
                "an invalid first config with a valid last one",
                stored(
                    MembershipV1 {
                        configs: vec![set(&[1, 2, 3, 4]), set(&[1, 2, 3])],
                        nodes: nodes(&[1, 2, 3, 4]),
                    },
                    applied(100),
                ),
                applied(9_000_000),
                "configuration 0",
            ),
            (
                "a three-way joint configuration",
                stored(
                    MembershipV1 {
                        configs: vec![set(&[1, 2, 3]), set(&[1, 2, 4]), set(&[1, 2, 5])],
                        nodes: nodes(&[1, 2, 3, 4, 5]),
                    },
                    applied(100),
                ),
                applied(9_000_000),
                "at most two",
            ),
            (
                // Seven, not three: the envelope is the departing retained
                // learners plus the unpromoted new voters plus the arriving
                // retained learners, so six is a state a legal plan reaches.
                "more learners than the mid-change envelope",
                stored(
                    MembershipV1 {
                        configs: vec![set(&[1, 2, 3])],
                        nodes: nodes(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
                    },
                    applied(100),
                ),
                applied(9_000_000),
                "learner envelope",
            ),
            (
                "a committed membership with no log id",
                stored(legal_joint.clone(), None),
                applied(9_000_000),
                "no log id",
            ),
            (
                "a membership ahead of last_applied",
                stored(legal_joint.clone(), applied(9_000_001)),
                applied(9_000_000),
                "runs ahead",
            ),
            (
                "a membership at the applied index from another term",
                stored(
                    legal_joint.clone(),
                    Some(LogIdV1 {
                        term: 8,
                        leader_node_id: vec![3u8; 32],
                        index: 9_000_000,
                    }),
                ),
                applied(9_000_000),
                "different entries at one index",
            ),
            (
                "a membership at the applied index from another leader",
                stored(
                    legal_joint.clone(),
                    Some(LogIdV1 {
                        term: 9,
                        leader_node_id: vec![0xEE; 32],
                        index: 9_000_000,
                    }),
                ),
                applied(9_000_000),
                "different entries at one index",
            ),
            (
                "a membership with nothing applied",
                stored(legal_joint.clone(), applied(100)),
                None,
                "nothing applied",
            ),
        ];

        for (label, membership, last_applied, expected) in cases {
            let mut meta = maximal_legal_meta();
            meta.membership = membership;
            meta.last_applied = last_applied;
            let state = meta_from_wire(&meta)
                .unwrap_or_else(|err| panic!("{label} must narrow before it is judged: {err}"));
            let err = validate_meta(&state).expect_err(&format!("{label} must be refused"));
            assert!(
                err.to_string().contains(expected),
                "{label}: expected a failure naming {expected}, got {err}"
            );
        }

        // And the legal joint change itself is accepted, so the rules above are
        // refusing the shapes rather than everything joint.
        let mut meta = maximal_legal_meta();
        meta.membership = stored(legal_joint, applied(100));
        meta.last_applied = applied(9_000_000);
        let state = meta_from_wire(&meta).expect("narrows");
        validate_meta(&state).expect("a legal joint change is reachable");
        assert!(state.model.membership.joint);
    }

    /// A log-id-only membership is refused too: the pair moves together.
    #[test]
    fn a_membership_log_id_without_a_membership_is_refused() {
        let mut meta = maximal_legal_meta();
        meta.membership.membership = crate::coordination::raft::MembershipV1 {
            configs: Vec::new(),
            nodes: Vec::new(),
        };
        // Refused at the wire layer rather than by the validator: the empty
        // shape is legal only as a *pair*, so a log id beside an empty
        // membership never narrows at all. The validator's own rule covers the
        // paths that do not go through `meta_from_wire`.
        let err = meta_from_wire(&meta).expect_err("an id with no membership");
        assert!(
            format!("{err:?}").contains("EmptyMembership"),
            "got: {err:?}"
        );

        // And with the id removed it is the ordinary uncommitted shape.
        meta.membership.log_id = None;
        let state = meta_from_wire(&meta).expect("an uncommitted membership narrows");
        validate_meta(&state).expect("and is legal");
    }

    /// The member set and membership log id are derived from the stored
    /// membership, so a state whose projection disagrees is refused.
    ///
    /// The derivation happens in `meta_from_wire`, so this perturbs the
    /// narrowed value rather than the wire one -- which is the only way to
    /// reach the check at all, and is what makes it a guard against the
    /// derivation drifting rather than against a second persisted copy.
    #[test]
    fn a_membership_projection_that_drifted_is_refused() {
        let mut state = meta_from_wire(&maximal_legal_meta()).expect("narrows");
        // A **role swap**, which leaves every count and the combined member set
        // exactly as they were. A projection compared as one set would not see
        // it at all.
        let voter = *state
            .model
            .membership
            .voters
            .iter()
            .next()
            .expect("a voter");
        let learner = *state
            .model
            .membership
            .learners
            .iter()
            .next()
            .expect("a learner");
        state.model.membership.voters.remove(&voter);
        state.model.membership.voters.insert(learner);
        state.model.membership.learners.remove(&learner);
        state.model.membership.learners.insert(voter);
        assert_eq!(
            state.model.membership.members(),
            project_membership(state.membership.membership()).members(),
            "the swap must leave the combined set untouched, or it proves nothing"
        );
        let err = validate_meta(&state).expect_err("swapped roles");
        assert!(err.to_string().contains("projection"), "got: {err}");

        let mut state = meta_from_wire(&maximal_legal_meta()).expect("narrows");
        state.model.membership_log_id = Some(LogId::new(1, [0u8; 32], 1));
        let err = validate_meta(&state).expect_err("wrong log id");
        assert!(err.to_string().contains("membership log id"), "got: {err}");
    }

    /// A standalone maximal binding cannot pass **complete-state** validation:
    /// its counters describe a hundred thousand rows that are not there.
    ///
    /// Stated as a test because it is the reason the validator is two
    /// functions. Folding them together would have forced one of two wrong
    /// answers -- either the maximal binding is rejected, or the counters stop
    /// being checked against the rows.
    #[test]
    fn the_maximal_binding_alone_fails_complete_state_validation() {
        let state = meta_from_wire(&maximal_legal_meta()).expect("narrows");
        let err = validate(&state).expect_err("no rows charge those counters");
        assert!(
            err.to_string().contains("the rows charge 0"),
            "the counters must be what refused it: {err}"
        );
    }

    // ── Complete-state validation ───────────────────────────────────────────

    /// A state apply produced validates whole.
    #[test]
    fn a_coherent_complete_state_validates() {
        let state = coherent_state();
        validate(&state).expect("a state apply produced must validate");

        // And it is not vacuous: every table the validator checks has rows.
        assert!(!state.model.resources.is_empty());
        assert!(!state.model.acl.is_empty());
        assert!(!state.model.admins.is_empty());
        assert!(!state.model.sessions.is_empty());
        assert!(!state.model.audit.is_empty());
        assert!(state.model.growth_bytes > 0 && state.model.reserve_bytes > 0);
    }

    // ── The invariant table ─────────────────────────────────────────────────

    /// One perturbation of a coherent state, and the rule it must trip.
    struct Negative {
        rule: &'static str,
        what: &'static str,
        break_it: fn(&mut DurableState),
    }

    /// A perturbation for every rule in [`INVARIANTS`].
    ///
    /// Each names the rule it targets, and the test below asserts that rule is
    /// the one that fires -- not merely that the state was refused. A negative
    /// that trips a *different* rule proves nothing about the one it claims,
    /// and several of these would: adding a row moves a counter, so anything
    /// after `aggregate-counters` would be caught by it instead. The harness
    /// recomputes the counters after every perturbation except that one, so
    /// each rule is reached on its own.
    static NEGATIVES: &[Negative] = &[
        Negative {
            rule: "table-row-caps",
            what: "one more admin than §12.1 allows",
            break_it: |state| {
                for n in 0..=limits::MAX_ADMINS_PER_GROUP {
                    let mut id = [0u8; 32];
                    id[0..8].copy_from_slice(&n.to_be_bytes());
                    state.model.admins.insert(id);
                }
            },
        },
        Negative {
            rule: "grants-per-resource",
            what: "one resource holding every grant",
            break_it: |state| {
                let resource = state
                    .model
                    .resources
                    .keys()
                    .next()
                    .expect("a resource")
                    .clone();
                for n in 0..=limits::MAX_GRANTS_PER_RESOURCE {
                    let mut id = [0u8; 32];
                    id[0..8].copy_from_slice(&n.to_be_bytes());
                    state
                        .model
                        .acl
                        .insert((resource.clone(), id), limits::RIGHT_READ);
                }
            },
        },
        Negative {
            rule: "sessions-per-actor",
            what: "one actor past the retained-session cap",
            break_it: |state| {
                for n in 0..=limits::MAX_RETAINED_SESSIONS_PER_ACTOR {
                    let mut client = [0u8; 16];
                    client[0..8].copy_from_slice(&n.to_be_bytes());
                    state.model.sessions.insert(
                        ([0xD1u8; 32], client),
                        Session {
                            last_sequence: 0,
                            request_hash: [0u8; 32],
                            last_result: None,
                            retired: true,
                        },
                    );
                }
            },
        },
        Negative {
            rule: "active-clients",
            what: "one actor past the active-client cap",
            break_it: |state| {
                for n in 0..=limits::MAX_ACTIVE_CLIENTS_PER_ACTOR {
                    let mut client = [0u8; 16];
                    client[0..8].copy_from_slice(&n.to_be_bytes());
                    state.model.sessions.insert(
                        ([0xD2u8; 32], client),
                        Session {
                            last_sequence: 0,
                            request_hash: [0u8; 32],
                            last_result: None,
                            retired: false,
                        },
                    );
                }
            },
        },
        Negative {
            rule: "aggregate-counters",
            what: "a growth counter one byte off the rows",
            break_it: |state| state.model.growth_bytes += 1,
        },
        Negative {
            rule: "audit-tip",
            what: "a tip naming the second-newest row",
            break_it: |state| {
                let ((term, index), row) = state
                    .model
                    .audit
                    .iter()
                    .rev()
                    .nth(1)
                    .map(|(k, v)| (*k, v.clone()))
                    .expect("at least two audit rows");
                state.model.audit_tip = Some(LogId::new(term, row.leader_node_id, index));
            },
        },
        Negative {
            rule: "audit-floor",
            what: "a floor that is not the oldest retained row",
            break_it: |state| {
                let ((term, index), row) = state
                    .model
                    .audit
                    .iter()
                    .next()
                    .map(|(k, v)| (*k, v.clone()))
                    .expect("an audit row");
                state.model.audit_floor = Some(LogId::new(term, row.leader_node_id, index + 1));
            },
        },
        Negative {
            rule: "audit-key-order",
            what: "a higher-term row at a lower index",
            break_it: |state| {
                let ((term, index), row) = state
                    .model
                    .audit
                    .iter()
                    .next_back()
                    .map(|(k, v)| (*k, v.clone()))
                    .expect("an audit row");
                state.model.audit.insert((term + 1, index - 1), row);
            },
        },
        Negative {
            rule: "audit-row-shape",
            what: "an operation code §5.2 does not declare",
            break_it: |state| {
                let key = *state.model.audit.keys().next().expect("an audit row");
                state
                    .model
                    .audit
                    .get_mut(&key)
                    .expect("the row")
                    .operation_code = 42;
            },
        },
        Negative {
            rule: "resource-row-shape",
            what: "a value over the §12.1 limit",
            break_it: |state| {
                let key = state
                    .model
                    .resources
                    .keys()
                    .next()
                    .expect("a resource")
                    .clone();
                state.model.resources.get_mut(&key).expect("the row").value =
                    vec![0u8; limits::MAX_VALUE + 1];
            },
        },
        Negative {
            rule: "grant-row-shape",
            what: "a grant conferring no rights",
            break_it: |state| {
                let key = state.model.acl.keys().next().expect("a grant").clone();
                state.model.acl.insert(key, 0);
            },
        },
        Negative {
            rule: "admin-revision",
            what: "an admin revision over §5.2's ceiling",
            break_it: |state| state.model.admin_revision = limits::MAX_COUNTER + 1,
        },
        Negative {
            rule: "session-row-shape",
            what: "a consumed sequence with no saved result",
            break_it: |state| {
                let key = *state
                    .model
                    .sessions
                    .iter()
                    .find(|(_, s)| s.last_result.is_some())
                    .map(|(k, _)| k)
                    .expect("an answered session");
                state
                    .model
                    .sessions
                    .get_mut(&key)
                    .expect("the row")
                    .last_result = None;
            },
        },
        Negative {
            rule: "intent-row-shape",
            what: "a member seed whose join permit does not verify",
            break_it: |state| {
                // The fixture's own intent, named rather than taken by
                // iteration order: the generated history retains intents of its
                // own now, and the lowest operation id is not reliably the one
                // carrying a seed.
                let intent = state
                    .model
                    .intents
                    .get_mut(&[0x55u8; 16])
                    .expect("the fixture's intent");
                let permit = intent.plan.member_seeds[0]
                    .join_permit
                    .as_mut()
                    .expect("a permit");
                let last = permit.len() - 1;
                permit[last] ^= 0xFF;
            },
        },
        Negative {
            rule: "audit-row-shape",
            what: "an outcome the operation cannot produce",
            break_it: |state| {
                let key = *state.model.audit.keys().next().expect("an audit row");
                let row = state.model.audit.get_mut(&key).expect("the row");
                row.operation_code = 1; // acquire
                row.result = crate::coordination::model::state::AuditResult::Client(
                    crate::coordination::reason::Outcome::Released,
                );
            },
        },
        Negative {
            rule: "resource-row-shape",
            what: "a free row carrying an impossible TTL",
            break_it: |state| {
                let key = state
                    .model
                    .resources
                    .iter()
                    .find(|(_, row)| !row.held)
                    .map(|(k, _)| k.clone())
                    .expect("a free resource");
                state.model.resources.get_mut(&key).expect("the row").ttl_ms =
                    limits::MIN_TTL_MS - 1;
            },
        },
        Negative {
            rule: "resource-row-shape",
            what: "a held row with no lease duration",
            break_it: |state| {
                // Zero is legal on a *free* row and not on a held one: a held
                // lease with no duration expired the moment it was granted, and
                // the expiry path would never fire for it.
                let key = state
                    .model
                    .resources
                    .iter()
                    .find(|(_, row)| row.held)
                    .map(|(k, _)| k.clone())
                    .expect("a held resource");
                state.model.resources.get_mut(&key).expect("the row").ttl_ms = 0;
            },
        },
        Negative {
            rule: "session-row-shape",
            what: "a saved snapshot of a held resource with no lease duration",
            break_it: |state| {
                let key = *state
                    .model
                    .sessions
                    .iter()
                    .find(|(_, s)| {
                        s.last_result
                            .as_ref()
                            .and_then(|r| r.snapshot.as_ref())
                            .is_some_and(|snapshot| snapshot.held)
                    })
                    .map(|(k, _)| k)
                    .expect("a session whose saved snapshot was held");
                state
                    .model
                    .sessions
                    .get_mut(&key)
                    .expect("the row")
                    .last_result
                    .as_mut()
                    .expect("a reply")
                    .snapshot
                    .as_mut()
                    .expect("a snapshot")
                    .ttl_ms = 0;
            },
        },
        Negative {
            rule: "session-row-shape",
            what: "a saved snapshot carrying an impossible TTL",
            break_it: |state| {
                let key = *state
                    .model
                    .sessions
                    .iter()
                    .find(|(_, s)| s.last_result.as_ref().is_some_and(|r| r.snapshot.is_some()))
                    .map(|(k, _)| k)
                    .expect("a session with a saved snapshot");
                state
                    .model
                    .sessions
                    .get_mut(&key)
                    .expect("the row")
                    .last_result
                    .as_mut()
                    .expect("a reply")
                    .snapshot
                    .as_mut()
                    .expect("a snapshot")
                    .ttl_ms = limits::MAX_TTL_MS + 1;
            },
        },
        Negative {
            rule: "stamped-log-ids",
            what: "an intent accepted above last_applied",
            break_it: |state| {
                let last = state.model.last_applied.expect("an applied entry");
                state
                    .model
                    .intents
                    .get_mut(&[0x55u8; 16])
                    .expect("the fixture's intent")
                    .accepted_log_id = LogId::new(last.term, last.leader_node_id, last.index + 1);
            },
        },
        Negative {
            rule: "route-row-shape",
            what: "a ticket for a different node",
            break_it: |state| {
                let node = *state.model.routes.keys().next().expect("a route");
                let other = crate::coordination::model::membership::test_ticket(&[0x9Eu8; 32]);
                state.model.routes.get_mut(&node).expect("the row").ticket = other;
            },
        },
    ];

    /// Every field §11.3 declares is named by a rule that constrains it.
    ///
    /// `every_invariant_has_an_independent_negative` proves the two *code*
    /// lists agree with each other, which is a real property and not this one:
    /// both lists could omit the same field and stay consistent. This sweep is
    /// written from the declared types instead -- every field of the stored meta
    /// row and of each §11.3 row value -- and asks which rule covers it. A field
    /// added to any of those types fails here until someone decides whether it
    /// needs a bound and says so.
    ///
    /// `Unconstrained` is a real answer for a field whose whole domain is legal:
    /// `held` is a bool, and every bool is a state. Recording it is what makes
    /// the sweep a decision rather than an omission.
    #[test]
    fn every_declared_field_is_covered_by_a_rule_or_recorded_as_unconstrained() {
        /// Which rule constrains a declared field, or why none does.
        enum Cover {
            /// The named rule in [`INVARIANTS`], or a `validate_meta` /
            /// `validate_meta_wire` check.
            Rule(&'static str),
            /// Every value of the field's type is legal state.
            Unconstrained(&'static str),
        }
        use Cover::{Rule, Unconstrained};

        // Written from the declared types, field by field, in their order.
        let sweep: &[(&str, &str, Cover)] = &[
            // CoordinationMetaV1 (§11.3)
            (
                "CoordinationMetaV1",
                "schema_version",
                Rule("wire: schema version"),
            ),
            (
                "CoordinationMetaV1",
                "group_id",
                Rule("wire: 32-byte width"),
            ),
            (
                "CoordinationMetaV1",
                "genesis_hash",
                Rule("wire: 32-byte width"),
            ),
            (
                "CoordinationMetaV1",
                "authority_public_key",
                Rule("meta: curve point"),
            ),
            (
                "CoordinationMetaV1",
                "authority_domain",
                Rule("meta: 1..=128 ASCII"),
            ),
            (
                "CoordinationMetaV1",
                "deployment_profile_code",
                Rule("meta: declared profile"),
            ),
            (
                "CoordinationMetaV1",
                "last_applied",
                Rule("wire: leader width; audit-tip"),
            ),
            (
                "CoordinationMetaV1",
                "membership",
                Rule("meta: reachability"),
            ),
            (
                "CoordinationMetaV1",
                "admin_revision",
                Rule("admin-revision"),
            ),
            (
                "CoordinationMetaV1",
                "recovery_export_hash",
                Rule("wire: width and §13 pairing"),
            ),
            (
                "CoordinationMetaV1",
                "recovered_from",
                Rule("wire: widths and domain"),
            ),
            (
                "CoordinationMetaV1",
                "aggregate_growth_bytes",
                Rule("aggregate-counters"),
            ),
            (
                "CoordinationMetaV1",
                "aggregate_reserve_bytes",
                Rule("aggregate-counters"),
            ),
            ("CoordinationMetaV1", "audit_floor", Rule("audit-floor")),
            // RecoveredFromV1 (§13)
            (
                "RecoveredFromV1",
                "source_group_id",
                Rule("wire: 32-byte width"),
            ),
            (
                "RecoveredFromV1",
                "source_genesis_hash",
                Rule("wire: 32-byte width"),
            ),
            (
                "RecoveredFromV1",
                "source_authority_public_key",
                Rule("wire: 32-byte width"),
            ),
            (
                "RecoveredFromV1",
                "source_authority_domain",
                Rule("wire: 1..=128 ASCII"),
            ),
            (
                "RecoveredFromV1",
                "export_hash",
                Rule("wire: width and §13 pairing"),
            ),
            (
                "RecoveredFromV1",
                "source_last_applied",
                Rule("wire: leader width"),
            ),
            // ResourceRowV1 (§11.3)
            ("ResourceRowV1", "epoch", Rule("resource-row-shape")),
            (
                "ResourceRowV1",
                "held",
                Unconstrained("a bool; the pairing with `holder` is checked by the codec"),
            ),
            (
                "ResourceRowV1",
                "holder_node_id",
                Rule("codec: width and lease coherence"),
            ),
            ("ResourceRowV1", "ttl_ms", Rule("resource-row-shape")),
            (
                "ResourceRowV1",
                "lease_revision",
                Rule("resource-row-shape"),
            ),
            (
                "ResourceRowV1",
                "value_revision",
                Rule("resource-row-shape"),
            ),
            ("ResourceRowV1", "value", Rule("resource-row-shape")),
            ("ResourceRowV1", "acl_revision", Rule("resource-row-shape")),
            // resource_acl (§11.3) -- a redb `u64`, no Fory root
            ("resource_acl", "rights", Rule("grant-row-shape")),
            (
                "resource_acl",
                "key",
                Rule("grant-row-shape: the resource must exist"),
            ),
            // ClientSessionV1 (§11.3)
            (
                "ClientSessionV1",
                "last_sequence",
                Rule("session-row-shape"),
            ),
            (
                "ClientSessionV1",
                "request_hash",
                Rule("codec: 32-byte width"),
            ),
            ("ClientSessionV1", "last_result", Rule("session-row-shape")),
            (
                "ClientSessionV1",
                "retired",
                Unconstrained("a bool; a tombstone and a live row are both states"),
            ),
            // SavedReplyV1, inside a session row
            (
                "SavedReplyV1",
                "disposition_code",
                Rule("codec: stored replies are Consumed"),
            ),
            ("SavedReplyV1", "outcome_code", Rule("codec: §5.2 outcome")),
            (
                "SavedReplyV1",
                "reason_code",
                Rule("codec: §5.2 reason; session-row-shape pairing"),
            ),
            ("SavedReplyV1", "snapshot", Rule("session-row-shape")),
            ("SavedReplyV1", "acl_revision", Rule("session-row-shape")),
            ("SavedReplyV1", "admin_revision", Rule("session-row-shape")),
            ("SavedReplyV1", "operation_id", Rule("codec: 16-byte width")),
            (
                "SavedReplyV1",
                "membership_log_id",
                Rule("codec: leader width"),
            ),
            (
                "SavedSnapshotV1",
                "group_id",
                Rule("codec: the holding group"),
            ),
            ("SavedSnapshotV1", "resource", Rule("codec: §4.2 identity")),
            ("SavedSnapshotV1", "epoch", Rule("session-row-shape")),
            (
                "SavedSnapshotV1",
                "holder",
                Rule("codec: width and lease coherence"),
            ),
            (
                "SavedSnapshotV1",
                "held",
                Rule("session-row-shape: pairing with holder"),
            ),
            ("SavedSnapshotV1", "ttl_ms", Rule("session-row-shape")),
            (
                "SavedSnapshotV1",
                "lease_revision",
                Rule("session-row-shape"),
            ),
            (
                "SavedSnapshotV1",
                "value_revision",
                Rule("session-row-shape"),
            ),
            ("SavedSnapshotV1", "acl_revision", Rule("session-row-shape")),
            // MembershipIntentV1 (§11.3)
            (
                "MembershipIntentV1",
                "actor_node_id",
                Rule("codec: 32-byte width"),
            ),
            (
                "MembershipIntentV1",
                "request_hash",
                Rule("codec: 32-byte width"),
            ),
            (
                "MembershipIntentV1",
                "expected_membership",
                Rule("codec: agrees with the plan"),
            ),
            (
                "MembershipIntentV1",
                "desired_plan",
                Rule("intent-row-shape"),
            ),
            ("MembershipIntentV1", "status_code", Rule("codec: §9 phase")),
            (
                "MembershipIntentV1",
                "result_code",
                Unconstrained("caller-supplied by §7.3's progress report; §5.2 declares no set"),
            ),
            (
                "MembershipIntentV1",
                "accepted_log_id",
                Rule("stamped-log-ids"),
            ),
            (
                "MembershipIntentV1",
                "completed_log_id",
                Rule("stamped-log-ids; codec pairing"),
            ),
            (
                "MembershipIntentV1",
                "observed_membership_log_id",
                Rule("stamped-log-ids"),
            ),
            // NodeRouteV1 (§11.3)
            ("NodeRouteV1", "ticket_bytes", Rule("route-row-shape")),
            (
                "NodeRouteV1",
                "source_operation_id",
                Rule("codec: 16-byte width"),
            ),
            ("NodeRouteV1", "introduced_log_id", Rule("stamped-log-ids")),
            // AuditRowV1 (§11.3)
            ("AuditRowV1", "leader_node_id", Rule("codec: 32-byte width")),
            (
                "AuditRowV1",
                "actor_node_id",
                Rule("codec: width or absent"),
            ),
            ("AuditRowV1", "operation_code", Rule("audit-row-shape")),
            ("AuditRowV1", "resource", Rule("codec: §4.2 identity")),
            (
                "AuditRowV1",
                "result_code",
                Rule("audit-row-shape: code space"),
            ),
            ("AuditRowV1", "epoch", Rule("audit-row-shape")),
            ("AuditRowV1", "value_revision", Rule("audit-row-shape")),
            // group_admins (§11.3) -- a redb key with a unit value
            (
                "group_admins",
                "subject_node_id",
                Rule("codec: redb key width"),
            ),
        ];

        // A field added to any of these types is a **compile** error here, not a
        // count that drifts: each pattern is exhaustive, so the sweep cannot
        // fall behind the declarations it claims to cover. The counts below
        // then check the sweep against the patterns.
        #[allow(unused_variables, clippy::too_many_arguments)]
        fn exhaustive(
            meta: CoordinationMetaV1,
            recovered: crate::coordination::raft::RecoveredFromV1,
            resource: ResourceRowV1,
            session: ClientSessionV1,
            reply: crate::coordination::contract::SavedReplyV1,
            snapshot: crate::coordination::contract::SavedSnapshotV1,
            intent: MembershipIntentV1,
            route: NodeRouteV1,
            audit: AuditRowV1,
        ) {
            let CoordinationMetaV1 {
                schema_version,
                group_id,
                genesis_hash,
                authority_public_key,
                authority_domain,
                deployment_profile_code,
                last_applied,
                membership,
                admin_revision,
                recovery_export_hash,
                recovered_from,
                aggregate_growth_bytes,
                aggregate_reserve_bytes,
                audit_floor,
            } = meta;
            let crate::coordination::raft::RecoveredFromV1 {
                source_group_id,
                source_genesis_hash,
                source_authority_public_key,
                source_authority_domain,
                export_hash,
                source_last_applied,
            } = recovered;
            let ResourceRowV1 {
                epoch,
                held,
                holder_node_id,
                ttl_ms,
                lease_revision,
                value_revision,
                value,
                acl_revision,
            } = resource;
            let ClientSessionV1 {
                last_sequence,
                request_hash,
                last_result,
                retired,
            } = session;
            let crate::coordination::contract::SavedReplyV1 {
                disposition_code,
                outcome_code,
                reason_code,
                snapshot: saved,
                acl_revision,
                admin_revision,
                operation_id,
                membership_log_id,
            } = reply;
            let crate::coordination::contract::SavedSnapshotV1 {
                group_id,
                resource,
                epoch,
                holder,
                held,
                ttl_ms,
                lease_revision,
                value_revision,
                acl_revision,
            } = snapshot;
            let MembershipIntentV1 {
                actor_node_id,
                request_hash,
                expected_membership,
                desired_plan,
                status_code,
                result_code,
                accepted_log_id,
                completed_log_id,
                observed_membership_log_id,
            } = intent;
            let NodeRouteV1 {
                ticket_bytes,
                source_operation_id,
                introduced_log_id,
            } = route;
            let AuditRowV1 {
                leader_node_id,
                actor_node_id,
                operation_code,
                resource,
                result_code,
                epoch,
                value_revision,
            } = audit;
        }
        let _ = exhaustive;

        let declared: BTreeMap<&str, usize> = BTreeMap::from([
            ("CoordinationMetaV1", 14),
            ("RecoveredFromV1", 6),
            ("ResourceRowV1", 8),
            ("resource_acl", 2),
            ("ClientSessionV1", 4),
            ("SavedReplyV1", 8),
            ("SavedSnapshotV1", 9),
            ("MembershipIntentV1", 9),
            ("NodeRouteV1", 3),
            ("AuditRowV1", 7),
            ("group_admins", 1),
        ]);
        let mut swept: BTreeMap<&str, usize> = BTreeMap::new();
        for (ty, _, _) in sweep {
            *swept.entry(ty).or_default() += 1;
        }
        assert_eq!(
            declared, swept,
            "the sweep must name every field of every §11.3 type; a count that moved means a \
             field was added or removed and its bound has not been decided"
        );

        // And every covered field names a rule that exists, so a renamed rule
        // cannot leave a field pointing at nothing.
        let rules: BTreeSet<&str> = INVARIANTS.iter().map(|i| i.name).collect();
        for (ty, field, cover) in sweep {
            if let Rule(name) = cover {
                // Rules outside `INVARIANTS` -- the codec's own narrowing, and
                // `validate_meta` -- are named by prefix rather than by
                // identifier, and carry their subject in the text.
                let is_invariant_rule = rules.iter().any(|r| name.starts_with(r));
                let is_prefixed = name.starts_with("wire:")
                    || name.starts_with("meta:")
                    || name.starts_with("codec:");
                assert!(
                    is_invariant_rule || is_prefixed,
                    "{ty}.{field} names rule `{name}`, which is neither an invariant nor a \
                     declared check"
                );
            }
        }

        // The unconstrained fields are few and each says why. If this grows,
        // the growth is the thing to look at.
        let unconstrained = sweep
            .iter()
            .filter(|(_, _, c)| matches!(c, Unconstrained(_)))
            .count();
        assert_eq!(
            unconstrained, 3,
            "three fields have no bound because every value of their type is legal state; a \
             different count means one was added or removed without a decision"
        );
    }

    /// Every rule has a negative, and every negative trips the rule it names.
    ///
    /// The second half is what makes the first worth having. A perturbation
    /// that merely gets the state refused proves nothing about the rule it was
    /// written for, and the earlier version of these tests had exactly that
    /// problem: an audit-tip perturbation at the same index was caught by the
    /// ordering check two lines later while the assertion matched a substring
    /// both messages carry.
    #[test]
    fn every_invariant_has_an_independent_negative() {
        let covered: BTreeSet<&str> = NEGATIVES.iter().map(|n| n.rule).collect();
        let declared: BTreeSet<&str> = INVARIANTS.iter().map(|i| i.name).collect();
        assert_eq!(
            declared, covered,
            "every §11.3 invariant needs a negative that reaches it, and every negative \
             needs a rule that exists"
        );

        for negative in NEGATIVES {
            let mut state = coherent_state();
            (negative.break_it)(&mut state);
            // The counters are a function of the rows, so any perturbation that
            // adds or resizes one moves them. Restoring them keeps the rule
            // under test the one that fires -- except, of course, when it is
            // the counters themselves.
            if negative.rule != "aggregate-counters" {
                state.model.growth_bytes = state.model.recompute_growth();
                state.model.reserve_bytes = state.model.recompute_reserve();
            }

            let err = validate(&state).expect_err(&format!(
                "{}: {} must be refused",
                negative.rule, negative.what
            ));
            let StorageError::StoredCorruption { what, .. } = &err else {
                panic!("{}: expected stored corruption, got {err:?}", negative.rule);
            };
            assert_eq!(
                *what, negative.rule,
                "{}: {} tripped `{what}` instead, so the rule it claims to cover is still \
                 unproven",
                negative.rule, negative.what
            );
        }
    }

    /// An audit row for an entry `last_applied` says was never applied is data
    /// loss: §11.3 writes both in one transaction, so the state is unreachable.
    #[test]
    fn an_audit_row_ahead_of_last_applied_is_refused() {
        let mut state = coherent_state();
        let tip = state.model.audit_tip.expect("a tip");
        state.model.last_applied = Some(LogId::new(tip.term, tip.leader_node_id, tip.index - 1));
        let err = validate(&state).expect_err("tip ahead");
        assert!(err.to_string().contains("runs ahead"), "got: {err}");

        let mut state = coherent_state();
        state.model.last_applied = None;
        let err = validate(&state).expect_err("nothing applied");
        assert!(err.to_string().contains("nothing applied"), "got: {err}");

        // The case a maximum-index comparison cannot see: the same index under
        // a different leader is a different entry.
        let mut state = coherent_state();
        let tip = state.model.audit_tip.expect("a tip");
        state.model.audit_tip = Some(LogId::new(tip.term, [0xEE; 32], tip.index));
        assert!(validate(&state).is_err(), "a different leader at one index");
    }

    /// The tip is the highest **index**, not the last key in table order.
    ///
    /// The table is keyed `(term, index)`, so redb sorts by term first. A
    /// corrupt row at a higher term and a lower index sorts last, and a tip
    /// taken from the last key would report it while hiding the genuinely
    /// newest entry.
    #[test]
    fn the_audit_tip_is_the_highest_index_not_the_last_key() {
        let base = coherent_state();
        let ((term, index), row) = base
            .model
            .audit
            .iter()
            .next_back()
            .map(|(k, v)| (*k, v.clone()))
            .expect("an audit row");

        let mut audit = base.model.audit.clone();
        audit.insert((term + 1, index - 1), row);
        assert_eq!(
            audit.iter().next_back().map(|(k, _)| *k),
            Some((term + 1, index - 1)),
            "the corrupt row really does sort last, which is the trap"
        );
        assert_eq!(
            super::rebuild_audit_tip(&audit).map(|id| id.index),
            Some(index),
            "the tip must still be the newest entry"
        );
    }

    // ── Round trip through redb ─────────────────────────────────────────────

    /// A complete state survives a write and a read, byte for byte.
    ///
    /// The property every restart depends on. It runs against a real database
    /// rather than an in-memory double: the ceilings, the redb tuple keys and
    /// the codec are all things a double would not have.
    #[test]
    fn a_complete_state_round_trips_through_redb() {
        let (_dir, database) = scratch_database();
        let state = coherent_state();

        commit(&database, state.clone(), &every_row(&state)).expect("commit");
        let loaded = load(&database).expect("load");

        assert_eq!(loaded.model.resources, state.model.resources);
        assert_eq!(loaded.model.acl, state.model.acl);
        assert_eq!(loaded.model.admins, state.model.admins);
        assert_eq!(loaded.model.sessions, state.model.sessions);
        assert_eq!(loaded.model.intents, state.model.intents);
        assert_eq!(loaded.model.routes, state.model.routes);
        assert_eq!(loaded.model.audit, state.model.audit);
        assert_eq!(loaded.model.last_applied, state.model.last_applied);
        assert_eq!(loaded.model.audit_floor, state.model.audit_floor);
        assert_eq!(loaded.model.audit_tip, state.model.audit_tip);
        assert_eq!(loaded.model.growth_bytes, state.model.growth_bytes);
        assert_eq!(loaded.model.reserve_bytes, state.model.reserve_bytes);
        assert_eq!(loaded.model.admin_revision, state.model.admin_revision);
        assert_eq!(loaded.model.authority, state.model.authority);
        assert_eq!(loaded.membership, state.membership);
        assert_eq!(
            loaded.model, state.model,
            "every field, not only the ones named above"
        );
    }

    /// A deletion is persisted as a deletion, not merely dropped from memory.
    #[test]
    fn a_recorded_deletion_removes_the_row_from_disk() {
        let (_dir, database) = scratch_database();
        let mut state = coherent_state();
        commit(&database, state.clone(), &every_row(&state)).expect("commit");

        let admin = *state.model.admins.iter().next().expect("an admin");
        state.model.admins.remove(&admin);
        state.model.growth_bytes = state.model.recompute_growth();

        let mut changes = ChangeSet::default();
        changes.admins.insert(admin);
        commit(&database, state.clone(), &changes).expect("commit the deletion");

        let loaded = load(&database).expect("load");
        assert!(
            !loaded.model.admins.contains(&admin),
            "a change set naming a removed key must delete the row"
        );
        assert_eq!(loaded.model.admins, state.model.admins);
    }

    /// A row that will not narrow is refused at load, with the ceiling checked
    /// before the value is decoded.
    #[test]
    fn a_corrupt_row_is_refused_at_load() {
        let (_dir, database) = scratch_database();
        let state = coherent_state();
        commit(&database, state.clone(), &every_row(&state)).expect("commit");

        let key = *state.model.routes.keys().next().unwrap_or(&[0u8; 32]);
        let write = durable_write(&database, Db::State).expect("write");
        {
            let mut table = write.open_table(ROUTES).expect("open");
            table
                .insert(key, vec![0xAAu8; MAX_ROUTE_ROW_BYTES + 1].as_slice())
                .expect("insert");
        }
        write.commit().expect("commit");

        let err = load(&database).expect_err("an over-ceiling row");
        assert!(
            err.to_string().contains("ceiling"),
            "the ceiling must be what refused it, not the decode: {err}"
        );
        assert!(err.puts_durability_in_question());
    }

    /// An undefined rights bit is refused rather than carried into state.
    #[test]
    fn an_undefined_rights_bit_is_refused_at_load() {
        let (_dir, database) = scratch_database();
        let state = coherent_state();
        commit(&database, state.clone(), &every_row(&state)).expect("commit");

        let (resource, subject) = state.model.acl.keys().next().expect("a grant").clone();
        let write = durable_write(&database, Db::State).expect("write");
        {
            let mut table = write.open_table(ACL).expect("open");
            table
                .insert(
                    (resource.namespace(), resource.key(), subject),
                    limits::ALL_RIGHTS | 0x80,
                )
                .expect("insert");
        }
        write.commit().expect("commit");

        let err = load(&database).expect_err("an undefined bit");
        assert!(err.to_string().contains("does not define"), "got: {err}");
    }
}
