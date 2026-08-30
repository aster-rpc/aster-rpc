//! The §13 recovery import (S5-B): one bounded, deterministic, all-or-nothing
//! transformation from a canonical recovery export into a new group's founding
//! state, run on every initial voter before `Raft::initialize`.
//!
//! The shape is two layers, and the split is the point:
//!
//! - [`transform`] is the **pure** function `(header, rows, new identity,
//!   genesis admins, bound hash) -> DurableState`. It owns every §13 rule the
//!   framing reader cannot: counter ceilings under checked arithmetic, TTL
//!   semantics, provenance relations, and cross-row consistency. Every initial
//!   voter runs it over the same verified bytes and the same signed genesis,
//!   so every voter computes the identical logical state — which is what makes
//!   the later cross-voter agreement a fact rather than a hope.
//! - [`import_recovery_export`] is the I/O wrapper: bound file read, exact
//!   BLAKE3 against the hash the new genesis binds, the canonical §11.3
//!   reader (which enforces framing, limits, widths, ordering, and inner Fory
//!   canonicality), the pristine-store check, and **one** durable
//!   [`rows::commit`]. A failure at any point leaves the store exactly as it
//!   was: nothing is written until everything has been verified.
//!
//! What the framing reader validates and what this module validates are
//! deliberately disjoint (recorded in the S5-B contract): the reader owns
//! widths, keys, values, rights bits and canonical bytes; this validator owns
//! everything that requires arithmetic or more than one row.

use std::path::Path;

use redb::Database;

use super::error::{StorageError, StorageResult};
use super::identity::StorageIdentity;
use super::rows::{self, DurableState};
use crate::coordination::model::limits;
use crate::coordination::model::state::{ChangeSet, NodeId, ResourceRow, State};
use crate::coordination::raft::RecoveredFromV1;
use crate::coordination::rowstream::{
    read_body, BodyError, BodyVisitor, RecoveryBodyRowV1, RecoveryHeaderV1, MAX_SNAPSHOT_BODY_BYTES,
};

/// The capability to bootstrap one storage directory from one recovery export.
///
/// Every field is derived from **one** verified genesis together with the
/// storage identity that genesis produced, inside [`RecoveryImport::seal`], and
/// nothing outside this module can build one any other way. That is the point
/// of the type. The alternative — a plain struct of public fields plus an
/// identity passed separately to the open — lets a caller pair identity and
/// genesis `A` with the export hash and administrators of `B`, and the
/// confirming Genesis deliberately will not rediscover the mismatch: it
/// re-verifies the signed document against the binding the import wrote, and
/// under a crossed pair both of those came from `A`.
///
/// So the identity travels *inside* the capability and the open path reads it
/// from here rather than accepting its own. There is exactly one identity in
/// play, and it is the one whose genesis bound this hash.
///
/// [`VerifiedGenesis`]: crate::coordination::model::authority::VerifiedGenesis
#[derive(Debug, Clone)]
pub(crate) struct RecoveryImport {
    path: std::path::PathBuf,
    /// The identity the sealing genesis produced. The store binds to this and
    /// [`transform`] reads the new group's authority from it.
    identity: StorageIdentity,
    /// `optional_recovery_export_hash` from the verified new genesis — the
    /// exact BLAKE3 the canonical body must have.
    expected_hash: [u8; 32],
    /// The new genesis's `initial_admins`, unioned with the exported set.
    genesis_admins: Vec<NodeId>,
}

impl RecoveryImport {
    /// Seal one capability from a verified genesis, the identity derived from
    /// it, and the operator-supplied export path.
    ///
    /// Refuses unless the identity really is *this* genesis's identity — **all
    /// five replicated fields**, plus the local node.
    ///
    /// The boot corridor derives both from the same document, so a disagreement
    /// is a wiring mistake rather than input; but it is precisely the wiring
    /// mistake that would otherwise found a group from the wrong export, so the
    /// comparison has to be total rather than representative. Two of the six
    /// carry their own argument:
    ///
    /// - **The authority key.** [`StorageIdentity`] binds a replicated ROOT
    ///   key, and it is the key every later signed document — including the
    ///   confirming Genesis of §13 step 5 — is verified under. A partial seal
    ///   that omitted it would let an identity naming a *different* ROOT be
    ///   sealed against this document, and the recovered group would bind an
    ///   authority no signature here was ever checked with. That is why
    ///   [`VerifiedGenesis`] retains the key `verify_genesis` used rather than
    ///   the caller re-supplying one.
    /// - **The local node.** §7.2 lets only a signed initial voter bootstrap,
    ///   and a recovery bootstrap is a bootstrap. Deriving that from the
    ///   document rather than trusting the corridor to have checked it keeps the
    ///   capability meaningful on its own: an unauthorised node cannot hold one.
    ///
    /// [`VerifiedGenesis`]: crate::coordination::model::authority::VerifiedGenesis
    pub(crate) fn seal(
        identity: &StorageIdentity,
        genesis: &crate::coordination::model::authority::VerifiedGenesis,
        path: std::path::PathBuf,
    ) -> Result<Self, String> {
        let expected_hash = *genesis
            .recovery_export_hash()
            .ok_or("this genesis binds no recovery export, so there is no import to authorise")?;
        for (what, agrees) in [
            (
                "genesis hash",
                identity.genesis_hash() == genesis.genesis_hash(),
            ),
            (
                "group id",
                identity.group_id().as_bytes() == genesis.group_id(),
            ),
            (
                "authority public key",
                identity.authority_public_key() == genesis.authority_public_key(),
            ),
            (
                "authority domain",
                identity.authority_domain() == genesis.authority_domain(),
            ),
            (
                "deployment profile",
                identity.deployment_profile_code() == genesis.deployment_profile_code(),
            ),
        ] {
            if !agrees {
                return Err(format!(
                    "the storage identity's {what} is not this genesis's; a recovery import \
                     may only be sealed from the identity its own genesis produced"
                ));
            }
        }
        if !genesis.is_initial_voter(identity.local_node_id().as_bytes()) {
            return Err(
                "this node is not one of the genesis's signed initial voters; §7.2 lets \
                 only an initial voter bootstrap, and a recovery bootstrap is a bootstrap"
                    .into(),
            );
        }
        Ok(Self {
            path,
            identity: identity.clone(),
            expected_hash,
            genesis_admins: genesis.initial_admins().to_vec(),
        })
    }

