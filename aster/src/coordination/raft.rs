//! §5.3.1 OpenRaft mirrors and §11's storage metadata root.
//!
//! OpenRaft's `serde` feature is **off**, so its own types never serialize
//! themselves. Every one that crosses the wire or reaches storage has an
//! Aster-owned Fory mirror here, converted at the boundary — which is what lets
//! the codec stay Fory throughout rather than becoming Fory-plus-serde.
//!
//! ## Round-tripped against the real OpenRaft values
//!
//! These types would compile perfectly while meaning something different from
//! the values they mirror, so each has a conversion pair and a test that walks
//! a real `openraft` value out and back. A mirror that merely *parses* is not a
//! mirror.
//!
//! ## Wrapper messages for unit variants
//!
//! §5.2.1 restricts union variants to message types, so `Blank` — which carries
//! nothing — still needs `BlankPayloadV1`. That is not ceremony: a
//! `UnionVariantDef` carries only a message `type_ref`, so a primitive or unit
//! variant is outside the cross-binding contract entirely.

use fory_derive::{ForyStruct, ForyUnion};

#[cfg(test)]
use super::contract::MAX_WIRE_STRING;
use super::contract::{ChangeMembershipActionV1, CommandV1, LogIdV1, SavedReplyV1};

/// `openraft::Vote { leader_id: LeaderId { term, node_id }, committed }`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/Vote")]
pub struct VoteV1 {
    #[fory(id = 1)]
    pub term: u64,
    #[fory(id = 2, bytes)]
    pub leader_node_id: Vec<u8>,
    #[fory(id = 3)]
    pub committed: bool,
}

/// A single NodeId, so a repeated one is a message rather than `Vec<Vec<u8>>`
/// (§5.2.1).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RaftNodeId")]
pub struct NodeIdV1 {
    #[fory(id = 1, bytes)]
    pub node_id: Vec<u8>,
}

/// One joint-configuration set. Sorted, so two equal memberships encode
/// identically.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/NodeIdSet")]
pub struct NodeIdSetV1 {
    #[fory(id = 1)]
    pub ids: Vec<NodeIdV1>,
}

/// One entry of OpenRaft's node-metadata map.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/NodeEntry")]
pub struct NodeEntryV1 {
    #[fory(id = 1, bytes)]
    pub node_id: Vec<u8>,
    /// `CoordinationNode` is empty in v1; the version lets it gain fields
    /// without changing this type's shape.
    #[fory(id = 2)]
    pub format_version: u32,
}

/// `openraft::Membership`: ordered joint configs plus the node map.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/Membership")]
pub struct MembershipV1 {
    #[fory(id = 1)]
    pub configs: Vec<NodeIdSetV1>,
    #[fory(id = 2)]
    pub nodes: Vec<NodeEntryV1>,
}

/// `openraft::StoredMembership { log_id, membership }`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/StoredMembership")]
pub struct StoredMembershipV1 {
    #[fory(id = 1)]
    pub log_id: Option<LogIdV1>,
    #[fory(id = 2)]
    pub membership: MembershipV1,
}

/// `openraft::SnapshotMeta { last_log_id, last_membership, snapshot_id }`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SnapshotMeta")]
pub struct SnapshotMetaV1 {
    #[fory(id = 1)]
    pub last_log_id: Option<LogIdV1>,
    #[fory(id = 2)]
    pub last_membership: StoredMembershipV1,
    #[fory(id = 3)]
    pub snapshot_id: String,
}

/// A unit variant still needs a message (§5.2.1).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/BlankPayload")]
pub struct BlankPayloadV1 {}

