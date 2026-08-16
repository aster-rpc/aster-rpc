//! Conversions between OpenRaft values, the §5.3.1 wire mirrors, and the model.
//!
//! The mirrors in [`super::raft`] declare the wire shape; this module is what
//! makes them *mirrors* rather than lookalikes. Slice 2's storage layer has to
//! move real `openraft::Entry`, `Vote`, `Membership` and `SnapshotMeta` values
//! through redb, and the only way a field cannot quietly come to mean something
//! else is for one conversion to be the single path in each direction.
//!
//! Every inbound conversion is fallible and every failure is named. Bytes read
//! back from a database or a peer are not trusted to be a node id merely because
//! they occupy the field where one belongs: a 31-byte `leader_node_id` is a
//! corrupt record, and silently zero-padding it would mint a log id attributable
//! to a leader that never existed.

use openraft::{CommittedLeaderId, Entry, EntryPayload, LogId, Membership, SnapshotMeta, Vote};

use super::contract::LogIdV1;
use super::ids::CoordinationNodeId;
use super::raft::{
    BlankPayloadV1, EntryPayloadV1, MembershipV1, NodeEntryV1, NodeIdSetV1, NodeIdV1, RaftEntryV1,
    SnapshotMetaV1, StoredMembershipV1, VoteV1,
};
use super::typeconfig::{CoordinationNode, CoordinationTypeConfig};

/// Why a wire mirror could not be read back as an OpenRaft or model value.
///
/// Distinguished by field, not collapsed into one "malformed" case: a node id of
/// the wrong length and a membership with no configs are different corruptions,
/// and a recovery operator needs to know which one the store holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MirrorError {
    /// A node id field was not exactly 32 bytes.
    NodeIdLength { field: &'static str, len: usize },
    /// A membership carried no configs. OpenRaft's invariant is at least one.
    EmptyMembership,
    /// A `NodeEntry` declared a format version this build does not mirror.
    UnknownNodeFormat(u32),
    /// An entry payload variant this build does not know (§5.2.1's `Unknown`).
    ///
    /// Reachable only from a peer or a store written by a newer version. A log
    /// entry whose payload cannot be interpreted must not be applied as a blank:
    /// that would commit a gap in the replicated history.
    UnknownPayloadVariant,
}

/// The `format_version` stamped on every mirrored node entry in v1.
pub const NODE_FORMAT_VERSION: u32 = 1;

fn node_id(field: &'static str, bytes: &[u8]) -> Result<CoordinationNodeId, MirrorError> {
    CoordinationNodeId::from_slice(bytes).map_err(|_| MirrorError::NodeIdLength {
        field,
        len: bytes.len(),
    })
}

// ── LogId ───────────────────────────────────────────────────────────────────

// The model's own `LogId` pairs live in [`super::model::wire`], which owns the
// model boundary; this module owns the OpenRaft one. Both directions are
// exercised together in `log_id_conversions_preserve_the_leader`, because the
// two paths must agree on what a leader is.

impl From<LogId<CoordinationNodeId>> for LogIdV1 {
    fn from(id: LogId<CoordinationNodeId>) -> Self {
        Self {
            term: id.leader_id.term,
            leader_node_id: id.leader_id.node_id.to_vec(),
            index: id.index,
        }
    }
}

impl TryFrom<&LogIdV1> for LogId<CoordinationNodeId> {
    type Error = MirrorError;

    fn try_from(wire: &LogIdV1) -> Result<Self, Self::Error> {
        Ok(LogId::new(
            CommittedLeaderId::new(
                wire.term,
                node_id("log_id.leader_node_id", &wire.leader_node_id)?,
            ),
            wire.index,
        ))
    }
}

// ── Vote ────────────────────────────────────────────────────────────────────

impl From<&Vote<CoordinationNodeId>> for VoteV1 {
    fn from(vote: &Vote<CoordinationNodeId>) -> Self {
        Self {
            term: vote.leader_id.term,
            leader_node_id: vote.leader_id.node_id.to_vec(),
            committed: vote.is_committed(),
        }
    }
}

