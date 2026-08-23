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

use crate::coordination::contract::{
    ChangeMembershipActionV1, LogIdV1, MemberSeedV1, NodeIdV1, ResourceKeyV1, SavedReplyV1,
    SavedSnapshotV1,
};
use crate::coordination::ids::{CoordinationNodeId, ResourceKey};
use crate::coordination::raft::{ClientSessionV1, MembershipIntentV1, NodeRouteV1, ResourceRowV1};
use crate::coordination::reason::{Disposition, Outcome, Reason};
use crate::coordination::rowstream::AuditRowV1;

use super::membership::is_terminal_phase;
use super::state::{
    AuditResult, AuditRow, Intent, LogId, MemberSeed, MembershipPlan, NodeId, OperationId,
    ResourceRow, Route, SavedReply, SavedSnapshot, Session,
};

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
    /// A `Vec<u8>` operation id that was not exactly 16 bytes.
    OperationIdWidth,
    /// A `Vec<u8>` request hash that was not exactly 32 bytes.
    HashWidth,
    /// A `(namespace, key)` pair that fails §4.2 validation.
    ResourceKey,
    /// An outcome code outside §5.2.
    OutcomeCode,
    /// A reason code outside §5.2.
    ReasonCode,
    /// A disposition code a stored reply cannot carry.
    ///
    /// §10 saves a result only for a sequence the entry **consumed**, so a
    /// stored `NotConsumed` describes a row no apply path writes.
    DispositionCode,
    /// A membership phase outside §9's declared set.
    PhaseCode,
    /// A member-seed ticket that is not valid UTF-8.
    ///
    /// The wire carries a ticket as a Fory `String` and the model holds its
    /// bytes, so a stored ticket that will not narrow back is a row this build
    /// cannot faithfully re-emit. Lossy replacement would silently change
    /// dialling material.
    TicketEncoding,
    /// `held` and `holder` disagreed: a lease held by nobody, or a free
    /// resource with a holder.
    LeaseIncoherent,
    /// A terminal intent with no completion log id, or an active one with one.
    ///
    /// §9 evicts terminal intents by completion log id, so the pairing is what
    /// makes eviction ordered at all.
    IntentCompletionIncoherent,
    /// A stored value bound to a different group than the store it was read
    /// from.
    GroupMismatch,
    /// A stored row's embedded key disagreed with the redb key it was filed
    /// under.
    KeyMismatch,
}

fn node_id(bytes: &[u8]) -> Result<NodeId, WireError> {
    bytes.try_into().map_err(|_| WireError::NodeIdWidth)
}

fn operation_id(bytes: &[u8]) -> Result<OperationId, WireError> {
    bytes.try_into().map_err(|_| WireError::OperationIdWidth)
}

fn hash(bytes: &[u8]) -> Result<[u8; 32], WireError> {
    bytes.try_into().map_err(|_| WireError::HashWidth)
}

fn resource_key(wire: &ResourceKeyV1) -> Result<ResourceKey, WireError> {
    ResourceKey::new(wire.namespace.clone(), wire.key.clone()).map_err(|_| WireError::ResourceKey)
}

fn resource_key_to_wire(key: &ResourceKey) -> ResourceKeyV1 {
    ResourceKeyV1 {
        namespace: key.namespace().to_string(),
        key: key.key().to_vec(),
    }
}

