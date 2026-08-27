//! The frozen §5.2 public wire declarations (S4-WIRE).
//!
//! Every request, unary reply, stream frame and status-detail key of
//! `aster.coordination.Store` v1, with explicit field ids, validated bounds,
//! declared optional-versus-empty semantics, and golden vectors. The internal
//! `ActionV1`/`CommandV1`/`CommandReplyV1` roots in
//! [`contract`](crate::coordination::contract) are the *storage and Raft*
//! contract; these are the *RPC* contract, and the two are frozen separately —
//! a storage row change must never silently reshape a public request.
//!
//! ## Declaration rules (the same ones §5.2.1 pins for the internal roots)
//!
//! - every field carries an explicit `#[fory(id = N)]`, so source order cannot
//!   change the wire;
//! - byte identities are `Vec<u8>` + `#[fory(bytes)]`, validated to their exact
//!   length in Rust — never fixed-size arrays, never zero-filled sentinels;
//! - an absent value is `Option::None`; an empty vector is **never** an
//!   identity sentinel;
//! - unions carry `#[fory(default)]` on one variant and an
//!   `#[fory(unknown)]` carrier, which is a runtime artifact and never data:
//!   a frame that decodes to `Unknown` is refused, not interpreted;
//! - unsigned wire integers are quantities (`u64`/`u32`); `i32` is reserved
//!   for declared code spaces.
//!
//! ## The mutation prefix
//!
//! Every sequenced mutating request repeats the §5.2 prefix in field ids 1–4:
//! `protocol_version`, `group_id`, `client_id`, `sequence`. The request hash is
//! **not** a request field: the server derives it from the decoded action's
//! canonical bytes ([`crate::coordination::hash::request_hash`]), so a client
//! cannot claim a hash its own bytes do not produce.
//!
//! The actor is absent from every request; the service derives it from the
//! immutable Iroh transport peer. A `target_node_id` names the object being
//! changed, never the caller.

use fory_derive::{ForyStruct, ForyUnion};

use crate::coordination::contract::{
    LogIdV1, MemberSeedV1, NodeIdV1, ResourceKeyV1, ResourceSnapshotV1, MAX_WIRE_STRING,
};
use crate::coordination::model::limits::{
    rights_are_known, MAX_JOIN_PERMIT, MAX_KEY, MAX_NAMESPACE, MAX_TTL_MS, MAX_VALUE, MIN_TTL_MS,
};

/// The Store RPC protocol version every v1 request and reply carries.
pub const STORE_PROTOCOL_VERSION: i32 = 1;

/// The §5.2 audit-stream row cap: `limit` may not exceed this.
pub const MAX_AUDIT_STREAM_LIMIT: u32 = 10_000;

// ── Status-detail keys (§5.4) ───────────────────────────────────────────────
//
// Machine decisions travel in structured details; clients MUST NOT parse the
// human-readable message. Values are decimal integers or lowercase hex as
// documented per key.

/// On `UNAVAILABLE` from a follower: the last known leader's NodeId, lowercase
/// hex, 64 characters. Absent when no leader is known.
pub const DETAIL_LEADER_NODE_ID: &str = "aster.coordination.leader-node-id";
/// On `UNAVAILABLE` from a follower: the term the hint was observed at,
/// decimal.
pub const DETAIL_LEADER_TERM: &str = "aster.coordination.leader-term";
/// On `OUT_OF_RANGE` from `audit_stream`: the retained floor's term, decimal.
pub const DETAIL_AUDIT_FLOOR_TERM: &str = "aster.coordination.audit-floor-term";
/// On `OUT_OF_RANGE` from `audit_stream`: the retained floor's index, decimal.
pub const DETAIL_AUDIT_FLOOR_INDEX: &str = "aster.coordination.audit-floor-index";
/// On `OUT_OF_RANGE` from `audit_stream`: the retained floor's leader NodeId,
/// lowercase hex.
pub const DETAIL_AUDIT_FLOOR_LEADER: &str = "aster.coordination.audit-floor-leader-node-id";

// ── Shared reply leaves ─────────────────────────────────────────────────────

/// One committed grant row, inside [`AclSnapshotWireV1`].
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AclGrant")]
pub struct AclGrantWireV1 {
    /// Exactly 32 bytes.
    #[fory(id = 1, bytes)]
    pub subject_node_id: Vec<u8>,
    #[fory(id = 2)]
    pub rights: u64,
}

/// The exact committed ACL of one resource. A resource that has never been
/// granted has `acl_revision = 0` and no rows; that is a value, not an
/// absence.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AclSnapshot")]
pub struct AclSnapshotWireV1 {
    #[fory(id = 1)]
    pub resource: ResourceKeyV1,
    #[fory(id = 2)]
    pub acl_revision: u64,
    /// Sorted by `subject_node_id`, unique.
    #[fory(id = 3)]
    pub grants: Vec<AclGrantWireV1>,
}

/// The exact mutable delegated administrator set. Immutable ROOT is part of
/// the group's static identity and is deliberately **not** a row here.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AdminSnapshot")]
pub struct AdminSnapshotWireV1 {
    #[fory(id = 1)]
    pub admin_revision: u64,
    /// Sorted, unique.
    #[fory(id = 2)]
    pub administrators: Vec<NodeIdV1>,
}

// ── Sequenced mutation requests ─────────────────────────────────────────────
//
// Field ids 1–4 are the §5.2 mutation prefix, identical in every one of these
// ten types; the method fields follow in the frozen §5.2 order from id 5.

/// `acquire` (§5.2 code 1).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AcquireRequest")]
pub struct AcquireRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    #[fory(id = 5)]
    pub resource: ResourceKeyV1,
    #[fory(id = 6)]
    pub ttl_ms: u64,
}

/// `renew` (§5.2 code 2).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RenewRequest")]
pub struct RenewRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    #[fory(id = 5)]
    pub fence: crate::coordination::contract::FenceV1,
}

/// `compare_and_set` (§5.2 code 3).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/CompareAndSetRequest")]
pub struct CompareAndSetRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    #[fory(id = 5)]
    pub fence: crate::coordination::contract::FenceV1,
    #[fory(id = 6)]
    pub expected_value_revision: u64,
    #[fory(id = 7, bytes)]
    pub new_value: Vec<u8>,
}

/// `release` (§5.2 code 4).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ReleaseRequest")]
pub struct ReleaseRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    #[fory(id = 5)]
    pub fence: crate::coordination::contract::FenceV1,
}