impl TryFrom<&VoteV1> for Vote<CoordinationNodeId> {
    type Error = MirrorError;

    fn try_from(wire: &VoteV1) -> Result<Self, Self::Error> {
        let node = node_id("vote.leader_node_id", &wire.leader_node_id)?;
        Ok(if wire.committed {
            Vote::new_committed(wire.term, node)
        } else {
            Vote::new(wire.term, node)
        })
    }
}

// ── Membership ──────────────────────────────────────────────────────────────

impl From<&Membership<CoordinationNodeId, CoordinationNode>> for MembershipV1 {
    fn from(membership: &Membership<CoordinationNodeId, CoordinationNode>) -> Self {
        Self {
            // Sorted within each config: OpenRaft holds a set, and two voters
            // must produce byte-identical encodings of the same membership.
            configs: membership
                .get_joint_config()
                .iter()
                .map(|set| {
                    let mut ids: Vec<Vec<u8>> = set.iter().map(|n| n.to_vec()).collect();
                    ids.sort_unstable();
                    NodeIdSetV1 {
                        ids: ids
                            .into_iter()
                            .map(|node_id| NodeIdV1 { node_id })
                            .collect(),
                    }
                })
                .collect(),
            nodes: {
                let mut nodes: Vec<NodeEntryV1> = membership
                    .nodes()
                    .map(|(id, _)| NodeEntryV1 {
                        node_id: id.to_vec(),
                        format_version: NODE_FORMAT_VERSION,
                    })
                    .collect();
                nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
                nodes
            },
        }
    }
}

impl TryFrom<&MembershipV1> for Membership<CoordinationNodeId, CoordinationNode> {
    type Error = MirrorError;

    fn try_from(wire: &MembershipV1) -> Result<Self, Self::Error> {
        if wire.configs.is_empty() {
            return Err(MirrorError::EmptyMembership);
        }
        let mut configs = Vec::with_capacity(wire.configs.len());
        for set in &wire.configs {
            let mut ids = std::collections::BTreeSet::new();
            for id in &set.ids {
                ids.insert(node_id("membership.config", &id.node_id)?);
            }
            configs.push(ids);
        }
        let mut nodes = std::collections::BTreeMap::new();
        for entry in &wire.nodes {
            if entry.format_version != NODE_FORMAT_VERSION {
                return Err(MirrorError::UnknownNodeFormat(entry.format_version));
            }
            nodes.insert(
                node_id("membership.node", &entry.node_id)?,
                CoordinationNode,
            );
        }
        // Every id named by a config must appear in the node map, which is
        // OpenRaft's own invariant; supplying the default for a missing one
        // keeps the reconstruction total rather than panicking inside the
        // library on a corrupt record.
        for set in &configs {
            for id in set {
                nodes.entry(*id).or_insert(CoordinationNode);
            }
        }
        Ok(Membership::new(configs, nodes))
    }
}

impl From<&openraft::StoredMembership<CoordinationNodeId, CoordinationNode>>
    for StoredMembershipV1
{
    fn from(stored: &openraft::StoredMembership<CoordinationNodeId, CoordinationNode>) -> Self {
        Self {
            log_id: stored.log_id().map(LogIdV1::from),
            membership: MembershipV1::from(stored.membership()),
        }
    }
}

impl TryFrom<&StoredMembershipV1>
    for openraft::StoredMembership<CoordinationNodeId, CoordinationNode>
{
    type Error = MirrorError;

    fn try_from(wire: &StoredMembershipV1) -> Result<Self, Self::Error> {
        let log_id = wire
            .log_id
            .as_ref()
            .map(LogId::<CoordinationNodeId>::try_from)
            .transpose()?;
        Ok(openraft::StoredMembership::new(
            log_id,
            Membership::try_from(&wire.membership)?,
        ))
    }
}

