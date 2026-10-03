//! Spike 5 (repaired) -- the canonical row-stream body.
//!
//! The first cut claimed "no whole-body materialization" and measured it with a
//! counter that could not observe the thing it claimed: the writer accumulated
//! the entire body in a `Vec<u8>`, the reader took the whole body as `&[u8]`
//! and retained every decoded row, and `peak_row_bytes` tracked only the
//! per-row encode buffer. The claim was true of the metric and false of the
//! code. This version streams for real -- `BodyWriter<W: Write>` writes
//! straight through and `read_body<R: Read>` drives a visitor -- so neither
//! side has anywhere to accumulate, and `spikes/coord-rowstream` measures the
//! result with a counting global allocator rather than a self-reported number.
//!
//! Also repaired: two roots, so a recovery body cannot carry the
//! authority-bearing snapshot row; component-wise ordering keys; per-section
//! byte ceilings; a bounded header; `row_counts` actually compared; and a
//! LEB128 reader that rejects overlong values instead of dropping bits.

use std::io::{Read, Write};

use fory_derive::{ForyStruct, ForyUnion};

use super::contract::{payload_fory, LogIdV1, ResourceKeyV1};
use super::raft::{ClientSessionV1, CoordinationMetaV1, MembershipIntentV1, StoredMembershipV1};

// ── Sections ─────────────────────────────────────────────────────────────────

pub const SECTION_RESOURCES: u64 = 1;
pub const SECTION_GRANTS: u64 = 2;
pub const SECTION_ADMINS: u64 = 3;
pub const SECTION_SESSIONS: u64 = 4;
pub const SECTION_INTENTS: u64 = 5;
pub const SECTION_ROUTES: u64 = 6;
pub const SECTION_AUDIT: u64 = 7;

pub const MAX_HEADER_BYTES: u64 = 64 * 1024;

/// §11.4's maximum snapshot size, as a bound on the whole body.
///
/// The per-section row counts and per-row byte ceilings already bound each
/// piece, and their product is far larger than this: 100,000 resource rows at
/// 128 KiB is 12 GiB. So the aggregate is a separate limit, not a consequence
/// of the others — a body that respects every section limit and still runs to
/// gigabytes is exactly the storage-pressure failure §11.4 says it is, and
/// discovering that by writing it is too late.
pub const MAX_SNAPSHOT_BODY_BYTES: u64 = 512 * 1024 * 1024;

/// The snapshot **body format** version stamped into
/// [`SnapshotHeaderV1::format_version`].
///
/// Distinct from the logical schema version and from the storage-value codec
/// version, and named because those are three different compatibility
/// concerns: the row-stream framing can change while the logical schema does
/// not, a redb value encoding can change while the framing does not, and a
/// logical migration can happen with both unchanged. Populating one from
/// another reads as correct precisely because they are all `1` today.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// The recovery **body format** version stamped into
/// [`RecoveryHeaderV1::format_version`].
///
/// Distinct from [`SNAPSHOT_FORMAT_VERSION`] for the same reason that constant
/// is distinct from the schema and codec versions: the two framings are
/// separate compatibility concerns and may diverge, and both being `1` today
/// is exactly the situation in which stamping one from the other reads as
/// correct. Unlike the snapshot constant — whose enforcement site is the
/// install path, its only consumer — this one is enforced by the body reader
/// itself, because a recovery body's consumers include the §13 import and the
/// golden vectors, and a newer export must be *refused*, never partially read.
pub const RECOVERY_FORMAT_VERSION: u32 = 1;

/// The logical schema version a v1 recovery header carries.
///
/// Declared here because this module compiles in the client graph and
/// `storage::identity::LOGICAL_SCHEMA_VERSION` is voter-only;
/// `the_recovery_versions_match_the_storage_identity` pins the two together
/// under the voter feature.
pub const RECOVERY_SCHEMA_VERSION: u32 = 1;

/// Per-section limits. One global 1 MiB row cap was far too loose: an admin row
/// is 32 bytes of payload and had four orders of magnitude of headroom.
pub struct SectionLimits {
    pub max_rows: u64,
    pub max_row_bytes: u64,
}

/// Row-count caps are the §12.1 constants themselves — `model::limits`
/// compiles in both graphs precisely because these are contract facts, so the
/// caps cannot drift from the model the way an earlier local restatement
/// could. The byte ceilings are framing facts with no model counterpart and
/// are pinned by `the_section_limits_are_frozen`.
pub fn limits(section: u64) -> Option<SectionLimits> {
    use super::model::limits as ml;
    let (max_rows, max_row_bytes) = match section {
        // 64 KiB value + 576 key + framing
        SECTION_RESOURCES => (ml::MAX_RESOURCES_PER_GROUP, 128 * 1024u64),
        SECTION_GRANTS => (ml::MAX_ACL_ROWS_PER_GROUP, 2 * 1024),
        SECTION_ADMINS => (ml::MAX_ADMINS_PER_GROUP, 256),
        SECTION_SESSIONS => (
            ml::MAX_ACTIVE_CLIENTS_PER_GROUP + ml::MAX_RETAINED_SESSIONS_PER_GROUP,
            4 * 1024,
        ),
        // 65, not 64: §9 retains 64 terminal intents and never evicts the
        // active one, so a legal group holds one more row than the cap. The
        // plan inside carries seeds + permits, hence the wide ceiling.
        SECTION_INTENTS => (ml::MAX_INTENT_ROWS, 64 * 1024),
        SECTION_ROUTES => (ml::MAX_RETAINED_ROUTES, 16 * 1024), // ticket
        SECTION_AUDIT => (ml::MAX_AUDIT_ROWS, 2 * 1024),
        _ => return None,
    };
    Some(SectionLimits {
        max_rows,
        max_row_bytes,
    })
}

/// Per-section declared counts. The header is typed, not opaque bytes, so the
/// reader can compare what it consumed against what the header promised -- a
/// check an earlier version described normatively and never implemented.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RowCounts")]
pub struct RowCountsV1 {
    #[fory(id = 1)]
    pub resources: u64,
    #[fory(id = 2)]
    pub grants: u64,
    #[fory(id = 3)]
    pub admins: u64,
    #[fory(id = 4)]
    pub sessions: u64,
    #[fory(id = 5)]
    pub intents: u64,
    #[fory(id = 6)]
    pub routes: u64,
    #[fory(id = 7)]
    pub audit: u64,
}

impl RowCountsV1 {
    pub fn get(&self, section: u64) -> Option<u64> {
        Some(match section {
            SECTION_RESOURCES => self.resources,
            SECTION_GRANTS => self.grants,
            SECTION_ADMINS => self.admins,
            SECTION_SESSIONS => self.sessions,
            SECTION_INTENTS => self.intents,
            SECTION_ROUTES => self.routes,
            SECTION_AUDIT => self.audit,
            _ => return None,
        })
    }
    pub fn set(&mut self, section: u64, n: u64) {
        match section {
            SECTION_RESOURCES => self.resources = n,
            SECTION_GRANTS => self.grants = n,
            SECTION_ADMINS => self.admins = n,
            SECTION_SESSIONS => self.sessions = n,
            SECTION_INTENTS => self.intents = n,
            SECTION_ROUTES => self.routes = n,
            SECTION_AUDIT => self.audit = n,
            _ => {}
        }
    }
}

/// §11.4's snapshot header.
///
/// One of **two** frozen header roots, not a shared one. An earlier revision
/// collapsed both into a single `BodyHeaderV1 { format_version, schema_version,
/// group_id, row_counts }`, which is neither: a recovery header carries ten
/// source-provenance fields that a snapshot header does not, and a snapshot
/// header carries the full `CoordinationMetaV1`. The shared type could express
/// neither, so a recovery export written through it silently dropped the
/// provenance a replacement genesis binds.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SnapshotHeader")]
pub struct SnapshotHeaderV1 {
    #[fory(id = 1)]
    pub format_version: u32,
    #[fory(id = 2)]
    pub schema_version: u32,
    #[fory(id = 3)]
    pub meta: CoordinationMetaV1,
    #[fory(id = 4)]
    pub row_counts: RowCountsV1,
}

/// §13's recovery-export header.
///
/// Every field is `source_*` because a recovery export describes the group it
/// came **from**: the importing group is new, and binding these to it is what
/// makes the export traceable to an authority rather than anonymous state.
/// Sessions, intents, routes and audit are absent by construction -- the row
/// profile cannot express them -- and their counts are therefore zero.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RecoveryHeader")]
pub struct RecoveryHeaderV1 {
    #[fory(id = 1)]
    pub format_version: u32,
    #[fory(id = 2)]
    pub schema_version: u32,
    #[fory(id = 3, bytes)]
    pub source_group_id: Vec<u8>,
    #[fory(id = 4, bytes)]
    pub source_genesis_hash: Vec<u8>,
    #[fory(id = 5, bytes)]
    pub source_authority_public_key: Vec<u8>,
    #[fory(id = 6)]
    pub source_authority_domain: String,
    #[fory(id = 7)]
    pub source_last_applied: Option<LogIdV1>,
    #[fory(id = 8)]
    pub source_membership: StoredMembershipV1,
    #[fory(id = 9)]
    pub source_audit_floor: Option<LogIdV1>,
    #[fory(id = 10)]
    pub source_audit_tip: Option<LogIdV1>,
    #[fory(id = 11)]
    pub source_admin_revision: u64,
    #[fory(id = 12)]
    pub row_counts: RowCountsV1,
}

/// What the framing needs from a header, so the two roots share one writer and
/// one reader without sharing a type.
pub trait BodyHeader: Sized + std::fmt::Debug + Clone + PartialEq {
    fn fory() -> fory_core::Fory;
    fn encode(&self) -> Result<Vec<u8>, BodyError>;
    fn decode(bytes: &[u8]) -> Result<Self, BodyError>;
    fn row_counts(&self) -> &RowCountsV1;
    /// Header-level validation, applied by both the writer and the reader.
    fn validate(&self) -> Result<(), BodyError>;
}

/// One runtime with **both** header roots registered, used to read either.
///
/// Fory ≥ 1.7 decodes a compatible-mode root whose wire type name is not
/// registered locally through a structural stub: the unknown fields are
/// dropped and the target comes back with defaults instead of an error. A
/// recovery header opened as a snapshot header would therefore decode into a
/// header with empty provenance. Registering the sibling root makes its name
/// resolve to a concrete, *different* target, which Fory refuses — so the two
/// roots stay mutually unreadable, as the framing requires.
fn both_headers_fory() -> fory_core::Fory {
    use crate::rpc::contract::{PayloadRegistry, WireField};
    let mut reg = PayloadRegistry::new();
    SnapshotHeaderV1::register_payload(&mut reg);
    RecoveryHeaderV1::register_payload(&mut reg);
    reg.into_fory()
}

impl BodyHeader for SnapshotHeaderV1 {
    fn fory() -> fory_core::Fory {
        both_headers_fory()
    }
    fn encode(&self) -> Result<Vec<u8>, BodyError> {
        Self::fory().serialize(self).map_err(|_| BodyError::Encode)
    }
    fn decode(bytes: &[u8]) -> Result<Self, BodyError> {
        Self::fory()
            .deserialize(bytes)
            .map_err(|_| BodyError::Decode)
    }
    fn row_counts(&self) -> &RowCountsV1 {
        &self.row_counts
    }
    /// Deliberately empty: snapshot header versions and the nested
    /// `CoordinationMetaV1` are enforced at the install path, the snapshot
    /// body's only consumer, where a mismatch has its own richer diagnostics
    /// and fatal/refuse classification. Enforcing them here too would change
    /// which error that path reports without adding a defense.
    fn validate(&self) -> Result<(), BodyError> {
        Ok(())
    }
}