/// `release_with_value` (§5.2 code 5).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ReleaseWithValueRequest")]
pub struct ReleaseWithValueRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    #[fory(id = 5)]
    pub fence: crate::coordination::contract::FenceV1,
    #[fory(id = 6)]
    pub expected_value_revision: u64,
    #[fory(id = 7, bytes)]
    pub final_value: Vec<u8>,
}

/// `revoke` (§5.2 code 6). `expected_epoch: None` is the unconditional
/// containment revoke; `Some(e)` fences only on an exact epoch match and
/// otherwise changes nothing.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RevokeRequest")]
pub struct RevokeRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    #[fory(id = 5)]
    pub resource: ResourceKeyV1,
    #[fory(id = 6)]
    pub expected_epoch: Option<u64>,
}

/// `set_grant` (§5.2 code 7). `target_node_id` names the grant's subject,
/// never the caller.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SetGrantRequest")]
pub struct SetGrantRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    #[fory(id = 5)]
    pub resource: ResourceKeyV1,
    #[fory(id = 6, bytes)]
    pub target_node_id: Vec<u8>,
    #[fory(id = 7)]
    pub rights: u64,
    #[fory(id = 8)]
    pub expected_acl_revision: u64,
}

/// `set_admin` (§5.2 code 8). Naming the immutable deployment ROOT is refused
/// with the terminal reason `26 RootAuthorityImmutable`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SetAdminRequest")]
pub struct SetAdminRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    #[fory(id = 5, bytes)]
    pub target_node_id: Vec<u8>,
    #[fory(id = 6)]
    pub enabled: bool,
    #[fory(id = 7)]
    pub expected_admin_revision: u64,
}

/// `change_membership` (§5.2 code 9). Vectors are sorted unique and disjoint;
/// `member_seeds` names only nodes the plan contains, with a signed permit
/// exactly for each new node. The deep §5.2/§9 plan rules run in the model —
/// identically at preflight and apply.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ChangeMembershipRequest")]
pub struct ChangeMembershipRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    /// Exactly 16 bytes.
    #[fory(id = 5, bytes)]
    pub operation_id: Vec<u8>,
    /// `None` is not a wildcard — it is a mismatch against any committed
    /// membership that has a log id.
    #[fory(id = 6)]
    pub expected_membership_log_id: Option<LogIdV1>,
    #[fory(id = 7)]
    pub desired_voters: Vec<NodeIdV1>,
    #[fory(id = 8)]
    pub retained_learners: Vec<NodeIdV1>,
    #[fory(id = 9)]
    pub member_seeds: Vec<MemberSeedV1>,
}

/// `cancel_membership` (§5.2 code 11).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/CancelMembershipRequest")]
pub struct CancelMembershipRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    /// Exactly 16 bytes.
    #[fory(id = 5, bytes)]
    pub operation_id: Vec<u8>,
    #[fory(id = 6)]
    pub expected_phase_code: i32,
}

/// The one reply shape of every **sequenced** mutating method (§5.2).
///
/// `retire_client` is the single deliberate exception — it is sessionless and
/// answers [`RetireClientReplyWireV1`].
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/MutationReply")]
pub struct MutationReplyWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 4)]
    pub sequence: u64,
    /// Exactly 32 bytes: the canonical hash the server derived from the
    /// decoded action, echoed so a journal can verify what was answered.
    #[fory(id = 5, bytes)]
    pub request_hash: Vec<u8>,
    /// `1 Consumed`, `2 NotConsumed`.
    #[fory(id = 6)]
    pub disposition_code: i32,
    /// §5.2 outcome codes.
    #[fory(id = 7)]
    pub outcome_code: i32,
    /// §5.2 reason codes; `0` means none.
    #[fory(id = 8)]
    pub reason_code: i32,
    #[fory(id = 9)]
    pub snapshot: Option<ResourceSnapshotV1>,
    /// `0` when not applicable.
    #[fory(id = 10)]
    pub acl_revision: u64,
    /// `0` when not applicable.
    #[fory(id = 11)]
    pub admin_revision: u64,
    /// Exactly 16 bytes when present.
    #[fory(id = 12, bytes)]
    pub operation_id: Option<Vec<u8>>,
    #[fory(id = 13)]
    pub membership_log_id: Option<LogIdV1>,
}

// ── The sessionless mutation (§5.2, §10) ────────────────────────────────────

/// `retire_client` (§5.2 code 10): sessionless, no mutation prefix, no
/// sequence, deduplicated on `(target_node_id, target_client_id)`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RetireClientRequest")]
pub struct RetireClientRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    /// Exactly 32 bytes: the session's owning actor — the object, not the
    /// caller.
    #[fory(id = 3, bytes)]
    pub target_node_id: Vec<u8>,
    /// Exactly 16 bytes.
    #[fory(id = 4, bytes)]
    pub target_client_id: Vec<u8>,
}

/// `retire_client`'s reply. No disposition field: a sessionless command has no
/// sequence to consume, and terminality is a property of the reason —
/// `18 RetainedSessionCapReached` is terminal.
///
/// The pairs are frozen, not the fields independently: `(9, 0)` retired,
/// `(100, 18)` the terminal capacity refusal, and `(100, 12
/// PermissionChanged)` for a cross-actor retirement without administrative
/// authority. A **self-targeted** request — the only kind the §10 closing
/// corridor and `retire_other_client` send — is always authorized, so its
/// answer space is exactly the first two.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RetireClientReply")]
pub struct RetireClientReplyWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    /// `9 ClientRetired`, or `100 Rejected`.
    #[fory(id = 3)]
    pub outcome_code: i32,
    /// `0`, `18 RetainedSessionCapReached`, or `12 PermissionChanged`.
    #[fory(id = 4)]
    pub reason_code: i32,
}

// ── Read and status requests (§5.2) ─────────────────────────────────────────

/// `read_linearizable`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ReadRequest")]
pub struct ReadRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3)]
    pub resource: ResourceKeyV1,
}

/// `read_linearizable`'s reply. The snapshot is always present: a resource
/// that has never been written projects as epoch 0, unheld, no value — that is
/// the register's value, not an absence, and the caller already proved `READ`
/// on exactly this key, so no existence oracle is created.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ReadReply")]
pub struct ReadReplyWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3)]
    pub snapshot: ResourceSnapshotV1,
}

/// `inspect_acl`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AclStatusRequest")]
pub struct AclStatusRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3)]
    pub resource: ResourceKeyV1,
}

/// `inspect_acl`'s reply.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AclStatusReply")]
pub struct AclStatusReplyWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3)]
    pub acl_snapshot: AclSnapshotWireV1,
}

/// `inspect_admins`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AdminStatusRequest")]
pub struct AdminStatusRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
}

/// `inspect_admins`' reply.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AdminStatusReply")]
pub struct AdminStatusReplyWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3)]
    pub admin_snapshot: AdminSnapshotWireV1,
}

