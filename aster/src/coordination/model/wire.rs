//! Typed conversions between the model's rows and the compiled wire types.
//!
//! Every conversion here exists so that **schema drift is a compile error**.
//! The model carried `(term, index)` while `LogIdV1` declared
//! `(term, leader_node_id, index)` for several revisions, and nothing caught it:
//! the two types were only ever related by a person remembering they should be.
//! With a total `From`/`TryFrom` pair in both directions, adding a field to
//! either side stops the build.
//!
//! ## Which direction is fallible, and why
//!
//! Model to wire is infallible: the model's values are already validated, so
//! widening them loses nothing.
//!
//! Wire to model is fallible, and that is the point. The wire types are `Vec<u8>`
//! where the model has `[u8; 32]`, `String` where the model has a validated
//! namespace, and `i32` where the model has an enum. Each of those is a place
//! where a peer, a corrupt row, or a future version can hand over something the
//! model must refuse rather than store.

use openraft::{CommittedLeaderId, LogId as RaftLogId};

use crate::coordination::contract::{LogIdV1, ResourceKeyV1};
use crate::coordination::ids::{CoordinationNodeId, ResourceKey};
use crate::coordination::reason::Outcome;
use crate::coordination::rowstream::AuditRowV1;

use super::state::{AuditResult, AuditRow, LogId, NodeId};

/// The OpenRaft log id for this type config.
///
/// Its `leader_id` is a `CommittedLeaderId<CoordinationNodeId>`. With
/// `single-term-leader` **off** -- which this crate's pinned feature set
/// requires -- that is `LeaderId { term, node_id }`, so the leader survives.
/// Under the other mode it would be `{ term, PhantomData }` and the node id
/// would be silently discarded, which is why the feature set is part of this
/// subsystem's contract rather than a dependency knob.
pub type OpenRaftLogId = RaftLogId<CoordinationNodeId>;

/// A wire value the model cannot accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// A `Vec<u8>` node id that was not exactly 32 bytes.
    NodeIdWidth,
    /// A `(namespace, key)` pair that fails §4.2 validation.
    ResourceKey,
    /// An outcome code outside §5.2.
    OutcomeCode,
}

fn node_id(bytes: &[u8]) -> Result<NodeId, WireError> {
    bytes.try_into().map_err(|_| WireError::NodeIdWidth)
}

// ── LogId ↔ LogIdV1 ─────────────────────────────────────────────────────────

impl From<&LogId> for LogIdV1 {
    fn from(id: &LogId) -> Self {
        LogIdV1 {
            term: id.term,
            leader_node_id: id.leader_node_id.to_vec(),
            index: id.index,
        }
    }
}

impl TryFrom<&LogIdV1> for LogId {
    type Error = WireError;

    fn try_from(id: &LogIdV1) -> Result<Self, WireError> {
        Ok(LogId {
            term: id.term,
            leader_node_id: node_id(&id.leader_node_id)?,
            index: id.index,
        })
    }
}

// ── LogId ↔ OpenRaft ────────────────────────────────────────────────────────

impl From<&LogId> for OpenRaftLogId {
    fn from(id: &LogId) -> Self {
        RaftLogId {
            leader_id: CommittedLeaderId::new(
                id.term,
                CoordinationNodeId::from_array(id.leader_node_id),
            ),
            index: id.index,
        }
    }
}

impl From<&OpenRaftLogId> for LogId {
    fn from(id: &OpenRaftLogId) -> Self {
        LogId {
            term: id.leader_id.term,
            leader_node_id: *id.leader_id.node_id.as_bytes(),
            index: id.index,
        }
    }
}

// ── AuditRow ↔ AuditRowV1 ───────────────────────────────────────────────────

impl From<&AuditRow> for AuditRowV1 {
    fn from(row: &AuditRow) -> Self {
        AuditRowV1 {
            leader_node_id: row.leader_node_id.to_vec(),
            // `None` means absent: §11.3 audits every committed command, and an
            // entry whose actor id would not narrow has no attribution to
            // record. Neither a zeroed 32-byte id nor an empty vector -- the
            // first reads as a real node, and the second is a width the
            // snapshot reader rejects.
            actor_node_id: row.actor.map(|a| a.to_vec()),
            operation_code: row.operation_code,
            resource: row.resource.as_ref().map(|k| ResourceKeyV1 {
                namespace: k.namespace().to_string(),
                key: k.key().to_vec(),
            }),
            result_code: row.result.code(),
            epoch: row.epoch,
            value_revision: row.value_revision,
        }
    }
}

impl TryFrom<&AuditRowV1> for AuditRow {
    type Error = WireError;