// ── SnapshotMeta ────────────────────────────────────────────────────────────

impl From<&SnapshotMeta<CoordinationNodeId, CoordinationNode>> for SnapshotMetaV1 {
    fn from(meta: &SnapshotMeta<CoordinationNodeId, CoordinationNode>) -> Self {
        Self {
            last_log_id: meta.last_log_id.map(LogIdV1::from),
            last_membership: StoredMembershipV1::from(&meta.last_membership),
            snapshot_id: meta.snapshot_id.clone(),
        }
    }
}

impl TryFrom<&SnapshotMetaV1> for SnapshotMeta<CoordinationNodeId, CoordinationNode> {
    type Error = MirrorError;

    fn try_from(wire: &SnapshotMetaV1) -> Result<Self, Self::Error> {
        Ok(SnapshotMeta {
            last_log_id: wire
                .last_log_id
                .as_ref()
                .map(LogId::<CoordinationNodeId>::try_from)
                .transpose()?,
            last_membership: openraft::StoredMembership::try_from(&wire.last_membership)?,
            snapshot_id: wire.snapshot_id.clone(),
        })
    }
}

// ── Entry ───────────────────────────────────────────────────────────────────

impl From<&Entry<CoordinationTypeConfig>> for RaftEntryV1 {
    fn from(entry: &Entry<CoordinationTypeConfig>) -> Self {
        Self {
            log_id: LogIdV1::from(entry.log_id),
            payload: match &entry.payload {
                EntryPayload::Blank => EntryPayloadV1::Blank(BlankPayloadV1 {}),
                EntryPayload::Normal(command) => EntryPayloadV1::Normal(command.clone()),
                EntryPayload::Membership(membership) => {
                    EntryPayloadV1::Membership(MembershipV1::from(membership))
                }
            },
        }
    }
}

impl TryFrom<&RaftEntryV1> for Entry<CoordinationTypeConfig> {
    type Error = MirrorError;