/// `membership_status`. `operation_id: None` asks about the active intent, if
/// any.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/MembershipStatusRequest")]
pub struct MembershipStatusRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    /// Exactly 16 bytes when present.
    #[fory(id = 3, bytes)]
    pub operation_id: Option<Vec<u8>>,
}

/// `membership_status`' reply. All fields are one linearizable replicated cut.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/MembershipStatusReply")]
pub struct MembershipStatusReplyWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    /// The intent asked about, or `None` when no intent matched.
    #[fory(id = 3, bytes)]
    pub operation_id: Option<Vec<u8>>,
    /// §5.2 intent status codes; `0` when no intent matched.
    #[fory(id = 4)]
    pub status_code: i32,
    #[fory(id = 5)]
    pub expected_membership_log_id: Option<LogIdV1>,
    #[fory(id = 6)]
    pub observed_membership_log_id: Option<LogIdV1>,
    #[fory(id = 7)]
    pub desired_voters: Vec<NodeIdV1>,
    #[fory(id = 8)]
    pub retained_learners: Vec<NodeIdV1>,
    /// §7.3's completion error code space; `0` means none.
    #[fory(id = 9)]
    pub error_code: i32,
}

/// `group_status`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/GroupStatusRequest")]
pub struct GroupStatusRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
}

/// `group_status`' reply — the reviewed §S4-STATUS-CUT mixed cut.
///
/// **Replicated, from one guarded read transaction after the leader barrier,
/// with the transaction-local applied witness at or past it**:
/// `deployment_profile_code`, `applied_log_id` (the pinned witness itself),
/// `membership_log_id`, `membership_joint`, `voters`, `learners`,
/// `pending_operation_id`. These are one consistent cut; the placement
/// manager consumes them together rather than combining two status calls.
///
/// **Local and advisory, sampled after that cut**: `lifecycle_code`,
/// `local_node_id`, `local_role_code`, `leader_node_id`, `term`,
/// `last_log_id`, `committed_log_id`, `quorum_available`, `fatal_code`. They
/// describe this node at some instant near the cut, never the group, and no
/// caller may turn them into permission to act. `quorum_available` is
/// `Option<bool>` because only a leader holds direct quorum evidence — a
/// follower truthfully answers `None`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/GroupStatusReply")]
pub struct GroupStatusReplyWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    /// Advisory. §7.6 lifecycle codes.
    #[fory(id = 3)]
    pub lifecycle_code: i32,
    /// Advisory. Exactly 32 bytes.
    #[fory(id = 4, bytes)]
    pub local_node_id: Vec<u8>,
    /// Advisory. `0 None`, `1 Learner`, `2 Voter`.
    #[fory(id = 5)]
    pub local_role_code: i32,
    /// Advisory. Exactly 32 bytes when present.
    #[fory(id = 6, bytes)]
    pub leader_node_id: Option<Vec<u8>>,
    /// Advisory.
    #[fory(id = 7)]
    pub term: u64,
    /// Advisory.
    #[fory(id = 8)]
    pub last_log_id: Option<LogIdV1>,
    /// Advisory.
    #[fory(id = 9)]
    pub committed_log_id: Option<LogIdV1>,
    /// Replicated: the guarded transaction's own pinned applied log id — the
    /// witness that proved the cut is at or past the barrier.
    #[fory(id = 10)]
    pub applied_log_id: Option<LogIdV1>,
    /// Replicated.
    #[fory(id = 11)]
    pub deployment_profile_code: i32,
    /// Replicated.
    #[fory(id = 12)]
    pub membership_log_id: Option<LogIdV1>,
    /// Replicated.
    #[fory(id = 13)]
    pub membership_joint: bool,
    /// Replicated. Sorted, unique.
    #[fory(id = 14)]
    pub voters: Vec<NodeIdV1>,
    /// Replicated. Sorted, unique, disjoint from `voters`.
    #[fory(id = 15)]
    pub learners: Vec<NodeIdV1>,
    /// Replicated. Exactly 16 bytes when present.
    #[fory(id = 16, bytes)]
    pub pending_operation_id: Option<Vec<u8>>,
    /// Advisory. `Some` only on a leader.
    #[fory(id = 17)]
    pub quorum_available: Option<bool>,
    /// Advisory. `0` means not fatal.
    #[fory(id = 18)]
    pub fatal_code: i32,
    /// Replicated. §14's audit floor: the lowest retained audit row's log
    /// id, `None` while the full history is retained.
    #[fory(id = 19)]
    pub audit_floor: Option<LogIdV1>,
    /// Replicated. Exactly 32 bytes: the group's genesis hash (§14's
    /// "group id/genesis hash" pair — the group id is field 2).
    #[fory(id = 20, bytes)]
    pub genesis_hash: Vec<u8>,
    /// Replicated. Exactly 32 bytes: the deployment ROOT public key this
    /// store is bound to (§4.1). Exposed so a client validating a §9
    /// membership plan uses the **group's** sealed authority evidence
    /// rather than trusted local configuration.
    #[fory(id = 21, bytes)]
    pub authority_public_key: Vec<u8>,
    /// Replicated. The authority domain the ROOT signs under (§4.1).
    #[fory(id = 22)]
    pub authority_domain: String,
}

/// `probe_client` — a diagnostic read, never authority to reassign a sequence
/// (§10). It reports whether a sequence was consumed, never what it answered.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ProbeClientRequest")]
pub struct ProbeClientRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    /// Exactly 32 bytes. An actor may probe only its own; an administrator any.
    #[fory(id = 3, bytes)]
    pub target_node_id: Vec<u8>,
    /// Exactly 16 bytes.
    #[fory(id = 4, bytes)]
    pub target_client_id: Vec<u8>,
}

/// `probe_client`'s reply. `exists = false` means no session was ever created
/// for the pair; the remaining fields are zero/empty.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ProbeClientReply")]
pub struct ProbeClientReplyWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub target_node_id: Vec<u8>,
    #[fory(id = 4, bytes)]
    pub target_client_id: Vec<u8>,
    #[fory(id = 5)]
    pub exists: bool,
    #[fory(id = 6)]
    pub last_sequence: u64,
    /// 32 bytes when `exists`, empty otherwise — this field is data about a
    /// hash, not an identity, so the empty form is a length, not a sentinel.
    #[fory(id = 7, bytes)]
    pub request_hash: Vec<u8>,
    /// The saved result's disposition code, `0` when none was saved.
    #[fory(id = 8)]
    pub disposition_code: i32,
    #[fory(id = 9)]
    pub retired: bool,
}