/// `openraft::EntryPayload`.
#[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/EntryPayload")]
pub enum EntryPayloadV1 {
    #[fory(id = 1)]
    Blank(BlankPayloadV1),
    #[fory(default)]
    #[fory(id = 2)]
    Normal(CommandV1),
    #[fory(id = 3)]
    Membership(MembershipV1),
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

/// `openraft::Entry`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RaftEntry")]
pub struct RaftEntryV1 {
    #[fory(id = 1)]
    pub log_id: LogIdV1,
    #[fory(id = 2)]
    pub payload: EntryPayloadV1,
}

// ── §5.3's RPC messages ─────────────────────────────────────────────────────

/// `openraft::VoteRequest`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/VoteRequest")]
pub struct VoteRequestV1 {
    #[fory(id = 1)]
    pub vote: VoteV1,
    #[fory(id = 2)]
    pub last_log_id: Option<LogIdV1>,
}

/// `openraft::VoteResponse`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/VoteResponse")]
pub struct VoteResponseV1 {
    #[fory(id = 1)]
    pub vote: VoteV1,
    #[fory(id = 2)]
    pub vote_granted: bool,
    #[fory(id = 3)]
    pub last_log_id: Option<LogIdV1>,
}

/// `openraft::AppendEntriesRequest`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AppendRequest")]
pub struct AppendRequestV1 {
    #[fory(id = 1)]
    pub vote: VoteV1,
    #[fory(id = 2)]
    pub prev_log_id: Option<LogIdV1>,
    #[fory(id = 3)]
    pub entries: Vec<RaftEntryV1>,
    #[fory(id = 4)]
    pub leader_commit: Option<LogIdV1>,
}

/// `AppendEntriesResponse::Success`. A unit variant still needs a message
/// (§5.2.1), so adding a field later is a field addition rather than a shape
/// change.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AppendSuccess")]
pub struct AppendSuccessV1 {}

/// `AppendEntriesResponse::PartialSuccess(Option<LogId>)`.
///
/// The matching log id is what OpenRaft resumes replication from, and `None` is
/// a distinct answer from `Some(..)` -- "nothing matched" rather than "matched
/// up to here". A mirror that elided this type would have lost the difference.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AppendPartialSuccess")]
pub struct AppendPartialSuccessV1 {
    #[fory(id = 1)]
    pub matching: Option<LogIdV1>,
}

/// `AppendEntriesResponse::Conflict`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AppendConflict")]
pub struct AppendConflictV1 {}

/// `AppendEntriesResponse::HigherVote(Vote)`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AppendHigherVote")]
pub struct AppendHigherVoteV1 {
    #[fory(id = 1)]
    pub vote: VoteV1,
}

/// `openraft::SnapshotResponse`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SnapshotResponse")]
pub struct SnapshotResponseV1 {
    #[fory(id = 1)]
    pub vote: VoteV1,
}

/// Every Raft message that crosses the wire, as one union (§5.3).
#[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RaftMessage")]
pub enum RaftMessageV1 {
    #[fory(default)]
    #[fory(id = 1)]
    VoteRequest(VoteRequestV1),
    #[fory(id = 2)]
    VoteResponse(VoteResponseV1),
    #[fory(id = 3)]
    AppendRequest(AppendRequestV1),
    #[fory(id = 4)]
    AppendSuccess(AppendSuccessV1),
    #[fory(id = 5)]
    AppendPartialSuccess(AppendPartialSuccessV1),
    #[fory(id = 6)]
    AppendConflict(AppendConflictV1),
    #[fory(id = 7)]
    AppendHigherVote(AppendHigherVoteV1),
    #[fory(id = 8)]
    SnapshotResponse(SnapshotResponseV1),
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

/// §5.3's unary envelope.
///
/// `payload` is bounded canonical Fory bytes rather than a nested
/// `RaftMessageV1`, because the receiver checks group, bootstrap state,
/// producer admission and the sender allowlist **before** decoding it: a node
/// that decoded first would be doing work on behalf of a sender it has not yet
/// agreed to talk to.
///
/// `sender_node_id` MUST equal the immutable Iroh transport NodeId. A response
/// uses the same field order with `responder_node_id` in place of the sender.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RaftEnvelope")]
pub struct RaftEnvelopeV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2)]
    pub raft_codec_version: i32,
    #[fory(id = 3, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 4, bytes)]
    pub sender_node_id: Vec<u8>,
    #[fory(id = 5, bytes)]
    pub payload: Vec<u8>,
}