    fn try_from(row: &AuditRowV1) -> Result<Self, WireError> {
        let resource = match &row.resource {
            Some(k) => Some(
                ResourceKey::new(k.namespace.clone(), k.key.clone())
                    .map_err(|_| WireError::ResourceKey)?,
            ),
            None => None,
        };
        Ok(AuditRow {
            leader_node_id: node_id(&row.leader_node_id)?,
            actor: match &row.actor_node_id {
                Some(bytes) => Some(node_id(bytes)?),
                None => None,
            },
            operation_code: row.operation_code,
            resource,
            // A stored row does not say which code space it is in, so the
            // operation code decides: 100/101 are the system commands.
            result: if row.operation_code == 100 || row.operation_code == 101 {
                AuditResult::System(row.result_code)
            } else {
                AuditResult::Client(
                    Outcome::from_code(row.result_code).ok_or(WireError::OutcomeCode)?,
                )
            },
            epoch: row.epoch,
            value_revision: row.value_revision,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_id() -> LogId {
        LogId::new(7, [0xABu8; 32], 42)
    }

    fn audit_row() -> AuditRow {
        AuditRow {
            leader_node_id: [0x11u8; 32],
            actor: Some([0x22u8; 32]),
            operation_code: 3,
            resource: Some(ResourceKey::new("ns.sub", b"\x00\xffkey".to_vec()).expect("valid")),
            result: AuditResult::Client(Outcome::Mutated),
            epoch: 9,
            value_revision: 4,
        }
    }

    // ── LogIdV1 ─────────────────────────────────────────────────────────────

    #[test]
    fn log_id_round_trips_through_the_wire_type_without_loss() {
        let id = log_id();
        let wire: LogIdV1 = (&id).into();

        assert_eq!(wire.term, 7);
        assert_eq!(wire.index, 42);
        assert_eq!(wire.leader_node_id, vec![0xABu8; 32]);
        assert_eq!(LogId::try_from(&wire), Ok(id));
    }

    #[test]
    fn a_wire_log_id_with_a_malformed_leader_is_rejected() {
        for leader in [vec![], vec![0u8; 31], vec![0u8; 33]] {
            let wire = LogIdV1 {
                term: 1,
                leader_node_id: leader.clone(),
                index: 1,
            };
            assert_eq!(
                LogId::try_from(&wire),
                Err(WireError::NodeIdWidth),
                "a {}-byte leader is not a NodeId",
                leader.len()
            );
        }
    }

    // ── OpenRaft ────────────────────────────────────────────────────────────

    /// The boundary that actually matters for Slice 2: OpenRaft hands the state
    /// machine a log id, and the model must reconstruct it exactly.
    #[test]
    fn log_id_round_trips_through_openraft_without_loss() {
        let id = log_id();
        let raft: OpenRaftLogId = (&id).into();

        assert_eq!(raft.leader_id.term, 7);
        assert_eq!(raft.index, 42);
        assert_eq!(raft.leader_id.node_id.as_bytes(), &[0xABu8; 32]);
        assert_eq!(LogId::from(&raft), id);
    }

    /// The pinned feature set retains the leader. Under `single-term-leader`
    /// OpenRaft's `CommittedLeaderId` is `{ term, PhantomData }` and the node id
    /// is discarded, so this assertion is what would fail if that feature were
    /// ever enabled -- which is why it is asserted rather than assumed.
    #[test]
    fn openraft_retains_the_leader_node_id_under_this_feature_set() {
        let a: OpenRaftLogId = (&LogId::new(1, [1u8; 32], 5)).into();
        let b: OpenRaftLogId = (&LogId::new(1, [2u8; 32], 5)).into();
        assert_ne!(
            a, b,
            "two leaders at one term and index must remain distinct after \
             conversion; if this fails, `single-term-leader` is enabled and the \
             leader is being discarded"
        );
    }

    /// Every field survives both hops, so a model value that went out to
    /// OpenRaft and came back is the same entry.
    #[test]
    fn every_log_id_field_survives_the_openraft_round_trip() {
        for id in [
            LogId::new(0, [0u8; 32], 0),
            LogId::new(u64::MAX, [0xFFu8; 32], u64::MAX),
            LogId::new(3, [0x5Au8; 32], 11),
        ] {
            let raft: OpenRaftLogId = (&id).into();
            assert_eq!(LogId::from(&raft), id);
        }
    }

    /// And the two wire representations agree with each other, so a log id
    /// persisted as `LogIdV1` and one received from OpenRaft describe the same
    /// entry.
    #[test]
    fn the_two_wire_representations_agree() {
        let id = log_id();
        let via_fory = LogId::try_from(&LogIdV1::from(&id)).expect("valid");
        let via_raft = LogId::from(&OpenRaftLogId::from(&id));
        assert_eq!(via_fory, via_raft);
    }

    // ── AuditRowV1 ──────────────────────────────────────────────────────────

    #[test]
    fn an_audit_row_round_trips_through_the_wire_type_without_loss() {
        let row = audit_row();
        let wire: AuditRowV1 = (&row).into();

        assert_eq!(wire.leader_node_id, vec![0x11u8; 32]);
        assert_eq!(wire.actor_node_id, Some(vec![0x22u8; 32]));
        assert_eq!(wire.operation_code, 3);
        assert_eq!(wire.result_code, Outcome::Mutated.code());
        assert_eq!(wire.epoch, 9);
        assert_eq!(wire.value_revision, 4);
        let resource = wire.resource.as_ref().expect("a resource");
        assert_eq!(resource.namespace, "ns.sub");
        assert_eq!(resource.key, b"\x00\xffkey");

        assert_eq!(AuditRow::try_from(&wire), Ok(row));
    }

    /// A row with no resource -- a classifier refusal, or a shapeless identity
    /// -- round-trips as `None` rather than as an empty key.
    #[test]
    fn an_audit_row_without_a_resource_round_trips_as_absent() {
        let mut row = audit_row();
        row.resource = None;
        let wire: AuditRowV1 = (&row).into();
        assert!(wire.resource.is_none());
        assert_eq!(AuditRow::try_from(&wire), Ok(row));
    }

    /// Each fallible field refuses independently, so a malformed row cannot be
    /// partially accepted.
    /// A system row round-trips as a system result, not as a client outcome.
    /// The operation code is what decides, since the stored row does not say
    /// which of the two disjoint code spaces its result belongs to.
    #[test]
    fn a_system_audit_row_round_trips_as_a_system_result() {
        let mut row = audit_row();
        row.operation_code = 100; // system Expire
        row.result = AuditResult::System(crate::coordination::contract::SYS_EXPIRED);
        row.resource = None;

        let wire: AuditRowV1 = (&row).into();
        assert_eq!(wire.result_code, crate::coordination::contract::SYS_EXPIRED);
        assert_eq!(AuditRow::try_from(&wire), Ok(row));

        // And the same numeric code under a *client* operation decodes as a
        // client outcome, which is what makes the tag load-bearing.
        let mut client = audit_row();
        client.operation_code = 1;
        client.result = AuditResult::Client(Outcome::Granted);
        let wire: AuditRowV1 = (&client).into();
        assert_eq!(wire.result_code, 1);
        assert_eq!(AuditRow::try_from(&wire), Ok(client));
    }

    #[test]
    fn each_malformed_wire_field_is_refused_on_its_own() {
        let good: AuditRowV1 = (&audit_row()).into();

        let mut bad_leader = good.clone();
        bad_leader.leader_node_id = vec![0u8; 8];
        assert_eq!(AuditRow::try_from(&bad_leader), Err(WireError::NodeIdWidth));

        // A **wrong-width** actor is a corrupt row. `None` is not: it is how
        // §11.3's row for a committed entry with no validated actor says so.
        // There is no empty-vector sentinel -- an empty `Some` is a width
        // failure like any other.
        let mut bad_actor = good.clone();
        bad_actor.actor_node_id = Some(vec![0u8; 8]);
        assert_eq!(AuditRow::try_from(&bad_actor), Err(WireError::NodeIdWidth));

        let mut absent_actor = good.clone();
        absent_actor.actor_node_id = None;
        assert_eq!(
            AuditRow::try_from(&absent_actor).map(|r| r.actor),
            Ok(None),
            "an absent actor is absent attribution, not a malformed one"
        );
        // And it survives the trip back out, rather than becoming 32 zero bytes
        // or an empty vector the snapshot reader would reject.
        let round_tripped: AuditRowV1 = (&AuditRow::try_from(&absent_actor).unwrap()).into();
        assert_eq!(round_tripped.actor_node_id, None);

        let mut empty_some = good.clone();
        empty_some.actor_node_id = Some(Vec::new());
        assert_eq!(
            AuditRow::try_from(&empty_some),
            Err(WireError::NodeIdWidth),
            "an empty vector is a width failure, never an absence sentinel"
        );

        let mut bad_key = good.clone();
        bad_key.resource = Some(ResourceKeyV1 {
            namespace: "NOT VALID".to_string(),
            key: b"k".to_vec(),
        });
        assert_eq!(AuditRow::try_from(&bad_key), Err(WireError::ResourceKey));

        let mut bad_outcome = good.clone();
        bad_outcome.result_code = 999;
        assert_eq!(
            AuditRow::try_from(&bad_outcome),
            Err(WireError::OutcomeCode)
        );
    }

    /// An audit row read back from the wire reconstructs its entry's full log
    /// id, given the table key -- the property that survives a log purge.
    #[test]
    fn a_wire_audit_row_and_its_table_key_reconstruct_a_full_log_id() {
        let row = audit_row();
        let wire: AuditRowV1 = (&row).into();
        let (term, index) = (5u64, 77u64);

        let decoded = AuditRow::try_from(&wire).expect("valid");
        assert_eq!(
            LogId::new(term, decoded.leader_node_id, index),
            LogId::new(5, [0x11u8; 32], 77)
        );
    }
}