impl BodyHeader for RecoveryHeaderV1 {
    fn fory() -> fory_core::Fory {
        both_headers_fory()
    }
    fn encode(&self) -> Result<Vec<u8>, BodyError> {
        Self::fory().serialize(self).map_err(|_| BodyError::Encode)
    }
    fn decode(bytes: &[u8]) -> Result<Self, BodyError> {
        Self::fory()
            .deserialize(bytes)
            .map_err(|_| BodyError::Decode)
    }
    fn row_counts(&self) -> &RowCountsV1 {
        &self.row_counts
    }
    /// The recovery header is validated by the framing itself: unlike the
    /// snapshot root it has more than one consumer (the §13 import, the golden
    /// vectors, operator tooling), so the rules cannot live at one call site.
    /// A newer version is *refused* — v1 has no cross-version recovery read —
    /// and the counts the row profile cannot express must be declared zero,
    /// which was previously a comment rather than a check.
    fn validate(&self) -> Result<(), BodyError> {
        if self.format_version != RECOVERY_FORMAT_VERSION {
            return Err(BodyError::UnsupportedVersion {
                field: "format_version",
                declared: self.format_version,
                supported: RECOVERY_FORMAT_VERSION,
            });
        }
        if self.schema_version != RECOVERY_SCHEMA_VERSION {
            return Err(BodyError::UnsupportedVersion {
                field: "schema_version",
                declared: self.schema_version,
                supported: RECOVERY_SCHEMA_VERSION,
            });
        }
        if self.source_group_id.len() != 32 {
            return Err(BodyError::InvalidHeader("source group id must be 32 bytes"));
        }
        if self.source_genesis_hash.len() != 32 {
            return Err(BodyError::InvalidHeader(
                "source genesis hash must be 32 bytes",
            ));
        }
        if self.source_authority_public_key.len() != 32 {
            return Err(BodyError::InvalidHeader(
                "source authority public key must be 32 bytes",
            ));
        }
        if self.source_authority_domain.is_empty()
            || self.source_authority_domain.len()
                > super::model::authority::MAX_AUTHORITY_DOMAIN_BYTES
        {
            return Err(BodyError::InvalidHeader("source authority domain width"));
        }
        let c = &self.row_counts;
        if c.sessions != 0 || c.intents != 0 || c.routes != 0 || c.audit != 0 {
            return Err(BodyError::InvalidHeader(
                "a recovery export declares no sessions, intents, routes, or audit rows",
            ));
        }
        Ok(())
    }
}

// ── Row types ────────────────────────────────────────────────────────────────

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SnapshotResourceRow")]
pub struct SnapshotResourceRowV1 {
    #[fory(id = 1)]
    pub key: ResourceKeyV1,
    #[fory(id = 2)]
    pub epoch: u64,
    #[fory(id = 3)]
    pub held: bool,
    #[fory(id = 4, bytes)]
    pub holder_node_id: Option<Vec<u8>>,
    #[fory(id = 5)]
    pub ttl_ms: u64,
    #[fory(id = 6)]
    pub lease_revision: u64,
    #[fory(id = 7)]
    pub value_revision: u64,
    #[fory(id = 8, bytes)]
    pub value: Vec<u8>,
    #[fory(id = 9)]
    pub acl_revision: u64,
}

/// Structurally cannot express holder/held, so a recovered group cannot inherit
/// lease authority even through a coding error.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RecoveryResourceRow")]
pub struct RecoveryResourceRowV1 {
    #[fory(id = 1)]
    pub key: ResourceKeyV1,
    #[fory(id = 2)]
    pub source_epoch: u64,
    #[fory(id = 3)]
    pub ttl_ms: u64,
    #[fory(id = 4)]
    pub lease_revision: u64,
    #[fory(id = 5)]
    pub value_revision: u64,
    #[fory(id = 6, bytes)]
    pub value: Vec<u8>,
    #[fory(id = 7)]
    pub acl_revision: u64,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/GrantRow")]
pub struct GrantRowV1 {
    #[fory(id = 1)]
    pub key: ResourceKeyV1,
    #[fory(id = 2, bytes)]
    pub subject_node_id: Vec<u8>,
    #[fory(id = 3)]
    pub rights: u64,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AdminRow")]
pub struct AdminRowV1 {
    #[fory(id = 1, bytes)]
    pub subject_node_id: Vec<u8>,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SessionRow")]
pub struct SessionRowV1 {
    #[fory(id = 1, bytes)]
    pub actor_node_id: Vec<u8>,
    #[fory(id = 2, bytes)]
    pub client_id: Vec<u8>,
    /// The §11 storage root itself, nested rather than flattened.
    ///
    /// An earlier revision copied `ClientSessionV1`'s four fields into this row.
    /// That left the declared storage root with no user anywhere, so its shape
    /// was never exercised, and the row and the root could drift apart while
    /// both round-tripped their own tests. A snapshot section and the table it
    /// restores must be the same type or restoring is a translation.
    #[fory(id = 3)]
    pub session: ClientSessionV1,
}

/// One retained membership intent, keyed by operation id.
///
/// The state is the §11 `MembershipIntentV1` storage root, nested for the same
/// reason as [`SessionRowV1::session`]: a flattened copy leaves the declared
/// root unused, and a root nothing constructs is a declaration rather than a
/// format.
// No `Default`: `MembershipIntentV1` has none deliberately, because a default
// intent would carry an all-zero `accepted_log_id`, which names an entry no
// leader ever wrote.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/IntentRow")]
pub struct IntentRowV1 {
    #[fory(id = 1, bytes)]
    pub operation_id: Vec<u8>,
    #[fory(id = 2)]
    pub intent: MembershipIntentV1,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RouteRow")]
pub struct RouteRowV1 {
    #[fory(id = 1, bytes)]
    pub node_id: Vec<u8>,
    #[fory(id = 2, bytes)]
    pub ticket_bytes: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub source_operation_id: Vec<u8>,
    #[fory(id = 4)]
    pub introduced_log_id: LogIdV1,
}

/// Stored value: term and index are the redb key, so they are not repeated.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AuditRow")]
pub struct AuditRowV1 {
    #[fory(id = 1, bytes)]
    pub leader_node_id: Vec<u8>,
    /// `None` only when a committed malformed client or retirement command's
    /// actor bytes could not be narrowed; `Some` is always exactly 32 bytes.
    ///
    /// An `Option`, not an empty-vector sentinel. The model, this stored row,
    /// the snapshot/export row and the public `AuditRecordWireV1` all use the
    /// same shape and the same width rule -- an earlier revision encoded absence
    /// as an empty vector while snapshot validation still demanded 32 bytes, so
    /// the very row §11.3 requires for a malformed entry could not be exported
    /// or installed.
    #[fory(id = 2, bytes)]
    pub actor_node_id: Option<Vec<u8>>,
    #[fory(id = 3)]
    pub operation_code: i32,
    #[fory(id = 4)]
    pub resource: Option<ResourceKeyV1>,
    #[fory(id = 5)]
    pub result_code: i32,
    #[fory(id = 6)]
    pub epoch: u64,
    #[fory(id = 7)]
    pub value_revision: u64,
}

/// Streamed row: a body has no table key, so it inlines term and index.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AuditStreamRow")]
pub struct AuditStreamRowV1 {
    #[fory(id = 1)]
    pub log_term: u64,
    #[fory(id = 2)]
    pub log_index: u64,
    #[fory(id = 3)]
    pub row: AuditRowV1,
}

impl AuditRowV1 {
    pub fn log_id(&self, key: (u64, u64)) -> LogIdV1 {
        LogIdV1 {
            term: key.0,
            leader_node_id: self.leader_node_id.clone(),
            index: key.1,
        }
    }
}
impl AuditStreamRowV1 {
    pub fn key(&self) -> (u64, u64) {
        (self.log_term, self.log_index)
    }
    pub fn log_id(&self) -> LogIdV1 {
        self.row.log_id(self.key())
    }
}

// ── Ordering keys ────────────────────────────────────────────────────────────

/// An owned typed tuple key. `Vec<Vec<u8>>`'s `Ord` is lexicographic over
/// components with each component compared bytewise, which is exactly redb's
/// tuple ordering.
///
/// Two earlier attempts were both wrong. Flattening `namespace || key` made
/// `("a","bc")` and `("ab","c")` compare equal. Length-prefixing removed that
/// collision but *inverted* the order: under `len || bytes`, `"b"` sorts before
/// `"aa"` because `1 < 2`, while redb sorts `"aa"` first. Only a real tuple
/// comparison gets both right.
pub type OrderKey = Vec<Vec<u8>>;

fn component_key(parts: &[&[u8]]) -> OrderKey {
    parts.iter().map(|p| p.to_vec()).collect()
}

fn resource_key_parts(k: &ResourceKeyV1) -> OrderKey {
    component_key(&[k.namespace.as_bytes(), &k.key])
}

// ── Two roots ────────────────────────────────────────────────────────────────

/// A row that can appear in a body. Two roots implement it, so the framing is
/// shared while the row sets stay disjoint.
pub trait BodyRow: Sized + std::fmt::Debug + Clone + PartialEq {
    /// The header root this profile's body begins with. Bound to the profile so
    /// a recovery body cannot be written under a snapshot header, or the
    /// reverse: the two are never alternatives within one stream.
    type Header: BodyHeader;

    fn section(&self) -> Option<u64>;
    fn order_key(&self) -> OrderKey;
    fn fory() -> fory_core::Fory;
    fn encode(f: &fory_core::Fory, r: &Self) -> Result<Vec<u8>, BodyError>;
    fn decode(f: &fory_core::Fory, b: &[u8]) -> Result<Self, BodyError>;
    fn legal_sections() -> &'static [u64];
    fn validate(r: &Self) -> Result<(), BodyError>;
}

#[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SnapshotBodyRow")]
pub enum SnapshotBodyRowV1 {
    #[fory(default)]
    #[fory(id = 1)]
    Resource(SnapshotResourceRowV1),
    #[fory(id = 2)]
    Grant(GrantRowV1),
    #[fory(id = 3)]
    Admin(AdminRowV1),
    #[fory(id = 4)]
    Session(SessionRowV1),
    #[fory(id = 5)]
    Intent(IntentRowV1),
    #[fory(id = 6)]
    Route(RouteRowV1),
    #[fory(id = 7)]
    Audit(AuditStreamRowV1),
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

/// A separate root, so a recovery body **cannot** carry the authority-bearing
/// snapshot resource row, nor sessions/intents/routes/audit. Sharing
/// `GrantRowV1`/`AdminRowV1` between the two roots is safe: `PayloadRegistry`
/// collisions occur within a runtime, not between runtimes.
#[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RecoveryBodyRow")]
pub enum RecoveryBodyRowV1 {
    #[fory(default)]
    #[fory(id = 1)]
    Resource(RecoveryResourceRowV1),
    #[fory(id = 2)]
    Grant(GrantRowV1),
    #[fory(id = 3)]
    Admin(AdminRowV1),
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

const SNAPSHOT_SECTIONS: &[u64] = &[
    SECTION_RESOURCES,
    SECTION_GRANTS,
    SECTION_ADMINS,
    SECTION_SESSIONS,
    SECTION_INTENTS,
    SECTION_ROUTES,
    SECTION_AUDIT,
];
const RECOVERY_SECTIONS: &[u64] = &[SECTION_RESOURCES, SECTION_GRANTS, SECTION_ADMINS];

impl BodyRow for SnapshotBodyRowV1 {
    type Header = SnapshotHeaderV1;

    fn section(&self) -> Option<u64> {
        Some(match self {
            SnapshotBodyRowV1::Resource(_) => SECTION_RESOURCES,
            SnapshotBodyRowV1::Grant(_) => SECTION_GRANTS,
            SnapshotBodyRowV1::Admin(_) => SECTION_ADMINS,
            SnapshotBodyRowV1::Session(_) => SECTION_SESSIONS,
            SnapshotBodyRowV1::Intent(_) => SECTION_INTENTS,
            SnapshotBodyRowV1::Route(_) => SECTION_ROUTES,
            SnapshotBodyRowV1::Audit(_) => SECTION_AUDIT,
            SnapshotBodyRowV1::Unknown(_) => return None,
        })
    }
    fn order_key(&self) -> OrderKey {
        match self {
            SnapshotBodyRowV1::Resource(r) => resource_key_parts(&r.key),
            SnapshotBodyRowV1::Grant(r) => {
                component_key(&[r.key.namespace.as_bytes(), &r.key.key, &r.subject_node_id])
            }
            SnapshotBodyRowV1::Admin(r) => component_key(&[&r.subject_node_id]),
            SnapshotBodyRowV1::Session(r) => component_key(&[&r.actor_node_id, &r.client_id]),
            SnapshotBodyRowV1::Intent(r) => component_key(&[&r.operation_id]),
            SnapshotBodyRowV1::Route(r) => component_key(&[&r.node_id]),
            SnapshotBodyRowV1::Audit(r) => {
                component_key(&[&r.log_term.to_be_bytes(), &r.log_index.to_be_bytes()])
            }
            SnapshotBodyRowV1::Unknown(_) => OrderKey::new(),
        }
    }
    fn fory() -> fory_core::Fory {
        payload_fory::<SnapshotBodyRowV1>()
    }
    fn encode(f: &fory_core::Fory, r: &Self) -> Result<Vec<u8>, BodyError> {
        f.serialize(r).map_err(|_| BodyError::Encode)
    }
    fn decode(f: &fory_core::Fory, b: &[u8]) -> Result<Self, BodyError> {
        f.deserialize(b).map_err(|_| BodyError::Decode)
    }
    fn legal_sections() -> &'static [u64] {
        SNAPSHOT_SECTIONS
    }
    fn validate(r: &Self) -> Result<(), BodyError> {
        validate_snapshot_row(r)
    }
}

impl BodyRow for RecoveryBodyRowV1 {
    type Header = RecoveryHeaderV1;