/// §5.3's unary response envelope.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RaftResponseEnvelope")]
pub struct RaftResponseEnvelopeV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2)]
    pub raft_codec_version: i32,
    #[fory(id = 3, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 4, bytes)]
    pub responder_node_id: Vec<u8>,
    #[fory(id = 5, bytes)]
    pub payload: Vec<u8>,
}

// ── §5.3's snapshot stream ──────────────────────────────────────────────────

/// The first and only header frame of an `install_snapshot` stream.
///
/// `vote_payload` and `snapshot_meta_payload` are bounded canonical Fory bytes
/// for the same reason [`RaftEnvelopeV1::payload`] is: the receiver checks
/// group, sender and bounds before decoding anything it was sent.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RaftSnapshotHeader")]
pub struct RaftSnapshotHeaderWireV1 {
    #[fory(id = 1)]
    pub protocol_version: i32,
    #[fory(id = 2)]
    pub raft_codec_version: i32,
    #[fory(id = 3, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 4, bytes)]
    pub sender_node_id: Vec<u8>,
    /// A canonical `VoteV1`.
    #[fory(id = 5, bytes)]
    pub vote_payload: Vec<u8>,
    /// A canonical `SnapshotMetaV1`.
    #[fory(id = 6, bytes)]
    pub snapshot_meta_payload: Vec<u8>,
    #[fory(id = 7)]
    pub total_len: u64,
    /// Exactly 32 bytes.
    #[fory(id = 8, bytes)]
    pub blake3_checksum: Vec<u8>,
}

/// One body frame: strictly increasing `offset`, at most 1 MiB of `data`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RaftSnapshotChunk")]
pub struct RaftSnapshotChunkWireV1 {
    #[fory(id = 1)]
    pub offset: u64,
    #[fory(id = 2, bytes)]
    pub data: Vec<u8>,
    #[fory(id = 3)]
    pub final_chunk: bool,
}

/// The snapshot request stream: exactly two variants, never renumbered.
#[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RaftSnapshotFrame")]
pub enum RaftSnapshotFrameV1 {
    #[fory(default)]
    #[fory(id = 1)]
    Header(RaftSnapshotHeaderWireV1),
    #[fory(id = 2)]
    Chunk(RaftSnapshotChunkWireV1),
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

// ── §7.1's signed documents ─────────────────────────────────────────────────

/// One initial voter and the ticket it is reachable at.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/VoterSeed")]
pub struct VoterSeedV1 {
    #[fory(id = 1, bytes)]
    pub node_id: Vec<u8>,
    /// At most 4 KiB, and only in the Open profile.
    #[fory(id = 2)]
    pub ticket: String,
}

/// The signed genesis document (§7.1).
///
/// The genesis hash is BLAKE3 over the domain-separated **signed** document, and
/// it is persisted in both databases: reusing a group id under a different
/// genesis hash is a hard startup failure rather than a recoverable mismatch.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/Genesis")]
pub struct GenesisV1 {
    #[fory(id = 1)]
    pub format_version: u32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    /// 1..=128 ASCII. Present here, and in [`JoinPermitV1`], because §7.1
    /// requires every signed document's domain to equal the configured
    /// expectation -- a document without the field cannot satisfy that rule.
    #[fory(id = 3)]
    pub authority_domain: String,
    #[fory(id = 4, bytes)]
    pub genesis_nonce: Vec<u8>,
    /// 1 Prod | 2 DevSingle | 3 DevEven.
    #[fory(id = 5)]
    pub deployment_profile_code: i32,
    /// Sorted by node id.
    #[fory(id = 6)]
    pub initial_voters: Vec<VoterSeedV1>,
    /// Sorted, 1..=64.
    #[fory(id = 7)]
    pub initial_admins: Vec<NodeIdV1>,
    #[fory(id = 8, bytes)]
    pub recovery_export_hash: Option<Vec<u8>>,
}

/// The signed join permit (§7.1).
///
/// Deliberately has **no** wall-clock expiry: time is not a portable bootstrap
/// authority. A permit adds no member on its own -- an existing administrator
/// must still commit a membership intent through the current quorum -- so it is
/// revoked operationally by refusing that plan.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/JoinPermit")]
pub struct JoinPermitV1 {
    #[fory(id = 1)]
    pub format_version: u32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3)]
    pub authority_domain: String,
    #[fory(id = 4, bytes)]
    pub genesis_hash: Vec<u8>,
    #[fory(id = 5, bytes)]
    pub candidate_node_id: Vec<u8>,
    /// At most 4 KiB, and only in the Open profile.
    #[fory(id = 6)]
    pub candidate_ticket: String,
    /// Sorted and non-empty: until the candidate applies a membership containing
    /// itself, these are the only senders it accepts Raft traffic from.
    #[fory(id = 7)]
    pub bootstrap_senders: Vec<NodeIdV1>,
    #[fory(id = 8, bytes)]
    pub join_nonce: Vec<u8>,
}