// ── Streams ─────────────────────────────────────────────────────────────────

/// `audit_stream`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AuditStreamRequest")]
pub struct AuditStreamRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    /// Rows strictly after this id, ascending. `None` starts at the retained
    /// floor.
    #[fory(id = 3)]
    pub after_log_id: Option<LogIdV1>,
    /// At most [`MAX_AUDIT_STREAM_LIMIT`].
    #[fory(id = 4)]
    pub limit: u32,
}

/// One retained audit row (§5.2). Never contains register values, tickets, or
/// credentials.
///
/// `actor_node_id` is `Option<NodeId>` with the same width rule as every other
/// link in the chain — `Some` is exactly 32 bytes, and there is no
/// empty-vector sentinel — so a committed malformed entry's audit row survives
/// from apply to a subscribed client.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AuditRecord")]
pub struct AuditRecordWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3)]
    pub log_id: LogIdV1,
    /// `None` only when the committed entry's actor bytes themselves could not
    /// be narrowed; a system command stores the proposing log leader.
    #[fory(id = 4, bytes)]
    pub actor_node_id: Option<Vec<u8>>,
    /// 1–11 are the mutation method codes; `100` system `Expire`, `101`
    /// `MembershipProgress`, `102` the committed signed-genesis installation.
    #[fory(id = 5)]
    pub operation_code: i32,
    /// `None` when the entry's resource identity was malformed or the
    /// operation has none.
    #[fory(id = 6)]
    pub resource: Option<ResourceKeyV1>,
    /// The public outcome code for client commands; the declared system result
    /// code for system commands.
    #[fory(id = 7)]
    pub result_code: i32,
    #[fory(id = 8)]
    pub epoch: u64,
    #[fory(id = 9)]
    pub value_revision: u64,
}

/// `recovery_export`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RecoveryExportRequest")]
pub struct RecoveryExportRequestV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
}

/// The `recovery_export` manifest — the first frame of the stream.
///
/// The body that follows is the **canonical §13 recovery byte stream**
/// (`RecoveryHeaderV1` plus its sectioned rows), produced from one guarded
/// read transaction after a leader barrier and checksummed whole. Slice 5 owns
/// import and transformation; this method only serves the bytes.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RecoveryExportManifest")]
pub struct RecoveryExportManifestWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    /// Total canonical body length in bytes. Bounded: a v1 export cannot
    /// exceed the §12.1 state budgets it exports.
    #[fory(id = 3)]
    pub total_len: u64,
    /// BLAKE3 of the whole canonical body; exactly 32 bytes.
    #[fory(id = 4, bytes)]
    pub blake3_checksum: Vec<u8>,
    /// The linearizable cut the export was taken at.
    #[fory(id = 5)]
    pub applied_log_id: Option<LogIdV1>,
}

/// One export chunk's byte ceiling. The server slices the canonical body
/// into at most this; the client refuses a larger chunk before writing a
/// byte of it, so both sides compile against the one constant.
pub const MAX_EXPORT_CHUNK_BYTES: usize = 1024 * 1024;

/// One contiguous chunk of the canonical recovery body.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RecoveryExportChunk")]
pub struct RecoveryExportChunkWireV1 {
    /// Byte offset of `data` in the canonical body; chunks are contiguous and
    /// ascending from zero.
    #[fory(id = 1)]
    pub offset: u64,
    #[fory(id = 2, bytes)]
    pub data: Vec<u8>,
    #[fory(id = 3)]
    pub final_chunk: bool,
}