    fn section(&self) -> Option<u64> {
        Some(match self {
            RecoveryBodyRowV1::Resource(_) => SECTION_RESOURCES,
            RecoveryBodyRowV1::Grant(_) => SECTION_GRANTS,
            RecoveryBodyRowV1::Admin(_) => SECTION_ADMINS,
            RecoveryBodyRowV1::Unknown(_) => return None,
        })
    }
    fn order_key(&self) -> OrderKey {
        match self {
            RecoveryBodyRowV1::Resource(r) => resource_key_parts(&r.key),
            RecoveryBodyRowV1::Grant(r) => {
                component_key(&[r.key.namespace.as_bytes(), &r.key.key, &r.subject_node_id])
            }
            RecoveryBodyRowV1::Admin(r) => component_key(&[&r.subject_node_id]),
            RecoveryBodyRowV1::Unknown(_) => OrderKey::new(),
        }
    }
    fn fory() -> fory_core::Fory {
        payload_fory::<RecoveryBodyRowV1>()
    }
    fn encode(f: &fory_core::Fory, r: &Self) -> Result<Vec<u8>, BodyError> {
        f.serialize(r).map_err(|_| BodyError::Encode)
    }
    fn decode(f: &fory_core::Fory, b: &[u8]) -> Result<Self, BodyError> {
        f.deserialize(b).map_err(|_| BodyError::Decode)
    }
    fn legal_sections() -> &'static [u64] {
        RECOVERY_SECTIONS
    }
    fn validate(r: &Self) -> Result<(), BodyError> {
        validate_recovery_row(r)
    }
}

// ── Minimal LEB128 ───────────────────────────────────────────────────────────

pub fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum BodyError {
    NonMinimalVarint,
    VarintOverflow,
    Truncated,
    HeaderTooLarge {
        declared: u64,
    },
    SectionOutOfOrder {
        got: u64,
        previous: u64,
    },
    IllegalSection(u64),
    RowCountTooLarge {
        section: u64,
        declared: u64,
        max: u64,
    },
    RowTooLarge {
        section: u64,
        declared: u64,
        max: u64,
    },
    WrongSectionForRow {
        section: u64,
    },
    RowsOutOfOrder,
    UnknownVariant,
    HeaderCountMismatch {
        section: u64,
        declared: u64,
        actual: u64,
    },
    IncompleteProfile {
        expected: Vec<u64>,
        got: Vec<u64>,
    },
    NoOpenSection,
    InvalidRow(&'static str),
    InvalidHeader(&'static str),
    /// The header's declared bytes are not the canonical encoding of the
    /// value they decode to — e.g. Fory-tolerated trailing bytes under an
    /// adjusted length. Distinct from `Decode` (not parseable at all) and
    /// from `InvalidHeader` (parseable, semantically illegal).
    NoncanonicalHeader,
    /// A row's declared bytes are not the canonical encoding of the value
    /// they decode to.
    NoncanonicalRow {
        section: u64,
    },
    /// A version field this build does not support. Newer is refused, never
    /// skipped: v1 has no automatic cross-version upgrade, so the actionable
    /// diagnostic names the field, what the body declared, and what this
    /// build supports.
    UnsupportedVersion {
        field: &'static str,
        declared: u32,
        supported: u32,
    },
    Decode,
    Encode,
    Io,
}

/// `Ok(None)` means a clean end of stream at a frame boundary. Anything else
/// that runs out mid-value is `Truncated`.
fn read_u8_opt<R: Read>(r: &mut R) -> Result<Option<u8>, BodyError> {
    let mut b = [0u8; 1];
    match r.read(&mut b) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(b[0])),
        Err(_) => Err(BodyError::Io),
    }
}

fn read_u8<R: Read>(r: &mut R) -> Result<u8, BodyError> {
    read_u8_opt(r)?.ok_or(BodyError::Truncated)
}

/// A varint at a position where end-of-stream is legal. Distinguishes "nothing
/// left" from "a varint started and then the stream ended", which an earlier
/// version conflated -- appending a lone `0x80` to a complete body was accepted
/// as a clean EOF.
pub fn read_varint_boundary<R: Read>(r: &mut R) -> Result<Option<u64>, BodyError> {
    match read_u8_opt(r)? {
        None => Ok(None),
        Some(first) => Ok(Some(continue_varint(r, first)?)),
    }
}

/// Minimal LEB128 with a real overflow guard. The earlier reader checked
/// `shift >= 64` but never bounded the tenth byte's value, so `0xff` in the
/// tenth position silently dropped bits instead of erroring.
pub fn read_varint<R: Read>(r: &mut R) -> Result<u64, BodyError> {
    let first = read_u8(r)?;
    continue_varint(r, first)
}

fn continue_varint<R: Read>(r: &mut R, first: u8) -> Result<u64, BodyError> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    let mut byte = first;
    loop {
        if shift == 63 {
            if byte > 1 {
                return Err(BodyError::VarintOverflow);
            }
        } else if shift > 63 {
            return Err(BodyError::VarintOverflow);
        }
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            if byte == 0 && shift > 0 {
                return Err(BodyError::NonMinimalVarint);
            }
            return Ok(result);
        }
        shift += 7;
        byte = read_u8(r)?;
    }
}

// ── Streaming writer ─────────────────────────────────────────────────────────

/// Writes straight through to `W`. Nothing here grows with the body: `buf` is
/// reused for exactly one row at a time.
///
/// Every rule the reader enforces, the writer enforces too, and by returning
/// `Result` rather than panicking: an earlier version used `assert!`/`expect`
/// throughout, so a malformed body was a crash rather than an error, and
/// nothing stopped it emitting rows in the wrong section or out of order.
pub struct BodyWriter<W: Write, T: BodyRow> {
    w: W,
    fory: fory_core::Fory,
    buf: Vec<u8>,
    header_counts: RowCountsV1,
    previous_section: u64,
    open: Option<(u64, u64, u64)>,
    last_key: Option<OrderKey>,
    emitted: Vec<u64>,
    _t: std::marker::PhantomData<T>,
}

impl<W: Write, T: BodyRow> BodyWriter<W, T> {
    pub fn new(mut w: W, header: &T::Header) -> Result<Self, BodyError> {
        header.validate()?;
        let enc = header.encode()?;
        if enc.len() as u64 > MAX_HEADER_BYTES {
            return Err(BodyError::HeaderTooLarge {
                declared: enc.len() as u64,
            });
        }
        let mut pre = Vec::new();
        write_varint(&mut pre, enc.len() as u64);
        w.write_all(&pre).map_err(|_| BodyError::Io)?;
        w.write_all(&enc).map_err(|_| BodyError::Io)?;
        Ok(BodyWriter {
            w,
            fory: T::fory(),
            buf: Vec::new(),
            header_counts: header.row_counts().clone(),
            previous_section: 0,
            open: None,
            last_key: None,
            emitted: Vec::new(),
            _t: std::marker::PhantomData,
        })
    }

    pub fn begin_section(&mut self, id: u64) -> Result<(), BodyError> {
        self.end_section()?;
        if id <= self.previous_section {
            return Err(BodyError::SectionOutOfOrder {
                got: id,
                previous: self.previous_section,
            });
        }
        if !T::legal_sections().contains(&id) {
            return Err(BodyError::IllegalSection(id));
        }
        let declared = self
            .header_counts
            .get(id)
            .ok_or(BodyError::IllegalSection(id))?;
        let lim = limits(id).ok_or(BodyError::IllegalSection(id))?;
        if declared > lim.max_rows {
            return Err(BodyError::RowCountTooLarge {
                section: id,
                declared,
                max: lim.max_rows,
            });
        }
        self.previous_section = id;
        self.last_key = None;
        self.buf.clear();
        write_varint(&mut self.buf, id);
        write_varint(&mut self.buf, declared);
        self.w.write_all(&self.buf).map_err(|_| BodyError::Io)?;
        self.open = Some((id, declared, 0));
        Ok(())
    }

    pub fn row(&mut self, row: &T) -> Result<(), BodyError> {
        let (sid, declared, written) = self.open.ok_or(BodyError::NoOpenSection)?;
        if row.section() != Some(sid) {
            return Err(BodyError::WrongSectionForRow { section: sid });
        }
        if written + 1 > declared {
            return Err(BodyError::HeaderCountMismatch {
                section: sid,
                declared,
                actual: written + 1,
            });
        }
        // Same logical validation the reader applies. Without it the writer
        // could emit a row its own reader would later reject -- a body that is
        // unreadable by construction, discovered only on install.
        T::validate(row)?;
        let k = row.order_key();
        if let Some(prev) = &self.last_key {
            if &k <= prev {
                return Err(BodyError::RowsOutOfOrder);
            }
        }
        let lim = limits(sid).ok_or(BodyError::IllegalSection(sid))?;
        let enc = T::encode(&self.fory, row)?;
        if enc.len() as u64 > lim.max_row_bytes {
            return Err(BodyError::RowTooLarge {
                section: sid,
                declared: enc.len() as u64,
                max: lim.max_row_bytes,
            });
        }
        self.last_key = Some(k);
        self.buf.clear();
        write_varint(&mut self.buf, enc.len() as u64);
        self.w.write_all(&self.buf).map_err(|_| BodyError::Io)?;
        self.w.write_all(&enc).map_err(|_| BodyError::Io)?;
        self.open = Some((sid, declared, written + 1));
        Ok(())
    }

    fn end_section(&mut self) -> Result<(), BodyError> {
        if let Some((sid, declared, written)) = self.open.take() {
            if declared != written {
                return Err(BodyError::HeaderCountMismatch {
                    section: sid,
                    declared,
                    actual: written,
                });
            }
            self.emitted.push(sid);
        }
        Ok(())
    }