/// The signature envelope around a genesis or join document.
///
/// The inner document is carried as **opaque bytes**, not as a typed field.
/// Verification has to sign and check the exact bytes received; a typed field
/// would invite a decode/re-encode round trip whose output could differ from
/// its input, and §7.1's re-encode check compares against these bytes.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SignedDocument")]
pub struct SignedCoordinationDocumentV1 {
    #[fory(id = 1)]
    pub format_version: u32,
    #[fory(id = 2, bytes)]
    pub document_bytes: Vec<u8>,
    /// Exactly 32 bytes.
    #[fory(id = 3, bytes)]
    pub authority_public_key: Vec<u8>,
    /// Exactly 64 bytes.
    #[fory(id = 4, bytes)]
    pub signature: Vec<u8>,
}

/// §13's recovery provenance, referenced by [`CoordinationMetaV1`].
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RecoveredFrom")]
pub struct RecoveredFromV1 {
    #[fory(id = 1, bytes)]
    pub source_group_id: Vec<u8>,
    #[fory(id = 2, bytes)]
    pub source_genesis_hash: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub export_hash: Vec<u8>,
}

/// §11's `coordination_meta` root: everything the store needs to know about
/// itself, in one row.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/CoordinationMeta")]
pub struct CoordinationMetaV1 {
    #[fory(id = 1)]
    pub schema_version: u32,
    #[fory(id = 2, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub genesis_hash: Vec<u8>,
    #[fory(id = 4, bytes)]
    pub authority_public_key: Vec<u8>,
    #[fory(id = 5)]
    pub authority_domain: String,
    #[fory(id = 6)]
    pub deployment_profile_code: i32,
    #[fory(id = 7)]
    pub last_applied: Option<LogIdV1>,
    #[fory(id = 8)]
    pub membership: StoredMembershipV1,
    #[fory(id = 9)]
    pub admin_revision: u64,
    #[fory(id = 10, bytes)]
    pub recovery_export_hash: Option<Vec<u8>>,
    #[fory(id = 11)]
    pub recovered_from: Option<RecoveredFromV1>,
    #[fory(id = 12)]
    pub aggregate_growth_bytes: u64,
    #[fory(id = 13)]
    pub aggregate_reserve_bytes: u64,
    #[fory(id = 14)]
    pub audit_floor: Option<LogIdV1>,
}

