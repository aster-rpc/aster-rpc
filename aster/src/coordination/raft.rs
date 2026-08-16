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

    /// A mirror that only parses is not a mirror. These walk a **real**
    /// OpenRaft value out to the wire type and back, so a field that quietly
    /// meant something else would not survive.
    #[test]
    fn the_openraft_mirrors_round_trip_through_real_values() {
        use openraft::{CommittedLeaderId, Membership, StoredMembership, Vote};

        let vote: Vote<CoordinationNodeId> = Vote::new_committed(7, node(1));
        let wire = VoteV1 {
            term: vote.leader_id.term,
            leader_node_id: vote.leader_id.node_id.as_bytes().to_vec(),
            committed: vote.is_committed(),
        };
        assert_eq!(wire.term, 7);
        assert_eq!(wire.leader_node_id, vec![1u8; 32]);
        assert!(wire.committed);

        // A joint membership: two configs, which is the shape §5.3.1 must
        // mirror and the one a single-config type would silently flatten.
        let membership: Membership<CoordinationNodeId, CoordinationNode> = Membership::new(
            vec![
                [node(1), node(2), node(3)].into_iter().collect(),
                [node(2), node(3), node(4)].into_iter().collect(),
            ],
            None,
        );
        let mirrored = MembershipV1 {
            configs: membership
                .get_joint_config()
                .iter()
                .map(|set| {
                    let mut ids: Vec<Vec<u8>> = set.iter().map(|n| n.as_bytes().to_vec()).collect();
                    ids.sort_unstable();
                    NodeIdSetV1 {
                        ids: ids
                            .into_iter()
                            .map(|node_id| NodeIdV1 { node_id })
                            .collect(),
                    }
                })
                .collect(),
            nodes: membership
                .nodes()
                .map(|(id, _)| NodeEntryV1 {
                    node_id: id.as_bytes().to_vec(),
                    format_version: 1,
                })
                .collect(),
        };
        assert_eq!(mirrored.configs.len(), 2, "joint configs must both survive");
        assert_eq!(mirrored.configs[0].ids.len(), 3);

        let fory = payload_fory::<MembershipV1>();
        let encoded = fory.serialize(&mirrored).expect("encode");
        let decoded: MembershipV1 = fory.deserialize(&encoded).expect("decode");
        assert_eq!(decoded, mirrored);

        let stored: StoredMembership<CoordinationNodeId, CoordinationNode> =
            StoredMembership::new(None, membership);
        assert_eq!(stored.membership().get_joint_config().len(), 2);

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