    /// The identity this capability was sealed against — the one the store
    /// binds to and the one [`transform`] writes into the founding state.
    pub(crate) fn identity(&self) -> &StorageIdentity {
        &self.identity
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

fn refused(path: &Path, detail: impl std::fmt::Display) -> StorageError {
    StorageError::RecoveryImportRefused {
        path: path.to_path_buf(),
        detail: detail.to_string(),
    }
}

/// Read, verify, transform, and durably commit a recovery export into a
/// freshly bound, pristine `state.redb`.
///
/// The identity is the sealed one: [`RecoveryImport`] carries the identity its
/// own genesis produced, so there is no second identity for this to be handed.
pub(crate) fn import_recovery_export(
    database: &Database,
    import: &RecoveryImport,
) -> StorageResult<()> {
    let path = import.path.as_path();
    let identity = &import.identity;

    // 0. The first §13 boundary, and the reason the import has boundaries of
    //    its own (S5-D). This runs *inside* `VoterStorage::open_with_recovery`,
    //    so by now `schema::open` has already bound the directory and passed
    //    the three shared transaction points doing it. A crash matrix that
    //    armed `transaction-commit` here would stop the child in the *bind* and
    //    file the result under "import", which reads as evidence and is about
    //    another operation entirely.
    super::crash::reached(super::crash::CrashPoint::ImportBound);

    // 1. The store must hold no durable replicated state and no data rows.
    //    The caller only runs an import on a `Fresh` open, but "rejected for
    //    non-empty storage" is this function's own §13 obligation, not its
    //    caller's mood.
    if !super::schema::state_is_import_pristine(database)? {
        return Err(refused(
            path,
            "the state database already holds replicated state; a recovery \
             import runs only against an empty, freshly bound store",
        ));
    }

    // 2. Bounded read through one pinned descriptor.
    let file = std::fs::File::open(path).map_err(|source| StorageError::Io {
        what: "opening recovery export",
        path: path.to_path_buf(),
        source,
    })?;
    let bytes = read_bounded(file, path, MAX_SNAPSHOT_BODY_BYTES)?;

    // 3. The exact hash the new signed genesis binds, before any parsing:
    //    the file's authenticity is the binding, not its parseability.
    let actual = blake3::hash(&bytes);
    if actual.as_bytes() != &import.expected_hash {
        return Err(refused(
            path,
            format!(
                "the export hashes to {} but the signed genesis binds {}; \
                 this is not the export the new group was created to import",
                actual.to_hex(),
                hex(&import.expected_hash),
            ),
        ));
    }

    // 4. The canonical §11.3 reader: framing, section order and presence, row
    //    caps, byte ceilings, widths, rights bits, row ordering, minimal
    //    varints, and inner Fory canonicality all live there (S5-A).
    let mut collected = Collect::default();
    read_body::<_, RecoveryBodyRowV1>(&mut &bytes[..], &mut collected)
        .map_err(|err| refused(path, format!("the export body is not canonical: {err:?}")))?;
    let header = collected
        .header
        .ok_or_else(|| refused(path, "the export body has no header"))?;

    // 5-6. Validate and transform — pure, deterministic, identical on every
    //      initial voter.
    let state = transform(
        &header,
        &collected.rows,
        identity,
        &import.genesis_admins,
        &import.expected_hash,
    )
    .map_err(|detail| refused(path, detail))?;

    // 7. The output must be a state this build would *load*, checked while
    //    refusing is still free.
    //
    //    Not a belt on top of the transform's braces. The two validators have
    //    genuinely different subjects: the transform enforces §13's rules about
    //    the input, and this enforces §11.3's rules about the result — over
    //    fields §13 only ever copies, notably the recorded source authority
    //    domain, which the framing reader bounds by width and the loader
    //    additionally requires to be ASCII. Without this, a body carrying a
    //    non-ASCII source domain would be transformed, committed, and only then
    //    refused by the production open, leaving behind exactly the dirty store
    //    the all-or-nothing claim says cannot exist.
    rows::validate_before_commit(&state).map_err(|err| {
        refused(
            path,
            format!("the transformed founding state is not one this build could load: {err}"),
        )
    })?;

    // Everything is verified and nothing is written. A crash here must leave
    // the replicated state exactly as pristine as it was at `ImportBound` —
    // which is what makes "verify, then write once" observable rather than
    // merely stated.
    super::crash::reached(super::crash::CrashPoint::ImportTransformed);

    // 8. One durable transaction. Either the whole founding state exists or
    //    none of it does; there is no partially imported store to observe.
    //    There is no boundary *inside* this, deliberately: a single redb
    //    transaction has no half, so a point between its begin and its commit
    //    would be a place the store cannot be observed in.
    let changes = every_key(&state);
    rows::commit(database, state, &changes)?;
    super::crash::reached(super::crash::CrashPoint::ImportCommitted);
    Ok(())
}

/// Size and read one **already-open** export, all through that descriptor.
///
/// Takes the `File` rather than the path, and that is the whole point. Stat the
/// name, then read the name, is two resolutions of one path: a rename between
/// them replaces the small file that passed the bound with an oversized one,
/// and the check becomes a fact about a file that is no longer there. `fstat`
/// on an open handle and a read through that same handle describe one inode
/// whatever happens to the name, and taking the handle as an argument is what
/// makes the substitution stageable in a test.
///
/// Two bounds, not one. The declared size refuses an oversized file before any
/// allocation; `take(max + 1)` then bounds the bytes actually read, because the
/// guarantee that matters is about this allocation rather than about an earlier
/// measurement. The `+ 1` is what distinguishes "exactly at the bound" from
/// "over it" without reading the whole of an oversized file.
fn read_bounded(file: std::fs::File, path: &Path, max: u64) -> StorageResult<Vec<u8>> {
    use std::io::Read;

    let declared = file
        .metadata()
        .map_err(|source| StorageError::Io {
            what: "sizing recovery export",
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if declared > max {
        return Err(refused(
            path,
            format!("the export file is {declared} bytes, over the {max} byte §11.4 body bound"),
        ));
    }

    let mut bytes = Vec::with_capacity(declared as usize);
    std::io::BufReader::new(file)
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| StorageError::Io {
            what: "reading recovery export",
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > max {
        return Err(refused(
            path,
            format!("the export file is over the {max} byte §11.4 body bound"),
        ));
    }
    Ok(bytes)
}

#[derive(Default)]
struct Collect {
    header: Option<RecoveryHeaderV1>,
    rows: Vec<RecoveryBodyRowV1>,
}

impl BodyVisitor<RecoveryBodyRowV1> for Collect {
    fn header(&mut self, header: &RecoveryHeaderV1) -> Result<(), BodyError> {
        self.header = Some(header.clone());
        Ok(())
    }
    fn row(&mut self, _section: u64, row: RecoveryBodyRowV1) -> Result<(), BodyError> {
        self.rows.push(row);
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn wide32(what: &str, bytes: &[u8]) -> Result<NodeId, String> {
    <NodeId>::try_from(bytes).map_err(|_| format!("{what} must be 32 bytes, got {}", bytes.len()))
}

/// The §13 bootstrap transformation, and the single import validator.
///
/// Returns the complete founding [`DurableState`] or the exact reason the
/// import is refused. Reads nothing but its arguments.
pub(crate) fn transform(
    header: &RecoveryHeaderV1,
    body_rows: &[RecoveryBodyRowV1],
    identity: &StorageIdentity,
    genesis_admins: &[NodeId],
    export_hash: &[u8; 32],
) -> Result<DurableState, String> {
    use crate::coordination::ids::ResourceKey;

    // ── Provenance relations ────────────────────────────────────────────────
    // The reader proved the widths; these are the relationships.
    let source_last_applied = header.source_last_applied.as_ref().ok_or(
        "the source group exported without a last-applied log id, \
                which no group that has applied its own genesis can do",
    )?;
    wide32(
        "the source last-applied leader id",
        &source_last_applied.leader_node_id,
    )?;
    if header.source_admin_revision == 0 {
        return Err(
            "the export declares source_admin_revision = 0, the pre-genesis \
             state; a group that can export has installed its genesis"
                .into(),
        );
    }

    // A log id inside the export is a position in the source's *own* history,
    // so "not ahead of" is not enough: two log ids at one index are the same
    // entry or the history is not a history. Comparing the index alone would
    // admit provenance that names a real index under a term and leader the
    // source never had there, which is a fabricated record that still passes
    // every width and ordering rule.
    //
    // The width check lives here rather than beside the other `wide32` calls
    // for one reason: these two log ids are optional, so there is no
    // unconditional site that sees them. The reader validates the header's
    // fixed-width fields, but `source_audit_floor`/`source_audit_tip` carry
    // their leader id inside an `Option<LogIdV1>` that no width rule reaches —
    // and this is the predicate every optional log id already passes through.
    let within = |what: &str,
                  id: &Option<crate::coordination::contract::LogIdV1>,
                  bound: &crate::coordination::contract::LogIdV1,
                  bound_name: &str|
     -> Result<(), String> {
        let Some(id) = id else { return Ok(()) };
        wide32(&format!("{what}'s leader id"), &id.leader_node_id)?;
        if id.index > bound.index {
            return Err(format!(
                "{what} is at index {} but {bound_name} is at index {}; provenance \
                 cannot run ahead of the state it describes",
                id.index, bound.index
            ));
        }
        if id.index == bound.index && id != bound {
            return Err(format!(
                "{what} and {bound_name} are both at index {} but are different log ids; \
                 one index in one history is one entry",
                id.index
            ));
        }
        Ok(())
    };
    within(
        "the source audit floor",
        &header.source_audit_floor,
        source_last_applied,
        "the source last-applied",
    )?;
    within(
        "the source audit tip",
        &header.source_audit_tip,
        source_last_applied,
        "the source last-applied",
    )?;
    if let Some(tip) = &header.source_audit_tip {
        within(
            "the source audit floor",
            &header.source_audit_floor,
            tip,
            "the source audit tip",
        )?;
    }

    // ── The source membership ───────────────────────────────────────────────
    //
    // Recorded provenance, never matched to the new group (frozen decision 1) —
    // but a record is still a record, and this one is written into the store and
    // served to operators. A group that could export has committed a membership,
    // so an absent or empty one says the export did not come from a v1 group
    // that ever ran.
    let membership = &header.source_membership;
    let membership_log_id = membership.log_id.as_ref().ok_or(
        "the source membership carries no log id, which no group that has committed \
         a membership can produce",
    )?;
    wide32(
        "the source membership leader id",
        &membership_log_id.leader_node_id,
    )?;
    if membership.membership.configs.is_empty() && membership.membership.nodes.is_empty() {
        return Err(
            "the source membership is empty; a group that can serve an export has \
             applied its own initial membership"
                .into(),
        );
    }
    rows::membership_is_canonical(&membership.membership)
        .map_err(|detail| format!("the source membership is {detail}"))?;
    within(
        "the source membership",
        &membership.log_id,
        source_last_applied,
        "the source last-applied",
    )?;

    // §13 founds a **new** group. A body whose source group id is the one being
    // created is either a crossed wiring or an operator re-importing a group
    // into itself, and both produce a store whose provenance says it recovered
    // from the group it is.
    if header.source_group_id.as_slice() == identity.group_id().as_bytes().as_slice() {
        return Err(
            "the export's source group id is the new group's own; §13 recovery founds \
             a new group from a dead one, and a group cannot be its own source"
                .into(),
        );
    }

    // ── Rows: ceilings, TTL, transformation ─────────────────────────────────
    let mut state = State::default();
    for row in body_rows {
        match row {
            RecoveryBodyRowV1::Resource(row) => {
                let key = ResourceKey::new(row.key.namespace.clone(), row.key.key.clone())
                    .map_err(|err| {
                        format!(
                            "an exported resource key is not a legal §4.2 \
                                            identity: {err:?}"
                        )
                    })?;
                // Checked arithmetic, and the result must itself be a legal
                // counter: the recovered row starts one epoch and one lease
                // revision past the source so no old fence can ever equal it.
                let epoch = advanced("epoch", &key, row.source_epoch)?;
                let lease_revision = advanced("lease revision", &key, row.lease_revision)?;
                for (what, value) in [
                    ("value revision", row.value_revision),
                    ("ACL revision", row.acl_revision),
                ] {
                    if value > limits::MAX_COUNTER {
                        return Err(format!(
                            "the exported {what} of {} is {value}, over the counter bound",
                            key_name(&key),
                        ));
                    }
                }
                // A recovered row starts free, and a free row's TTL is either
                // a legal lease TTL or the never-leased zero (§11.3's load
                // rule, applied here so the store this writes always loads).
                let ttl_legal = (limits::MIN_TTL_MS..=limits::MAX_TTL_MS).contains(&row.ttl_ms)
                    || row.ttl_ms == 0;
                if !ttl_legal {
                    return Err(format!(
                        "the exported TTL of {} is {} ms, outside 0 or {}..={}",
                        key_name(&key),
                        row.ttl_ms,
                        limits::MIN_TTL_MS,
                        limits::MAX_TTL_MS
                    ));
                }
                state.resources.insert(
                    key,
                    ResourceRow {
                        epoch,
                        held: false,
                        holder: None,
                        ttl_ms: row.ttl_ms,
                        lease_revision,
                        value_revision: row.value_revision,
                        value: row.value.clone(),
                        acl_revision: row.acl_revision,
                    },
                );
            }
            RecoveryBodyRowV1::Grant(row) => {
                let key = ResourceKey::new(row.key.namespace.clone(), row.key.key.clone())
                    .map_err(|err| {
                        format!(
                            "an exported grant key is not a legal §4.2 \
                                            identity: {err:?}"
                        )
                    })?;
                let subject = wide32("a grant subject", &row.subject_node_id)?;
                if row.rights == 0 {
                    return Err(format!(
                        "the export carries a zero-rights grant on {} for {}; zero \
                         rights is the removal and the row must not exist",
                        key_name(&key),
                        hex(&subject)
                    ));
                }
                state.acl.insert((key, subject), row.rights);
            }
            RecoveryBodyRowV1::Admin(row) => {
                let admin = wide32("an admin subject", &row.subject_node_id)?;
                state.admins.insert(admin);
            }
            RecoveryBodyRowV1::Unknown(_) => {
                // The reader refuses unknown variants before the visitor sees
                // them; reaching this arm is a bug in the reader, not input.
                return Err("an unknown row variant reached the import".into());
            }
        }
    }

    // ── Cross-row consistency ───────────────────────────────────────────────
    if state.admins.is_empty() {
        return Err(
            "the export carries no administrators, but source_admin_revision is \
             nonzero; the source store disagrees with itself"
                .into(),
        );
    }
    // The same rule the new group's ROOT gets, applied to the source's. ROOT
    // authorizes without a delegated row, so the source's administrator table
    // never held it and a body that carries it was not written by
    // `stream_recovery_body` over a loadable store. Checked against the
    // *exported* set, before the union, so the message names the export rather
    // than the new genesis.
    {
        let source_root = wide32(
            "the source authority public key",
            &header.source_authority_public_key,
        )?;
        if state.admins.contains(&source_root) {
            return Err(
                "the export carries its own source ROOT key as a delegated administrator \
                 row; ROOT authorizes without a row, so no v1 store held that row"
                    .into(),
            );
        }
    }
    // One ordered pass, not a scan per resource. `acl` is a `BTreeMap` keyed
    // `(ResourceKey, NodeId)`, so every grant on one resource is contiguous —
    // a nested filter here would be 100,000 × 100,000 at the §12.1 caps, which
    // is a maximal legal export the import would appear to hang on.
    let mut run: Option<(&crate::coordination::ids::ResourceKey, u64)> = None;
    for (key, _subject) in state.acl.keys() {
        if !state.resources.contains_key(key) {
            return Err(format!(
                "the export grants on {} but exports no such resource; §8.1 moves \
                 the two in one step, so this body was not produced by a v1 group",
                key_name(key)
            ));
        }
        run = match run {
            Some((current, n)) if current == key => Some((current, n + 1)),
            _ => Some((key, 1)),
        };
        let (current, n) = run.expect("just set");
        if n > limits::MAX_GRANTS_PER_RESOURCE {
            return Err(format!(
                "the export carries more than {} grants on {}, over the \
                 per-resource cap",
                limits::MAX_GRANTS_PER_RESOURCE,
                key_name(current)
            ));
        }
    }

    // ── Administrators: union, then the caps the union can newly break ──────
    for admin in genesis_admins {
        state.admins.insert(*admin);
    }
    if state.admins.len() as u64 > limits::MAX_ADMINS_PER_GROUP {
        return Err(format!(
            "the union of exported and new-genesis administrators holds {} \
             entries, over the {} cap",
            state.admins.len(),
            limits::MAX_ADMINS_PER_GROUP
        ));
    }
    if state.admins.contains(identity.authority_public_key()) {
        return Err(
            "the administrator union contains the new group's ROOT key; ROOT \
             authorizes without a delegated row and the row is refused at load"
                .into(),
        );
    }
    state.admin_revision = header
        .source_admin_revision
        .checked_add(1)
        .filter(|next| *next <= limits::MAX_COUNTER)
        .ok_or_else(|| {
            format!(
                "advancing the admin revision past {} overflows the counter bound",
                header.source_admin_revision
            )
        })?;

    // ── The new group's binding and the recomputed budgets ──────────────────
    state.authority.public_key = *identity.authority_public_key();
    state.authority.domain = identity.authority_domain().to_string();
    state.authority.group_id = *identity.group_id().as_bytes();
    state.authority.genesis_hash = *identity.genesis_hash();
    state.deployment_profile_code = identity.deployment_profile_code();
    state.growth_bytes = state.recompute_growth();
    state.reserve_bytes = state.recompute_reserve();
    if state.growth_bytes > limits::GROWTH_BUDGET {
        return Err(format!(
            "the imported state charges {} growth bytes, over the {} budget",
            state.growth_bytes,
            limits::GROWTH_BUDGET
        ));
    }

    Ok(DurableState {
        model: state,
        membership: Default::default(),
        recovery_export_hash: Some(export_hash.to_vec()),
        recovered_from: Some(RecoveredFromV1 {
            source_group_id: header.source_group_id.clone(),
            source_genesis_hash: header.source_genesis_hash.clone(),
            source_authority_public_key: header.source_authority_public_key.clone(),
            source_authority_domain: header.source_authority_domain.clone(),
            export_hash: export_hash.to_vec(),
            source_last_applied: header.source_last_applied.clone(),
        }),
    })
}

fn key_name(key: &crate::coordination::ids::ResourceKey) -> String {
    format!("{}/{}", key.namespace(), hex(key.key()))
}

fn advanced(
    what: &str,
    key: &crate::coordination::ids::ResourceKey,
    source: u64,
) -> Result<u64, String> {
    source
        .checked_add(1)
        .filter(|next| *next <= limits::MAX_COUNTER)
        .ok_or_else(|| {
            format!(
                "advancing the {what} of {} past {source} overflows the counter bound",
                key_name(key)
            )
        })
}

/// Name every row the transformed state holds, so [`rows::commit`] writes all
/// of it. Sessions, intents, routes and audit are structurally empty.
fn every_key(state: &DurableState) -> ChangeSet {
    ChangeSet {
        resources: state.model.resources.keys().cloned().collect(),
        grants: state.model.acl.keys().cloned().collect(),
        admins: state.model.admins.iter().copied().collect(),
        sessions: Default::default(),
        intents: Default::default(),
        routes: Default::default(),
        audit: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::identity::StorageIdentity;
    use super::super::schema;
    use super::super::scratch::ScratchDir;
    use super::*;
    use crate::coordination::contract::{LogIdV1, ResourceKeyV1};
    use crate::coordination::ids::{CoordinationGroupId, ResourceKey};
    use crate::coordination::rowstream::{
        write_varint, AdminRowV1, BodyRow, BodyWriter, GrantRowV1,
    };
    use redb::Database;

    fn nid(b: u8) -> NodeId {
        [b; 32]
    }

    fn root_key() -> NodeId {
        ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
            .verifying_key()
            .to_bytes()
    }

    fn new_identity() -> StorageIdentity {
        StorageIdentity::new(
            CoordinationGroupId::from_array([0xEE; 32]),
            [0xE1; 32],
            root_key(),
            "new.aster0.net",
            crate::coordination::ids::CoordinationNodeId::from_array([0xE5; 32]),
            1,
        )
    }

    fn log_id(index: u64) -> LogIdV1 {
        LogIdV1 {
            term: 3,
            leader_node_id: vec![0xDD; 32],
            index,
        }
    }

    /// The canonical source export, shared with the crash matrix.
    ///
    /// One definition in `fixture`, because the §13 import has two independent
    /// consumers of these exact bytes — these unit tests, and the S5-D crash
    /// matrix that kills a process part-way through importing them — and two
    /// spellings of "the canonical export" would drift.
    fn baseline() -> (RecoveryHeaderV1, Vec<RecoveryBodyRowV1>) {
        super::super::fixture::recovery_export_parts()
    }

    fn source_membership(index: u64) -> crate::coordination::raft::StoredMembershipV1 {
        let mut membership = baseline().0.source_membership;
        membership.log_id = Some(log_id(index));
        membership
    }

    fn encode_export(header: &RecoveryHeaderV1, rows: &[RecoveryBodyRowV1]) -> Vec<u8> {
        super::super::fixture::encode_recovery_export(header, rows)
    }

    /// A freshly bound, pristine `state.redb` — what `schema::open` leaves a
    /// `Fresh` directory holding when the import runs.
    fn bound_db(identity: &StorageIdentity) -> (ScratchDir, Database) {
        let dir = ScratchDir::new("import");
        let database = Database::create(dir.join("state.redb")).expect("create state.redb");
        let write = schema::durable_write(&database, super::super::error::Db::State).unwrap();
        let meta = crate::coordination::raft::CoordinationMetaV1 {
            schema_version: super::super::identity::LOGICAL_SCHEMA_VERSION,
            group_id: identity.group_id().as_bytes().to_vec(),
            genesis_hash: identity.genesis_hash().to_vec(),
            authority_public_key: identity.authority_public_key().to_vec(),
            authority_domain: identity.authority_domain().to_string(),
            deployment_profile_code: identity.deployment_profile_code(),
            ..Default::default()
        };
        schema::write_state_binding(&write, &meta).unwrap();
        schema::durable_commit(write, super::super::error::Db::State).unwrap();
        (dir, database)
    }

    /// A capability built directly, which only this module can do. Production
    /// reaches [`RecoveryImport::seal`] instead, and
    /// `only_a_genesis_can_seal_its_own_import` covers that door.
    fn capability(
        path: std::path::PathBuf,
        identity: &StorageIdentity,
        expected_hash: [u8; 32],
        genesis_admins: &[NodeId],
    ) -> RecoveryImport {
        RecoveryImport {
            path,
            identity: identity.clone(),
            expected_hash,
            genesis_admins: genesis_admins.to_vec(),
        }
    }

    fn run_import(
        bytes: &[u8],
        expected_hash: [u8; 32],
        genesis_admins: &[NodeId],
    ) -> (ScratchDir, Database, StorageResult<()>) {
        let identity = new_identity();
        let (dir, database) = bound_db(&identity);
        let path = dir.join("export.recovery");
        std::fs::write(&path, bytes).unwrap();
        let outcome = import_recovery_export(
            &database,
            &capability(path, &identity, expected_hash, genesis_admins),
        );
        (dir, database, outcome)
    }

    fn key(k: &[u8]) -> ResourceKey {
        ResourceKey::new("portal.roaming", k.to_vec()).unwrap()
    }

    /// The §13 transformation, observed through the production loader: values,
    /// value revisions and ACLs preserved; holders structurally cleared; epoch
    /// and lease revision advanced by exactly one; administrators unioned and
    /// the admin revision advanced; sessions/intents/routes/audit empty; both
    /// budgets recomputed; provenance recorded with the bound hash.
    #[test]
    fn an_import_transforms_and_round_trips_through_the_loader() {
        let (header, rows) = baseline();
        let bytes = encode_export(&header, &rows);
        let hash = *blake3::hash(&bytes).as_bytes();
        let genesis_admin = nid(0x41);
        let (_dir, database, outcome) = run_import(&bytes, hash, &[genesis_admin]);
        outcome.expect("the import succeeds");

        let loaded = rows::load(&database).expect("the imported store loads");
        let tree1 = loaded.model.resources.get(&key(b"tree-1")).unwrap();
        assert_eq!(tree1.epoch, 10, "epoch advances by one");
        assert_eq!(tree1.lease_revision, 11, "lease revision advances by one");
        assert!(!tree1.held);
        assert_eq!(tree1.holder, None);
        assert_eq!(tree1.value, vec![0x01, 0x02, 0x03]);
        assert_eq!(tree1.value_revision, 7);
        assert_eq!(tree1.acl_revision, 2);
        assert_eq!(tree1.ttl_ms, 30_000);
        assert_eq!(loaded.model.resources.len(), 2);
        assert_eq!(loaded.model.acl.len(), 2);
        assert_eq!(
            loaded.model.acl.get(&(key(b"tree-1"), nid(0x21))),
            Some(&0x3f)
        );
        assert_eq!(
            loaded.model.admins,
            [nid(0x31), genesis_admin].into_iter().collect(),
            "the exported and genesis administrators are unioned"
        );
        assert_eq!(loaded.model.admin_revision, 6, "source 5 advances to 6");
        assert!(loaded.model.sessions.is_empty());
        assert!(loaded.model.intents.is_empty());
        assert!(loaded.model.routes.is_empty());
        assert!(loaded.model.audit.is_empty());
        assert_eq!(loaded.model.last_applied, None);
        assert!(loaded.model.counters_match_recomputation());
        assert_eq!(loaded.recovery_export_hash, Some(hash.to_vec()));
        let provenance = loaded.recovered_from.expect("provenance is recorded");
        assert_eq!(provenance.source_group_id, vec![0xAA; 32]);
        assert_eq!(provenance.source_genesis_hash, vec![0xBB; 32]);
        assert_eq!(provenance.source_authority_public_key, vec![0xCC; 32]);
        assert_eq!(provenance.source_authority_domain, "old.aster0.net");
        assert_eq!(provenance.export_hash, hash.to_vec());
        assert_eq!(provenance.source_last_applied, Some(log_id(40)));
        // The new group's own binding, not the source's.
        assert_eq!(loaded.model.authority.group_id, [0xEE; 32]);
        assert_eq!(loaded.model.authority.genesis_hash, [0xE1; 32]);
    }

    /// The transformation is a pure function: two voters that independently
    /// import the same bytes under the same new group hold identical logical
    /// state — the §13 cross-voter agreement.
    #[test]
    fn two_independent_imports_agree_exactly() {
        let (header, rows) = baseline();
        let bytes = encode_export(&header, &rows);
        let hash = *blake3::hash(&bytes).as_bytes();
        let (_da, database_a, a) = run_import(&bytes, hash, &[nid(0x41)]);
        let (_db, database_b, b) = run_import(&bytes, hash, &[nid(0x41)]);
        a.unwrap();
        b.unwrap();
        assert_eq!(
            rows::load(&database_a).unwrap(),
            rows::load(&database_b).unwrap()
        );
    }

    /// Every refusal is atomic: the input is refused with an actionable
    /// detail, and the store afterwards is exactly the bound-pristine store
    /// it was before — nothing partial to observe, no cleanup to run.
    #[test]
    fn every_refusal_is_atomic_and_names_its_rule() {
        /// One refusal case: label, export bytes, an optional hash override
        /// (otherwise the body's own hash, so the *genesis binds these exact
        /// bytes* and only the validator can refuse them), the new genesis's
        /// administrators, and the phrase the refusal must name.
        type Case = (
            &'static str,
            Vec<u8>,
            Option<[u8; 32]>,
            Vec<NodeId>,
            &'static str,
        );

        let cases: Vec<Case> = {
            let (header, rows) = baseline();
            let bytes = encode_export(&header, &rows);
            let with_header = |f: &dyn Fn(&mut RecoveryHeaderV1)| {
                let (mut h, r) = baseline();
                f(&mut h);
                encode_export(&h, &r)
            };
            let with_rows = |f: &dyn Fn(&mut Vec<RecoveryBodyRowV1>, &mut RecoveryHeaderV1)| {
                let (mut h, mut r) = baseline();
                f(&mut r, &mut h);
                encode_export(&h, &r)
            };
            let noncanonical = {
                // A trailing byte inside the final row's Fory frame, length
                // adjusted — and the genesis binds the hash of exactly these
                // mutated bytes, so only the reader can refuse it.
                let (header, rows) = baseline();
                let intact = encode_export(&header, &rows);
                let last =
                    RecoveryBodyRowV1::encode(&RecoveryBodyRowV1::fory(), rows.last().unwrap())
                        .unwrap();
                let mut mutated = intact[..intact.len() - last.len()].to_vec();
                // Replace the last row's length varint (it precedes the row
                // bytes) with len + 1 and append the padded row.
                let mut tail = Vec::new();
                write_varint(&mut tail, last.len() as u64);
                assert!(mutated.ends_with(&tail), "the varint precedes the row");
                mutated.truncate(mutated.len() - tail.len());
                let mut lengthened = Vec::new();
                write_varint(&mut lengthened, last.len() as u64 + 1);
                mutated.extend_from_slice(&lengthened);
                mutated.extend_from_slice(&last);
                mutated.push(0x00);
                mutated
            };
            let truncated = bytes[..bytes.len() - 3].to_vec();
            let snapshot_as_recovery = {
                use crate::coordination::rowstream::{SnapshotBodyRowV1, SnapshotHeaderV1};
                let header = SnapshotHeaderV1 {
                    format_version: 1,
                    schema_version: 1,
                    meta: Default::default(),
                    row_counts: Default::default(),
                };
                let mut w = BodyWriter::<_, SnapshotBodyRowV1>::new(Vec::new(), &header).unwrap();
                for section in 1..=7u64 {
                    w.begin_section(section).unwrap();
                }
                w.finish().unwrap()
            };
            vec![
                (
                    "wrong hash",
                    bytes.clone(),
                    Some([0x99; 32]),
                    vec![nid(0x41)],
                    "the signed genesis binds",
                ),
                (
                    "noncanonical inner bytes under a correctly bound hash",
                    noncanonical,
                    None,
                    vec![nid(0x41)],
                    "not canonical",
                ),
                (
                    "a truncated export",
                    truncated,
                    None,
                    vec![nid(0x41)],
                    "not canonical",
                ),
                (
                    "a snapshot body presented as a recovery export",
                    snapshot_as_recovery,
                    None,
                    vec![nid(0x41)],
                    "not canonical",
                ),
                (
                    "an epoch at the counter bound",
                    with_rows(&|rows, _| {
                        let RecoveryBodyRowV1::Resource(row) = &mut rows[0] else {
                            panic!()
                        };
                        row.source_epoch = limits::MAX_COUNTER;
                    }),
                    None,
                    vec![nid(0x41)],
                    "overflows the counter bound",
                ),
                (
                    "a lease revision at the counter bound",
                    with_rows(&|rows, _| {
                        let RecoveryBodyRowV1::Resource(row) = &mut rows[0] else {
                            panic!()
                        };
                        row.lease_revision = limits::MAX_COUNTER;
                    }),
                    None,
                    vec![nid(0x41)],
                    "overflows the counter bound",
                ),
                (
                    "a value revision over the counter bound",
                    with_rows(&|rows, _| {
                        let RecoveryBodyRowV1::Resource(row) = &mut rows[0] else {
                            panic!()
                        };
                        row.value_revision = limits::MAX_COUNTER + 1;
                    }),
                    None,
                    vec![nid(0x41)],
                    "over the counter bound",
                ),
                (
                    "an admin revision at the counter bound",
                    with_header(&|h| h.source_admin_revision = limits::MAX_COUNTER),
                    None,
                    vec![nid(0x41)],
                    "overflows the counter bound",
                ),
                (
                    "a pre-genesis source",
                    with_header(&|h| h.source_admin_revision = 0),
                    None,
                    vec![nid(0x41)],
                    "source_admin_revision = 0",
                ),
                (
                    "no source last-applied",
                    with_header(&|h| h.source_last_applied = None),
                    None,
                    vec![nid(0x41)],
                    "without a last-applied log id",
                ),
                (
                    "an audit tip ahead of last-applied",
                    with_header(&|h| h.source_audit_tip = Some(log_id(41))),
                    None,
                    vec![nid(0x41)],
                    "cannot run ahead",
                ),
                (
                    "an audit floor above its tip",
                    with_header(&|h| {
                        h.source_audit_floor = Some(log_id(39));
                        h.source_audit_tip = Some(log_id(20));
                    }),
                    None,
                    vec![nid(0x41)],
                    "cannot run ahead",
                ),
                (
                    // Same index, different term: a real index under a term the
                    // source never had there. Every width and ordering rule
                    // still passes; only exactness catches it.
                    "an audit tip at the last-applied index under another term",
                    with_header(&|h| {
                        let mut tip = log_id(40);
                        tip.term = 9;
                        h.source_audit_tip = Some(tip);
                    }),
                    None,
                    vec![nid(0x41)],
                    "one index in one history is one entry",
                ),
                (
                    "an audit floor at the tip index under another leader",
                    with_header(&|h| {
                        let mut floor = log_id(39);
                        floor.leader_node_id = vec![0xEE; 32];
                        h.source_audit_floor = Some(floor);
                    }),
                    None,
                    vec![nid(0x41)],
                    "one index in one history is one entry",
                ),
                (
                    // The reader validates the header's fixed-width fields;
                    // these two carry a leader id inside an `Option<LogIdV1>`
                    // that no width rule reaches, so the import owns it.
                    "an audit floor whose leader id is not 32 bytes",
                    with_header(&|h| {
                        let mut floor = log_id(10);
                        floor.leader_node_id = vec![0xDD; 31];
                        h.source_audit_floor = Some(floor);
                    }),
                    None,
                    vec![nid(0x41)],
                    "leader id must be 32 bytes",
                ),
                (
                    "an audit tip whose leader id is not 32 bytes",
                    with_header(&|h| {
                        let mut tip = log_id(39);
                        tip.leader_node_id = Vec::new();
                        h.source_audit_tip = Some(tip);
                    }),
                    None,
                    vec![nid(0x41)],
                    "leader id must be 32 bytes",
                ),
                (
                    "a source membership with no log id",
                    with_header(&|h| h.source_membership.log_id = None),
                    None,
                    vec![nid(0x41)],
                    "carries no log id",
                ),
                (
                    // The `StoredMembershipV1::default()` this fixture itself
                    // used to carry, which no group that ever ran can produce.
                    "a wholly default source membership",
                    with_header(&|h| h.source_membership = Default::default()),
                    None,
                    vec![nid(0x41)],
                    "carries no log id",
                ),
                (
                    "a source membership whose config is empty",
                    with_header(&|h| {
                        h.source_membership = crate::coordination::raft::StoredMembershipV1 {
                            log_id: Some(log_id(12)),
                            membership: Default::default(),
                        };
                    }),
                    None,
                    vec![nid(0x41)],
                    "the source membership is empty",
                ),
                (
                    // Unsorted ids: the conversion would silently repair it,
                    // and a repaired record is not the record the source held.
                    "a non-canonical source membership",
                    with_header(&|h| {
                        let mut membership = source_membership(12);
                        membership.membership.configs[0].ids.reverse();
                        h.source_membership = membership;
                    }),
                    None,
                    vec![nid(0x41)],
                    "the canonical encoding of itself",
                ),
                (
                    "a source membership ahead of the source last-applied",
                    with_header(&|h| h.source_membership = source_membership(41)),
                    None,
                    vec![nid(0x41)],
                    "cannot run ahead",
                ),
                (
                    "a source membership at the last-applied index under another term",
                    with_header(&|h| {
                        let mut membership = source_membership(40);
                        membership.log_id.as_mut().unwrap().term = 9;
                        h.source_membership = membership;
                    }),
                    None,
                    vec![nid(0x41)],
                    "one index in one history is one entry",
                ),
                (
                    "an export whose source group is the group being founded",
                    with_header(&|h| h.source_group_id = vec![0xEE; 32]),
                    None,
                    vec![nid(0x41)],
                    "a group cannot be its own source",
                ),
                (
                    "the source ROOT key carried as a delegated administrator row",
                    with_rows(&|rows, header| {
                        rows.push(RecoveryBodyRowV1::Admin(AdminRowV1 {
                            subject_node_id: header.source_authority_public_key.clone(),
                        }));
                        header.row_counts.admins = 2;
                    }),
                    None,
                    vec![nid(0x41)],
                    "its own source ROOT key",
                ),
                (
                    // The framing reader bounds the source domain by width
                    // only; §7.1 additionally requires ASCII, and the loader
                    // enforces that. Without the pre-commit whole-state check
                    // this body transforms, commits, and is refused by the next
                    // open — leaving the dirty store the atomicity claim denies.
                    "a non-ASCII source authority domain",
                    with_header(&|h| h.source_authority_domain = "öld.aster0.net".into()),
                    None,
                    vec![nid(0x41)],
                    "not one this build could load",
                ),
                (
                    "a TTL below the lease floor",
                    with_rows(&|rows, _| {
                        let RecoveryBodyRowV1::Resource(row) = &mut rows[0] else {
                            panic!()
                        };
                        row.ttl_ms = 1;
                    }),
                    None,
                    vec![nid(0x41)],
                    "outside 0 or",
                ),
                (
                    "a grant on a resource the export does not carry",
                    with_rows(&|rows, header| {
                        rows.push(RecoveryBodyRowV1::Grant(GrantRowV1 {
                            key: ResourceKeyV1 {
                                namespace: "portal.roaming".into(),
                                key: b"tree-9".to_vec(),
                            },
                            subject_node_id: nid(0x21).to_vec(),
                            rights: 1,
                        }));
                        header.row_counts.grants = 3;
                    }),
                    None,
                    vec![nid(0x41)],
                    "exports no such resource",
                ),
                (
                    "a zero-rights grant",
                    with_rows(&|rows, header| {
                        rows.push(RecoveryBodyRowV1::Grant(GrantRowV1 {
                            key: ResourceKeyV1 {
                                namespace: "portal.roaming".into(),
                                key: b"tree-2".to_vec(),
                            },
                            subject_node_id: nid(0x23).to_vec(),
                            rights: 0,
                        }));
                        header.row_counts.grants = 3;
                    }),
                    None,
                    vec![nid(0x41)],
                    "zero rights is the removal",
                ),
                (
                    "an administrator union over the cap",
                    {
                        let (mut h, mut r) = baseline();
                        r.retain(|row| !matches!(row, RecoveryBodyRowV1::Admin(_)));
                        for b in 0..limits::MAX_ADMINS_PER_GROUP as u8 {
                            r.push(RecoveryBodyRowV1::Admin(AdminRowV1 {
                                subject_node_id: nid(0x80 + b).to_vec(),
                            }));
                        }
                        h.row_counts.admins = limits::MAX_ADMINS_PER_GROUP;
                        encode_export(&h, &r)
                    },
                    None,
                    vec![nid(0x41)],
                    "over the 64 cap",
                ),
                (
                    "the new ROOT key inside the administrator union",
                    bytes.clone(),
                    None,
                    vec![root_key()],
                    "contains the new group's ROOT key",
                ),
                (
                    // 257 grants on one resource: the group-wide ACL cap has
                    // ample room, so only the per-resource cap can refuse it.
                    "a 257th grant on one resource",
                    {
                        let (mut h, mut r) = baseline();
                        r.retain(|row| !matches!(row, RecoveryBodyRowV1::Grant(_)));
                        for i in 0..=limits::MAX_GRANTS_PER_RESOURCE {
                            let mut subject = [0u8; 32];
                            subject[..8].copy_from_slice(&i.to_be_bytes());
                            r.push(RecoveryBodyRowV1::Grant(GrantRowV1 {
                                key: ResourceKeyV1 {
                                    namespace: "portal.roaming".into(),
                                    key: b"tree-1".to_vec(),
                                },
                                subject_node_id: subject.to_vec(),
                                rights: 1,
                            }));
                        }
                        h.row_counts.grants = limits::MAX_GRANTS_PER_RESOURCE + 1;
                        encode_export(&h, &r)
                    },
                    None,
                    vec![nid(0x41)],
                    "over the per-resource cap",
                ),
            ]
        };

        for (label, bytes, hash_override, genesis_admins, needle) in cases {
            let hash = hash_override.unwrap_or_else(|| *blake3::hash(&bytes).as_bytes());
            let (_dir, database, outcome) = run_import(&bytes, hash, &genesis_admins);
            let err = outcome.err().unwrap_or_else(|| {
                panic!("{label}: the import must refuse");
            });
            let message = err.to_string();
            assert!(
                message.contains(needle),
                "{label}: expected {needle:?} in {message:?}"
            );
            assert!(
                matches!(err, StorageError::RecoveryImportRefused { .. })
                    || matches!(err, StorageError::Io { .. }),
                "{label}: refusals are the import's own variant, got {err:?}"
            );
            // Atomic: the store is still exactly bound-pristine.
            assert!(
                schema::state_is_import_pristine(&database).unwrap(),
                "{label}: a refused import must write nothing"
            );
        }
    }

    /// §13 step 5: after the pre-Raft transform, the **same** signed genesis
    /// commits and *confirms* — apply re-verifies its signature, group,
    /// bound hash and profile against the replicated binding the import
    /// wrote, then takes the already-installed branch — without overwriting
    /// the imported administrator union. A different signed genesis at the
    /// same group is a fatal state-machine disagreement. And the
    /// genesis-applied fact a recovered store reports flips only when the
    /// confirming command's audit row lands, which is what makes the runtime
    /// propose it at all.
    #[test]
    fn the_confirming_genesis_verifies_and_preserves_the_import() {
        use crate::coordination::model::apply::{apply_command_recording, Command, GenesisCommand};
        use crate::coordination::model::authority::{genesis_hash, testing as auth};
        use crate::coordination::model::shape;
        use crate::coordination::model::state::LogId;
        use crate::coordination::raft::SignedCoordinationDocumentV1;

        let (header, body_rows) = baseline();
        let bytes = encode_export(&header, &body_rows);
        let hash = *blake3::hash(&bytes).as_bytes();

        // The new group's signed genesis binds the export hash; its admins
        // are its voters, exactly as `genesis_for` writes them.
        let voters: Vec<NodeId> = vec![[1u8; 32], [2u8; 32], [3u8; 32]];
        let mut genesis = auth::genesis_for(&voters, shape::PROFILE_PROD);
        genesis.recovery_export_hash = Some(hash.to_vec());
        let signed = auth::sign_genesis(&genesis);
        let envelope: SignedCoordinationDocumentV1 =
            crate::coordination::contract::payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&signed)
                .expect("the signed genesis decodes");
        let bound_hash = genesis_hash(&envelope.document_bytes);
        let authority = auth::authority();
        let identity = StorageIdentity::new(
            CoordinationGroupId::from_array(authority.group_id),
            bound_hash,
            authority.public_key,
            authority.domain.clone(),
            crate::coordination::ids::CoordinationNodeId::from_array([1u8; 32]),
            shape::PROFILE_PROD,
        );

        let durable = transform(&header, &body_rows, &identity, &voters, &hash)
            .expect("the transform succeeds");
        assert!(durable.model.admin_revision > 0);
        assert!(
            !super::super::machine::genesis_applied_fact(&durable),
            "an imported store has not applied its genesis yet, whatever its \
             admin revision says"
        );

        let mut state = durable.model.clone();
        let union: std::collections::BTreeSet<NodeId> = state.admins.clone();
        assert!(
            voters.iter().all(|voter| union.contains(voter)) && union.contains(&nid(0x31)),
            "the union carries both sides before the confirm"
        );
        let revision_before = state.admin_revision;

        let mut changes = crate::coordination::model::state::ChangeSet::default();
        apply_command_recording(
            &mut state,
            LogId::new(1, [1u8; 32], 2),
            &Command::Genesis(GenesisCommand {
                signed_genesis: signed.clone(),
            }),
            &mut changes,
        )
        .expect("the confirming genesis applies");
        assert_eq!(state.admins, union, "confirmed, not overwritten");
        assert_eq!(state.admin_revision, revision_before);
        assert!(
            state
                .audit
                .values()
                .any(|row| row.operation_code == crate::coordination::contract::OPERATION_GENESIS),
            "the confirm leaves its audit row"
        );

        // The fact flips with that audit row — this is what gates both the
        // runtime's proposal loop and readiness.
        let confirmed = DurableState {
            model: state.clone(),
            membership: Default::default(),
            recovery_export_hash: durable.recovery_export_hash.clone(),
            recovered_from: durable.recovered_from.clone(),
        };
        assert!(super::super::machine::genesis_applied_fact(&confirmed));

        // A different signed genesis at the same group: fatal, before
        // `last_applied` moves.
        let mut other = genesis.clone();
        other.genesis_nonce = vec![0x66; 32];
        let err = apply_command_recording(
            &mut state,
            LogId::new(1, [1u8; 32], 3),
            &Command::Genesis(GenesisCommand {
                signed_genesis: auth::sign_genesis(&other),
            }),
            &mut crate::coordination::model::state::ChangeSet::default(),
        )
        .expect_err("a different genesis cannot be reconciled");
        assert!(
            format!("{err:?}").contains("GenesisRefused"),
            "got: {err:?}"
        );
    }

    /// A directory that is already **bound but pristine** — a voter that
    /// bound its store and died before `Raft::initialize` — refuses a
    /// recovery import at the open, naming the outcome.
    ///
    /// The two guards are not redundant, and this is the case that separates
    /// them: the state side really is pristine here, so
    /// `state_is_import_pristine` passes and only the `OpenOutcome::Fresh`
    /// check stands between a bound directory and an import writing into it.
    /// That ordering matters because `check_directory_state` runs *after* the
    /// open in the boot corridor — by the time the mode check refuses the
    /// start, an unguarded import would already have written.
    #[test]
    fn a_bound_but_pristine_directory_refuses_an_import_at_the_open() {
        use super::super::identity::OpenPolicy;
        use super::super::schema::StorageLayout;
        use super::super::VoterStorage;

        let (header, body_rows) = baseline();
        let bytes = encode_export(&header, &body_rows);
        let hash = *blake3::hash(&bytes).as_bytes();

        let dir = ScratchDir::new("import-open");
        let export = dir.join("export.recovery");
        std::fs::write(&export, &bytes).unwrap();
        let identity = new_identity();
        let layout = StorageLayout::new(dir.to_path_buf());

        // Bind the directory the way a first start does, then let it go —
        // both sides bound, nothing initialized.
        drop(
            VoterStorage::open(layout.clone(), identity.clone(), OpenPolicy::production())
                .expect("the first open binds the directory"),
        );

        let import = capability(export, &identity, hash, &[nid(0x41)]);
        // `match` rather than `expect_err`: `VoterStorage` is deliberately not
        // `Debug`, so the `Result`'s success arm cannot be formatted.
        let err = match VoterStorage::open_with_recovery(layout, OpenPolicy::production(), &import)
        {
            Ok(_) => panic!("a bound directory must not receive an import"),
            Err(err) => err,
        };
        let message = err.to_string();
        assert!(
            message.contains("is not fresh"),
            "the refusal must name the directory state, not the pristine \
             check that also passes here: {message}"
        );
        // And nothing was written: the state side is still importable-clean,
        // which is exactly why the outer guard had to be the one to refuse.
        let database =
            redb::Database::create(dir.join("state.redb")).expect("reopen the state database");
        assert!(schema::state_is_import_pristine(&database).unwrap());
    }

    /// Non-empty storage refuses before the file is even read, and an
    /// oversized file refuses before it is loaded into memory.
    #[test]
    fn a_dirty_store_and_an_oversized_file_are_refused() {
        let (header, rows) = baseline();
        let bytes = encode_export(&header, &rows);
        let hash = *blake3::hash(&bytes).as_bytes();

        // Dirty: a durable meta field is set, as an interrupted second import
        // or a re-bootstrap would find.
        let identity = new_identity();
        let (dir, database) = bound_db(&identity);
        {
            let write = schema::durable_write(&database, super::super::error::Db::State).unwrap();
            let mut meta = crate::coordination::raft::CoordinationMetaV1 {
                schema_version: super::super::identity::LOGICAL_SCHEMA_VERSION,
                group_id: identity.group_id().as_bytes().to_vec(),
                genesis_hash: identity.genesis_hash().to_vec(),
                authority_public_key: identity.authority_public_key().to_vec(),
                authority_domain: identity.authority_domain().to_string(),
                deployment_profile_code: identity.deployment_profile_code(),
                ..Default::default()
            };
            meta.admin_revision = 1;
            schema::write_state_binding(&write, &meta).unwrap();
            schema::durable_commit(write, super::super::error::Db::State).unwrap();
        }
        let path = dir.join("export.recovery");
        std::fs::write(&path, &bytes).unwrap();
        let err = import_recovery_export(
            &database,
            &capability(path.clone(), &identity, hash, &[nid(0x41)]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("already holds replicated state"));

        // Oversized: a sparse file over the §11.4 bound is refused from its
        // metadata, before a read.
        let identity = new_identity();
        let (dir, database) = bound_db(&identity);
        let path = dir.join("export.recovery");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_SNAPSHOT_BODY_BYTES + 1).unwrap();
        drop(file);
        let err =
            import_recovery_export(&database, &capability(path, &identity, hash, &[nid(0x41)]))
                .unwrap_err();
        assert!(err.to_string().contains("body bound"));
        assert!(schema::state_is_import_pristine(&database).unwrap());
    }

    /// The §11.4 bound is a fact about the **descriptor** the import reads, not
    /// about the name it was given.
    ///
    /// The race is staged exactly where it would happen: the handle is opened,
    /// an oversized file is then renamed over the very path it came from, and
    /// only then is the file sized and read. Both must still describe the small
    /// file. The final assertion is what makes this discriminating — through the
    /// name, the same path now yields the oversized content, which is what a
    /// `metadata(path)` + `read(path)` pair would have sized and allocated.
    #[test]
    fn the_size_bound_follows_the_handle_and_not_the_name() {
        let dir = ScratchDir::new("import-rename");
        let path = dir.join("export.recovery");
        let small = vec![0xA5u8; 16];
        std::fs::write(&path, &small).unwrap();

        // The import's own first step: one open, before anything is measured.
        let file = std::fs::File::open(&path).unwrap();

        // The substitution: a file over the bound replaces the name atomically,
        // as an operator's `mv` or a tool's write-then-rename would.
        let replacement = dir.join("oversized.recovery");
        std::fs::write(&replacement, vec![0x5Au8; 4096]).unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        let read =
            read_bounded(file, &path, 64).expect("the pinned handle is still the small file");
        assert_eq!(
            read, small,
            "the read followed the descriptor, not the name"
        );
        assert_eq!(
            std::fs::read(&path).unwrap().len(),
            4096,
            "and the name now resolves to the oversized file a second open would have read"
        );

        // An ordinary oversized file is refused from its declared size, before
        // a byte is allocated.
        let big = dir.join("big.recovery");
        std::fs::write(&big, vec![0x11u8; 200]).unwrap();
        let err = read_bounded(std::fs::File::open(&big).unwrap(), &big, 64).unwrap_err();
        assert!(
            err.to_string().contains("is 200 bytes, over the 64 byte"),
            "{err}"
        );
    }

    /// The streaming bound is not a restatement of the declared size, and this
    /// is the case that proves it: a **FIFO** reports a length of zero however
    /// much data it will deliver. An operator who points
    /// `recovery_export_path` at a named pipe — a plausible way to feed an
    /// export from a decrypting or decompressing pipeline — passes any
    /// declared-size gate and would then read without any bound at all.
    /// `take(max + 1)` is what stops that, and the refusal is the same one an
    /// oversized file gets.
    ///
    /// **Every blocking step here is timeout-bounded, and that is not
    /// defensiveness — it is a correctness property of the test itself.** A
    /// FIFO open blocks until the other end appears, so any implementation that
    /// stops reading through the handle it was given and reopens the *name*
    /// blocks forever: the writer has finished and closed its end, and nothing
    /// will ever pair with the new reader. A test that simply called
    /// `read_bounded` would then hang the whole suite instead of failing, which
    /// is exactly what happened to a mutation run of this file. Running the
    /// call on a worker and failing on timeout turns that deadlock back into
    /// the assertion failure it should always have been.
    #[cfg(unix)]
    #[test]
    fn a_source_that_declares_no_size_is_still_bounded() {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        use std::sync::mpsc;
        use std::time::Duration;

        /// Generous enough that a loaded machine never trips it, short enough
        /// that a genuine deadlock is reported rather than waited on.
        const PATIENCE: Duration = Duration::from_secs(20);

        let dir = ScratchDir::new("import-fifo");
        let path = dir.join("export.fifo");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) },
            0,
            "mkfifo failed: {}",
            std::io::Error::last_os_error()
        );

        // Both ends block until they are paired, so the writer is started
        // first and the reader's open is what releases it.
        let writer_path = path.clone();
        std::thread::spawn(move || {
            let mut pipe = std::fs::OpenOptions::new()
                .write(true)
                .open(&writer_path)
                .expect("open the fifo for writing");
            // Far under a pipe buffer, so this never blocks on the reader.
            let _ = pipe.write_all(&vec![0x77u8; 4096]);
        });

        // The reader's own open is bounded too: if the writer thread failed
        // before pairing, this would otherwise never return.
        let (opened_tx, opened_rx) = mpsc::channel();
        let open_path = path.clone();
        std::thread::spawn(move || {
            let _ = opened_tx.send(std::fs::File::open(&open_path));
        });
        let file = opened_rx
            .recv_timeout(PATIENCE)
            .expect("the fifo reader and writer never paired")
            .expect("open the fifo for reading");
        assert_eq!(
            file.metadata().unwrap().len(),
            0,
            "a fifo declares no size, which is the whole point of this case"
        );

        let (read_tx, read_rx) = mpsc::channel();
        let read_path = path.clone();
        std::thread::spawn(move || {
            let _ = read_tx.send(read_bounded(file, &read_path, 64).map(|_| ()));
        });
        let outcome = read_rx.recv_timeout(PATIENCE).expect(
            "read_bounded never returned: it stopped reading through the handle it was \
             given and reopened the name, which on a fifo can never pair again",
        );
        let err = outcome.expect_err("an unsized source must still be bounded by the read");
        assert!(err.to_string().contains("over the 64 byte"), "{err}");
    }

    /// A capability exists only when the identity and the genesis are one
    /// document's. The crossed pair — identity and genesis `A`, export hash and
    /// administrators `B` — is refused at construction, which is the only place
    /// it can be refused: the confirming Genesis checks the signed document
    /// against the binding the import wrote, and under a crossed pair both of
    /// those came from `A`, so it would confirm happily.
    #[test]
    fn only_a_genesis_can_seal_its_own_import() {
        use crate::coordination::model::authority::{
            genesis_hash, testing as auth, verify_genesis,
        };
        use crate::coordination::model::shape;

        let sealed_for = |genesis: &crate::coordination::model::authority::VerifiedGenesis| {
            let authority = auth::authority();
            StorageIdentity::new(
                CoordinationGroupId::from_array(*genesis.group_id()),
                *genesis.genesis_hash(),
                authority.public_key,
                genesis.authority_domain(),
                crate::coordination::ids::CoordinationNodeId::from_array([1u8; 32]),
                genesis.deployment_profile_code(),
            )
        };
        let verified = |document: &crate::coordination::raft::GenesisV1| {
            let authority = auth::authority();
            verify_genesis(
                &authority.public_key,
                &authority.domain,
                &auth::sign_genesis(document),
            )
            .expect("the fixture genesis verifies")
        };

        let voters: Vec<NodeId> = vec![[1u8; 32], [2u8; 32], [3u8; 32]];
        let mut a = auth::genesis_for(&voters, shape::PROFILE_PROD);
        a.recovery_export_hash = Some(vec![0xA0; 32]);
        let mut b = a.clone();
        b.genesis_nonce = vec![0x66; 32];
        b.recovery_export_hash = Some(vec![0xB0; 32]);
        let (a, b) = (verified(&a), verified(&b));
        assert_ne!(a.genesis_hash(), b.genesis_hash());

        // The honest pair seals, and carries A's hash and admins.
        let identity_a = sealed_for(&a);
        let capability = RecoveryImport::seal(&identity_a, &a, "/tmp/export".into())
            .expect("a genesis seals its own import");
        assert_eq!(capability.expected_hash, [0xA0; 32]);
        assert_eq!(capability.identity(), &identity_a);
        assert_eq!(capability.genesis_admins, a.initial_admins().to_vec());

        // The crossed pair does not exist. This is the P0: without the seal,
        // `A`'s identity would bind the store while `B`'s hash decided which
        // export founded it.
        let err = RecoveryImport::seal(&identity_a, &b, "/tmp/export".into())
            .expect_err("identity A and genesis B are not one document");
        assert!(err.contains("genesis hash"), "got: {err}");

        // And a genesis that binds no export authorises no import at all.
        let mut plain = auth::genesis_for(&voters, shape::PROFILE_PROD);
        plain.recovery_export_hash = None;
        let plain = verified(&plain);
        let err = RecoveryImport::seal(&sealed_for(&plain), &plain, "/tmp/export".into())
            .expect_err("no bound export, no capability");
        assert!(err.contains("binds no recovery export"), "got: {err}");

        // Each replicated field is compared, not a representative sample. The
        // two below are the ones a partial seal would plausibly omit, and each
        // is refused on its own with the others left honest.

        // A different ROOT. The identity's key is what the store binds as its
        // replicated authority and what every later signed document — including
        // the confirming Genesis — is verified under, so sealing this pair
        // would found a group under an authority that signed nothing here.
        let other_root = ed25519_dalek::SigningKey::from_bytes(&[0x5A; 32])
            .verifying_key()
            .to_bytes();
        assert_ne!(other_root, *a.authority_public_key());
        let wrong_root = StorageIdentity::new(
            *identity_a.group_id(),
            *identity_a.genesis_hash(),
            other_root,
            identity_a.authority_domain(),
            *identity_a.local_node_id(),
            identity_a.deployment_profile_code(),
        );
        let err = RecoveryImport::seal(&wrong_root, &a, "/tmp/export".into())
            .expect_err("a ROOT this document was not verified under cannot seal it");
        assert!(err.contains("authority public key"), "got: {err}");

        // A local node the document never named as an initial voter. §7.2 lets
        // only a signed initial voter bootstrap, and a recovery bootstrap is a
        // bootstrap; every replicated field here is correct.
        let stranger = crate::coordination::ids::CoordinationNodeId::from_array([0x9F; 32]);
        assert!(!a.is_initial_voter(stranger.as_bytes()));
        let not_a_voter = StorageIdentity::new(
            *identity_a.group_id(),
            *identity_a.genesis_hash(),
            *identity_a.authority_public_key(),
            identity_a.authority_domain(),
            stranger,
            identity_a.deployment_profile_code(),
        );
        let err = RecoveryImport::seal(&not_a_voter, &a, "/tmp/export".into())
            .expect_err("an unauthorised node cannot hold a recovery capability");
        assert!(err.contains("signed initial voters"), "got: {err}");

        // The identity really is derived from the document: a hash that is not
        // this genesis's is the same refusal from the other side.
        let wrong = StorageIdentity::new(
            *identity_a.group_id(),
            genesis_hash(b"not this document"),
            *identity_a.authority_public_key(),
            identity_a.authority_domain(),
            *identity_a.local_node_id(),
            identity_a.deployment_profile_code(),
        );
        assert!(RecoveryImport::seal(&wrong, &a, "/tmp/export".into()).is_err());
    }
}