/// The `recovery_export` stream frame.
#[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RecoveryExportFrame")]
pub enum RecoveryExportFrameV1 {
    #[fory(default)]
    #[fory(id = 1)]
    Manifest(RecoveryExportManifestWireV1),
    #[fory(id = 2)]
    Chunk(RecoveryExportChunkWireV1),
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

// ── Validation ──────────────────────────────────────────────────────────────

/// Why a decoded §5.2 value violates its declared bounds.
///
/// Stage 0 (no mutation identity) and stage 4 (request-shape) both answer with
/// an `RpcStatus`, but they are distinguished by *which* fields failed — the
/// pipeline consults [`prefix`]-level results for stage 0 and body-level
/// results for stage 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireInvalid {
    /// A field that must be exactly 32 bytes is not.
    NodeIdWidth(&'static str),
    /// The group id is not exactly 32 bytes.
    GroupIdWidth,
    /// A client/operation id that must be exactly 16 bytes is not.
    Id16Width(&'static str),
    /// A hash field that must be exactly 32 bytes is not.
    HashWidth(&'static str),
    /// The protocol version is not the v1 constant.
    ProtocolVersion,
    /// A namespace is empty, over 64 bytes, or ill-formed.
    Namespace,
    /// A resource key is empty or over 512 bytes.
    Key,
    /// A value exceeds 64 KiB.
    Value(&'static str),
    /// A TTL is outside 5,000–300,000 ms.
    Ttl,
    /// A rights word carries a bit v1 does not define.
    Rights,
    /// A ticket exceeds [`MAX_WIRE_STRING`] bytes.
    Ticket,
    /// A signed join permit exceeds 8 KiB.
    JoinPermit,
    /// An audit limit is zero or above [`MAX_AUDIT_STREAM_LIMIT`].
    AuditLimit,
}

impl WireInvalid {
    /// A short operator-facing description. Never parsed by clients.
    pub fn describe(self) -> String {
        match self {
            WireInvalid::NodeIdWidth(field) => format!("{field} must be exactly 32 bytes"),
            WireInvalid::GroupIdWidth => "group_id must be exactly 32 bytes".into(),
            WireInvalid::Id16Width(field) => format!("{field} must be exactly 16 bytes"),
            WireInvalid::HashWidth(field) => format!("{field} must be exactly 32 bytes"),
            WireInvalid::ProtocolVersion => "unsupported Store protocol version".into(),
            WireInvalid::Namespace => {
                format!("namespace must be 1..={MAX_NAMESPACE} bytes")
            }
            WireInvalid::Key => format!("key must be 1..={MAX_KEY} bytes"),
            WireInvalid::Value(field) => format!("{field} must be at most {MAX_VALUE} bytes"),
            WireInvalid::Ttl => format!("ttl_ms must be {MIN_TTL_MS}..={MAX_TTL_MS}"),
            WireInvalid::Rights => "rights carries a bit v1 does not define".into(),
            WireInvalid::Ticket => format!("ticket must be at most {MAX_WIRE_STRING} bytes"),
            WireInvalid::JoinPermit => {
                format!("signed_join_permit must be at most {MAX_JOIN_PERMIT} bytes")
            }
            WireInvalid::AuditLimit => {
                format!("limit must be 1..={MAX_AUDIT_STREAM_LIMIT}")
            }
        }
    }
}

/// The validated §5.2 mutation prefix of one sequenced request.
///
/// Producing this is stage 0's *identity* question and nothing more: a request
/// whose prefix narrows **has** a mutation identity, even when a body field is
/// oversized — those are stage-4 shape questions, checked after session
/// resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutationPrefix {
    pub group_id: [u8; 32],
    pub client_id: [u8; 16],
    pub sequence: u64,
}

/// Narrow a §5.2 mutation prefix, or say which part has no identity.
pub fn prefix(
    protocol_version: i32,
    group_id: &[u8],
    client_id: &[u8],
    sequence: u64,
) -> Result<MutationPrefix, WireInvalid> {
    if protocol_version != STORE_PROTOCOL_VERSION {
        return Err(WireInvalid::ProtocolVersion);
    }
    let group_id: [u8; 32] = group_id.try_into().map_err(|_| WireInvalid::GroupIdWidth)?;
    let client_id: [u8; 16] = client_id
        .try_into()
        .map_err(|_| WireInvalid::Id16Width("client_id"))?;
    Ok(MutationPrefix {
        group_id,
        client_id,
        sequence,
    })
}

/// Exactly 32 bytes, or the named width failure.
pub fn node_id(bytes: &[u8], field: &'static str) -> Result<[u8; 32], WireInvalid> {
    bytes
        .try_into()
        .map_err(|_| WireInvalid::NodeIdWidth(field))
}

/// Exactly 16 bytes, or the named width failure.
pub fn id16(bytes: &[u8], field: &'static str) -> Result<[u8; 16], WireInvalid> {
    bytes.try_into().map_err(|_| WireInvalid::Id16Width(field))
}

/// The §12.1 request-shape bounds of a resource key. Validity beyond size —
/// the §4.2 namespace grammar — is the model's shared rule and runs in the
/// pipeline's stage 4 via the same function apply uses.
pub fn resource_bounds(resource: &ResourceKeyV1) -> Result<(), WireInvalid> {
    if resource.namespace.is_empty() || resource.namespace.len() > MAX_NAMESPACE {
        return Err(WireInvalid::Namespace);
    }
    if resource.key.is_empty() || resource.key.len() > MAX_KEY {
        return Err(WireInvalid::Key);
    }
    Ok(())
}

/// The §12.1 TTL bounds.
pub fn ttl_bounds(ttl_ms: u64) -> Result<(), WireInvalid> {
    if !(MIN_TTL_MS..=MAX_TTL_MS).contains(&ttl_ms) {
        return Err(WireInvalid::Ttl);
    }
    Ok(())
}

/// The §12.1 value bound, named for its field.
pub fn value_bounds(value: &[u8], field: &'static str) -> Result<(), WireInvalid> {
    if value.len() > MAX_VALUE {
        return Err(WireInvalid::Value(field));
    }
    Ok(())
}

/// The §8.1 known-rights rule.
pub fn rights_bounds(rights: u64) -> Result<(), WireInvalid> {
    if !rights_are_known(rights) {
        return Err(WireInvalid::Rights);
    }
    Ok(())
}

/// The §12.1 member-seed size bounds.
pub fn member_seed_bounds(seed: &MemberSeedV1) -> Result<(), WireInvalid> {
    node_id(&seed.node_id, "member_seeds[].node_id")?;
    if seed.ticket.len() > MAX_WIRE_STRING {
        return Err(WireInvalid::Ticket);
    }
    if let Some(permit) = &seed.signed_join_permit {
        if permit.len() > MAX_JOIN_PERMIT {
            return Err(WireInvalid::JoinPermit);
        }
    }
    Ok(())
}

/// The audit stream's request bounds.
pub fn audit_limit_bounds(limit: u32) -> Result<(), WireInvalid> {
    if limit == 0 || limit > MAX_AUDIT_STREAM_LIMIT {
        return Err(WireInvalid::AuditLimit);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::contract::{payload_fory, FenceV1};

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn resource() -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: "app.head".into(),
            key: vec![0x01, 0x02],
        }
    }

    fn fence() -> FenceV1 {
        FenceV1 {
            group_id: vec![0xAA; 32],
            resource: resource(),
            epoch: 7,
            holder: vec![0xBB; 32],
        }
    }

    fn log_id() -> LogIdV1 {
        LogIdV1 {
            term: 3,
            leader_node_id: vec![0xCC; 32],
            index: 41,
        }
    }

    fn snapshot() -> ResourceSnapshotV1 {
        ResourceSnapshotV1 {
            group_id: vec![0xAA; 32],
            resource: resource(),
            epoch: 7,
            holder: Some(vec![0xBB; 32]),
            held: true,
            ttl_ms: 30_000,
            lease_revision: 2,
            value_revision: 5,
            acl_revision: 3,
            value: Some(vec![0xEE; 3]),
            value_redacted: false,
        }
    }

    /// Every public root round-trips through its own payload runtime, and
    /// `Some(vec![])` stays distinguishable from `None` where the §4.3
    /// redaction rule depends on it.
    #[test]
    fn every_public_root_round_trips() {
        macro_rules! round_trip {
            ($ty:ty, $value:expr) => {{
                let value: $ty = $value;
                let fory = payload_fory::<$ty>();
                let decoded: $ty = fory
                    .deserialize(&fory.serialize(&value).expect("encode"))
                    .expect("decode");
                assert_eq!(decoded, value, stringify!($ty));
            }};
        }

        round_trip!(
            AcquireRequestV1,
            AcquireRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 1,
                resource: resource(),
                ttl_ms: 30_000,
            }
        );
        round_trip!(
            RenewRequestV1,
            RenewRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 2,
                fence: fence(),
            }
        );
        round_trip!(
            CompareAndSetRequestV1,
            CompareAndSetRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 3,
                fence: fence(),
                expected_value_revision: 5,
                new_value: vec![0xEE; 3],
            }
        );
        round_trip!(
            ReleaseRequestV1,
            ReleaseRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 4,
                fence: fence(),
            }
        );
        round_trip!(
            ReleaseWithValueRequestV1,
            ReleaseWithValueRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 5,
                fence: fence(),
                expected_value_revision: 5,
                // Empty-but-present is a real final value.
                final_value: Vec::new(),
            }
        );
        round_trip!(
            RevokeRequestV1,
            RevokeRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 6,
                resource: resource(),
                expected_epoch: Some(7),
            }
        );
        round_trip!(
            SetGrantRequestV1,
            SetGrantRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 7,
                resource: resource(),
                target_node_id: vec![0xDD; 32],
                rights: 0x03,
                expected_acl_revision: 3,
            }
        );
        round_trip!(
            SetAdminRequestV1,
            SetAdminRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 8,
                target_node_id: vec![0xDD; 32],
                enabled: true,
                expected_admin_revision: 1,
            }
        );
        round_trip!(
            ChangeMembershipRequestV1,
            ChangeMembershipRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 9,
                operation_id: vec![0x22; 16],
                expected_membership_log_id: Some(log_id()),
                desired_voters: vec![NodeIdV1 {
                    node_id: vec![0xDD; 32]
                }],
                retained_learners: Vec::new(),
                member_seeds: Vec::new(),
            }
        );
        round_trip!(
            CancelMembershipRequestV1,
            CancelMembershipRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 10,
                operation_id: vec![0x22; 16],
                expected_phase_code: 2,
            }
        );
        round_trip!(
            MutationReplyWireV1,
            MutationReplyWireV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                client_id: vec![0x11; 16],
                sequence: 1,
                request_hash: vec![0x33; 32],
                disposition_code: 1,
                outcome_code: 1,
                reason_code: 0,
                snapshot: Some(snapshot()),
                acl_revision: 3,
                admin_revision: 0,
                operation_id: None,
                membership_log_id: None,
            }
        );
        round_trip!(
            RetireClientRequestV1,
            RetireClientRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                target_node_id: vec![0xDD; 32],
                target_client_id: vec![0x11; 16],
            }
        );
        round_trip!(
            RetireClientReplyWireV1,
            RetireClientReplyWireV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                outcome_code: 9,
                reason_code: 0,
            }
        );
        round_trip!(
            ReadRequestV1,
            ReadRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                resource: resource(),
            }
        );
        round_trip!(
            ReadReplyWireV1,
            ReadReplyWireV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                snapshot: snapshot(),
            }
        );
        round_trip!(
            AclStatusRequestV1,
            AclStatusRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                resource: resource(),
            }
        );
        round_trip!(
            AclStatusReplyWireV1,
            AclStatusReplyWireV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                acl_snapshot: AclSnapshotWireV1 {
                    resource: resource(),
                    acl_revision: 3,
                    grants: vec![AclGrantWireV1 {
                        subject_node_id: vec![0xDD; 32],
                        rights: 0x03,
                    }],
                },
            }
        );
        round_trip!(
            AdminStatusRequestV1,
            AdminStatusRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
            }
        );
        round_trip!(
            AdminStatusReplyWireV1,
            AdminStatusReplyWireV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                admin_snapshot: AdminSnapshotWireV1 {
                    admin_revision: 1,
                    administrators: vec![NodeIdV1 {
                        node_id: vec![0xDD; 32]
                    }],
                },
            }
        );
        round_trip!(
            MembershipStatusRequestV1,
            MembershipStatusRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                operation_id: None,
            }
        );
        round_trip!(
            MembershipStatusReplyWireV1,
            MembershipStatusReplyWireV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                operation_id: Some(vec![0x22; 16]),
                status_code: 4,
                expected_membership_log_id: Some(log_id()),
                observed_membership_log_id: Some(log_id()),
                desired_voters: vec![NodeIdV1 {
                    node_id: vec![0xDD; 32]
                }],
                retained_learners: Vec::new(),
                error_code: 0,
            }
        );
        round_trip!(
            GroupStatusRequestV1,
            GroupStatusRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
            }
        );
        round_trip!(
            GroupStatusReplyWireV1,
            GroupStatusReplyWireV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                lifecycle_code: 3,
                local_node_id: vec![0xDD; 32],
                local_role_code: 2,
                leader_node_id: Some(vec![0xDD; 32]),
                term: 3,
                last_log_id: Some(log_id()),
                committed_log_id: Some(log_id()),
                applied_log_id: Some(log_id()),
                deployment_profile_code: 1,
                membership_log_id: Some(log_id()),
                membership_joint: false,
                voters: vec![NodeIdV1 {
                    node_id: vec![0xDD; 32]
                }],
                learners: Vec::new(),
                pending_operation_id: None,
                quorum_available: Some(true),
                fatal_code: 0,
                audit_floor: Some(log_id()),
                genesis_hash: vec![0x77; 32],
                authority_public_key: vec![0xCC; 32],
                authority_domain: "wire.test".into(),
            }
        );
        round_trip!(
            ProbeClientRequestV1,
            ProbeClientRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                target_node_id: vec![0xDD; 32],
                target_client_id: vec![0x11; 16],
            }
        );
        round_trip!(
            ProbeClientReplyWireV1,
            ProbeClientReplyWireV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                target_node_id: vec![0xDD; 32],
                target_client_id: vec![0x11; 16],
                exists: true,
                last_sequence: 5,
                request_hash: vec![0x33; 32],
                disposition_code: 1,
                retired: false,
            }
        );
        round_trip!(
            AuditStreamRequestV1,
            AuditStreamRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                after_log_id: Some(log_id()),
                limit: 100,
            }
        );
        round_trip!(
            AuditRecordWireV1,
            AuditRecordWireV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                log_id: log_id(),
                actor_node_id: None,
                operation_code: 7,
                resource: Some(resource()),
                result_code: 6,
                epoch: 7,
                value_revision: 5,
            }
        );
        round_trip!(
            RecoveryExportRequestV1,
            RecoveryExportRequestV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
            }
        );
        round_trip!(
            RecoveryExportFrameV1,
            RecoveryExportFrameV1::Manifest(RecoveryExportManifestWireV1 {
                protocol_version: 1,
                group_id: vec![0xAA; 32],
                total_len: 10,
                blake3_checksum: vec![0x44; 32],
                applied_log_id: Some(log_id()),
            })
        );
        round_trip!(
            RecoveryExportFrameV1,
            RecoveryExportFrameV1::Chunk(RecoveryExportChunkWireV1 {
                offset: 0,
                data: b"0123456789".to_vec(),
                final_chunk: true,
            })
        );
    }

    /// `Some(vec![])` and `None` are different wire values on every optional
    /// byte field that carries them — the §4.3 redaction rule depends on it.
    #[test]
    fn an_absent_holder_is_not_an_empty_one() {
        let fory = payload_fory::<ResourceSnapshotV1>();
        let mut with_none = snapshot();
        with_none.holder = None;
        with_none.value = None;
        let mut with_empty = snapshot();
        with_empty.holder = None;
        with_empty.value = Some(Vec::new());
        assert_ne!(
            fory.serialize(&with_none).expect("encode"),
            fory.serialize(&with_empty).expect("encode"),
        );
    }

    // ── Validation ──────────────────────────────────────────────────────────

    #[test]
    fn the_prefix_narrows_or_names_what_failed() {
        assert!(prefix(1, &[0xAA; 32], &[0x11; 16], 1).is_ok());
        assert_eq!(
            prefix(2, &[0xAA; 32], &[0x11; 16], 1),
            Err(WireInvalid::ProtocolVersion)
        );
        assert_eq!(
            prefix(1, &[0xAA; 31], &[0x11; 16], 1),
            Err(WireInvalid::GroupIdWidth)
        );
        assert_eq!(
            prefix(1, &[0xAA; 32], &[0x11; 15], 1),
            Err(WireInvalid::Id16Width("client_id"))
        );
        // Sequence 0 has an identity; it is the *classifier* that refuses it.
        assert!(prefix(1, &[0xAA; 32], &[0x11; 16], 0).is_ok());
    }

    #[test]
    fn the_bounds_functions_enforce_the_frozen_limits() {
        assert!(resource_bounds(&resource()).is_ok());
        assert_eq!(
            resource_bounds(&ResourceKeyV1 {
                namespace: String::new(),
                key: vec![1],
            }),
            Err(WireInvalid::Namespace)
        );
        assert_eq!(
            resource_bounds(&ResourceKeyV1 {
                namespace: "a".repeat(MAX_NAMESPACE + 1),
                key: vec![1],
            }),
            Err(WireInvalid::Namespace)
        );
        assert_eq!(
            resource_bounds(&ResourceKeyV1 {
                namespace: "ns".into(),
                key: Vec::new(),
            }),
            Err(WireInvalid::Key)
        );
        assert_eq!(
            resource_bounds(&ResourceKeyV1 {
                namespace: "ns".into(),
                key: vec![0; MAX_KEY + 1],
            }),
            Err(WireInvalid::Key)
        );

        assert!(ttl_bounds(MIN_TTL_MS).is_ok());
        assert!(ttl_bounds(MAX_TTL_MS).is_ok());
        assert_eq!(ttl_bounds(MIN_TTL_MS - 1), Err(WireInvalid::Ttl));
        assert_eq!(ttl_bounds(MAX_TTL_MS + 1), Err(WireInvalid::Ttl));

        assert!(value_bounds(&vec![0; MAX_VALUE], "new_value").is_ok());
        assert_eq!(
            value_bounds(&vec![0; MAX_VALUE + 1], "new_value"),
            Err(WireInvalid::Value("new_value"))
        );

        assert!(rights_bounds(0x3F).is_ok());
        assert_eq!(rights_bounds(0x40), Err(WireInvalid::Rights));

        assert!(audit_limit_bounds(1).is_ok());
        assert!(audit_limit_bounds(MAX_AUDIT_STREAM_LIMIT).is_ok());
        assert_eq!(audit_limit_bounds(0), Err(WireInvalid::AuditLimit));
        assert_eq!(
            audit_limit_bounds(MAX_AUDIT_STREAM_LIMIT + 1),
            Err(WireInvalid::AuditLimit)
        );

        assert_eq!(
            node_id(&[0xDD; 33], "target_node_id"),
            Err(WireInvalid::NodeIdWidth("target_node_id"))
        );
        assert_eq!(
            id16(&[0x22; 17], "operation_id"),
            Err(WireInvalid::Id16Width("operation_id"))
        );
    }

    #[test]
    fn member_seed_bounds_check_every_size() {
        let good = MemberSeedV1 {
            node_id: vec![0xDD; 32],
            ticket: "t".repeat(64),
            signed_join_permit: Some(vec![0; 1024]),
        };
        assert!(member_seed_bounds(&good).is_ok());
        assert_eq!(
            member_seed_bounds(&MemberSeedV1 {
                node_id: vec![0xDD; 31],
                ..good.clone()
            }),
            Err(WireInvalid::NodeIdWidth("member_seeds[].node_id"))
        );
        assert_eq!(
            member_seed_bounds(&MemberSeedV1 {
                ticket: "t".repeat(MAX_WIRE_STRING + 1),
                ..good.clone()
            }),
            Err(WireInvalid::Ticket)
        );
        assert_eq!(
            member_seed_bounds(&MemberSeedV1 {
                signed_join_permit: Some(vec![0; MAX_JOIN_PERMIT + 1]),
                ..good
            }),
            Err(WireInvalid::JoinPermit)
        );
    }

    // ── Golden vectors ──────────────────────────────────────────────────────

    /// The canonical encodings of one representative value per public root.
    ///
    /// If this fails, the wire changed. If the change is intended, deliberate,
    /// and recorded in the plan, update the vector from the printed block —
    /// otherwise the change is a break of the frozen §5.2 contract.
    #[test]
    fn golden_public_wire_vectors_are_unchanged() {
        let vectors: Vec<(&str, Vec<u8>)> = vec![
            (
                "AcquireRequestV1",
                payload_fory::<AcquireRequestV1>()
                    .serialize(&AcquireRequestV1 {
                        protocol_version: 1,
                        group_id: vec![0xAA; 32],
                        client_id: vec![0x11; 16],
                        sequence: 1,
                        resource: resource(),
                        ttl_ms: 30_000,
                    })
                    .expect("encode"),
            ),
            (
                "MutationReplyWireV1",
                payload_fory::<MutationReplyWireV1>()
                    .serialize(&MutationReplyWireV1 {
                        protocol_version: 1,
                        group_id: vec![0xAA; 32],
                        client_id: vec![0x11; 16],
                        sequence: 1,
                        request_hash: vec![0x33; 32],
                        disposition_code: 1,
                        outcome_code: 1,
                        reason_code: 0,
                        snapshot: None,
                        acl_revision: 0,
                        admin_revision: 0,
                        operation_id: None,
                        membership_log_id: None,
                    })
                    .expect("encode"),
            ),
            (
                "RetireClientRequestV1",
                payload_fory::<RetireClientRequestV1>()
                    .serialize(&RetireClientRequestV1 {
                        protocol_version: 1,
                        group_id: vec![0xAA; 32],
                        target_node_id: vec![0xDD; 32],
                        target_client_id: vec![0x11; 16],
                    })
                    .expect("encode"),
            ),
            (
                "GroupStatusReplyWireV1",
                payload_fory::<GroupStatusReplyWireV1>()
                    .serialize(&GroupStatusReplyWireV1 {
                        protocol_version: 1,
                        group_id: vec![0xAA; 32],
                        lifecycle_code: 3,
                        local_node_id: vec![0xDD; 32],
                        local_role_code: 2,
                        leader_node_id: None,
                        term: 3,
                        last_log_id: None,
                        committed_log_id: None,
                        applied_log_id: Some(log_id()),
                        deployment_profile_code: 1,
                        membership_log_id: Some(log_id()),
                        membership_joint: false,
                        voters: vec![NodeIdV1 {
                            node_id: vec![0xDD; 32],
                        }],
                        learners: Vec::new(),
                        pending_operation_id: None,
                        quorum_available: None,
                        fatal_code: 0,
                        audit_floor: None,
                        genesis_hash: vec![0xAB; 32],
                        authority_public_key: vec![0xCC; 32],
                        authority_domain: "wire.test".into(),
                    })
                    .expect("encode"),
            ),
            (
                "AuditRecordWireV1",
                payload_fory::<AuditRecordWireV1>()
                    .serialize(&AuditRecordWireV1 {
                        protocol_version: 1,
                        group_id: vec![0xAA; 32],
                        log_id: log_id(),
                        actor_node_id: None,
                        operation_code: 7,
                        resource: Some(resource()),
                        result_code: 6,
                        epoch: 7,
                        value_revision: 5,
                    })
                    .expect("encode"),
            ),
            (
                "RecoveryExportRequestV1",
                payload_fory::<RecoveryExportRequestV1>()
                    .serialize(&RecoveryExportRequestV1 {
                        protocol_version: 1,
                        group_id: vec![0xAA; 32],
                    })
                    .expect("encode"),
            ),
            (
                "RecoveryExportFrameV1::Manifest",
                payload_fory::<RecoveryExportFrameV1>()
                    .serialize(&RecoveryExportFrameV1::Manifest(
                        RecoveryExportManifestWireV1 {
                            protocol_version: 1,
                            group_id: vec![0xAA; 32],
                            total_len: 10,
                            blake3_checksum: vec![0x44; 32],
                            applied_log_id: Some(log_id()),
                        },
                    ))
                    .expect("encode"),
            ),
            (
                "RecoveryExportFrameV1::Chunk",
                payload_fory::<RecoveryExportFrameV1>()
                    .serialize(&RecoveryExportFrameV1::Chunk(RecoveryExportChunkWireV1 {
                        offset: 0,
                        data: b"0123456789".to_vec(),
                        final_chunk: true,
                    }))
                    .expect("encode"),
            ),
        ];

        let actual: Vec<(String, String)> = vectors
            .iter()
            .map(|(name, bytes)| ((*name).to_string(), to_hex(bytes)))
            .collect();

        let expected: Vec<(String, String)> = GOLDEN
            .iter()
            .map(|(n, h)| ((*n).to_string(), (*h).to_string()))
            .collect();

        if actual != expected {
            let rendered: String = actual
                .iter()
                .map(|(n, h)| format!("    (\"{n}\", \"{h}\"),\n"))
                .collect();
            panic!(
                "public wire encodings changed. If the change is intended and \
                 recorded, update GOLDEN to:\n{rendered}"
            );
        }
    }

    /// Pinned 2026-08-24 from the first encode of the frozen declarations;
    /// from then on it pins the §5.2 wire. Re-pinned 2026-08-25 for the §14
    /// closure: `GroupStatusReplyWireV1` gained the replicated `audit_floor`
    /// (id 19) and `genesis_hash` (id 20) trailing fields — a recorded,
    /// intended extension of the pre-release v1 reply; every other vector is
    /// byte-identical. Extended 2026-08-27 (S5-A) with the recovery-export
    /// request and both stream-frame variants; every prior vector is
    /// byte-identical.
    const GOLDEN: &[(&str, &str)] = &[
        ("AcquireRequestV1", "01ff1e002a70f78395aced17e64200926223f0471c88641a02641c6fcaea2df402851112762485092980d00ed80ec405c829cc29d41e01b0ea010220aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa10111111111111111111111111111111111e022080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c829226170702e68656164020102"),
        ("MutationReplyWireV1", "01ff1e00371050d40f629748ed4200926223f0471c88641a02641c6fcaea29759498268737b123d780d00ee80eec0ec405d805dc05e005c829cc29d429e61ef229f61e0100000202020020aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1011111111111111111111111111111111203333333333333333333333333333333333333333333333333333333333333333fdfdfd"),
        ("RetireClientRequestV1", "01ff1e002930d78a4e8e7a39e44200926223f0471c88641a02641c6fcaea3976249a224e89682367d8921424a6c405c829cc29d0290220aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa20dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd1011111111111111111111111111111111"),
        ("GroupStatusReplyWireV1",
            "01ff1e0055604d0eb6926176f64200926223f0471c88641a02641c6fcaea3174d1751fd94c13a4bb123d78f401dc0ec405cc05d405ec05fc0305fe0201c829d029da29e21ee61eea1ef21ef81678fc001678fe0129fe041efc0529fc0629fc071500030206040200fd20aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa20ddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddfdfdfdff1e021db0293536d64121e34200926223f0471c88641a02641c6fcaea124a70d106c40ecc0ec829032920ccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccff1e03032920cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc01081e041a60c8b14607e62de14200926223f0471c88641a02641c6fcaea164e70624418c42920dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd00fdfd20abababababababababababababababababababababababababababababababab20cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc26776972652e74657374"),
        ("AuditRecordWireV1", "01ff1e002e10e667386bd047e94200926223f0471c88641a02641c6fcaea25f4141a27d8904e88c0e00ee40ec405d405dc05c829cc1ed229da1e0705020e0c20aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1e021db0293536d64121e34200926223f0471c88641a02641c6fcaea124a70d106c40ecc0ec829032920ccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccfdff1e042080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c829226170702e68656164020102"),
        ("RecoveryExportRequestV1", "01ff1e002760f3c1a3a8e81ee24200926223f0471c88641a02641c6fcaea41f62413aa48e3a4bbdd19f62485092980c405c8290220aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ("RecoveryExportFrameV1::Manifest", "01ff230021f013608d4b9108054200926223f0471c88641a02641c6fcaea39762413aa48e3a4bbdd19f4b1030801001e022d40225367125543e54200926223f0471c88641a02641c6fcaea41762413aa48e3a4bbdd19f5806a0a494ccc0ec405c829d029d61e0a0220aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa204444444444444444444444444444444444444444444444444444444444444444ff1e041db0293536d64121e34200926223f0471c88641a02641c6fcaea124a70d106c40ecc0ec829032920cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"),
        ("RecoveryExportFrameV1::Chunk", "01ff230021f013608d4b9108054200926223f0471c88641a02641c6fcaea39762413aa48e3a4bbdd19f4b1030802001e022720dcc25fa4592fe34200926223f0471c88641a02641c6fcaea39762413aa48e3a4bbdd19f447a354cc01c40ec82901000a30313233343536373839"),
    ];
}