/// A stored membership intent (§11), typed rather than opaque so a snapshot
/// that cannot type-check a plan cannot install an unreadable intent.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/MembershipIntent")]
pub struct MembershipIntentV1 {
    #[fory(id = 1, bytes)]
    pub actor_node_id: Vec<u8>,
    #[fory(id = 2, bytes)]
    pub request_hash: Vec<u8>,
    #[fory(id = 3)]
    pub expected_membership: Option<LogIdV1>,
    #[fory(id = 4)]
    pub desired_plan: ChangeMembershipActionV1,
    #[fory(id = 5)]
    pub status_code: i32,
    #[fory(id = 6)]
    pub result_code: i32,
    #[fory(id = 7)]
    pub accepted_log_id: LogIdV1,
    #[fory(id = 8)]
    pub completed_log_id: Option<LogIdV1>,
    #[fory(id = 9)]
    pub observed_membership_log_id: Option<LogIdV1>,
}

/// A stored resource row (§11).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ResourceRow")]
pub struct ResourceRowV1 {
    #[fory(id = 1)]
    pub epoch: u64,
    #[fory(id = 2)]
    pub held: bool,
    #[fory(id = 3, bytes)]
    pub holder_node_id: Option<Vec<u8>>,
    #[fory(id = 4)]
    pub ttl_ms: u64,
    #[fory(id = 5)]
    pub lease_revision: u64,
    #[fory(id = 6)]
    pub value_revision: u64,
    #[fory(id = 7, bytes)]
    pub value: Vec<u8>,
    #[fory(id = 8)]
    pub acl_revision: u64,
}

/// A stored client session (§11).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ClientSession")]
pub struct ClientSessionV1 {
    #[fory(id = 1)]
    pub last_sequence: u64,
    #[fory(id = 2, bytes)]
    pub request_hash: Vec<u8>,
    /// `None` only for a sequence-0 tombstone.
    #[fory(id = 3)]
    pub last_result: Option<SavedReplyV1>,
    #[fory(id = 4)]
    pub retired: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::contract::payload_fory;
    use crate::coordination::ids::CoordinationNodeId;
    use crate::coordination::typeconfig::CoordinationNode;

    fn node(byte: u8) -> CoordinationNodeId {
        CoordinationNodeId::from_array([byte; 32])
    }

    /// Every declared root registers and round-trips through its own Fory
    /// runtime. A declaration that cannot build a runtime is not a declaration.
    #[test]
    fn every_declared_root_round_trips() {
        macro_rules! round_trip {
            ($ty:ty, $value:expr) => {{
                let fory = payload_fory::<$ty>();
                let encoded = fory.serialize(&$value).expect("encode");
                let decoded: $ty = fory.deserialize(&encoded).expect("decode");
                assert_eq!(decoded, $value, concat!(stringify!($ty), " round trip"));
            }};
        }

        let log = LogIdV1 {
            term: 3,
            leader_node_id: vec![1u8; 32],
            index: 9,
        };
        let membership = MembershipV1 {
            configs: vec![NodeIdSetV1 {
                ids: vec![NodeIdV1 {
                    node_id: vec![2u8; 32],
                }],
            }],
            nodes: vec![NodeEntryV1 {
                node_id: vec![2u8; 32],
                format_version: 1,
            }],
        };
        let stored = StoredMembershipV1 {
            log_id: Some(log.clone()),
            membership: membership.clone(),
        };

        round_trip!(
            VoteV1,
            VoteV1 {
                term: 4,
                leader_node_id: vec![3u8; 32],
                committed: true,
            }
        );
        round_trip!(MembershipV1, membership.clone());
        round_trip!(StoredMembershipV1, stored.clone());
        round_trip!(
            SnapshotMetaV1,
            SnapshotMetaV1 {
                last_log_id: Some(log.clone()),
                last_membership: stored.clone(),
                snapshot_id: "snap-1".into(),
            }
        );
        round_trip!(
            RaftEntryV1,
            RaftEntryV1 {
                log_id: log.clone(),
                payload: EntryPayloadV1::Blank(BlankPayloadV1 {}),
            }
        );
        round_trip!(
            RaftEntryV1,
            RaftEntryV1 {
                log_id: log.clone(),
                payload: EntryPayloadV1::Membership(membership.clone()),
            }
        );
        round_trip!(
            CoordinationMetaV1,
            CoordinationMetaV1 {
                schema_version: 1,
                group_id: vec![4u8; 32],
                genesis_hash: vec![5u8; 32],
                authority_public_key: vec![6u8; 32],
                authority_domain: "example.portal.roaming".into(),
                deployment_profile_code: 1,
                last_applied: Some(log.clone()),
                membership: stored.clone(),
                admin_revision: 2,
                recovery_export_hash: None,
                recovered_from: Some(RecoveredFromV1 {
                    source_group_id: vec![7u8; 32],
                    source_genesis_hash: vec![8u8; 32],
                    export_hash: vec![9u8; 32],
                }),
                aggregate_growth_bytes: 1234,
                aggregate_reserve_bytes: 5678,
                audit_floor: Some(log.clone()),
            }
        );
        round_trip!(
            ResourceRowV1,
            ResourceRowV1 {
                epoch: 1,
                held: true,
                holder_node_id: Some(vec![1u8; 32]),
                ttl_ms: 30_000,
                lease_revision: 2,
                value_revision: 3,
                value: b"v".to_vec(),
                acl_revision: 4,
            }
        );
        round_trip!(
            ClientSessionV1,
            ClientSessionV1 {
                last_sequence: 5,
                request_hash: vec![0xAA; 32],
                last_result: None,
                retired: true,
            }
        );
    }