/// Narrow an optional stored log id, refusing a malformed one rather than
/// reading it as absent.
fn opt_log_id(wire: &Option<LogIdV1>) -> Result<Option<LogId>, WireError> {
    match wire {
        Some(id) => Ok(Some(LogId::try_from(id)?)),
        None => Ok(None),
    }
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
            resource: row.resource.as_ref().map(resource_key_to_wire),
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
            Some(k) => Some(resource_key(k)?),
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
            // operation code decides — through the one predicate the durable
            // validator also uses, so the two cannot disagree about a command
            // added later.
            result: if crate::coordination::model::apply::is_system_operation(row.operation_code) {
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

// ── ResourceRow ↔ ResourceRowV1 ─────────────────────────────────────────────

impl From<&ResourceRow> for ResourceRowV1 {
    fn from(row: &ResourceRow) -> Self {
        ResourceRowV1 {
            epoch: row.epoch,
            held: row.held,
            // `None` when unheld, never 32 zero bytes: an all-zero NodeId is a
            // NodeId, so a sentinel would be a lease held by a node whose key
            // is all zeros.
            holder_node_id: row.holder.map(|h| h.to_vec()),
            ttl_ms: row.ttl_ms,
            lease_revision: row.lease_revision,
            value_revision: row.value_revision,
            value: row.value.clone(),
            acl_revision: row.acl_revision,
        }
    }
}

impl TryFrom<&ResourceRowV1> for ResourceRow {
    type Error = WireError;

    fn try_from(row: &ResourceRowV1) -> Result<Self, WireError> {
        let holder = match &row.holder_node_id {
            Some(bytes) => Some(node_id(bytes)?),
            None => None,
        };
        // Every model path assigns the two together -- `decide_acquire` sets
        // both, and every release, revoke and expiry clears both -- so a row
        // where they disagree came from somewhere other than this state
        // machine.
        if row.held != holder.is_some() {
            return Err(WireError::LeaseIncoherent);
        }
        Ok(ResourceRow {
            epoch: row.epoch,
            held: row.held,
            holder,
            ttl_ms: row.ttl_ms,
            lease_revision: row.lease_revision,
            value_revision: row.value_revision,
            value: row.value.clone(),
            acl_revision: row.acl_revision,
        })
    }
}

// ── SavedSnapshot ↔ SavedSnapshotV1 ─────────────────────────────────────────
//
// Both of these take the group id, because the wire type carries one and the
// model's does not. It is not padding: a session row is replicated state, so
// the group it names must be the group holding it, and a row that names another
// group is a row from another store.

pub fn saved_snapshot_to_wire(group_id: &[u8], saved: &SavedSnapshot) -> SavedSnapshotV1 {
    SavedSnapshotV1 {
        group_id: group_id.to_vec(),
        resource: resource_key_to_wire(&saved.resource),
        epoch: saved.epoch,
        holder: saved.holder.map(|h| h.to_vec()),
        held: saved.held,
        ttl_ms: saved.ttl_ms,
        lease_revision: saved.lease_revision,
        value_revision: saved.value_revision,
        acl_revision: saved.acl_revision,
    }
}

pub fn saved_snapshot_from_wire(
    group_id: &[u8],
    wire: &SavedSnapshotV1,
) -> Result<SavedSnapshot, WireError> {
    if wire.group_id != group_id {
        return Err(WireError::GroupMismatch);
    }
    let holder = match &wire.holder {
        Some(bytes) => Some(node_id(bytes)?),
        None => None,
    };
    if wire.held != holder.is_some() {
        return Err(WireError::LeaseIncoherent);
    }
    Ok(SavedSnapshot {
        resource: resource_key(&wire.resource)?,
        epoch: wire.epoch,
        holder,
        held: wire.held,
        ttl_ms: wire.ttl_ms,
        lease_revision: wire.lease_revision,
        value_revision: wire.value_revision,
        acl_revision: wire.acl_revision,
    })
}

// ── SavedReply ↔ SavedReplyV1 ───────────────────────────────────────────────

/// The one disposition a **stored** reply can carry.
///
/// §10 saves a result only where the entry consumed the sequence: a classifier
/// refusal is `NotConsumed` and writes no session row at all. So the stored
/// field is not free-form, and a row carrying anything else is not a row this
/// state machine wrote.
const STORED_DISPOSITION: Disposition = Disposition::Consumed;

pub fn saved_reply_to_wire(group_id: &[u8], reply: &SavedReply) -> SavedReplyV1 {
    SavedReplyV1 {
        disposition_code: disposition_code(STORED_DISPOSITION),
        outcome_code: reply.outcome.code(),
        reason_code: reply.reason.code(),
        snapshot: reply
            .snapshot
            .as_ref()
            .map(|s| saved_snapshot_to_wire(group_id, s)),
        acl_revision: reply.acl_revision,
        admin_revision: reply.admin_revision,
        operation_id: reply.operation_id.map(|id| id.to_vec()),
        membership_log_id: reply.membership_log_id.as_ref().map(Into::into),
    }
}

pub fn saved_reply_from_wire(
    group_id: &[u8],
    wire: &SavedReplyV1,
) -> Result<SavedReply, WireError> {
    if wire.disposition_code != disposition_code(STORED_DISPOSITION) {
        return Err(WireError::DispositionCode);
    }
    let snapshot = match &wire.snapshot {
        Some(s) => Some(saved_snapshot_from_wire(group_id, s)?),
        None => None,
    };
    Ok(SavedReply {
        outcome: Outcome::from_code(wire.outcome_code).ok_or(WireError::OutcomeCode)?,
        reason: Reason::from_code(wire.reason_code).ok_or(WireError::ReasonCode)?,
        snapshot,
        acl_revision: wire.acl_revision,
        admin_revision: wire.admin_revision,
        operation_id: match &wire.operation_id {
            Some(bytes) => Some(operation_id(bytes)?),
            None => None,
        },
        membership_log_id: opt_log_id(&wire.membership_log_id)?,
    })
}

/// The §5.2.1 disposition code. Shared with [`super::project`] rather than
/// restated, so the stored and public forms cannot drift apart.
fn disposition_code(disposition: Disposition) -> i32 {
    match disposition {
        Disposition::Consumed => 1,
        Disposition::NotConsumed => 2,
    }
}

// ── Session ↔ ClientSessionV1 ───────────────────────────────────────────────

pub fn session_to_wire(group_id: &[u8], session: &Session) -> ClientSessionV1 {
    ClientSessionV1 {
        last_sequence: session.last_sequence,
        request_hash: session.request_hash.to_vec(),
        last_result: session
            .last_result
            .as_ref()
            .map(|r| saved_reply_to_wire(group_id, r)),
        retired: session.retired,
    }
}

pub fn session_from_wire(group_id: &[u8], wire: &ClientSessionV1) -> Result<Session, WireError> {
    Ok(Session {
        last_sequence: wire.last_sequence,
        request_hash: hash(&wire.request_hash)?,
        last_result: match &wire.last_result {
            Some(reply) => Some(saved_reply_from_wire(group_id, reply)?),
            None => None,
        },
        retired: wire.retired,
    })
}

// ── MembershipPlan ↔ ChangeMembershipActionV1 ───────────────────────────────

/// Build the stored plan action.
///
/// Takes the operation id and expected membership because the declared type
/// carries both, and because the §12.1 intent charge is measured from exactly
/// this value -- [`MembershipPlan::encoded_len`] calls this, so the charge and
/// the bytes written are the same thing rather than two constructions someone
/// keeps in step.
///
/// Fallible for one reason: a ticket is a Fory `String` on the wire and bytes
/// in the model. Every ticket that entered through the command boundary came
/// *from* a `String`, so this succeeds for anything the model built; a stored
/// ticket that will not narrow is a row this build cannot re-emit faithfully,
/// and replacing the invalid bytes would change dialling material.
pub fn plan_to_wire(
    plan: &MembershipPlan,
    operation_id: &OperationId,
    expected_membership: Option<LogId>,
) -> Result<ChangeMembershipActionV1, WireError> {
    let ids = |nodes: &[NodeId]| -> Vec<NodeIdV1> {
        nodes
            .iter()
            .map(|n| NodeIdV1 {
                node_id: n.to_vec(),
            })
            .collect()
    };
    let mut member_seeds = Vec::with_capacity(plan.member_seeds.len());
    for seed in &plan.member_seeds {
        member_seeds.push(MemberSeedV1 {
            node_id: seed.node_id.to_vec(),
            ticket: String::from_utf8(seed.ticket.clone())
                .map_err(|_| WireError::TicketEncoding)?,
            signed_join_permit: seed.join_permit.clone(),
        });
    }
    Ok(ChangeMembershipActionV1 {
        operation_id: operation_id.to_vec(),
        expected_membership_log_id: expected_membership.as_ref().map(Into::into),
        desired_voters: ids(&plan.desired_voters),
        retained_learners: ids(&plan.retained_learners),
        member_seeds,
    })
}

/// Narrow a stored plan action back to its three parts.
///
/// Returns the operation id and expected membership alongside the plan rather
/// than discarding them: they are what the caller checks the row key and the
/// intent's own `expected_membership` against.
pub fn plan_from_wire(
    wire: &ChangeMembershipActionV1,
) -> Result<(OperationId, Option<LogId>, MembershipPlan), WireError> {
    let nodes = |ids: &[NodeIdV1]| -> Result<Vec<NodeId>, WireError> {
        ids.iter().map(|n| node_id(&n.node_id)).collect()
    };
    let mut member_seeds = Vec::with_capacity(wire.member_seeds.len());
    for seed in &wire.member_seeds {
        member_seeds.push(MemberSeed {
            node_id: node_id(&seed.node_id)?,
            ticket: seed.ticket.clone().into_bytes(),
            join_permit: seed.signed_join_permit.clone(),
        });
    }
    Ok((
        operation_id(&wire.operation_id)?,
        opt_log_id(&wire.expected_membership_log_id)?,
        MembershipPlan {
            desired_voters: nodes(&wire.desired_voters)?,
            retained_learners: nodes(&wire.retained_learners)?,
            member_seeds,
        },
    ))
}

// ── Intent ↔ MembershipIntentV1 ─────────────────────────────────────────────

pub fn intent_to_wire(
    operation_id: &OperationId,
    intent: &Intent,
) -> Result<MembershipIntentV1, WireError> {
    Ok(MembershipIntentV1 {
        actor_node_id: intent.actor.to_vec(),
        request_hash: intent.request_hash.to_vec(),
        expected_membership: intent.expected_membership.as_ref().map(Into::into),
        desired_plan: plan_to_wire(&intent.plan, operation_id, intent.expected_membership)?,
        status_code: intent.status_code,
        result_code: intent.result_code,
        accepted_log_id: (&intent.accepted_log_id).into(),
        completed_log_id: intent.completed_log_id.as_ref().map(Into::into),
        observed_membership_log_id: intent.observed_membership_log_id.as_ref().map(Into::into),
    })
}

/// Narrow a stored intent, filed under `operation_id`.
///
/// The stored plan repeats the operation id and the expected membership, so
/// both are **checked** against the row rather than preferred or overwritten. A
/// row whose two copies disagree has no defensible reading: picking either one
/// would make a plan the controller re-runs after a restart differ from the
/// plan that was accepted.
pub fn intent_from_wire(
    operation_id: &OperationId,
    wire: &MembershipIntentV1,
) -> Result<Intent, WireError> {
    let (plan_operation_id, plan_expected, plan) = plan_from_wire(&wire.desired_plan)?;
    if plan_operation_id != *operation_id {
        return Err(WireError::KeyMismatch);
    }
    let expected_membership = opt_log_id(&wire.expected_membership)?;
    if plan_expected != expected_membership {
        return Err(WireError::KeyMismatch);
    }
    if !PHASES.contains(&wire.status_code) {
        return Err(WireError::PhaseCode);
    }
    let completed_log_id = opt_log_id(&wire.completed_log_id)?;
    // §9 orders eviction of terminal intents by completion log id, and apply
    // stamps it exactly when an intent becomes terminal. Either half without
    // the other makes that ordering undefined.
    if is_terminal_phase(wire.status_code) != completed_log_id.is_some() {
        return Err(WireError::IntentCompletionIncoherent);
    }
    Ok(Intent {
        actor: node_id(&wire.actor_node_id)?,
        request_hash: hash(&wire.request_hash)?,
        expected_membership,
        plan,
        status_code: wire.status_code,
        result_code: wire.result_code,
        accepted_log_id: LogId::try_from(&wire.accepted_log_id)?,
        completed_log_id,
        observed_membership_log_id: opt_log_id(&wire.observed_membership_log_id)?,
    })
}

/// Every §9 phase, listed so an unknown stored code is refused rather than
/// carried into an intent the phase machine cannot advance.
const PHASES: [i32; 7] = [
    super::membership::PHASE_PENDING,
    super::membership::PHASE_ADDING_LEARNERS,
    super::membership::PHASE_CHANGING_VOTERS,
    super::membership::PHASE_COMPLETED,
    super::membership::PHASE_FAILED,
    super::membership::PHASE_CANCEL_REQUESTED,
    super::membership::PHASE_CANCELLED,
];

// ── Route ↔ NodeRouteV1 ─────────────────────────────────────────────────────

impl From<&Route> for NodeRouteV1 {
    fn from(route: &Route) -> Self {
        NodeRouteV1 {
            // Bytes, not a `String`: a route is stored dialling material and
            // the declared type keeps it opaque, so no encoding question
            // arises on this side.
            ticket_bytes: route.ticket.clone(),
            source_operation_id: route.source_operation_id.to_vec(),
            introduced_log_id: (&route.introduced_log_id).into(),
        }
    }
}

impl TryFrom<&NodeRouteV1> for Route {
    type Error = WireError;

    fn try_from(wire: &NodeRouteV1) -> Result<Self, WireError> {
        Ok(Route {
            ticket: wire.ticket_bytes.clone(),
            source_operation_id: operation_id(&wire.source_operation_id)?,
            introduced_log_id: LogId::try_from(&wire.introduced_log_id)?,
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

    // ── §11.3 storage rows ──────────────────────────────────────────────────

    const GROUP: [u8; 32] = [0xC0; 32];
    const OTHER_GROUP: [u8; 32] = [0xC1; 32];

    fn key() -> ResourceKey {
        ResourceKey::new("ns.sub", b"\x00\xffkey".to_vec()).expect("valid")
    }

    fn held_row() -> ResourceRow {
        ResourceRow {
            epoch: 4,
            held: true,
            holder: Some([0x33u8; 32]),
            ttl_ms: 30_000,
            lease_revision: 2,
            value_revision: 3,
            value: b"payload".to_vec(),
            acl_revision: 1,
        }
    }

    fn saved_reply() -> SavedReply {
        SavedReply {
            outcome: Outcome::Mutated,
            reason: Reason::None,
            snapshot: Some(SavedSnapshot::of(key(), &held_row())),
            acl_revision: 1,
            admin_revision: 6,
            operation_id: Some([0x44u8; 16]),
            membership_log_id: Some(LogId::new(2, [0x55u8; 32], 11)),
        }
    }

    fn plan() -> MembershipPlan {
        MembershipPlan {
            desired_voters: vec![[0x61u8; 32], [0x62u8; 32], [0x63u8; 32]],
            retained_learners: vec![[0x64u8; 32]],
            member_seeds: vec![MemberSeed {
                node_id: [0x65u8; 32],
                ticket: b"aster1seed".to_vec(),
                join_permit: Some(vec![0x66u8; 8]),
            }],
        }
    }

    fn intent() -> Intent {
        Intent {
            actor: [0x71u8; 32],
            request_hash: [0x72u8; 32],
            expected_membership: Some(LogId::new(3, [0x73u8; 32], 21)),
            plan: plan(),
            status_code: super::super::membership::PHASE_ADDING_LEARNERS,
            result_code: 0,
            accepted_log_id: LogId::new(3, [0x73u8; 32], 22),
            completed_log_id: None,
            observed_membership_log_id: Some(LogId::new(3, [0x73u8; 32], 20)),
        }
    }

    const OPERATION: OperationId = [0x88u8; 16];

    /// Both lease states survive the round trip, and an unheld row reports no
    /// holder rather than 32 zero bytes.
    #[test]
    fn a_resource_row_round_trips_at_both_lease_states() {
        let held = held_row();
        let wire: ResourceRowV1 = (&held).into();
        assert_eq!(wire.holder_node_id, Some(vec![0x33u8; 32]));
        assert_eq!(ResourceRow::try_from(&wire), Ok(held));

        let free = ResourceRow {
            held: false,
            holder: None,
            ..held_row()
        };
        let wire: ResourceRowV1 = (&free).into();
        assert_eq!(wire.holder_node_id, None, "never a zeroed sentinel");
        assert_eq!(ResourceRow::try_from(&wire), Ok(free));
    }

    /// A lease held by nobody, and a free resource with a holder, are both
    /// refused. Every model path assigns `held` and `holder` together, so a row
    /// where they disagree did not come from this state machine.
    #[test]
    fn an_incoherent_lease_is_refused_in_both_directions() {
        let mut held_by_nobody: ResourceRowV1 = (&held_row()).into();
        held_by_nobody.holder_node_id = None;
        assert_eq!(
            ResourceRow::try_from(&held_by_nobody),
            Err(WireError::LeaseIncoherent)
        );

        let mut free_with_holder: ResourceRowV1 = (&held_row()).into();
        free_with_holder.held = false;
        assert_eq!(
            ResourceRow::try_from(&free_with_holder),
            Err(WireError::LeaseIncoherent)
        );

        // And a holder that is not a NodeId is a width failure, which is a
        // different fault from an incoherent pair.
        let mut bad_width: ResourceRowV1 = (&held_row()).into();
        bad_width.holder_node_id = Some(vec![0u8; 31]);
        assert_eq!(
            ResourceRow::try_from(&bad_width),
            Err(WireError::NodeIdWidth)
        );
    }

    /// A saved reply round-trips whole, including the snapshot the exact-replay
    /// property depends on.
    #[test]
    fn a_saved_reply_round_trips_with_every_field() {
        let reply = saved_reply();
        let wire = saved_reply_to_wire(&GROUP, &reply);

        assert_eq!(wire.outcome_code, Outcome::Mutated.code());
        assert_eq!(wire.operation_id, Some(vec![0x44u8; 16]));
        let snap = wire.snapshot.as_ref().expect("a snapshot");
        assert_eq!(snap.group_id, GROUP.to_vec());
        assert_eq!(snap.holder, Some(vec![0x33u8; 32]));

        assert_eq!(saved_reply_from_wire(&GROUP, &wire), Ok(reply));
    }

    /// A reply whose snapshot names another group is refused. A session row is
    /// replicated state, so the group it names must be the group holding it.
    #[test]
    fn a_saved_reply_bound_to_another_group_is_refused() {
        let wire = saved_reply_to_wire(&OTHER_GROUP, &saved_reply());
        assert_eq!(
            saved_reply_from_wire(&GROUP, &wire),
            Err(WireError::GroupMismatch)
        );
    }

    /// A stored reply carries the consumed disposition, because §10 saves a
    /// result only where the entry consumed the sequence. A classifier refusal
    /// is `NotConsumed` and writes no session row at all.
    #[test]
    fn a_stored_reply_must_be_consumed() {
        let wire = saved_reply_to_wire(&GROUP, &saved_reply());
        assert_eq!(wire.disposition_code, 1);

        let mut not_consumed = wire.clone();
        not_consumed.disposition_code = 2;
        assert_eq!(
            saved_reply_from_wire(&GROUP, &not_consumed),
            Err(WireError::DispositionCode)
        );

        let mut nonsense = wire;
        nonsense.disposition_code = 0;
        assert_eq!(
            saved_reply_from_wire(&GROUP, &nonsense),
            Err(WireError::DispositionCode)
        );
    }

    /// Each fallible field of a stored reply refuses on its own, so a corrupt
    /// row cannot be partially accepted.
    #[test]
    fn each_malformed_saved_reply_field_is_refused_on_its_own() {
        let good = saved_reply_to_wire(&GROUP, &saved_reply());

        let cases: Vec<(&str, SavedReplyV1, WireError)> = vec![
            (
                "outcome",
                SavedReplyV1 {
                    outcome_code: 999,
                    ..good.clone()
                },
                WireError::OutcomeCode,
            ),
            (
                "reason",
                SavedReplyV1 {
                    reason_code: 999,
                    ..good.clone()
                },
                WireError::ReasonCode,
            ),
            (
                "operation id",
                SavedReplyV1 {
                    operation_id: Some(vec![0u8; 15]),
                    ..good.clone()
                },
                WireError::OperationIdWidth,
            ),
            (
                "membership leader",
                SavedReplyV1 {
                    membership_log_id: Some(LogIdV1 {
                        term: 1,
                        leader_node_id: vec![0u8; 8],
                        index: 1,
                    }),
                    ..good.clone()
                },
                WireError::NodeIdWidth,
            ),
            (
                "snapshot key",
                SavedReplyV1 {
                    snapshot: Some(SavedSnapshotV1 {
                        resource: ResourceKeyV1 {
                            namespace: "NOT VALID".into(),
                            key: b"k".to_vec(),
                        },
                        ..good.snapshot.clone().expect("a snapshot")
                    }),
                    ..good.clone()
                },
                WireError::ResourceKey,
            ),
        ];

        for (label, wire, expected) in cases {
            assert_eq!(
                saved_reply_from_wire(&GROUP, &wire),
                Err(expected),
                "{label} must refuse on its own"
            );
        }
    }

    /// A session row round-trips at both shapes that exist: an answered
    /// sequence, and a sequence-zero tombstone with no saved result.
    #[test]
    fn a_session_round_trips_with_and_without_a_saved_result() {
        let answered = Session {
            last_sequence: 5,
            request_hash: [0x99u8; 32],
            last_result: Some(saved_reply()),
            retired: false,
        };
        let wire = session_to_wire(&GROUP, &answered);
        assert_eq!(session_from_wire(&GROUP, &wire), Ok(answered));

        // A tombstone is a row, not an absence: `retired` is what marks it, and
        // `last_result` stays whatever was preserved.
        let tombstone = Session {
            last_sequence: 0,
            request_hash: [0u8; 32],
            last_result: None,
            retired: true,
        };
        let wire = session_to_wire(&GROUP, &tombstone);
        assert!(wire.retired);
        assert!(wire.last_result.is_none());
        assert_eq!(session_from_wire(&GROUP, &wire), Ok(tombstone));
    }

    #[test]
    fn a_session_with_a_malformed_request_hash_is_refused() {
        let mut wire = session_to_wire(
            &GROUP,
            &Session {
                last_sequence: 1,
                request_hash: [0u8; 32],
                last_result: None,
                retired: false,
            },
        );
        wire.request_hash = vec![0u8; 31];
        assert_eq!(session_from_wire(&GROUP, &wire), Err(WireError::HashWidth));
    }

    /// An intent round-trips whole, plan included.
    #[test]
    fn an_intent_round_trips_with_its_plan() {
        let intent = intent();
        let wire = intent_to_wire(&OPERATION, &intent).expect("encode");

        assert_eq!(wire.desired_plan.operation_id, OPERATION.to_vec());
        assert_eq!(wire.desired_plan.desired_voters.len(), 3);
        assert_eq!(wire.desired_plan.member_seeds[0].ticket, "aster1seed");

        assert_eq!(intent_from_wire(&OPERATION, &wire), Ok(intent));
    }

    /// The stored plan repeats the operation id and expected membership, and
    /// both are checked against the row rather than preferred. A row whose two
    /// copies disagree has no defensible reading: a controller re-running the
    /// plan after a restart would run something other than what was accepted.
    #[test]
    fn a_stored_intent_whose_plan_disagrees_with_its_row_is_refused() {
        let wire = intent_to_wire(&OPERATION, &intent()).expect("encode");

        assert_eq!(
            intent_from_wire(&[0x89u8; 16], &wire),
            Err(WireError::KeyMismatch),
            "filed under a different operation id"
        );

        let mut drifted = wire.clone();
        drifted.expected_membership = Some(LogIdV1 {
            term: 99,
            leader_node_id: vec![0x73u8; 32],
            index: 99,
        });
        assert_eq!(
            intent_from_wire(&OPERATION, &drifted),
            Err(WireError::KeyMismatch),
            "the intent and its plan expect different memberships"
        );

        // And the same disagreement from the other side.
        let mut drifted = wire;
        drifted.desired_plan.expected_membership_log_id = None;
        assert_eq!(
            intent_from_wire(&OPERATION, &drifted),
            Err(WireError::KeyMismatch)
        );
    }

    /// A phase outside §9's set is refused rather than carried into an intent
    /// the phase machine cannot advance.
    #[test]
    fn an_unknown_membership_phase_is_refused() {
        let good = intent_to_wire(&OPERATION, &intent()).expect("encode");
        for phase in [0, 8, -1, i32::MAX] {
            let mut wire = good.clone();
            wire.status_code = phase;
            assert_eq!(
                intent_from_wire(&OPERATION, &wire),
                Err(WireError::PhaseCode),
                "phase {phase} is not declared"
            );
        }

        // Every declared phase is accepted, paired with the completion log id
        // that phase requires.
        for phase in super::PHASES {
            let mut model = intent();
            model.status_code = phase;
            model.completed_log_id =
                super::is_terminal_phase(phase).then_some(LogId::new(3, [0x73u8; 32], 30));
            let wire = intent_to_wire(&OPERATION, &model).expect("encode");
            assert_eq!(intent_from_wire(&OPERATION, &wire), Ok(model));
        }
    }

    /// §9 evicts terminal intents by completion log id, and apply stamps it
    /// exactly when an intent becomes terminal. Either half without the other
    /// leaves that ordering undefined.
    #[test]
    fn an_intents_completion_log_id_must_match_its_phase() {
        let mut active = intent();
        active.status_code = super::super::membership::PHASE_PENDING;
        active.completed_log_id = Some(LogId::new(3, [0x73u8; 32], 30));
        let wire = intent_to_wire(&OPERATION, &active).expect("encode");
        assert_eq!(
            intent_from_wire(&OPERATION, &wire),
            Err(WireError::IntentCompletionIncoherent),
            "an active intent has not completed"
        );

        let mut terminal = intent();
        terminal.status_code = super::super::membership::PHASE_COMPLETED;
        terminal.completed_log_id = None;
        let wire = intent_to_wire(&OPERATION, &terminal).expect("encode");
        assert_eq!(
            intent_from_wire(&OPERATION, &wire),
            Err(WireError::IntentCompletionIncoherent),
            "a terminal intent has no eviction ordering without one"
        );
    }

    /// A ticket that is not valid UTF-8 is refused rather than replaced.
    ///
    /// Every ticket that entered through the command boundary came from a Fory
    /// `String`, so this is unreachable from a conforming history -- which is
    /// the point: `from_utf8_lossy` would have silently rewritten dialling
    /// material for the one input that is not.
    #[test]
    fn a_non_utf8_ticket_is_refused_rather_than_replaced() {
        let mut broken = intent();
        broken.plan.member_seeds[0].ticket = vec![0xff, 0xfe];
        assert_eq!(
            intent_to_wire(&OPERATION, &broken),
            Err(WireError::TicketEncoding)
        );
    }

    /// The §12.1 intent charge measures the same value that is stored.
    ///
    /// An earlier revision built its own copy of the action for measurement,
    /// so the two could differ without anything failing.
    #[test]
    fn the_intent_charge_measures_the_bytes_that_are_stored() {
        use crate::coordination::contract::{payload_fory, ChangeMembershipActionV1};

        let intent = intent();
        let stored = intent_to_wire(&OPERATION, &intent).expect("encode");
        let encoded = payload_fory::<ChangeMembershipActionV1>()
            .serialize(&stored.desired_plan)
            .expect("encode");
        assert_eq!(intent.charge_len(&OPERATION), encoded.len());
    }

    /// A route round-trips, and its two fallible fields refuse independently.
    #[test]
    fn a_route_round_trips_and_refuses_malformed_fields() {
        let route = Route {
            ticket: b"aster1route".to_vec(),
            source_operation_id: [0xA1u8; 16],
            introduced_log_id: LogId::new(4, [0xA2u8; 32], 44),
        };
        let wire: NodeRouteV1 = (&route).into();
        assert_eq!(Route::try_from(&wire), Ok(route));

        let mut bad_operation = wire.clone();
        bad_operation.source_operation_id = vec![0u8; 15];
        assert_eq!(
            Route::try_from(&bad_operation),
            Err(WireError::OperationIdWidth)
        );

        let mut bad_leader = wire;
        bad_leader.introduced_log_id.leader_node_id = vec![0u8; 8];
        assert_eq!(Route::try_from(&bad_leader), Err(WireError::NodeIdWidth));
    }

    /// A route ticket is opaque bytes on both sides, so no encoding question
    /// arises there -- unlike a member seed's, which is a Fory `String`.
    #[test]
    fn a_route_ticket_survives_bytes_no_string_could_carry() {
        let route = Route {
            ticket: vec![0xff, 0xfe, 0x00],
            source_operation_id: [0u8; 16],
            introduced_log_id: LogId::new(1, [0u8; 32], 1),
        };
        let wire: NodeRouteV1 = (&route).into();
        assert_eq!(Route::try_from(&wire), Ok(route));
    }
}