    fn try_from(wire: &RaftEntryV1) -> Result<Self, Self::Error> {
        Ok(Entry {
            log_id: LogId::try_from(&wire.log_id)?,
            payload: match &wire.payload {
                EntryPayloadV1::Blank(_) => EntryPayload::Blank,
                EntryPayloadV1::Normal(command) => EntryPayload::Normal(command.clone()),
                EntryPayloadV1::Membership(membership) => {
                    EntryPayload::Membership(Membership::try_from(membership)?)
                }
                EntryPayloadV1::Unknown(_) => return Err(MirrorError::UnknownPayloadVariant),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::contract::{payload_fory, CommandV1};
    use crate::coordination::contract::{ExpireCommandV1, ResourceKeyV1};
    use crate::coordination::model::state::LogId as ModelLogId;

    fn node(b: u8) -> CoordinationNodeId {
        CoordinationNodeId::from_array([b; 32])
    }

    fn membership() -> Membership<CoordinationNodeId, CoordinationNode> {
        Membership::new(
            vec![
                [node(1), node(2), node(3)].into_iter().collect(),
                [node(2), node(3), node(4)].into_iter().collect(),
            ],
            None,
        )
    }

    /// The round trip that matters: a real OpenRaft value out and back, equal to
    /// what went in. A mirror that only parses would pass an encode/decode test
    /// and fail this one.
    #[test]
    fn vote_round_trips_through_the_mirror() {
        for vote in [
            Vote::new_committed(7, node(1)),
            Vote::new(9, node(2)),
            Vote::new(0, node(0)),
        ] {
            let wire = VoteV1::from(&vote);
            assert_eq!(Vote::try_from(&wire).expect("a well-formed vote"), vote);
        }
    }

    /// Committed and uncommitted votes are different values, and a mirror that
    /// dropped the flag would round-trip one as the other.
    #[test]
    fn the_committed_flag_survives_the_mirror() {
        let committed = Vote::new_committed(7, node(1));
        let uncommitted: Vote<CoordinationNodeId> = Vote::new(7, node(1));
        assert_ne!(committed, uncommitted);
        assert_ne!(VoteV1::from(&committed), VoteV1::from(&uncommitted));
        assert_eq!(
            Vote::try_from(&VoteV1::from(&uncommitted)).expect("well formed"),
            uncommitted
        );
    }

    #[test]
    fn joint_membership_round_trips_with_both_configs() {
        let m = membership();
        let wire = MembershipV1::from(&m);
        assert_eq!(wire.configs.len(), 2, "a joint config is two configs");
        let back = Membership::try_from(&wire).expect("a well-formed membership");
        assert_eq!(back.get_joint_config(), m.get_joint_config());
        assert_eq!(back.nodes().count(), m.nodes().count());
    }

    #[test]
    fn stored_membership_and_snapshot_meta_round_trip() {
        let stored = openraft::StoredMembership::new(
            Some(LogId::new(CommittedLeaderId::new(3, node(9)), 42)),
            membership(),
        );
        let back = openraft::StoredMembership::try_from(&StoredMembershipV1::from(&stored))
            .expect("well formed");
        assert_eq!(back.log_id(), stored.log_id());
        assert_eq!(
            back.membership().get_joint_config(),
            stored.membership().get_joint_config()
        );

        let meta = SnapshotMeta {
            last_log_id: Some(LogId::new(CommittedLeaderId::new(3, node(9)), 42)),
            last_membership: stored,
            snapshot_id: "snap-1".into(),
        };
        let back = SnapshotMeta::try_from(&SnapshotMetaV1::from(&meta)).expect("well formed");
        assert_eq!(back.last_log_id, meta.last_log_id);
        assert_eq!(back.snapshot_id, meta.snapshot_id);
    }

    /// All three payload variants, including `Blank`, which has no fields and is
    /// therefore the one a mirror can most easily lose.
    #[test]
    fn every_entry_payload_variant_round_trips() {
        let command = CommandV1::Expire(ExpireCommandV1 {
            resource: ResourceKeyV1 {
                namespace: "ns".into(),
                key: b"k".to_vec(),
            },
            epoch: 1,
            lease_revision: 1,
        });
        let entries = vec![
            Entry::<CoordinationTypeConfig> {
                log_id: LogId::new(CommittedLeaderId::new(1, node(1)), 1),
                payload: EntryPayload::Blank,
            },
            Entry {
                log_id: LogId::new(CommittedLeaderId::new(1, node(1)), 2),
                payload: EntryPayload::Normal(command),
            },
            Entry {
                log_id: LogId::new(CommittedLeaderId::new(1, node(1)), 3),
                payload: EntryPayload::Membership(membership()),
            },
        ];

        for entry in entries {
            let wire = RaftEntryV1::from(&entry);
            // Through Fory as well as through the conversion: storage writes the
            // encoded bytes, so a field that converts but does not encode is
            // still a lost field.
            let fory = payload_fory::<RaftEntryV1>();
            let decoded: RaftEntryV1 = fory
                .deserialize(&fory.serialize(&wire).expect("encode"))
                .expect("decode");
            assert_eq!(decoded, wire);

            let back = Entry::try_from(&decoded).expect("a well-formed entry");
            assert_eq!(back.log_id, entry.log_id);
            assert_eq!(
                std::mem::discriminant(&back.payload),
                std::mem::discriminant(&entry.payload)
            );
        }
    }

    /// A log id keeps its leader through every hop. Two entries at the same term
    /// and index under different leaders are different entries, so a conversion
    /// that reconstructed the leader would merge them.
    #[test]
    fn log_id_conversions_preserve_the_leader() {
        let a = ModelLogId::new(7, [0x5Au8; 32], 4_294_967_296);
        let b = ModelLogId::new(7, [0x5Bu8; 32], 4_294_967_296);

        assert_eq!(
            ModelLogId::try_from(&LogIdV1::from(&a)).expect("well formed"),
            a
        );
        assert_ne!(LogIdV1::from(&a), LogIdV1::from(&b));

        // Model to OpenRaft and back.
        let raft: LogId<CoordinationNodeId> = (&a).into();
        assert_eq!(raft.leader_id.node_id.as_bytes(), &[0x5Au8; 32]);
        assert_eq!(raft.leader_id.term, 7);
        assert_eq!(raft.index, 4_294_967_296);
        assert_eq!(ModelLogId::from(&raft), a);

        // Wire to OpenRaft and back, so the two inbound paths agree.
        let wire = LogIdV1::from(&a);
        let raft = LogId::<CoordinationNodeId>::try_from(&wire).expect("well formed");
        assert_eq!(LogIdV1::from(raft), wire);
    }

    /// Bytes in a node id field are not a node id merely by position. Zero
    /// padding a short field would mint a leader that never existed.
    #[test]
    fn a_short_node_id_is_a_named_corruption_not_a_pad() {
        let wire = LogIdV1 {
            term: 1,
            leader_node_id: vec![0u8; 31],
            index: 1,
        };
        assert_eq!(
            LogId::<CoordinationNodeId>::try_from(&wire),
            Err(MirrorError::NodeIdLength {
                field: "log_id.leader_node_id",
                len: 31
            })
        );
        // The model boundary refuses the same bytes, by its own error type.
        assert!(ModelLogId::try_from(&wire).is_err());

        let vote = VoteV1 {
            term: 1,
            leader_node_id: Vec::new(),
            committed: true,
        };
        assert_eq!(
            Vote::try_from(&vote),
            Err(MirrorError::NodeIdLength {
                field: "vote.leader_node_id",
                len: 0
            })
        );
    }

    /// A payload from a newer version is refused, not applied as a blank. An
    /// entry the state machine cannot interpret is a gap in history, and
    /// committing a blank in its place would let replicas that understand it and
    /// replicas that do not diverge while both believe they applied the entry.
    #[test]
    fn an_unknown_payload_variant_is_refused_rather_than_blanked() {
        let wire = RaftEntryV1 {
            log_id: LogIdV1 {
                term: 1,
                leader_node_id: vec![1u8; 32],
                index: 1,
            },
            // Case 9 is not a payload this build defines, which is exactly what
            // decoding a newer peer's entry produces.
            payload: EntryPayloadV1::Unknown(fory_core::UnknownCase::new(9u32, ())),
        };
        assert_eq!(
            Entry::try_from(&wire).err(),
            Some(MirrorError::UnknownPayloadVariant)
        );
    }

    #[test]
    fn a_membership_with_no_configs_is_rejected() {
        let wire = MembershipV1 {
            configs: Vec::new(),
            nodes: Vec::new(),
        };
        assert_eq!(
            Membership::try_from(&wire),
            Err(MirrorError::EmptyMembership)
        );
    }

    /// A node record from a future format is refused rather than read as v1.
    #[test]
    fn an_unknown_node_format_is_refused() {
        let mut wire = MembershipV1::from(&membership());
        wire.nodes[0].format_version = 2;
        assert_eq!(
            Membership::try_from(&wire),
            Err(MirrorError::UnknownNodeFormat(2))
        );
    }

    /// Two voters encoding the same membership must produce the same bytes, or
    /// snapshot digests diverge across replicas that agree on the state.
    #[test]
    fn membership_encoding_is_order_independent() {
        let forward = Membership::new(
            vec![[node(1), node(2), node(3)].into_iter().collect()],
            None,
        );
        let reverse = Membership::new(
            vec![[node(3), node(2), node(1)].into_iter().collect()],
            None,
        );
        let fory = payload_fory::<MembershipV1>();
        assert_eq!(
            fory.serialize(&MembershipV1::from(&forward))
                .expect("encode"),
            fory.serialize(&MembershipV1::from(&reverse))
                .expect("encode"),
        );
    }
}