    /// Every wire root round-trips at its **empty** and its **maximal** shape,
    /// not only at a convenient middle one. Empty collections and absent
    /// options are where a codec most often substitutes a default, and the
    /// maximal shape is where a bound is either enforced or discovered late.
    #[test]
    fn wire_roots_round_trip_at_their_empty_and_maximal_shapes() {
        macro_rules! round_trip {
            ($ty:ty, $value:expr) => {{
                let fory = payload_fory::<$ty>();
                let value = $value;
                let encoded = fory.serialize(&value).expect("encode");
                let decoded: $ty = fory.deserialize(&encoded).expect("decode");
                assert_eq!(decoded, value, concat!(stringify!($ty), " round trip"));
            }};
        }

        // Empty shapes: no entries, no voters, no admins, every option absent.
        round_trip!(
            AppendRequestV1,
            AppendRequestV1 {
                vote: VoteV1::default(),
                prev_log_id: None,
                entries: Vec::new(),
                leader_commit: None,
            }
        );
        round_trip!(
            AppendPartialSuccessV1,
            AppendPartialSuccessV1 { matching: None }
        );
        round_trip!(AppendSuccessV1, AppendSuccessV1 {});
        round_trip!(AppendConflictV1, AppendConflictV1 {});
        round_trip!(
            MembershipV1,
            MembershipV1 {
                configs: Vec::new(),
                nodes: Vec::new(),
            }
        );
        round_trip!(
            RaftSnapshotChunkWireV1,
            RaftSnapshotChunkWireV1 {
                offset: 0,
                data: Vec::new(),
                final_chunk: false,
            }
        );

        // Maximal shapes: the declared ceilings, so a bound that is only a
        // comment fails here rather than in production.
        // One byte under 4 KiB, deliberately: see
        // `contract::tests::a_string_field_of_4096_bytes_does_not_decode`.
        const MAX_TICKET: usize = MAX_WIRE_STRING;
        const MAX_DOMAIN: usize = 128;
        const MAX_CHUNK: usize = 1024 * 1024;

        round_trip!(
            GenesisV1,
            GenesisV1 {
                format_version: 1,
                group_id: vec![0xFFu8; 32],
                authority_domain: "d".repeat(MAX_DOMAIN),
                genesis_nonce: vec![0xFFu8; 32],
                deployment_profile_code: 3,
                initial_voters: (0..5u8)
                    .map(|i| VoterSeedV1 {
                        node_id: vec![i; 32],
                        ticket: "t".repeat(MAX_TICKET),
                    })
                    .collect(),
                initial_admins: (0..64u8)
                    .map(|i| NodeIdV1 {
                        node_id: vec![i; 32],
                    })
                    .collect(),
                recovery_export_hash: Some(vec![0xABu8; 32]),
            }
        );
        round_trip!(
            JoinPermitV1,
            JoinPermitV1 {
                format_version: 1,
                group_id: vec![0xFFu8; 32],
                authority_domain: "d".repeat(MAX_DOMAIN),
                genesis_hash: vec![0x11u8; 32],
                candidate_node_id: vec![0x22u8; 32],
                candidate_ticket: "t".repeat(MAX_TICKET),
                bootstrap_senders: (0..5u8)
                    .map(|i| NodeIdV1 {
                        node_id: vec![i; 32]
                    })
                    .collect(),
                join_nonce: vec![0x33u8; 32],
            }
        );
        round_trip!(
            RaftSnapshotChunkWireV1,
            RaftSnapshotChunkWireV1 {
                offset: u64::MAX,
                data: vec![0x5Au8; MAX_CHUNK],
                final_chunk: true,
            }
        );
        round_trip!(
            RaftSnapshotHeaderWireV1,
            RaftSnapshotHeaderWireV1 {
                protocol_version: 1,
                raft_codec_version: 1,
                group_id: vec![0xAAu8; 32],
                sender_node_id: vec![0xBBu8; 32],
                vote_payload: vec![1u8; 64],
                snapshot_meta_payload: vec![2u8; 256],
                total_len: u64::MAX,
                blake3_checksum: vec![0xCCu8; 32],
            }
        );
        round_trip!(
            SignedCoordinationDocumentV1,
            SignedCoordinationDocumentV1 {
                format_version: 1,
                document_bytes: vec![0x7Fu8; 8192],
                authority_public_key: vec![0xDDu8; 32],
                signature: vec![0xEEu8; 64],
            }
        );
        round_trip!(
            RaftEnvelopeV1,
            RaftEnvelopeV1 {
                protocol_version: 1,
                raft_codec_version: 1,
                group_id: vec![0xAAu8; 32],
                sender_node_id: vec![0xBBu8; 32],
                payload: vec![0u8; 4096],
            }
        );
        round_trip!(
            RaftResponseEnvelopeV1,
            RaftResponseEnvelopeV1 {
                protocol_version: 1,
                raft_codec_version: 1,
                group_id: vec![0xAAu8; 32],
                responder_node_id: vec![0xBBu8; 32],
                payload: Vec::new(),
            }
        );
    }