    /// A body must carry its root's complete section profile, every section
    /// exactly once and in ascending order.
    pub fn finish(mut self) -> Result<W, BodyError> {
        self.end_section()?;
        if self.emitted != T::legal_sections() {
            return Err(BodyError::IncompleteProfile {
                expected: T::legal_sections().to_vec(),
                got: self.emitted.clone(),
            });
        }
        self.w.flush().map_err(|_| BodyError::Io)?;
        Ok(self.w)
    }
}

// ── Streaming reader ─────────────────────────────────────────────────────────

/// The reader hands each row to the visitor and forgets it. Retention is the
/// caller's choice, which is what makes the memory claim testable at all.
pub trait BodyVisitor<T: BodyRow> {
    fn header(&mut self, header: &T::Header) -> Result<(), BodyError>;
    fn row(&mut self, section: u64, row: T) -> Result<(), BodyError>;
}

/// Logical validation of a decoded row -- widths and value bounds that the
/// encoded-byte ceiling does not imply. A 32-byte NodeId field can hold 4 bytes
/// within any byte ceiling, so the ceiling is not the check.
const MAX_NAMESPACE: usize = 64;
const MAX_KEY: usize = 512;
const MAX_VALUE: usize = 64 * 1024;
const RIGHTS_MASK: u64 = 0x3f;

fn check_node_id(b: &[u8]) -> Result<(), BodyError> {
    if b.len() != 32 {
        return Err(BodyError::InvalidRow("node id must be 32 bytes"));
    }
    Ok(())
}
fn check_resource_key(k: &ResourceKeyV1) -> Result<(), BodyError> {
    if k.namespace.is_empty() || k.namespace.len() > MAX_NAMESPACE {
        return Err(BodyError::InvalidRow("namespace width"));
    }
    if k.key.is_empty() || k.key.len() > MAX_KEY {
        return Err(BodyError::InvalidRow("resource key width"));
    }
    Ok(())
}

pub fn validate_snapshot_row(r: &SnapshotBodyRowV1) -> Result<(), BodyError> {
    match r {
        SnapshotBodyRowV1::Resource(x) => {
            check_resource_key(&x.key)?;
            if x.value.len() > MAX_VALUE {
                return Err(BodyError::InvalidRow("value width"));
            }
            match (&x.holder_node_id, x.held) {
                (Some(h), _) => check_node_id(h)?,
                (None, true) => return Err(BodyError::InvalidRow("held with no holder")),
                (None, false) => {}
            }
        }
        SnapshotBodyRowV1::Grant(x) => {
            check_resource_key(&x.key)?;
            check_node_id(&x.subject_node_id)?;
            if x.rights & !RIGHTS_MASK != 0 {
                return Err(BodyError::InvalidRow("unknown rights bits"));
            }
        }
        SnapshotBodyRowV1::Admin(x) => check_node_id(&x.subject_node_id)?,
        SnapshotBodyRowV1::Session(x) => {
            check_node_id(&x.actor_node_id)?;
            if x.client_id.len() != 16 {
                return Err(BodyError::InvalidRow("client id must be 16 bytes"));
            }
            if x.session.request_hash.len() != 32 && !x.session.request_hash.is_empty() {
                return Err(BodyError::InvalidRow("request hash width"));
            }
        }
        SnapshotBodyRowV1::Intent(x) => {
            if x.operation_id.len() != 16 {
                return Err(BodyError::InvalidRow("operation id must be 16 bytes"));
            }
            check_node_id(&x.intent.actor_node_id)?;
        }
        SnapshotBodyRowV1::Route(x) => check_node_id(&x.node_id)?,
        SnapshotBodyRowV1::Audit(x) => {
            check_node_id(&x.row.leader_node_id)?;
            if let Some(actor) = &x.row.actor_node_id {
                check_node_id(actor)?;
            }
            if let Some(k) = &x.row.resource {
                check_resource_key(k)?;
            }
        }
        SnapshotBodyRowV1::Unknown(_) => return Err(BodyError::UnknownVariant),
    }
    Ok(())
}

pub fn validate_recovery_row(r: &RecoveryBodyRowV1) -> Result<(), BodyError> {
    match r {
        RecoveryBodyRowV1::Resource(x) => {
            check_resource_key(&x.key)?;
            if x.value.len() > MAX_VALUE {
                return Err(BodyError::InvalidRow("value width"));
            }
        }
        RecoveryBodyRowV1::Grant(x) => {
            check_resource_key(&x.key)?;
            check_node_id(&x.subject_node_id)?;
            if x.rights & !RIGHTS_MASK != 0 {
                return Err(BodyError::InvalidRow("unknown rights bits"));
            }
        }
        RecoveryBodyRowV1::Admin(x) => check_node_id(&x.subject_node_id)?,
        RecoveryBodyRowV1::Unknown(_) => return Err(BodyError::UnknownVariant),
    }
    Ok(())
}

/// The section profile is derived from the root, not supplied by the caller.
/// An earlier version let callers pass any `expected_sections`, so a partial
/// body was readable by asking for exactly the partial profile it happened to
/// contain -- which made the "sections 1-7 exactly once" rule unenforced.
pub fn read_body<R: Read, T: BodyRow>(
    r: &mut R,
    v: &mut dyn BodyVisitor<T>,
) -> Result<(), BodyError> {
    let fory = T::fory();

    let hlen = read_varint(r)?;
    if hlen > MAX_HEADER_BYTES {
        return Err(BodyError::HeaderTooLarge { declared: hlen });
    }
    let mut hbuf = vec![0u8; hlen as usize];
    r.read_exact(&mut hbuf).map_err(|_| BodyError::Truncated)?;
    let header = T::Header::decode(&hbuf)?;
    // Canonicality: the declared bytes must be exactly what this build would
    // emit for the decoded value. Minimal outer varints alone do not give the
    // body one encoding, because Fory tolerates trailing bytes — a frame of
    // `fory(value) || 0x00` decodes to the same value under an adjusted
    // length, and a genesis could legitimately bind the hash of those
    // noncanonical bytes. Decode/re-encode equality closes the gap.
    if T::Header::encode(&header)? != hbuf {
        return Err(BodyError::NoncanonicalHeader);
    }
    drop(hbuf);
    header.validate()?;
    v.header(&header)?;

    let mut seen: Vec<u64> = Vec::new();
    let mut previous = 0u64;
    let mut buf: Vec<u8> = Vec::new();

    // Clean EOF is only legal here, at a section boundary.
    while let Some(sid) = read_varint_boundary(r)? {
        if sid <= previous {
            return Err(BodyError::SectionOutOfOrder { got: sid, previous });
        }
        if !T::legal_sections().contains(&sid) {
            return Err(BodyError::IllegalSection(sid));
        }
        let lim = limits(sid).ok_or(BodyError::IllegalSection(sid))?;
        previous = sid;

        let declared = read_varint(r)?;
        if declared > lim.max_rows {
            return Err(BodyError::RowCountTooLarge {
                section: sid,
                declared,
                max: lim.max_rows,
            });
        }
        // The header promised a count; the section must declare the same.
        let promised = header
            .row_counts()
            .get(sid)
            .ok_or(BodyError::IllegalSection(sid))?;
        if promised != declared {
            return Err(BodyError::HeaderCountMismatch {
                section: sid,
                declared: promised,
                actual: declared,
            });
        }

        let mut actual = 0u64;
        let mut last_key: Option<OrderKey> = None;
        for _ in 0..declared {
            let rlen = read_varint(r)?;
            if rlen > lim.max_row_bytes {
                return Err(BodyError::RowTooLarge {
                    section: sid,
                    declared: rlen,
                    max: lim.max_row_bytes,
                });
            }
            buf.clear();
            buf.resize(rlen as usize, 0);
            r.read_exact(&mut buf).map_err(|_| BodyError::Truncated)?;
            let row = T::decode(&fory, &buf)?;
            // Same canonicality rule as the header, per row: Fory's
            // trailing-byte tolerance must not let two byte streams name one
            // logical body.
            if T::encode(&fory, &row)? != buf {
                return Err(BodyError::NoncanonicalRow { section: sid });
            }

            match row.section() {
                None => return Err(BodyError::UnknownVariant),
                Some(sec) if sec != sid => {
                    return Err(BodyError::WrongSectionForRow { section: sid })
                }
                _ => {}
            }
            T::validate(&row)?;
            let k = row.order_key();
            if let Some(prev) = &last_key {
                if &k <= prev {
                    return Err(BodyError::RowsOutOfOrder);
                }
            }
            last_key = Some(k);
            actual += 1;
            v.row(sid, row)?;
        }
        if actual != declared {
            return Err(BodyError::HeaderCountMismatch {
                section: sid,
                declared,
                actual,
            });
        }
        seen.push(sid);
    }

    if seen != T::legal_sections() {
        return Err(BodyError::IncompleteProfile {
            expected: T::legal_sections().to_vec(),
            got: seen,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The section limits are frozen format facts, pinned as the numbers
    /// themselves. The row caps come from `model::limits`, so this also pins
    /// those constants against an accidental edit silently changing what the
    /// frozen v1 framing accepts; the byte ceilings exist only here.
    #[test]
    fn the_section_limits_are_frozen() {
        let expect: &[(u64, u64, u64)] = &[
            (SECTION_RESOURCES, 100_000, 128 * 1024),
            (SECTION_GRANTS, 100_000, 2 * 1024),
            (SECTION_ADMINS, 64, 256),
            (SECTION_SESSIONS, 1_024 + 16_384, 4 * 1024),
            (SECTION_INTENTS, 65, 64 * 1024),
            (SECTION_ROUTES, 64, 16 * 1024),
            (SECTION_AUDIT, 100_000, 2 * 1024),
        ];
        for &(section, rows, bytes) in expect {
            let lim = limits(section).unwrap();
            assert_eq!(lim.max_rows, rows, "row cap for section {section}");
            assert_eq!(
                lim.max_row_bytes, bytes,
                "byte ceiling for section {section}"
            );
        }
        assert!(limits(0).is_none());
        assert!(limits(8).is_none());
        assert_eq!(
            crate::coordination::model::limits::MAX_INTENT_ROWS,
            crate::coordination::model::limits::MAX_RETAINED_INTENTS + 1,
            "64 terminal intents plus the one active intent §9 never evicts"
        );
    }

    /// The recovery header's version constants and the storage identity are
    /// one number each; this is the only module that can compare them, since
    /// `storage` is voter-only and `rowstream` is in the client graph.
    #[cfg(feature = "coordination")]
    #[test]
    fn the_recovery_versions_match_the_storage_identity() {
        assert_eq!(RECOVERY_FORMAT_VERSION, 1);
        assert_eq!(RECOVERY_SCHEMA_VERSION, 1);
        assert_eq!(SNAPSHOT_FORMAT_VERSION, 1);
        assert_eq!(
            RECOVERY_SCHEMA_VERSION,
            crate::coordination::storage::identity::LOGICAL_SCHEMA_VERSION,
        );
    }

    fn key(ns: &str, k: &[u8]) -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: ns.into(),
            key: k.to_vec(),
        }
    }
    fn nid(b: u8) -> Vec<u8> {
        vec![b; 32]
    }
    fn admin(b: u8) -> SnapshotBodyRowV1 {
        SnapshotBodyRowV1::Admin(AdminRowV1 {
            subject_node_id: nid(b),
        })
    }
    fn r_admin(b: u8) -> RecoveryBodyRowV1 {
        RecoveryBodyRowV1::Admin(AdminRowV1 {
            subject_node_id: nid(b),
        })
    }
    fn snap_resource(i: u64) -> SnapshotResourceRowV1 {
        SnapshotResourceRowV1 {
            key: key("portal.roaming", &i.to_be_bytes()),
            epoch: i,
            held: true,
            holder_node_id: Some(nid(9)),
            ttl_ms: 30_000,
            lease_revision: i,
            value_revision: i,
            value: vec![1, 2, 3],
            acl_revision: 0,
        }
    }
    fn rec_resource(i: u64) -> RecoveryResourceRowV1 {
        RecoveryResourceRowV1 {
            key: key("portal.roaming", &i.to_be_bytes()),
            source_epoch: i,
            ttl_ms: 30_000,
            lease_revision: i,
            value_revision: i,
            value: vec![1, 2, 3],
            acl_revision: 0,
        }
    }
    fn audit(term: u64, index: u64) -> SnapshotBodyRowV1 {
        SnapshotBodyRowV1::Audit(AuditStreamRowV1 {
            log_term: term,
            log_index: index,
            row: AuditRowV1 {
                leader_node_id: nid(4),
                actor_node_id: Some(nid(5)),
                operation_code: 1,
                resource: Some(key("p", b"t")),
                result_code: 1,
                epoch: 1,
                value_revision: 1,
            },
        })
    }

    #[derive(Default)]
    struct Counting {
        rows: usize,
    }
    impl<T: BodyRow> BodyVisitor<T> for Counting {
        fn header(&mut self, _h: &T::Header) -> Result<(), BodyError> {
            Ok(())
        }
        fn row(&mut self, _s: u64, _r: T) -> Result<(), BodyError> {
            self.rows += 1;
            Ok(())
        }
    }

    fn header_for(counts: RowCountsV1) -> SnapshotHeaderV1 {
        SnapshotHeaderV1 {
            format_version: 1,
            schema_version: 1,
            meta: meta(),
            row_counts: counts,
        }
    }

    fn meta() -> CoordinationMetaV1 {
        CoordinationMetaV1 {
            schema_version: 1,
            group_id: vec![7u8; 16],
            genesis_hash: vec![0x11u8; 32],
            authority_public_key: nid(3),
            authority_domain: "aster0.net".into(),
            deployment_profile_code: 1,
            last_applied: Some(LogIdV1 {
                term: 1,
                leader_node_id: nid(1),
                index: 9,
            }),
            membership: StoredMembershipV1::default(),
            admin_revision: 1,
            recovery_export_hash: None,
            recovered_from: None,
            aggregate_growth_bytes: 0,
            aggregate_reserve_bytes: 0,
            audit_floor: None,
        }
    }

    fn recovery_header_for(counts: RowCountsV1) -> RecoveryHeaderV1 {
        RecoveryHeaderV1 {
            format_version: 1,
            schema_version: 1,
            source_group_id: vec![7u8; 32],
            source_genesis_hash: vec![0x11u8; 32],
            source_authority_public_key: nid(3),
            source_authority_domain: "aster0.net".into(),
            source_last_applied: None,
            source_membership: StoredMembershipV1::default(),
            source_audit_floor: None,
            source_audit_tip: None,
            source_admin_revision: 1,
            row_counts: counts,
        }
    }

    /// A complete snapshot body: every legal section, exactly once.
    fn full_snapshot() -> Vec<u8> {
        let c = RowCountsV1 {
            resources: 1,
            grants: 1,
            admins: 2,
            sessions: 0,
            intents: 0,
            routes: 0,
            audit: 2,
        };
        let mut w = BodyWriter::<_, SnapshotBodyRowV1>::new(Vec::new(), &header_for(c)).unwrap();
        w.begin_section(SECTION_RESOURCES).unwrap();
        w.row(&SnapshotBodyRowV1::Resource(snap_resource(1)))
            .unwrap();
        w.begin_section(SECTION_GRANTS).unwrap();
        w.row(&SnapshotBodyRowV1::Grant(GrantRowV1 {
            key: key("p", b"k"),
            subject_node_id: nid(2),
            rights: 0x3f,
        }))
        .unwrap();
        w.begin_section(SECTION_ADMINS).unwrap();
        w.row(&admin(1)).unwrap();
        w.row(&admin(2)).unwrap();
        w.begin_section(SECTION_SESSIONS).unwrap();
        w.begin_section(SECTION_INTENTS).unwrap();
        w.begin_section(SECTION_ROUTES).unwrap();
        w.begin_section(SECTION_AUDIT).unwrap();
        w.row(&audit(1, 1)).unwrap();
        w.row(&audit(1, 2)).unwrap();
        w.finish().unwrap()
    }

    fn session_row(n: u8) -> SnapshotBodyRowV1 {
        SnapshotBodyRowV1::Session(SessionRowV1 {
            actor_node_id: nid(n),
            client_id: vec![n; 16],
            session: ClientSessionV1 {
                last_sequence: 5,
                request_hash: vec![0xAAu8; 32],
                last_result: None,
                retired: false,
            },
        })
    }

    fn intent_row(n: u8) -> SnapshotBodyRowV1 {
        use crate::coordination::contract::ChangeMembershipActionV1;

        SnapshotBodyRowV1::Intent(IntentRowV1 {
            operation_id: vec![n; 16],
            intent: MembershipIntentV1 {
                actor_node_id: nid(n),
                request_hash: vec![0xBBu8; 32],
                expected_membership: None,
                desired_plan: ChangeMembershipActionV1::default(),
                status_code: 1,
                result_code: 0,
                accepted_log_id: LogIdV1 {
                    term: 1,
                    leader_node_id: nid(1),
                    index: 3,
                },
                completed_log_id: None,
                observed_membership_log_id: None,
            },
        })
    }

    /// Sessions and intents carry the §11 storage roots themselves, and both
    /// survive the body whole. Nothing exercised these two sections before, so
    /// the row types were declarations rather than a format.
    #[test]
    fn session_and_intent_rows_carry_their_storage_roots() {
        let c = RowCountsV1 {
            resources: 0,
            grants: 0,
            admins: 1,
            sessions: 2,
            intents: 1,
            routes: 0,
            audit: 0,
        };
        let mut w = BodyWriter::<_, SnapshotBodyRowV1>::new(Vec::new(), &header_for(c)).unwrap();
        w.begin_section(SECTION_RESOURCES).unwrap();
        w.begin_section(SECTION_GRANTS).unwrap();
        w.begin_section(SECTION_ADMINS).unwrap();
        w.row(&admin(1)).unwrap();
        w.begin_section(SECTION_SESSIONS).unwrap();
        w.row(&session_row(1)).unwrap();
        w.row(&session_row(2)).unwrap();
        w.begin_section(SECTION_INTENTS).unwrap();
        w.row(&intent_row(4)).unwrap();
        w.begin_section(SECTION_ROUTES).unwrap();
        w.begin_section(SECTION_AUDIT).unwrap();
        let body = w.finish().unwrap();

        #[derive(Default)]
        struct Keep {
            rows: Vec<SnapshotBodyRowV1>,
        }
        impl BodyVisitor<SnapshotBodyRowV1> for Keep {
            fn header(&mut self, _h: &SnapshotHeaderV1) -> Result<(), BodyError> {
                Ok(())
            }
            fn row(&mut self, _s: u64, r: SnapshotBodyRowV1) -> Result<(), BodyError> {
                self.rows.push(r);
                Ok(())
            }
        }

        let mut keep = Keep::default();
        read_body::<_, SnapshotBodyRowV1>(&mut body.as_slice(), &mut keep).unwrap();
        assert_eq!(keep.rows.len(), 4);
        assert_eq!(keep.rows[1], session_row(1), "the session root survives");
        assert_eq!(keep.rows[3], intent_row(4), "the intent root survives");
    }

    /// **The audit row §11.3 now requires for a malformed entry must survive a
    /// snapshot.** An earlier revision encoded an absent actor as an empty
    /// vector while this validator still demanded exactly 32 bytes, so the row
    /// could be written into state and then neither exported nor installed.
    #[test]
    fn an_audit_row_with_no_actor_survives_a_snapshot_body() {
        let counts = RowCountsV1 {
            resources: 0,
            grants: 0,
            admins: 1,
            sessions: 0,
            intents: 0,
            routes: 0,
            audit: 2,
        };
        let absent = SnapshotBodyRowV1::Audit(AuditStreamRowV1 {
            log_term: 1,
            log_index: 5,
            row: AuditRowV1 {
                leader_node_id: nid(1),
                actor_node_id: None,
                operation_code: 1,
                resource: None,
                result_code: 100,
                epoch: 0,
                value_revision: 0,
            },
        });
        let present = SnapshotBodyRowV1::Audit(AuditStreamRowV1 {
            log_term: 1,
            log_index: 6,
            row: AuditRowV1 {
                leader_node_id: nid(1),
                actor_node_id: Some(nid(5)),
                operation_code: 1,
                resource: None,
                result_code: 100,
                epoch: 0,
                value_revision: 0,
            },
        });

        let mut w =
            BodyWriter::<_, SnapshotBodyRowV1>::new(Vec::new(), &header_for(counts)).unwrap();
        w.begin_section(SECTION_RESOURCES).unwrap();
        w.begin_section(SECTION_GRANTS).unwrap();
        w.begin_section(SECTION_ADMINS).unwrap();
        w.row(&admin(1)).unwrap();
        w.begin_section(SECTION_SESSIONS).unwrap();
        w.begin_section(SECTION_INTENTS).unwrap();
        w.begin_section(SECTION_ROUTES).unwrap();
        w.begin_section(SECTION_AUDIT).unwrap();
        w.row(&absent).expect("an absent actor is writable");
        w.row(&present).unwrap();
        let body = w.finish().expect("a complete body");

        #[derive(Default)]
        struct Keep {
            rows: Vec<SnapshotBodyRowV1>,
        }
        impl BodyVisitor<SnapshotBodyRowV1> for Keep {
            fn header(&mut self, _h: &SnapshotHeaderV1) -> Result<(), BodyError> {
                Ok(())
            }
            fn row(&mut self, _s: u64, r: SnapshotBodyRowV1) -> Result<(), BodyError> {
                self.rows.push(r);
                Ok(())
            }
        }
        let mut keep = Keep::default();
        read_body::<_, SnapshotBodyRowV1>(&mut body.as_slice(), &mut keep)
            .expect("an absent actor is installable");
        assert_eq!(keep.rows[1], absent, "absence survives, unchanged");
        assert_eq!(keep.rows[2], present);
    }

    /// `None` is absence; a wrong width is still corruption. There is no
    /// empty-vector sentinel anywhere in the chain.
    #[test]
    fn a_wrong_width_actor_is_still_rejected_by_snapshot_validation() {
        let row = |actor: Option<Vec<u8>>| {
            SnapshotBodyRowV1::Audit(AuditStreamRowV1 {
                log_term: 1,
                log_index: 5,
                row: AuditRowV1 {
                    leader_node_id: nid(1),
                    actor_node_id: actor,
                    operation_code: 1,
                    resource: None,
                    result_code: 100,
                    epoch: 0,
                    value_revision: 0,
                },
            })
        };
        assert!(validate_snapshot_row(&row(None)).is_ok());
        assert!(validate_snapshot_row(&row(Some(nid(5)))).is_ok());
        assert!(validate_snapshot_row(&row(Some(vec![0u8; 8]))).is_err());
        assert!(
            validate_snapshot_row(&row(Some(Vec::new()))).is_err(),
            "an empty vector is a width failure, not an absence sentinel"
        );
    }

    /// Both header roots round-trip through Fory with every field intact. The
    /// recovery header's ten source fields are the ones a single shared header
    /// type could not express at all.
    #[test]
    fn both_header_roots_round_trip() {
        let snap = header_for(RowCountsV1::default());
        assert_eq!(
            SnapshotHeaderV1::decode(&snap.encode().unwrap()).unwrap(),
            snap
        );
        assert_eq!(snap.meta.authority_domain, "aster0.net");

        let rec = recovery_header_for(RowCountsV1::default());
        let back = RecoveryHeaderV1::decode(&rec.encode().unwrap()).unwrap();
        assert_eq!(back, rec);
        assert_eq!(back.source_authority_public_key, nid(3));
        assert_eq!(back.source_genesis_hash, vec![0x11u8; 32]);
    }

    /// The two headers are different roots, not two spellings of one. A
    /// recovery body opened as a snapshot must fail rather than decode into a
    /// header with empty provenance.
    #[test]
    fn a_recovery_header_is_not_readable_as_a_snapshot_header() {
        let rec = recovery_header_for(RowCountsV1 {
            resources: 0,
            grants: 0,
            admins: 0,
            ..Default::default()
        });
        let encoded = rec.encode().unwrap();
        assert!(
            SnapshotHeaderV1::decode(&encoded).is_err(),
            "distinct roots must not silently interchange"
        );
    }

    #[test]
    fn full_snapshot_body_round_trips() {
        let body = full_snapshot();
        let mut v = Counting::default();
        read_body::<_, SnapshotBodyRowV1>(&mut body.as_slice(), &mut v).unwrap();
        assert_eq!(v.rows, 6);
    }

    /// THE ordering test: component-wise equivalence with redb tuple order,
    /// not merely absence of collisions. Length-prefixed byte keys got this
    /// backwards -- under `len || bytes`, "b" sorts before "aa".
    #[test]
    fn order_key_matches_tuple_ordering() {
        // Adversarial pairs: differing lengths, prefixes, and empty parts.
        let cases: &[(&str, &[u8], &str, &[u8])] = &[
            ("b", b"x", "aa", b"x"),    // shorter-but-greater first component
            ("a", b"bc", "ab", b"c"),   // the old flattening collision
            ("a", b"b", "a", b"bb"),    // prefix in second component
            ("aa", b"", "aa", b"\x00"), // empty vs one zero byte
            ("z", b"", "za", b""),      // prefix in first component
        ];
        for (n1, k1, n2, k2) in cases {
            let a = resource_key_parts(&key(n1, k1));
            let b = resource_key_parts(&key(n2, k2));
            // The reference: redb compares tuples component-wise, bytewise.
            let want =
                (n1.as_bytes().to_vec(), k1.to_vec()).cmp(&(n2.as_bytes().to_vec(), k2.to_vec()));
            assert_eq!(
                a.cmp(&b),
                want,
                "mismatch for ({n1:?},{k1:?}) vs ({n2:?},{k2:?})"
            );
        }
        // And the specific inversion the previous encoding produced.
        assert!(resource_key_parts(&key("aa", b"x")) < resource_key_parts(&key("b", b"x")));
    }

    /// A trailing partial varint must not be accepted as clean EOF.
    #[test]
    fn truncated_suffix_is_not_clean_eof() {
        let mut body = full_snapshot();
        assert!(
            read_body::<_, SnapshotBodyRowV1>(&mut body.as_slice(), &mut Counting::default())
                .is_ok()
        );
        body.push(0x80); // a varint that never terminates
        assert_eq!(
            read_body::<_, SnapshotBodyRowV1>(&mut body.as_slice(), &mut Counting::default()),
            Err(BodyError::Truncated)
        );
    }

    /// A partial profile is refused even though the caller can no longer ask
    /// for a matching partial expectation.
    #[test]
    fn incomplete_section_profile_is_refused() {
        let c = RowCountsV1 {
            admins: 1,
            ..Default::default()
        };
        let mut w = BodyWriter::<_, SnapshotBodyRowV1>::new(Vec::new(), &header_for(c)).unwrap();
        w.begin_section(SECTION_ADMINS).unwrap();
        w.row(&admin(1)).unwrap();
        assert!(matches!(
            w.finish(),
            Err(BodyError::IncompleteProfile { .. })
        ));
    }

    /// The header's promised count must equal what the section declares.
    #[test]
    fn header_counts_are_compared() {
        let body = full_snapshot();
        // Re-encode a header claiming a different admin count.
        let c = RowCountsV1 {
            resources: 1,
            grants: 1,
            admins: 99,
            audit: 2,
            ..Default::default()
        };
        let hf = payload_fory::<SnapshotHeaderV1>();
        let bad_hdr = hf.serialize(&header_for(c)).unwrap();
        let mut out = Vec::new();
        write_varint(&mut out, bad_hdr.len() as u64);
        out.extend_from_slice(&bad_hdr);
        // Splice the original body's sections after the tampered header.
        let mut orig = body.as_slice();
        let olen = read_varint(&mut orig).unwrap();
        let mut skip = vec![0u8; olen as usize];
        std::io::Read::read_exact(&mut orig, &mut skip).unwrap();
        out.extend_from_slice(orig);

        assert!(matches!(
            read_body::<_, SnapshotBodyRowV1>(&mut out.as_slice(), &mut Counting::default()),
            Err(BodyError::HeaderCountMismatch { .. })
        ));
    }

    /// THE two-root test that matters: both roots use variant/section 1 for
    /// different resource shapes, so decoding must reject rather than
    /// compatibly drop the holder fields.
    #[test]
    fn recovery_root_rejects_a_holder_bearing_snapshot_row() {
        let snap_f = payload_fory::<SnapshotBodyRowV1>();
        let enc = snap_f
            .serialize(&SnapshotBodyRowV1::Resource(snap_resource(1)))
            .expect("encode snapshot row");

        let rec_f = payload_fory::<RecoveryBodyRowV1>();
        match rec_f.deserialize::<RecoveryBodyRowV1>(&enc) {
            Err(_) => {} // rejected outright: good
            Ok(decoded) => {
                // If it decodes at all it must NOT silently become a valid
                // recovery resource row carrying dropped authority.
                panic!(
                    "recovery root accepted a snapshot resource row: {decoded:?} -- \
                     holder/held would have been silently dropped"
                );
            }
        }
    }

    #[test]
    fn recovery_profile_excludes_authority_sections() {
        assert_eq!(RecoveryBodyRowV1::legal_sections(), RECOVERY_SECTIONS);
        for s in [
            SECTION_SESSIONS,
            SECTION_INTENTS,
            SECTION_ROUTES,
            SECTION_AUDIT,
        ] {
            assert!(!RecoveryBodyRowV1::legal_sections().contains(&s));
        }
        // A full snapshot body is not readable as a recovery body.
        let body = full_snapshot();
        assert!(matches!(
            read_body::<_, RecoveryBodyRowV1>(&mut body.as_slice(), &mut Counting::default()),
            Err(BodyError::IllegalSection(_)) | Err(BodyError::Decode)
        ));
    }

    #[test]
    fn recovery_body_round_trips() {
        let c = RowCountsV1 {
            resources: 1,
            grants: 0,
            admins: 1,
            ..Default::default()
        };
        let mut w =
            BodyWriter::<_, RecoveryBodyRowV1>::new(Vec::new(), &recovery_header_for(c)).unwrap();
        w.begin_section(SECTION_RESOURCES).unwrap();
        w.row(&RecoveryBodyRowV1::Resource(rec_resource(1)))
            .unwrap();
        w.begin_section(SECTION_GRANTS).unwrap();
        w.begin_section(SECTION_ADMINS).unwrap();
        w.row(&r_admin(1)).unwrap();
        let body = w.finish().unwrap();

        let mut v = Counting::default();
        read_body::<_, RecoveryBodyRowV1>(&mut body.as_slice(), &mut v).unwrap();
        assert_eq!(v.rows, 2);
    }

    /// Logical widths, not just encoded byte ceilings: a short NodeId fits any
    /// ceiling but is still invalid.
    #[test]
    fn decoded_rows_are_logically_validated() {
        assert!(matches!(
            validate_snapshot_row(&SnapshotBodyRowV1::Admin(AdminRowV1 {
                subject_node_id: vec![1, 2, 3],
            })),
            Err(BodyError::InvalidRow(_))
        ));
        assert!(matches!(
            validate_snapshot_row(&SnapshotBodyRowV1::Grant(GrantRowV1 {
                key: key("p", b"k"),
                subject_node_id: nid(1),
                rights: 0x80, // unknown bit
            })),
            Err(BodyError::InvalidRow(_))
        ));
        let mut held_no_holder = snap_resource(1);
        held_no_holder.holder_node_id = None;
        held_no_holder.held = true;
        assert!(matches!(
            validate_snapshot_row(&SnapshotBodyRowV1::Resource(held_no_holder)),
            Err(BodyError::InvalidRow(_))
        ));
    }

    /// The writer enforces the same rules, and returns errors rather than
    /// panicking.
    /// The writer refuses a logically invalid row rather than emitting a body
    /// only its own reader would reject.
    #[test]
    fn writer_validates_rows() {
        let c = RowCountsV1 {
            admins: 1,
            ..Default::default()
        };
        let mut w = BodyWriter::<_, SnapshotBodyRowV1>::new(Vec::new(), &header_for(c)).unwrap();
        w.begin_section(SECTION_ADMINS).unwrap();
        assert!(matches!(
            w.row(&SnapshotBodyRowV1::Admin(AdminRowV1 {
                subject_node_id: vec![1, 2, 3], // not 32 bytes
            })),
            Err(BodyError::InvalidRow(_))
        ));
    }

    #[test]
    fn writer_rejects_wrong_section_and_bad_order() {
        let c = RowCountsV1 {
            admins: 2,
            ..Default::default()
        };
        let mut w = BodyWriter::<_, SnapshotBodyRowV1>::new(Vec::new(), &header_for(c)).unwrap();
        w.begin_section(SECTION_ADMINS).unwrap();
        assert!(matches!(
            w.row(&audit(1, 1)),
            Err(BodyError::WrongSectionForRow { .. })
        ));
        w.row(&admin(5)).unwrap();
        assert_eq!(w.row(&admin(2)), Err(BodyError::RowsOutOfOrder));
        assert!(matches!(w.row(&admin(9)), Ok(())));
        assert!(matches!(
            w.row(&admin(10)),
            Err(BodyError::HeaderCountMismatch { .. })
        ));
    }

    #[test]
    fn overlong_varint_is_rejected() {
        let mut over = vec![0x80u8; 9];
        over.push(0x02);
        assert_eq!(
            read_varint(&mut over.as_slice()),
            Err(BodyError::VarintOverflow)
        );
        let mut ok = vec![0x80u8; 9];
        ok.push(0x01);
        assert!(read_varint(&mut ok.as_slice()).is_ok());
        assert_eq!(
            read_varint(&mut [0x81u8, 0x00].as_slice()),
            Err(BodyError::NonMinimalVarint)
        );
    }

    #[test]
    fn per_section_row_byte_ceilings_apply() {
        assert_eq!(limits(SECTION_ADMINS).unwrap().max_row_bytes, 256);
        assert_eq!(limits(SECTION_RESOURCES).unwrap().max_row_bytes, 128 * 1024);
    }

    #[test]
    fn different_insertion_orders_produce_identical_bytes() {
        // An earlier version of this test built both byte streams and compared
        // neither: its closure discarded the writer's output, so the only real
        // assertion was `full_snapshot() == full_snapshot()` — the same helper,
        // twice, in the same order. This one returns the bytes.
        let build = |mut rows: Vec<SnapshotBodyRowV1>| -> Vec<u8> {
            rows.sort_by_key(|r| r.order_key());
            let c = RowCountsV1 {
                admins: rows.len() as u64,
                ..Default::default()
            };
            let mut w =
                BodyWriter::<_, SnapshotBodyRowV1>::new(Vec::new(), &header_for(c)).unwrap();
            for section in SNAPSHOT_SECTIONS {
                w.begin_section(*section).unwrap();
                if *section == SECTION_ADMINS {
                    for r in &rows {
                        w.row(r).unwrap();
                    }
                }
            }
            w.finish().unwrap()
        };
        let forward = vec![admin(1), admin(2), admin(3)];
        let reverse = vec![admin(3), admin(1), admin(2)];
        assert_ne!(forward, reverse);
        assert_eq!(build(forward), build(reverse));
    }

    /// The same property for the root whose bytes a replacement genesis binds:
    /// two writers fed the same recovery rows in different insertion orders
    /// emit byte-identical bodies, and therefore one BLAKE3.
    #[test]
    fn recovery_bodies_are_byte_deterministic_across_insertion_orders() {
        let build = |mut rows: Vec<RecoveryBodyRowV1>| -> Vec<u8> {
            rows.sort_by_key(|r| r.order_key());
            let (mut nr, mut ng, mut na) = (0u64, 0u64, 0u64);
            for r in &rows {
                match r.section().unwrap() {
                    SECTION_RESOURCES => nr += 1,
                    SECTION_GRANTS => ng += 1,
                    _ => na += 1,
                }
            }
            let c = RowCountsV1 {
                resources: nr,
                grants: ng,
                admins: na,
                ..Default::default()
            };
            let mut w =
                BodyWriter::<_, RecoveryBodyRowV1>::new(Vec::new(), &recovery_header_for(c))
                    .unwrap();
            for section in RECOVERY_SECTIONS {
                w.begin_section(*section).unwrap();
                for r in rows.iter().filter(|r| r.section() == Some(*section)) {
                    w.row(r).unwrap();
                }
            }
            w.finish().unwrap()
        };
        let forward = vec![
            RecoveryBodyRowV1::Resource(rec_resource(1)),
            RecoveryBodyRowV1::Resource(rec_resource(2)),
            r_admin(1),
            r_admin(2),
        ];
        let reverse = vec![
            r_admin(2),
            RecoveryBodyRowV1::Resource(rec_resource(2)),
            r_admin(1),
            RecoveryBodyRowV1::Resource(rec_resource(1)),
        ];
        let a = build(forward);
        assert_eq!(a, build(reverse));
        // And one hash: this equality is what lets the manifest checksum and
        // the genesis binding name "the" export rather than "an" encoding.
        assert_eq!(
            blake3::hash(&a),
            blake3::hash(&build(vec![
                RecoveryBodyRowV1::Resource(rec_resource(2)),
                r_admin(1),
                r_admin(2),
                RecoveryBodyRowV1::Resource(rec_resource(1)),
            ]))
        );
    }

    #[test]
    fn stored_and_streamed_audit_rows_agree() {
        let SnapshotBodyRowV1::Audit(streamed) = audit(7, 42) else {
            panic!()
        };
        assert_eq!(streamed.log_id(), streamed.row.log_id((7, 42)));
        assert_eq!(streamed.log_id().term, 7);
    }

    // ── S5-A: recovery-format freeze evidence ────────────────────────────────

    /// An empty-but-complete recovery body under the given header, built by
    /// hand so tests can splice arbitrary section framing after it.
    fn manual_recovery_bytes(header: &RecoveryHeaderV1, sections: &[(u64, u64)]) -> Vec<u8> {
        let enc = BodyHeader::encode(header).unwrap();
        let mut out = Vec::new();
        write_varint(&mut out, enc.len() as u64);
        out.extend_from_slice(&enc);
        for (id, count) in sections {
            write_varint(&mut out, *id);
            write_varint(&mut out, *count);
        }
        out
    }

    fn read_recovery(bytes: &[u8]) -> Result<(), BodyError> {
        let mut c = Counting::default();
        read_body::<_, RecoveryBodyRowV1>(&mut &bytes[..], &mut c)
    }

    /// The recovery header is validated by both ends of the framing: an
    /// unsupported *newer* version is refused with a diagnostic naming the
    /// field and both values — never skipped — and the widths/counts the row
    /// profile cannot check are checked here.
    #[test]
    fn a_recovery_header_is_validated_by_both_ends() {
        let ok = recovery_header_for(RowCountsV1::default());
        assert!(BodyWriter::<Vec<u8>, RecoveryBodyRowV1>::new(Vec::new(), &ok).is_ok());

        let cases: Vec<(RecoveryHeaderV1, BodyError)> = vec![
            (
                RecoveryHeaderV1 {
                    format_version: 2,
                    ..recovery_header_for(RowCountsV1::default())
                },
                BodyError::UnsupportedVersion {
                    field: "format_version",
                    declared: 2,
                    supported: RECOVERY_FORMAT_VERSION,
                },
            ),
            (
                RecoveryHeaderV1 {
                    schema_version: 9,
                    ..recovery_header_for(RowCountsV1::default())
                },
                BodyError::UnsupportedVersion {
                    field: "schema_version",
                    declared: 9,
                    supported: RECOVERY_SCHEMA_VERSION,
                },
            ),
            (
                RecoveryHeaderV1 {
                    source_group_id: vec![7u8; 16],
                    ..recovery_header_for(RowCountsV1::default())
                },
                BodyError::InvalidHeader("source group id must be 32 bytes"),
            ),
            (
                RecoveryHeaderV1 {
                    source_genesis_hash: vec![0x11u8; 31],
                    ..recovery_header_for(RowCountsV1::default())
                },
                BodyError::InvalidHeader("source genesis hash must be 32 bytes"),
            ),
            (
                RecoveryHeaderV1 {
                    source_authority_public_key: Vec::new(),
                    ..recovery_header_for(RowCountsV1::default())
                },
                BodyError::InvalidHeader("source authority public key must be 32 bytes"),
            ),
            (
                RecoveryHeaderV1 {
                    source_authority_domain: String::new(),
                    ..recovery_header_for(RowCountsV1::default())
                },
                BodyError::InvalidHeader("source authority domain width"),
            ),
            (
                RecoveryHeaderV1 {
                    source_authority_domain: "a".repeat(129),
                    ..recovery_header_for(RowCountsV1::default())
                },
                BodyError::InvalidHeader("source authority domain width"),
            ),
            (
                recovery_header_for(RowCountsV1 {
                    sessions: 1,
                    ..Default::default()
                }),
                BodyError::InvalidHeader(
                    "a recovery export declares no sessions, intents, routes, or audit rows",
                ),
            ),
            (
                recovery_header_for(RowCountsV1 {
                    audit: 7,
                    ..Default::default()
                }),
                BodyError::InvalidHeader(
                    "a recovery export declares no sessions, intents, routes, or audit rows",
                ),
            ),
        ];
        for (header, expect) in cases {
            // Writer side.
            let got = BodyWriter::<Vec<u8>, RecoveryBodyRowV1>::new(Vec::new(), &header)
                .err()
                .expect("the writer must refuse the header");
            assert_eq!(got, expect);
            // Reader side, through the real decoder: the header still encodes
            // (the shape is legal Fory), so the refusal is semantic, not a
            // parse accident.
            let bytes = manual_recovery_bytes(&header, &[]);
            assert_eq!(read_recovery(&bytes).unwrap_err(), expect);
        }
    }

    /// Section ids must strictly ascend in both directions; a repeated or
    /// descending section is `SectionOutOfOrder`, not a silent merge.
    #[test]
    fn section_order_is_enforced_in_both_directions() {
        // Writer: grants then resources.
        let h = recovery_header_for(RowCountsV1::default());
        let mut w = BodyWriter::<Vec<u8>, RecoveryBodyRowV1>::new(Vec::new(), &h).unwrap();
        w.begin_section(SECTION_GRANTS).unwrap();
        assert_eq!(
            w.begin_section(SECTION_RESOURCES).unwrap_err(),
            BodyError::SectionOutOfOrder {
                got: SECTION_RESOURCES,
                previous: SECTION_GRANTS
            }
        );
        // Writer: the same section twice.
        let mut w = BodyWriter::<Vec<u8>, RecoveryBodyRowV1>::new(
            Vec::new(),
            &recovery_header_for(RowCountsV1::default()),
        )
        .unwrap();
        w.begin_section(SECTION_ADMINS).unwrap();
        assert_eq!(
            w.begin_section(SECTION_ADMINS).unwrap_err(),
            BodyError::SectionOutOfOrder {
                got: SECTION_ADMINS,
                previous: SECTION_ADMINS
            }
        );
        // Reader: section 2 then section 1.
        let bytes = manual_recovery_bytes(
            &recovery_header_for(RowCountsV1::default()),
            &[(SECTION_GRANTS, 0), (SECTION_RESOURCES, 0)],
        );
        assert_eq!(
            read_recovery(&bytes).unwrap_err(),
            BodyError::SectionOutOfOrder {
                got: SECTION_RESOURCES,
                previous: SECTION_GRANTS
            }
        );
    }

    /// A recovery body naming an authority-bearing section is refused by
    /// section id alone — pinned as `IllegalSection`, complementing the
    /// whole-snapshot cross-decode test whose refusal may fire earlier at row
    /// decode.
    #[test]
    fn a_recovery_reader_pins_the_illegal_section() {
        let bytes = manual_recovery_bytes(
            &recovery_header_for(RowCountsV1::default()),
            &[
                (SECTION_RESOURCES, 0),
                (SECTION_GRANTS, 0),
                (SECTION_ADMINS, 0),
                (SECTION_SESSIONS, 0),
            ],
        );
        assert_eq!(
            read_recovery(&bytes).unwrap_err(),
            BodyError::IllegalSection(SECTION_SESSIONS)
        );
    }

    /// Declared lengths are checked before any allocation or read from them:
    /// each stream below *ends* immediately after the hostile declaration, so
    /// reaching for the promised bytes would report `Truncated` — the specific
    /// error asserted proves the bound fired first.
    #[test]
    fn preallocation_bounds_fire_before_any_read() {
        // Header length over MAX_HEADER_BYTES.
        let mut bytes = Vec::new();
        write_varint(&mut bytes, MAX_HEADER_BYTES + 1);
        assert_eq!(
            read_recovery(&bytes).unwrap_err(),
            BodyError::HeaderTooLarge {
                declared: MAX_HEADER_BYTES + 1
            }
        );

        // Row count over the section cap, declared in the section framing.
        // The header must agree (its counts are compared later), so it is
        // built with the same hostile count — which the header validation
        // does not cap-check, the section framing does.
        let over = limits(SECTION_ADMINS).unwrap().max_rows + 1;
        let bytes = manual_recovery_bytes(
            &recovery_header_for(RowCountsV1 {
                admins: over,
                ..Default::default()
            }),
            &[
                (SECTION_RESOURCES, 0),
                (SECTION_GRANTS, 0),
                (SECTION_ADMINS, over),
            ],
        );
        assert_eq!(
            read_recovery(&bytes).unwrap_err(),
            BodyError::RowCountTooLarge {
                section: SECTION_ADMINS,
                declared: over,
                max: limits(SECTION_ADMINS).unwrap().max_rows,
            }
        );

        // Writer side of the same bound.
        let header = recovery_header_for(RowCountsV1 {
            admins: over,
            ..Default::default()
        });
        let mut w = BodyWriter::<Vec<u8>, RecoveryBodyRowV1>::new(Vec::new(), &header).unwrap();
        w.begin_section(SECTION_RESOURCES).unwrap();
        w.begin_section(SECTION_GRANTS).unwrap();
        assert_eq!(
            w.begin_section(SECTION_ADMINS).unwrap_err(),
            BodyError::RowCountTooLarge {
                section: SECTION_ADMINS,
                declared: over,
                max: limits(SECTION_ADMINS).unwrap().max_rows,
            }
        );

        // Row length over the section's byte ceiling. Writer-side is
        // unreachable for a validated row — §12.1 ceilings admit every
        // maximal legal row and validation rejects wider ones first — so the
        // reader-side declaration is the live edge.
        let ceiling = limits(SECTION_ADMINS).unwrap().max_row_bytes;
        let mut bytes = manual_recovery_bytes(
            &recovery_header_for(RowCountsV1 {
                admins: 1,
                ..Default::default()
            }),
            &[
                (SECTION_RESOURCES, 0),
                (SECTION_GRANTS, 0),
                (SECTION_ADMINS, 1),
            ],
        );
        write_varint(&mut bytes, ceiling + 1);
        assert_eq!(
            read_recovery(&bytes).unwrap_err(),
            BodyError::RowTooLarge {
                section: SECTION_ADMINS,
                declared: ceiling + 1,
                max: ceiling,
            }
        );
    }

    /// The recovery root refuses the same framing corruption the snapshot
    /// tests pin: a trailing partial varint is `Truncated`, not a clean EOF,
    /// and a non-minimal LEB128 is refused even where the value would be
    /// legal.
    #[test]
    fn recovery_framing_rejects_truncation_and_non_minimal_varints() {
        let good = {
            let mut w = BodyWriter::<_, RecoveryBodyRowV1>::new(
                Vec::new(),
                &recovery_header_for(RowCountsV1 {
                    admins: 1,
                    ..Default::default()
                }),
            )
            .unwrap();
            w.begin_section(SECTION_RESOURCES).unwrap();
            w.begin_section(SECTION_GRANTS).unwrap();
            w.begin_section(SECTION_ADMINS).unwrap();
            w.row(&r_admin(1)).unwrap();
            w.finish().unwrap()
        };
        assert!(read_recovery(&good).is_ok());

        // A trailing partial varint after a valid body.
        let mut truncated = good.clone();
        truncated.push(0x80);
        assert_eq!(read_recovery(&truncated).unwrap_err(), BodyError::Truncated);

        // Cut mid-row.
        let cut = &good[..good.len() - 1];
        assert_eq!(read_recovery(cut).unwrap_err(), BodyError::Truncated);

        // A non-minimal section id where a section id is expected: the body
        // is complete through section 3, then declares `[0x81, 0x00]` — a
        // two-byte encoding of 1.
        let mut nonminimal = good.clone();
        nonminimal.extend_from_slice(&[0x81, 0x00]);
        assert_eq!(
            read_recovery(&nonminimal).unwrap_err(),
            BodyError::NonMinimalVarint
        );
    }

    /// Recovery rows are logically validated on both ends — widths and rights
    /// bits the byte ceiling does not imply, for the root whose bytes a
    /// genesis binds. The snapshot-row test cannot stand in for this: the two
    /// roots have separate validators.
    #[test]
    fn recovery_rows_are_logically_validated() {
        let cases: Vec<(RecoveryBodyRowV1, &str)> = vec![
            (
                RecoveryBodyRowV1::Resource(RecoveryResourceRowV1 {
                    value: vec![0u8; MAX_VALUE + 1],
                    ..rec_resource(1)
                }),
                "value width",
            ),
            (
                RecoveryBodyRowV1::Grant(GrantRowV1 {
                    key: key("p", b"k"),
                    subject_node_id: nid(2),
                    rights: 0x40,
                }),
                "unknown rights bits",
            ),
            (
                RecoveryBodyRowV1::Grant(GrantRowV1 {
                    key: key("p", b"k"),
                    subject_node_id: vec![2u8; 4],
                    rights: 1,
                }),
                "node id must be 32 bytes",
            ),
            (
                RecoveryBodyRowV1::Admin(AdminRowV1 {
                    subject_node_id: vec![1u8; 31],
                }),
                "node id must be 32 bytes",
            ),
        ];
        for (row, why) in cases {
            assert_eq!(
                validate_recovery_row(&row).unwrap_err(),
                BodyError::InvalidRow(why)
            );
            // Writer side refuses to emit it.
            let section = row.section().unwrap();
            let mut counts = RowCountsV1::default();
            counts.set(section, 1);
            let mut w = BodyWriter::<Vec<u8>, RecoveryBodyRowV1>::new(
                Vec::new(),
                &recovery_header_for(counts),
            )
            .unwrap();
            for s in RECOVERY_SECTIONS {
                if *s > section {
                    break;
                }
                w.begin_section(*s).unwrap();
            }
            assert_eq!(w.row(&row).unwrap_err(), BodyError::InvalidRow(why));
            // Reader side refuses the same row arriving in raw bytes.
            let mut bytes = manual_recovery_bytes(
                &recovery_header_for({
                    let mut c = RowCountsV1::default();
                    c.set(section, 1);
                    c
                }),
                &RECOVERY_SECTIONS
                    .iter()
                    .filter(|s| **s <= section)
                    .map(|s| (*s, if *s == section { 1 } else { 0 }))
                    .collect::<Vec<_>>(),
            );
            let enc = RecoveryBodyRowV1::encode(&RecoveryBodyRowV1::fory(), &row).unwrap();
            write_varint(&mut bytes, enc.len() as u64);
            bytes.extend_from_slice(&enc);
            assert_eq!(
                read_recovery(&bytes).unwrap_err(),
                BodyError::InvalidRow(why)
            );
        }
    }

    /// Fory tolerates trailing bytes; the framing must not.
    ///
    /// Two stages per case, deliberately. First prove Fory **alone** accepts
    /// the appended byte and still yields the original value — if a Fory
    /// upgrade ever makes it strict, this precondition fails loudly and the
    /// canonicality guard is known to be redundant rather than silently
    /// double-checked. Then prove `read_body` refuses the same bytes once the
    /// inner length is adjusted, with the distinct noncanonical error — not
    /// `Decode`, because the bytes *do* decode; that is the whole problem. A
    /// new genesis could legitimately bind the BLAKE3 of such a mutated body,
    /// so the checksum cannot repair this; only the reader can.
    #[test]
    fn noncanonical_inner_fory_bytes_are_refused() {
        // ── Header ──
        let header = recovery_header_for(RowCountsV1::default());
        let mut enc = BodyHeader::encode(&header).unwrap();
        enc.push(0x00);
        let tolerated = <RecoveryHeaderV1 as BodyHeader>::decode(&enc)
            .expect("precondition: Fory alone tolerates the trailing byte");
        assert_eq!(
            tolerated, header,
            "precondition: the padded bytes decode to the same value"
        );
        let mut bytes = Vec::new();
        write_varint(&mut bytes, enc.len() as u64);
        bytes.extend_from_slice(&enc);
        for id in RECOVERY_SECTIONS {
            write_varint(&mut bytes, *id);
            write_varint(&mut bytes, 0);
        }
        assert_eq!(
            read_recovery(&bytes).unwrap_err(),
            BodyError::NoncanonicalHeader
        );

        // ── Every row variant ──
        let fory = RecoveryBodyRowV1::fory();
        let rows = vec![
            RecoveryBodyRowV1::Resource(rec_resource(1)),
            RecoveryBodyRowV1::Grant(GrantRowV1 {
                key: key("p", b"k"),
                subject_node_id: nid(2),
                rights: 0x3f,
            }),
            r_admin(1),
        ];
        for row in rows {
            let section = row.section().unwrap();
            let mut enc = RecoveryBodyRowV1::encode(&fory, &row).unwrap();
            enc.push(0x00);
            let tolerated = RecoveryBodyRowV1::decode(&fory, &enc)
                .expect("precondition: Fory alone tolerates the trailing byte");
            assert_eq!(tolerated, row);

            let mut counts = RowCountsV1::default();
            counts.set(section, 1);
            let mut bytes = manual_recovery_bytes(
                &recovery_header_for(counts),
                &RECOVERY_SECTIONS
                    .iter()
                    .filter(|s| **s <= section)
                    .map(|s| (*s, u64::from(*s == section)))
                    .collect::<Vec<_>>(),
            );
            write_varint(&mut bytes, enc.len() as u64);
            bytes.extend_from_slice(&enc);
            assert_eq!(
                read_recovery(&bytes).unwrap_err(),
                BodyError::NoncanonicalRow { section },
                "section {section}"
            );
        }
    }

    /// Independent golden vectors for the recovery format (S5-A).
    ///
    /// These pin the exact bytes — header root, each row root, the complete
    /// framed body, and the BLAKE3 a replacement genesis would bind — so a
    /// codec, framing, or declaration change is a visible re-pin, never a
    /// silent drift. The body vector is the full §13 canonical form: header
    /// length varint, Fory header, all three sections in ascending order.
    #[test]
    fn golden_recovery_vectors_are_unchanged() {
        fn to_hex(b: &[u8]) -> String {
            b.iter().map(|x| format!("{x:02x}")).collect()
        }

        let header = RecoveryHeaderV1 {
            format_version: RECOVERY_FORMAT_VERSION,
            schema_version: RECOVERY_SCHEMA_VERSION,
            source_group_id: vec![0xAA; 32],
            source_genesis_hash: vec![0xBB; 32],
            source_authority_public_key: vec![0xCC; 32],
            source_authority_domain: "aster0.net".into(),
            source_last_applied: Some(LogIdV1 {
                term: 3,
                leader_node_id: vec![0xDD; 32],
                index: 41,
            }),
            source_membership: StoredMembershipV1::default(),
            source_audit_floor: None,
            source_audit_tip: Some(LogIdV1 {
                term: 3,
                leader_node_id: vec![0xDD; 32],
                index: 40,
            }),
            source_admin_revision: 5,
            row_counts: RowCountsV1 {
                resources: 1,
                grants: 1,
                admins: 1,
                ..Default::default()
            },
        };
        let resource = RecoveryResourceRowV1 {
            key: key("portal.roaming", b"tree-1"),
            source_epoch: 9,
            ttl_ms: 30_000,
            lease_revision: 4,
            value_revision: 7,
            value: vec![0x01, 0x02, 0x03],
            acl_revision: 2,
        };
        let grant = GrantRowV1 {
            key: key("portal.roaming", b"tree-1"),
            subject_node_id: vec![0xEE; 32],
            rights: 0x3f,
        };
        let admin = AdminRowV1 {
            subject_node_id: vec![0xEE; 32],
        };

        let body = {
            let mut w = BodyWriter::<_, RecoveryBodyRowV1>::new(Vec::new(), &header).unwrap();
            w.begin_section(SECTION_RESOURCES).unwrap();
            w.row(&RecoveryBodyRowV1::Resource(resource.clone()))
                .unwrap();
            w.begin_section(SECTION_GRANTS).unwrap();
            w.row(&RecoveryBodyRowV1::Grant(grant.clone())).unwrap();
            w.begin_section(SECTION_ADMINS).unwrap();
            w.row(&RecoveryBodyRowV1::Admin(admin.clone())).unwrap();
            w.finish().unwrap()
        };
        // The pinned body must read back cleanly: the vector is canonical, not
        // merely stable.
        assert!(read_recovery(&body).is_ok());

        let fory = RecoveryBodyRowV1::fory();
        let actual: Vec<(&str, String)> = vec![
            (
                "RecoveryHeaderV1",
                to_hex(&BodyHeader::encode(&header).unwrap()),
            ),
            (
                "RecoveryBodyRowV1::Resource",
                to_hex(
                    &RecoveryBodyRowV1::encode(&fory, &RecoveryBodyRowV1::Resource(resource))
                        .unwrap(),
                ),
            ),
            (
                "RecoveryBodyRowV1::Grant",
                to_hex(
                    &RecoveryBodyRowV1::encode(&fory, &RecoveryBodyRowV1::Grant(grant)).unwrap(),
                ),
            ),
            (
                "RecoveryBodyRowV1::Admin",
                to_hex(
                    &RecoveryBodyRowV1::encode(&fory, &RecoveryBodyRowV1::Admin(admin)).unwrap(),
                ),
            ),
            ("recovery_body", to_hex(&body)),
            (
                "recovery_body_blake3",
                blake3::hash(&body).to_hex().to_string(),
            ),
        ];

        let expected: &[(&str, &str)] = GOLDEN_RECOVERY;
        let matches = actual.len() == expected.len()
            && actual
                .iter()
                .zip(expected.iter())
                .all(|((an, ah), (en, eh))| an == en && ah == eh);
        if !matches {
            let rendered: String = actual
                .iter()
                .map(|(n, h)| format!("    (\"{n}\", \"{h}\"),\n"))
                .collect();
            panic!(
                "recovery encodings changed. If the change is intended and \
                 recorded, update GOLDEN_RECOVERY to:\n{rendered}"
            );
        }
    }

    /// Pinned 2026-08-27 from the first encode of the frozen declarations
    /// (S5-A); from then on it pins the §13 recovery format.
    const GOLDEN_RECOVERY: &[(&str, &str)] = &[
        ("RecoveryHeaderV1", "01ff1e003640feff5538fd52ec4200926223f0471c88641a02641c6fcaea2df62413aa48e3a720064880ec0ec40cc80ccc29d029d429d815de1ee01ee61eea1ef01e05010120aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa20bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb20cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc2a6173746572302e6e6574ff1e021db0293536d64121e34200926223f0471c88641a02641c6fcaea124a70d106c40ecc0ec829032920dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd1e0423905a2b61cfa86ee24200926223f0471c88641a02641c6fcaea31f65374483eb08c092323a1e0c61ec81efd1e0620c0a01f31e86b34e24200926223f0471c88641a02641c6fcaea1fb08c092323a1e0c41678c816780000fdff1e03032820dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd1e0828300991f9eaaa66e74200926223f0471c88641a02641c6fcaea1e5672ce1ca1a9a4c40ec80ecc0ed00ed40ed80edc0e01010100000000"),
        ("RecoveryBodyRowV1::Resource", "01ff23001f600e3d914d5f70054200926223f0471c88641a02641c6fcaea325620472a222c36706c5672c001001e022f00b9a04aa9d63de74200926223f0471c88641a02641c6fcaea39762413aa48e3b1249d48889d8bacc80ecc0ed00ed40edc0ec41ed82909b0ea010407021e042080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e6706747265652d3103010203"),
        ("RecoveryBodyRowV1::Grant", "01ff23001f600e3d914d5f70054200926223f0471c88641a02641c6fcaea325620472a222c36706c5672c002001e0220c0c28f1e012224e34200926223f0471c88641a02641c6fcaea1ec08806a759cb00cc0ec41ec8293f1e042080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e6706747265652d3120eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
        ("RecoveryBodyRowV1::Admin", "01ff23001f600e3d914d5f70054200926223f0471c88641a02641c6fcaea325620472a222c36706c5672c003001e021c1034a7e7c96b1ce14200926223f0471c88641a02641c6fcaea1eb419841b59cb00c42920eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
        ("recovery_body", "b80301ff1e003640feff5538fd52ec4200926223f0471c88641a02641c6fcaea2df62413aa48e3a720064880ec0ec40cc80ccc29d029d429d815de1ee01ee61eea1ef01e05010120aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa20bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb20cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc2a6173746572302e6e6574ff1e021db0293536d64121e34200926223f0471c88641a02641c6fcaea124a70d106c40ecc0ec829032920dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd1e0423905a2b61cfa86ee24200926223f0471c88641a02641c6fcaea31f65374483eb08c092323a1e0c61ec81efd1e0620c0a01f31e86b34e24200926223f0471c88641a02641c6fcaea1fb08c092323a1e0c41678c816780000fdff1e03032820dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd1e0828300991f9eaaa66e74200926223f0471c88641a02641c6fcaea1e5672ce1ca1a9a4c40ec80ecc0ed00ed40ed80edc0e010101000000000101b10101ff23001f600e3d914d5f70054200926223f0471c88641a02641c6fcaea325620472a222c36706c5672c001001e022f00b9a04aa9d63de74200926223f0471c88641a02641c6fcaea39762413aa48e3b1249d48889d8bacc80ecc0ed00ed40edc0ec41ed82909b0ea010407021e042080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e6706747265652d31030102030201b90101ff23001f600e3d914d5f70054200926223f0471c88641a02641c6fcaea325620472a222c36706c5672c002001e0220c0c28f1e012224e34200926223f0471c88641a02641c6fcaea1ec08806a759cb00cc0ec41ec8293f1e042080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e6706747265652d3120eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee03017401ff23001f600e3d914d5f70054200926223f0471c88641a02641c6fcaea325620472a222c36706c5672c003001e021c1034a7e7c96b1ce14200926223f0471c88641a02641c6fcaea1eb419841b59cb00c42920eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
        ("recovery_body_blake3", "ddf8ef56eb535a921d550b4791b0a03009a648fdb9d5a3c06237577aa07ab51d"),
    ];
}
