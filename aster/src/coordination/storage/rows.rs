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
    AuditRow, ChangeSet, GroupAuthority, LogId, ResourceRow, Route, State,
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

    // The model's member set is *derived* from the stored membership, never
    // stored beside it: two copies of the same fact are two things that can
    // disagree, and §9 protects a route by asking whether its node is a member.
    let members: BTreeSet<[u8; 32]> = membership
        .membership()
        .nodes()
        .map(|(id, _)| *id.as_bytes())
        .collect();
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
            members,
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
fn rebuild_audit_tip(audit: &BTreeMap<(u64, u64), AuditRow>) -> Option<LogId> {
    audit
        .iter()
        .next_back()
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
    if model.authority.domain.len()
        > crate::coordination::model::authority::MAX_AUTHORITY_DOMAIN_BYTES
    {
        return Err(bad(
            "authority_domain",
            format!(
                "{} bytes, over the {}-byte limit",
                model.authority.domain.len(),
                crate::coordination::model::authority::MAX_AUTHORITY_DOMAIN_BYTES
            ),
        ));
    }

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
    // An unknown profile code permits nothing, so it cannot reach a state
    // machine that would then fall back to the production rule.
    let voters = state
        .membership
        .membership()
        .get_joint_config()
        .last()
        .map_or(0, std::collections::BTreeSet::len);
    if voters > 0 && shape::profile_permits_voters(model.deployment_profile_code, voters) {
        return Err(bad(
            "deployment_profile_code",
            format!(
                "profile {} does not permit {voters} voters",
                model.deployment_profile_code
            ),
        ));
    }

    // The model's projection of the stored membership is derived at load, so a
    // disagreement here means the derivation itself drifted rather than that
    // two persisted copies differ.
    let members: BTreeSet<[u8; 32]> = state
        .membership
        .membership()
        .nodes()
        .map(|(id, _)| *id.as_bytes())
        .collect();
    if model.members != members {
        return Err(bad(
            "membership",
            "the member set does not match the stored membership".into(),
        ));
    }
    let membership_log_id = state.membership.log_id().as_ref().map(LogId::from);
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

/// The half of the check that reads the rows.
///
/// Everything here is recomputed and compared: the caps count real rows, the
/// counters are re-summed from them, and the audit tip and floor are rebuilt.
fn validate_rows(state: &DurableState) -> StorageResult<()> {
    let bad = |what: &'static str, detail: String| StorageError::corrupt(Db::State, what, detail);
    let model = &state.model;

    // ── §12.1 slot caps ─────────────────────────────────────────────────────
    let caps: [(&'static str, u64, u64); 7] = [
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
            limits::MAX_RETAINED_INTENTS,
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
            return Err(bad(table, format!("{count} rows, over the {cap}-row cap")));
        }
    }

    // Per-resource and per-actor sub-caps. Group totals do not imply them: one
    // resource may hold every grant, and one actor every session.
    let mut grants_per_resource: BTreeMap<&ResourceKey, u64> = BTreeMap::new();
    for (resource, _) in model.acl.keys() {
        *grants_per_resource.entry(resource).or_default() += 1;
    }
    if let Some((resource, count)) = grants_per_resource
        .into_iter()
        .find(|(_, n)| *n > limits::MAX_GRANTS_PER_RESOURCE)
    {
        return Err(bad(
            "resource_acl",
            format!(
                "{resource:?} holds {count} grants, over the {}-grant cap",
                limits::MAX_GRANTS_PER_RESOURCE
            ),
        ));
    }

    let mut retained_per_actor: BTreeMap<[u8; 32], u64> = BTreeMap::new();
    let mut active_per_actor: BTreeMap<[u8; 32], u64> = BTreeMap::new();
    for ((actor, _), session) in &model.sessions {
        *retained_per_actor.entry(*actor).or_default() += 1;
        if !session.retired {
            *active_per_actor.entry(*actor).or_default() += 1;
        }
    }
    for (scope, counts, cap) in [
        (
            "retained",
            &retained_per_actor,
            limits::MAX_RETAINED_SESSIONS_PER_ACTOR,
        ),
        (
            "active",
            &active_per_actor,
            limits::MAX_ACTIVE_CLIENTS_PER_ACTOR,
        ),
    ] {
        if let Some((actor, count)) = counts.iter().find(|(_, n)| **n > cap) {
            return Err(bad(
                "client_sessions",
                format!("actor {actor:02x?} holds {count} {scope} sessions, over the {cap} cap"),
            ));
        }
    }
    let active_group = active_per_actor.values().sum::<u64>();
    if active_group > limits::MAX_ACTIVE_CLIENTS_PER_GROUP {
        return Err(bad(
            "client_sessions",
            format!(
                "{active_group} active sessions, over the {}-session cap",
                limits::MAX_ACTIVE_CLIENTS_PER_GROUP
            ),
        ));
    }

    // ── §12.1 counters ──────────────────────────────────────────────────────
    //
    // Recomputed from the rows and compared. The persisted figure is evidence,
    // never an input: apply maintains it incrementally, so the only thing that
    // can catch an incremental update that went wrong is the full sum.
    let growth = model.recompute_growth();
    if model.growth_bytes != growth {
        return Err(bad(
            "aggregate_growth_bytes",
            format!(
                "persisted {} but the rows charge {growth}",
                model.growth_bytes
            ),
        ));
    }
    let reserve = model.recompute_reserve();
    if model.reserve_bytes != reserve {
        return Err(bad(
            "aggregate_reserve_bytes",
            format!(
                "persisted {} but the rows charge {reserve}",
                model.reserve_bytes
            ),
        ));
    }
    // ── §11.3 audit identity ────────────────────────────────────────────────
    let tip = rebuild_audit_tip(&model.audit);
    if model.audit_tip != tip {
        return Err(bad(
            "audit_tip",
            format!(
                "state holds {:?} but the rows give {tip:?}",
                model.audit_tip
            ),
        ));
    }
    match (tip, model.last_applied) {
        (Some(tip), None) => {
            return Err(bad(
                "audit_tip",
                format!("an audit row at {tip:?} with nothing applied"),
            ))
        }
        (Some(tip), Some(last)) if tip.index > last.index => {
            return Err(bad(
                "audit_tip",
                format!("{tip:?} runs ahead of last_applied {last:?}"),
            ))
        }
        // The case a maximum-index comparison cannot see: the same index under
        // a different term or leader is a *different* entry.
        (Some(tip), Some(last)) if tip.index == last.index && tip != last => {
            return Err(bad(
                "audit_tip",
                format!("{tip:?} and last_applied {last:?} are different entries at one index"),
            ))
        }
        _ => {}
    }

    // The floor is the whole log id of the oldest retained row whenever
    // anything has been pruned. With rows present it is checkable exactly; with
    // none, the floor names a row that is gone and there is nothing to compare.
    if let (Some(floor), Some(((term, index), oldest))) =
        (model.audit_floor, model.audit.iter().next())
    {
        let expected = LogId::new(*term, oldest.leader_node_id, *index);
        if floor != expected {
            return Err(bad(
                "audit_floor",
                format!("persisted {floor:?} but the oldest retained row is {expected:?}"),
            ));
        }
    }

    // ── §5.2 client counters ────────────────────────────────────────────────
    //
    // A sequence above the ceiling is out of contract *and* dangerous: `last`
    // is a `u64` read back from storage, so a corrupt `u64::MAX` wraps to 0 in
    // release and a stale sequence then looks like the exact next one.
    if let Some(((actor, client), session)) = model
        .sessions
        .iter()
        .find(|(_, s)| s.last_sequence > limits::MAX_COUNTER)
    {
        return Err(bad(
            "client_sessions",
            format!(
                "session {actor:02x?}/{client:02x?} holds sequence {}, over the {} ceiling",
                session.last_sequence,
                limits::MAX_COUNTER
            ),
        ));
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

    /// The membership a model state implies, so a fixture's two views agree.
    fn membership_for(model: &State) -> StoredMembership<CoordinationNodeId, CoordinationNode> {
        let voters: BTreeSet<CoordinationNodeId> = model
            .members
            .iter()
            .map(|id| CoordinationNodeId::from_array(*id))
            .collect();
        let nodes: BTreeMap<CoordinationNodeId, CoordinationNode> =
            voters.iter().map(|id| (*id, CoordinationNode)).collect();
        StoredMembership::new(
            model.membership_log_id.as_ref().map(Into::into),
            Membership::new(vec![voters], nodes),
        )
    }

    /// A coherent complete state: every row, count, charge, audit tip and
    /// metadata value agrees, because apply produced it.
    fn coherent_state() -> DurableState {
        let model = sample_applied_state(400);
        assert!(
            model.counters_match_recomputation(),
            "the history fixture must itself be coherent"
        );
        let membership = membership_for(&model);
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
        assert_eq!(state.model.members.len(), shape::MAX_VOTERS + 2 + 2);
    }

    /// Each metadata-local constraint refuses on its own.
    #[test]
    fn each_metadata_local_constraint_refuses_on_its_own() {
        let narrow = |meta: CoordinationMetaV1| meta_from_wire(&meta);

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
        let state = narrow(meta).expect("32 bytes narrows");
        let err = validate_meta(&state).expect_err("not a key");
        assert!(err.to_string().contains("decompress"), "got: {err}");

        // A domain past §7.1's limit.
        let mut meta = maximal_legal_meta();
        meta.authority_domain =
            "d".repeat(crate::coordination::model::authority::MAX_AUTHORITY_DOMAIN_BYTES + 1);
        let state = narrow(meta).expect("narrows");
        let err = validate_meta(&state).expect_err("too long");
        assert!(err.to_string().contains("authority_domain"), "got: {err}");

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
            let state = narrow(meta).expect("narrows");
            let err = validate_meta(&state).expect_err("over budget");
            assert!(err.to_string().contains("budget"), "{field}: {err}");
        }

        // A voter count the profile does not permit. Four voters is legal in
        // OpenRaft and illegal in Aster, which is exactly the distinction
        // §7.1 draws.
        let mut meta = maximal_legal_meta();
        meta.membership
            .membership
            .configs
            .last_mut()
            .expect("a config")
            .ids
            .pop();
        let state = narrow(meta).expect("narrows");
        let err = validate_meta(&state).expect_err("four voters");
        assert!(err.to_string().contains("does not permit"), "got: {err}");

        // An unknown profile permits nothing rather than falling back.
        let mut meta = maximal_legal_meta();
        meta.deployment_profile_code = 99;
        let state = narrow(meta).expect("narrows");
        assert!(
            validate_meta(&state).is_err(),
            "an unknown profile permits nothing"
        );

        // An audit floor above what was applied.
        let mut meta = maximal_legal_meta();
        meta.audit_floor = Some(LogIdV1 {
            term: 9,
            leader_node_id: vec![3u8; 32],
            index: 9_000_001,
        });
        let state = narrow(meta).expect("narrows");
        let err = validate_meta(&state).expect_err("floor ahead");
        assert!(err.to_string().contains("audit_floor"), "got: {err}");
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
        state.model.members.insert([0xEE; 32]);
        let err = validate_meta(&state).expect_err("extra member");
        assert!(err.to_string().contains("member set"), "got: {err}");

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

    /// Every derived value is compared against a recomputation, so a persisted
    /// one that drifted is refused rather than trusted.
    #[test]
    fn each_derived_value_is_recomputed_and_compared() {
        let base = coherent_state();

        let mut drifted = base.clone();
        drifted.model.growth_bytes += 1;
        let err = validate(&drifted).expect_err("growth drifted");
        assert!(err.to_string().contains("the rows charge"), "got: {err}");

        let mut drifted = base.clone();
        drifted.model.reserve_bytes -= 1;
        let err = validate(&drifted).expect_err("reserve drifted");
        assert!(err.to_string().contains("the rows charge"), "got: {err}");

        // The tip is **rebuilt** from the rows, not compared against the
        // ordering rule that follows it. The perturbation is chosen so only the
        // rebuild can catch it: a tip naming the second-newest retained row
        // sits below `last_applied`, so the "runs ahead" and "same index,
        // different entry" checks both pass. Verified by mutation -- replacing
        // the rebuild with the persisted value survived a tip perturbed at the
        // same index, because the ordering check caught that one instead and
        // the assertion was matching on a substring both messages carry.
        let mut drifted = base.clone();
        let ((term, index), second_newest) = drifted
            .model
            .audit
            .iter()
            .rev()
            .nth(1)
            .map(|(k, v)| (*k, v.clone()))
            .expect("at least two audit rows");
        drifted.model.audit_tip = Some(LogId::new(term, second_newest.leader_node_id, index));
        let err = validate(&drifted).expect_err("tip drifted");
        assert!(
            err.to_string().contains("but the rows give"),
            "the rebuild must be what refused it: {err}"
        );

        // And the case a maximum-index comparison cannot see: the same index
        // under a different leader is a different entry.
        let mut drifted = base.clone();
        let tip = drifted.model.audit_tip.expect("a tip");
        drifted.model.audit_tip = Some(LogId::new(tip.term, [0xEE; 32], tip.index));
        assert!(
            validate(&drifted).is_err(),
            "a different leader at one index"
        );

        // A floor that does not name the oldest retained row.
        let mut drifted = base.clone();
        let ((term, index), oldest) = drifted
            .model
            .audit
            .iter()
            .next()
            .map(|(k, v)| (*k, v.clone()))
            .expect("a row");
        drifted.model.audit_floor = Some(LogId::new(term, oldest.leader_node_id, index + 1));
        let err = validate(&drifted).expect_err("floor drifted");
        assert!(err.to_string().contains("audit_floor"), "got: {err}");
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
    }

    /// A §12.1 cap is checked against the rows actually present.
    #[test]
    fn a_table_over_its_cap_is_refused() {
        let mut state = coherent_state();
        for n in 0..=limits::MAX_ADMINS_PER_GROUP {
            let mut id = [0u8; 32];
            id[0..8].copy_from_slice(&n.to_be_bytes());
            state.model.admins.insert(id);
        }
        state.model.growth_bytes = state.model.recompute_growth();
        let err = validate(&state).expect_err("over the admin cap");
        assert!(err.to_string().contains("row cap"), "got: {err}");
    }

    /// Per-resource and per-actor sub-caps are checked too: a group total in
    /// range does not imply them, since one resource may hold every grant.
    #[test]
    fn the_per_subject_sub_caps_are_checked_independently() {
        let mut state = coherent_state();
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
            state.model.acl.insert((resource.clone(), id), 0);
        }
        state.model.growth_bytes = state.model.recompute_growth();
        let err = validate(&state).expect_err("over the per-resource cap");
        assert!(err.to_string().contains("grant cap"), "got: {err}");
    }

    /// A stored sequence above §5.2's ceiling is refused rather than read back
    /// as a `u64` that wraps.
    #[test]
    fn a_sequence_over_the_counter_ceiling_is_refused() {
        let mut state = coherent_state();
        let key = *state.model.sessions.keys().next().expect("a session");
        state
            .model
            .sessions
            .get_mut(&key)
            .expect("the row")
            .last_sequence = limits::MAX_COUNTER + 1;
        let err = validate(&state).expect_err("over the ceiling");
        assert!(err.to_string().contains("ceiling"), "got: {err}");
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