    /// The snapshot stream's two frames are one union, and a header must not be
    /// readable as a chunk. Variant ids are wire contract: renumbering them
    /// would make an old sender's header decode as somebody's chunk data.
    #[test]
    fn snapshot_frames_stay_distinguishable() {
        let header = RaftSnapshotFrameV1::Header(RaftSnapshotHeaderWireV1 {
            protocol_version: 1,
            raft_codec_version: 1,
            group_id: vec![0xAAu8; 32],
            sender_node_id: vec![0xBBu8; 32],
            vote_payload: Vec::new(),
            snapshot_meta_payload: Vec::new(),
            total_len: 10,
            blake3_checksum: vec![0u8; 32],
        });
        let chunk = RaftSnapshotFrameV1::Chunk(RaftSnapshotChunkWireV1 {
            offset: 0,
            data: b"0123456789".to_vec(),
            final_chunk: true,
        });

        let fory = payload_fory::<RaftSnapshotFrameV1>();
        for frame in [&header, &chunk] {
            let decoded: RaftSnapshotFrameV1 = fory
                .deserialize(&fory.serialize(frame).expect("encode"))
                .expect("decode");
            assert_eq!(&decoded, frame);
        }
        assert_ne!(
            fory.serialize(&header).expect("encode"),
            fory.serialize(&chunk).expect("encode")
        );
    }

    /// A mirror that only parses is not a mirror. This walks **real** OpenRaft
    /// values out to the wire type and back through
    /// [`crate::coordination::convert`], which is the conversion storage will
    /// use -- an earlier revision mapped the fields by hand inside the test, so
    /// it proved the test author understood the shape rather than that the code
    /// did.
    #[test]
    fn the_openraft_mirrors_round_trip_through_real_values() {
        use openraft::{CommittedLeaderId, Membership, StoredMembership, Vote};

        let vote: Vote<CoordinationNodeId> = Vote::new_committed(7, node(1));
        let wire = VoteV1::from(&vote);
        assert_eq!(wire.term, 7);
        assert_eq!(wire.leader_node_id, vec![1u8; 32]);
        assert!(wire.committed);
        assert_eq!(Vote::try_from(&wire).expect("well formed"), vote);

        // A joint membership: two configs, which is the shape §5.3.1 must
        // mirror and the one a single-config type would silently flatten.
        let membership: Membership<CoordinationNodeId, CoordinationNode> = Membership::new(
            vec![
                [node(1), node(2), node(3)].into_iter().collect(),
                [node(2), node(3), node(4)].into_iter().collect(),
            ],
            None,
        );
        let mirrored = MembershipV1::from(&membership);
        assert_eq!(mirrored.configs.len(), 2, "joint configs must both survive");
        assert_eq!(mirrored.configs[0].ids.len(), 3);

        let fory = payload_fory::<MembershipV1>();
        let encoded = fory.serialize(&mirrored).expect("encode");
        let decoded: MembershipV1 = fory.deserialize(&encoded).expect("decode");
        assert_eq!(decoded, mirrored);
        assert_eq!(
            Membership::try_from(&decoded)
                .expect("well formed")
                .get_joint_config(),
            membership.get_joint_config(),
            "through the wire and back, unchanged"
        );

        let stored: StoredMembership<CoordinationNodeId, CoordinationNode> =
            StoredMembership::new(None, membership);
        let back =
            StoredMembership::try_from(&StoredMembershipV1::from(&stored)).expect("well formed");
        assert_eq!(back.membership().get_joint_config().len(), 2);
        assert_eq!(back.log_id(), stored.log_id());

        // The committed leader id retains its node under this feature set --
        // asserted here as well as in `model::wire`, because these mirrors
        // would silently lose it too.
        let leader = CommittedLeaderId::new(7, node(5));
        assert_eq!(leader.node_id.as_bytes(), &[5u8; 32]);
    }

    /// A `Normal` entry carries a whole `CommandV1`, so the log's payload and
    /// the state machine's input are the same type rather than two that must be
    /// kept in step.
    #[test]
    fn a_normal_entry_carries_a_command() {
        use crate::coordination::contract::{ExpireCommandV1, ResourceKeyV1};

        let entry = RaftEntryV1 {
            log_id: LogIdV1 {
                term: 1,
                leader_node_id: vec![1u8; 32],
                index: 1,
            },
            payload: EntryPayloadV1::Normal(CommandV1::Expire(ExpireCommandV1 {
                resource: ResourceKeyV1 {
                    namespace: "ns".into(),
                    key: b"k".to_vec(),
                },
                epoch: 1,
                lease_revision: 1,
            })),
        };
        let fory = payload_fory::<RaftEntryV1>();
        let encoded = fory.serialize(&entry).expect("encode");
        let decoded: RaftEntryV1 = fory.deserialize(&encoded).expect("decode");
        assert_eq!(decoded, entry);
    }
}
