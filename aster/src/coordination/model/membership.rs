//! Membership intent state (§9) -- the replicated record of a membership
//! change, with **no network execution**.
//!
//! Slice 1 owns the state machine: accepting an intent, advancing its phase,
//! cancelling it, evicting terminal history, and retaining routes. Adding a
//! learner, waiting for catch-up and calling OpenRaft's `change_membership` is
//! Slice 3's controller -- apply must be a pure function of replicated state,
//! and a controller is neither pure nor replicated.
//!
//! ## The intent is the plan, not a digest of it
//!
//! `IntentRowV1` stores the whole `ChangeMembershipActionV1`: desired voters,
//! retained learners and member seeds. It has to. A failover controller resumes
//! pending work after a restart, and it cannot reconstruct a plan from a hash
//! and a byte count -- which is what an earlier revision of this model stored,
//! making resumption impossible in a way no test of the model alone would show.
//!
//! ## `operation_id` is group-global, and retention makes that finite
//!
//! Reusing an id with the identical request hash returns the existing intent;
//! another plan returns `17 OperationIdConflict`, even from a different
//! administrator. Only 64 terminal intents are retained, so an evicted id is
//! treated as **unknown** and a resubmission is accepted as new. That is safe
//! in a way evicting a client tombstone would not be, because a membership plan
//! is idempotent against committed membership -- re-running an evicted plan
//! converges rather than double-applying.
//!
//! ## Phase transitions are checked, not assumed
//!
//! `MembershipProgress` names both the phase it expects and the phase it moves
//! to, and **both** are validated. An earlier revision checked only the
//! expected phase, so a controller report could drive an intent from `Pending`
//! straight to `Cancelled`, or out of a terminal phase entirely.

use std::collections::BTreeSet;

use super::limits::{charge_intent, charge_route, MAX_RETAINED_INTENTS, MAX_RETAINED_ROUTES};
use super::shape::{self, ShapeLimit};
use super::state::{Intent, LogId, MembershipPlan, NodeId, OperationId, Route, State};
use crate::coordination::contract::{SYS_MEMBERSHIP_ADVANCED, SYS_MEMBERSHIP_PHASE_MISMATCH};
use crate::coordination::reason::{Outcome, RefusalReason};

/// Membership-intent status codes (§5.2).
pub const PHASE_PENDING: i32 = 1;
pub const PHASE_ADDING_LEARNERS: i32 = 2;
pub const PHASE_CHANGING_VOTERS: i32 = 3;
pub const PHASE_COMPLETED: i32 = 4;
pub const PHASE_FAILED: i32 = 5;
pub const PHASE_CANCEL_REQUESTED: i32 = 6;
pub const PHASE_CANCELLED: i32 = 7;

/// Is this phase terminal -- evictable, and no longer the one active intent?
///
/// `CancelRequested` is deliberately **not** terminal: the cancellation has been
/// asked for but the controller has not unwound yet, so the intent is still
/// active and must not be evicted out from under it.
pub fn is_terminal_phase(phase: i32) -> bool {
    matches!(phase, PHASE_COMPLETED | PHASE_FAILED | PHASE_CANCELLED)
}

/// Is `to` a legal successor of `from` (§9)?
///
/// The controller drives forward through the add-learners and change-voters
/// phases, and any active phase may fail. `CancelRequested` is reached only by
/// `cancel_membership`, never by a progress report, and unwinds to `Cancelled`.
/// Nothing leaves a terminal phase.
pub fn is_legal_transition(from: i32, to: i32) -> bool {
    if is_terminal_phase(from) {
        return false;
    }
    match (from, to) {
        (PHASE_PENDING, PHASE_ADDING_LEARNERS)
        | (PHASE_ADDING_LEARNERS, PHASE_CHANGING_VOTERS)
        | (PHASE_CHANGING_VOTERS, PHASE_COMPLETED)
        | (PHASE_CANCEL_REQUESTED, PHASE_CANCELLED) => true,
        // Any active phase may fail.
        (_, PHASE_FAILED) => true,
        _ => false,
    }
}

/// A `change_membership` request (§5.2 method 9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeMembershipAction {
    pub operation_id: OperationId,
    /// The committed membership the plan was built against. `None` means "none
    /// committed yet" -- a distinct value, not a zero.
    pub expected_membership_log_id: Option<LogId>,
    pub plan: MembershipPlan,
}

/// A `cancel_membership` request (§5.2 method 11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelMembershipAction {
    pub operation_id: OperationId,
    pub expected_phase_code: i32,
}

/// A system `MembershipProgress` command (§5.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipProgressCommand {
    pub operation_id: OperationId,
    pub expected_phase: i32,
    pub new_phase: i32,
    pub observed_membership_log_id: Option<LogId>,
    /// The system result code to record on reaching a terminal phase.
    pub result_code: i32,
}

/// What a membership command writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Apply(Box<IntentEffect>),
    /// The identical request was already accepted under this id: return the
    /// existing intent rather than creating a second one.
    Existing {
        phase: i32,
    },
    Refuse(RefusalReason),
    /// Refused by a §12.1 request-shape bound, which has no §5.2 reason code.
    RefuseShapeless(ShapeLimit),
}

/// Every §9/§12.1 shape bound a membership plan can violate.
///
/// Ordered so a plan violating two reports the same one on every voter.
///
/// `current_members` is the committed membership, which decides what "new node"
/// means -- and therefore which seeds must carry a join permit. Passing it in
/// rather than reading state here keeps the function a pure predicate over the
/// two things it compares.
fn plan_shape(plan: &MembershipPlan, current_members: &BTreeSet<NodeId>) -> Option<ShapeLimit> {
    if let Some(limit) =
        shape::check_membership_plan(plan.desired_voters.len(), plan.retained_learners.len())
    {
        return Some(limit);
    }

    // §9: both vectors are **sorted unique**. Without this a plan may name the
    // same voter five times and satisfy a count check while describing a
    // one-voter group.
    if !shape::sorted_unique(&plan.desired_voters) || !shape::sorted_unique(&plan.retained_learners)
    {
        return Some(ShapeLimit::MembershipPlanOrder);
    }
    let seed_ids: Vec<NodeId> = plan.member_seeds.iter().map(|s| s.node_id).collect();
    if !shape::sorted_unique(&seed_ids) {
        return Some(ShapeLimit::MembershipPlanOrder);
    }
    // A voter that is also a learner is two different roles for one node.
    if plan
        .desired_voters
        .iter()
        .any(|v| plan.retained_learners.contains(v))
    {
        return Some(ShapeLimit::MembershipPlanOrder);
    }

    let in_plan =
        |node: &NodeId| plan.desired_voters.contains(node) || plan.retained_learners.contains(node);
    for seed in &plan.member_seeds {
        if let Some(limit) =
            shape::check_member_seed(seed.ticket.len(), seed.join_permit.as_ref().map(Vec::len))
        {
            return Some(limit);
        }
        // §9: seeds carry route material for the plan's own members. Without
        // this rule the seed vector is unbounded independently of the plan, so a
        // legal three-voter plan could carry sixty-five seeds and insert
        // sixty-five routes past §12.1's cap of 64 -- which is exactly how the
        // cap was bypassed. Bounding seeds by plan membership makes the route
        // count per intent at most `MAX_VOTERS + MAX_RETAINED_LEARNERS`.
        if !in_plan(&seed.node_id) {
            return Some(ShapeLimit::MemberSeedUnknownNode);
        }
        // §9: a valid signed join permit is mandatory exactly for a new node and
        // absent for an existing member. (Verifying the signature is §7.1's
        // job -- it needs the authority key, which the state machine does not
        // hold. What apply can decide deterministically is presence.)
        let is_new = !current_members.contains(&seed.node_id);
        if is_new != seed.join_permit.is_some() {
            return Some(ShapeLimit::MemberSeedPermit);
        }
    }

    // §9: every **new** node must be seeded, or the group commits a membership
    // containing a node it has no route to.
    let seeded = |node: &NodeId| plan.member_seeds.iter().any(|s| &s.node_id == node);
    for node in plan.desired_voters.iter().chain(&plan.retained_learners) {
        if !current_members.contains(node) && !seeded(node) {
            return Some(ShapeLimit::MemberSeedUnknownNode);
        }
    }
    None
}

/// The intent an accepted command writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentEffect {
    pub operation_id: OperationId,
    pub intent: Intent,
    /// The outcome this command reports.
    pub outcome: Outcome,
    /// Terminal intents to evict, oldest-completed first.
    pub evict: Vec<OperationId>,
    /// Routes to retain, one per member seed carrying a ticket.
    pub routes: Vec<(NodeId, Route)>,
    /// Routes displaced by the ones above.
    pub evict_routes: Vec<NodeId>,
    /// The committed membership this command observed, if it advanced it.
    pub membership_log_id: Option<LogId>,
    /// The voters and learners this plan makes current, when it completes.
    ///
    /// Carried on the effect rather than re-read, so the member set and the
    /// membership log id are established by the same value.
    pub intent_members: Vec<NodeId>,
}

impl Decision {
    pub fn outcome(&self) -> Option<Outcome> {
        match self {
            Decision::Apply(e) => Some(e.outcome),
            Decision::Existing { .. } => Some(Outcome::MembershipAccepted),
            Decision::Refuse(_) | Decision::RefuseShapeless(_) => None,
        }
    }
}

/// Decide one `change_membership`.
pub fn decide_change(
    state: &State,
    actor: &NodeId,
    request_hash: &[u8; 32],
    action: &ChangeMembershipAction,
) -> Decision {
    // §12.1 request-shape bounds, checked before anything is decided against
    // replicated state: a plan outside the v1 shape was never a well-posed
    // request. These were declared and had no caller.
    if let Some(limit) = plan_shape(&action.plan, &state.members) {
        return Decision::RefuseShapeless(limit);
    }

    if !state.admins.contains(actor) {
        return Decision::Refuse(RefusalReason::PermissionChanged);
    }

    // A still-known id resolves before anything else: the point of a
    // group-global id is that resubmitting it is answerable without re-deciding.
    if let Some(existing) = state.intents.get(&action.operation_id) {
        if &existing.request_hash == request_hash {
            return Decision::Existing {
                phase: existing.status_code,
            };
        }
        return Decision::Refuse(RefusalReason::OperationIdConflict);
    }

    // Compared against the committed membership log id, not a synthetic
    // counter: `None` is "no membership committed yet", which is a value in its
    // own right rather than a zero.
    if action.expected_membership_log_id != state.membership_log_id {
        return Decision::Refuse(RefusalReason::MembershipRevisionMismatch);
    }

    if state
        .intents
        .values()
        .any(|i| !is_terminal_phase(i.status_code))
    {
        return Decision::Refuse(RefusalReason::MembershipIntentActive);
    }

    // §12.1: intent and route rows are growth-funded, so an accepted plan must
    // fit the budget like any other write. An earlier revision checked the byte
    // budget for resource values only, so these two tables wrote past a
    // saturated budget.
    let routes = routes_from(action);
    let added = charge_intent(action.plan.encoded_len())
        + routes
            .iter()
            .map(|(_, r)| charge_route(r.ticket.len()))
            .sum::<u64>();
    if !state.growth_fits(added) {
        return Decision::Refuse(RefusalReason::StateBudgetExhausted);
    }

    let intent = Intent {
        actor: *actor,
        request_hash: *request_hash,
        expected_membership: action.expected_membership_log_id,
        plan: action.plan.clone(),
        status_code: PHASE_PENDING,
        result_code: 0,
        // Stamped by apply, which knows the log id.
        accepted_log_id: LogId::new(0, [0u8; 32], 0),
        completed_log_id: None,
        observed_membership_log_id: None,
    };

    Decision::Apply(Box::new(IntentEffect {
        operation_id: action.operation_id,
        intent,
        outcome: Outcome::MembershipAccepted,
        // `Pending` is not terminal, so it displaces nothing: the cap bounds
        // terminal history only.
        evict: Vec::new(),
        evict_routes: route_evictions(state, &routes),
        routes,
        membership_log_id: None,
        intent_members: Vec::new(),
    }))
}

/// Which routes to evict so the retained set fits §12.1's cap of 64.
///
/// Oldest-first by introducing log id, and a route for a node in the **current
/// committed membership** is never a candidate: routes are advisory dialling
/// hints, so losing one costs a discovery round trip -- except for a current
/// member, where losing it can cost the ability to reach a voter.
///
/// An earlier revision returned an empty list unconditionally, so successive
/// valid intents grew the table without bound and the cap enforced nothing.
fn route_evictions(state: &State, incoming: &[(NodeId, Route)]) -> Vec<NodeId> {
    let new_nodes = incoming
        .iter()
        .filter(|(node, _)| !state.routes.contains_key(node))
        .count() as u64;
    if new_nodes == 0 {
        return Vec::new();
    }

    let mut candidates: Vec<((u64, u64), NodeId)> = state
        .routes
        .iter()
        .filter(|(node, _)| !state.members.contains(*node))
        .filter(|(node, _)| !incoming.iter().any(|(n, _)| n == *node))
        .map(|(node, route)| {
            (
                (route.introduced_log_id.term, route.introduced_log_id.index),
                *node,
            )
        })
        .collect();
    candidates.sort_unstable();

    let retained = state.routes.len() as u64;
    let over = (retained + new_nodes).saturating_sub(MAX_RETAINED_ROUTES);
    candidates
        .into_iter()
        .take(over as usize)
        .map(|(_, node)| node)
        .collect()
}

/// The routes a plan's member seeds introduce.
fn routes_from(action: &ChangeMembershipAction) -> Vec<(NodeId, Route)> {
    action
        .plan
        .member_seeds
        .iter()
        .filter(|seed| !seed.ticket.is_empty())
        .map(|seed| {
            (
                seed.node_id,
                Route {
                    ticket: seed.ticket.clone(),
                    source_operation_id: action.operation_id,
                    introduced_log_id: LogId::new(0, [0u8; 32], 0),
                },
            )
        })
        .collect()
}

/// Decide one `cancel_membership`.
pub fn decide_cancel(state: &State, actor: &NodeId, action: &CancelMembershipAction) -> Decision {
    if !state.admins.contains(actor) {
        return Decision::Refuse(RefusalReason::PermissionChanged);
    }

    let Some(existing) = state.intents.get(&action.operation_id) else {
        return Decision::Refuse(RefusalReason::MembershipCannotCancel);
    };

    if existing.status_code != action.expected_phase_code {
        return Decision::Refuse(RefusalReason::MembershipRevisionMismatch);
    }

    let mut intent = existing.clone();
    let (status, evict) = match existing.status_code {
        // Nothing has been done yet, so there is nothing to unwind: a pending
        // intent cancels **immediately** and terminally. Routing it through
        // `CancelRequested` would leave an active intent nobody will ever
        // advance, blocking every future change.
        PHASE_PENDING => (PHASE_CANCELLED, evictions(state, 1)),
        // The controller has learners in flight and must unwind them, so the
        // cancellation is *requested* and completes when it reports back.
        PHASE_ADDING_LEARNERS => (PHASE_CANCEL_REQUESTED, Vec::new()),
        // Past the point of no return: the voter set is already changing, and
        // unwinding a partially applied membership change is not something v1
        // can do safely.
        _ => return Decision::Refuse(RefusalReason::MembershipCannotCancel),
    };

    intent.status_code = status;
    if is_terminal_phase(status) {
        intent.result_code = crate::coordination::contract::SYS_MEMBERSHIP_ADVANCED;
    }

    Decision::Apply(Box::new(IntentEffect {
        operation_id: action.operation_id,
        intent,
        // §5.2 has a dedicated outcome for this: `10 MembershipCancelled`.
        // Reporting `8 MembershipAccepted` would tell a client its change was
        // accepted when it was just cancelled.
        outcome: Outcome::MembershipCancelled,
        evict,
        routes: Vec::new(),
        evict_routes: Vec::new(),
        membership_log_id: None,
        intent_members: Vec::new(),
    }))
}

/// Decide one system `MembershipProgress`.
///
/// `None` when the report does not apply -- a stale controller report, which
/// commits harmlessly like a stale `Expire`.
pub fn decide_progress(state: &State, command: &MembershipProgressCommand) -> Option<IntentEffect> {
    let existing = state.intents.get(&command.operation_id)?;
    if existing.status_code != command.expected_phase {
        return None;
    }
    // Both ends are checked. Validating only the expected phase would let a
    // report drive an intent from `Pending` straight to `Cancelled`, or out of
    // a terminal phase entirely.
    if !is_legal_transition(existing.status_code, command.new_phase) {
        return None;
    }

    let mut intent = existing.clone();
    intent.status_code = command.new_phase;
    intent.observed_membership_log_id = command.observed_membership_log_id;

    let becoming_terminal = is_terminal_phase(command.new_phase);
    if becoming_terminal {
        intent.result_code = command.result_code;
    }

    Some(IntentEffect {
        operation_id: command.operation_id,
        intent,
        outcome: Outcome::MembershipAccepted,
        evict: if becoming_terminal {
            evictions(state, 1)
        } else {
            Vec::new()
        },
        routes: Vec::new(),
        evict_routes: Vec::new(),
        // A completed change is what advances committed membership.
        membership_log_id: (command.new_phase == PHASE_COMPLETED)
            .then_some(command.observed_membership_log_id)
            .flatten(),
        intent_members: if command.new_phase == PHASE_COMPLETED {
            existing
                .plan
                .desired_voters
                .iter()
                .chain(&existing.plan.retained_learners)
                .copied()
                .collect()
        } else {
            Vec::new()
        },
    })
}

/// The system result code a progress report records.
pub fn progress_result_code(applied: bool) -> i32 {
    if applied {
        SYS_MEMBERSHIP_ADVANCED
    } else {
        SYS_MEMBERSHIP_PHASE_MISMATCH
    }
}

/// Which terminal intents to evict so `retained + incoming` fits the cap.
///
/// Ordered by **completion** log id, which is what §9 specifies: two intents
/// accepted in one order can complete in the other, and evicting by acceptance
/// would discard the more recently finished one. Ties break by operation id so
/// the order is total on every voter.
fn evictions(state: &State, incoming_terminal: u64) -> Vec<OperationId> {
    let mut terminal: Vec<((u64, u64), OperationId)> = state
        .intents
        .iter()
        .filter(|(_, i)| is_terminal_phase(i.status_code))
        .map(|(id, i)| {
            let completed = i
                .completed_log_id
                .map_or((u64::MAX, u64::MAX), |l| (l.term, l.index));
            (completed, *id)
        })
        .collect();
    terminal.sort_unstable();

    let over = (terminal.len() as u64 + incoming_terminal).saturating_sub(MAX_RETAINED_INTENTS);
    terminal
        .into_iter()
        .take(over as usize)
        .map(|(_, id)| id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::state::MemberSeed;
    use crate::coordination::reason::Reason;

    const ADMIN: NodeId = [1u8; 32];
    const OTHER: NodeId = [2u8; 32];
    const LEADER: NodeId = [0x11u8; 32];

    fn op(n: u8) -> OperationId {
        [n; 16]
    }

    fn log(index: u64) -> LogId {
        LogId::new(1, LEADER, index)
    }

    fn base() -> State {
        let mut s = State::default();
        s.admins.insert(ADMIN);
        s.admin_revision = 1;
        s.growth_bytes = s.recompute_growth();
        s
    }

    /// A §9-legal plan: exactly three voters, sorted and unique, each seeded.
    ///
    /// Every voter is seeded because §9 requires a seed for every **new** node,
    /// and these fixtures start from an empty committed membership -- so all
    /// three are new, and each seed carries the join permit a new node must
    /// have. `voters` seeds their ids and is padded and sorted, so a fixture
    /// cannot accidentally build a shape the contract forbids.
    fn plan(voters: &[u8]) -> MembershipPlan {
        plan_against(voters, &BTreeSet::new())
    }

    /// The same, against a stated committed membership: a node already in it is
    /// not new, so its seed must **not** carry a permit.
    fn plan_against(voters: &[u8], members: &BTreeSet<NodeId>) -> MembershipPlan {
        let mut ids: Vec<u8> = voters.to_vec();
        while ids.len() < 3 {
            ids.push(100 + ids.len() as u8);
        }
        ids.truncate(3);
        let mut nodes: Vec<NodeId> = ids
            .iter()
            .map(|b| {
                let mut id = [0u8; 32];
                id[0] = *b;
                id
            })
            .collect();
        nodes.sort_unstable();
        nodes.dedup();

        MembershipPlan {
            desired_voters: nodes.clone(),
            retained_learners: Vec::new(),
            member_seeds: nodes
                .iter()
                .map(|node| MemberSeed {
                    node_id: *node,
                    ticket: b"aster1ticket".to_vec(),
                    join_permit: (!members.contains(node)).then(|| vec![0xEEu8; 64]),
                })
                .collect(),
        }
    }

    fn change(id: OperationId, voters: &[u8], expected: Option<LogId>) -> ChangeMembershipAction {
        ChangeMembershipAction {
            operation_id: id,
            expected_membership_log_id: expected,
            plan: plan(voters),
        }
    }

    fn intent(phase: i32, completed: Option<LogId>) -> Intent {
        Intent {
            actor: ADMIN,
            request_hash: [7u8; 32],
            expected_membership: None,
            plan: plan(&[1, 2, 3]),
            status_code: phase,
            result_code: 0,
            accepted_log_id: log(1),
            completed_log_id: completed,
            observed_membership_log_id: None,
        }
    }

    fn reason(d: Decision) -> Reason {
        match d {
            Decision::Refuse(r) => r.get(),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn effect(d: Decision) -> IntentEffect {
        match d {
            Decision::Apply(e) => *e,
            other => panic!("expected an applied intent, got {other:?}"),
        }
    }

    /// Fill the route table to `count` entries, oldest first by index.
    fn with_routes(s: &mut State, count: u64) {
        for i in 0..count {
            let mut node = [0u8; 32];
            node[0..8].copy_from_slice(&i.to_be_bytes());
            node[31] = 0xAA;
            s.routes.insert(
                node,
                Route {
                    ticket: b"aster1old".to_vec(),
                    introduced_log_id: log(i + 1),
                    source_operation_id: op(0),
                },
            );
        }
        s.growth_bytes = s.recompute_growth();
    }

    // ── §9 plan shape ───────────────────────────────────────────────────────

    fn shapeless(d: Decision) -> ShapeLimit {
        match d {
            Decision::RefuseShapeless(limit) => limit,
            other => panic!("expected a shape refusal, got {other:?}"),
        }
    }

    /// **The route-cap bypass.** A legal three-voter plan carrying sixty-five
    /// tiny seeds passed every earlier check: the voter count was right, each
    /// seed was individually small, and an empty route table offered no
    /// eviction candidates -- so apply inserted sixty-five routes and the cap of
    /// 64 was simply not reached by the code that enforces it. Bounding seeds by
    /// plan membership closes it at the source: a plan holds at most
    /// `MAX_VOTERS + MAX_RETAINED_LEARNERS` nodes, so it can introduce at most
    /// that many routes.
    #[test]
    fn a_plan_cannot_carry_seeds_for_nodes_it_does_not_contain() {
        let s = base();
        let mut p = plan(&[1, 2, 3]);
        for i in 0..65u8 {
            let mut node = [0u8; 32];
            node[0] = 200;
            node[1] = i;
            p.member_seeds.push(MemberSeed {
                node_id: node,
                ticket: b"t".to_vec(),
                join_permit: Some(vec![0xEEu8; 64]),
            });
        }
        p.member_seeds.sort_by_key(|s| s.node_id);

        assert_eq!(
            shapeless(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &ChangeMembershipAction {
                    operation_id: op(1),
                    expected_membership_log_id: None,
                    plan: p,
                }
            )),
            ShapeLimit::MemberSeedUnknownNode,
        );
    }

    /// The bound that makes the cap safe, stated as the property rather than the
    /// mechanism: no accepted plan can introduce more routes than a plan can
    /// hold nodes.
    #[test]
    fn an_accepted_plan_introduces_at_most_one_route_per_plan_node() {
        let s = base();
        let e = effect(decide_change(
            &s,
            &ADMIN,
            &[7u8; 32],
            &change(op(1), &[1, 2, 3], None),
        ));
        assert!(
            e.routes.len() <= super::shape::MAX_VOTERS + super::shape::MAX_RETAINED_LEARNERS,
            "an intent cannot introduce more routes than its plan has nodes"
        );
    }

    /// §9: the node vectors are **sorted unique**. An unsorted plan is a
    /// different request from its sorted spelling -- they hash differently --
    /// so it is refused rather than normalised.
    #[test]
    fn an_unsorted_or_duplicated_plan_is_refused() {
        let s = base();
        let sorted = plan(&[1, 2, 3]);

        let mut reversed = sorted.clone();
        reversed.desired_voters.reverse();
        assert_eq!(
            shapeless(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &ChangeMembershipAction {
                    operation_id: op(1),
                    expected_membership_log_id: None,
                    plan: reversed,
                }
            )),
            ShapeLimit::MembershipPlanOrder,
        );

        // Three entries naming one node: a count check alone would call this a
        // legal three-voter group.
        let mut duplicated = sorted.clone();
        duplicated.desired_voters = vec![[1u8; 32], [1u8; 32], [1u8; 32]];
        assert_eq!(
            shapeless(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &ChangeMembershipAction {
                    operation_id: op(1),
                    expected_membership_log_id: None,
                    plan: duplicated,
                }
            )),
            ShapeLimit::MembershipPlanOrder,
        );

        // Unsorted seeds, same rule.
        let mut unsorted_seeds = sorted.clone();
        unsorted_seeds.member_seeds.reverse();
        assert_eq!(
            shapeless(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &ChangeMembershipAction {
                    operation_id: op(1),
                    expected_membership_log_id: None,
                    plan: unsorted_seeds,
                }
            )),
            ShapeLimit::MembershipPlanOrder,
        );
    }

    /// A node cannot be both a voter and a learner: that is two roles for one
    /// node, and the joint-config arithmetic has no answer for it.
    #[test]
    fn a_node_cannot_be_both_a_voter_and_a_learner() {
        let s = base();
        let mut p = plan(&[1, 2, 3]);
        p.retained_learners = vec![p.desired_voters[0]];
        assert_eq!(
            shapeless(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &ChangeMembershipAction {
                    operation_id: op(1),
                    expected_membership_log_id: None,
                    plan: p,
                }
            )),
            ShapeLimit::MembershipPlanOrder,
        );
    }

    /// §9: a join permit is mandatory exactly for a new node and absent for an
    /// existing member. Both directions, because either one alone would let the
    /// other through.
    #[test]
    fn a_join_permit_is_required_exactly_for_a_new_node() {
        let mut s = base();

        // New node, no permit.
        let mut p = plan(&[1, 2, 3]);
        p.member_seeds[0].join_permit = None;
        assert_eq!(
            shapeless(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &ChangeMembershipAction {
                    operation_id: op(1),
                    expected_membership_log_id: None,
                    plan: p,
                }
            )),
            ShapeLimit::MemberSeedPermit,
        );

        // Existing member, permit present.
        let committed = plan(&[1, 2, 3]);
        s.members = committed.desired_voters.iter().copied().collect();
        assert_eq!(
            shapeless(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &ChangeMembershipAction {
                    operation_id: op(1),
                    expected_membership_log_id: None,
                    plan: committed,
                }
            )),
            ShapeLimit::MemberSeedPermit,
        );

        // The same plan with permits dropped is accepted, so the rule is a
        // correspondence and not a blanket refusal.
        assert!(matches!(
            decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &ChangeMembershipAction {
                    operation_id: op(1),
                    expected_membership_log_id: None,
                    plan: plan_against(&[1, 2, 3], &s.members),
                }
            ),
            Decision::Apply(_)
        ));
    }

    /// §9: every new node must be seeded, or the group commits a membership
    /// containing a node it has no route to.
    #[test]
    fn a_new_node_without_a_seed_is_refused() {
        let s = base();
        let mut p = plan(&[1, 2, 3]);
        p.member_seeds.remove(0);
        assert_eq!(
            shapeless(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &ChangeMembershipAction {
                    operation_id: op(1),
                    expected_membership_log_id: None,
                    plan: p,
                }
            )),
            ShapeLimit::MemberSeedUnknownNode,
        );
    }

    // ── Route retention ─────────────────────────────────────────────────────

    #[test]
    fn a_full_route_table_evicts_the_oldest_to_make_room() {
        let mut s = base();
        with_routes(&mut s, MAX_RETAINED_ROUTES);
        let e = effect(decide_change(
            &s,
            &ADMIN,
            &[7u8; 32],
            &change(op(1), &[1, 2, 3], None),
        ));

        // Three seeds arrive, so exactly three incumbents leave -- the three
        // with the lowest introducing log ids.
        assert_eq!(e.routes.len(), 3);
        let oldest: Vec<NodeId> = (0..3u64)
            .map(|i| {
                let mut n = [0u8; 32];
                n[0..8].copy_from_slice(&i.to_be_bytes());
                n[31] = 0xAA;
                n
            })
            .collect();
        assert_eq!(e.evict_routes, oldest);
    }

    #[test]
    fn an_unfilled_route_table_evicts_nothing() {
        let mut s = base();
        // Three seeds arrive, so leave room for exactly three.
        with_routes(&mut s, MAX_RETAINED_ROUTES - 3);
        let e = effect(decide_change(
            &s,
            &ADMIN,
            &[7u8; 32],
            &change(op(1), &[1, 2, 3], None),
        ));
        assert!(e.evict_routes.is_empty());
    }

    #[test]
    fn re_seeding_a_known_node_needs_no_eviction() {
        let mut s = base();
        with_routes(&mut s, MAX_RETAINED_ROUTES);
        // Every seed in the plan is already routed, so the table does not grow;
        // three incumbents are dropped to keep the count at the cap.
        for node in plan(&[1, 2, 3]).desired_voters {
            s.routes.insert(
                node,
                Route {
                    ticket: b"aster1stale".to_vec(),
                    introduced_log_id: log(500),
                    source_operation_id: op(0),
                },
            );
        }
        for i in 0..3u64 {
            let mut n = [0u8; 32];
            n[0..8].copy_from_slice(&i.to_be_bytes());
            n[31] = 0xAA;
            s.routes.remove(&n);
        }
        s.growth_bytes = s.recompute_growth();

        let e = effect(decide_change(
            &s,
            &ADMIN,
            &[7u8; 32],
            &change(op(1), &[1, 2, 3], None),
        ));
        assert!(e.evict_routes.is_empty(), "replacing a route is not growth");
    }

    #[test]
    fn a_current_members_route_is_never_evicted() {
        let mut s = base();
        with_routes(&mut s, MAX_RETAINED_ROUTES);
        // Make the oldest route belong to a committed voter. Losing an
        // arbitrary peer's dialling hint costs a discovery round trip; losing a
        // voter's can cost quorum.
        let mut oldest = [0u8; 32];
        oldest[31] = 0xAA;
        s.members.insert(oldest);

        let e = effect(decide_change(
            &s,
            &ADMIN,
            &[7u8; 32],
            &change(op(1), &[1, 2, 3], None),
        ));
        assert!(!e.evict_routes.contains(&oldest));
        assert_eq!(e.evict_routes.len(), 3, "the next-oldest go instead");
        let next: Vec<NodeId> = (1..4u64)
            .map(|i| {
                let mut n = [0u8; 32];
                n[0..8].copy_from_slice(&i.to_be_bytes());
                n[31] = 0xAA;
                n
            })
            .collect();
        assert_eq!(e.evict_routes, next);
    }

    #[test]
    fn completing_a_change_makes_its_plan_the_protected_member_set() {
        let mut s = base();
        let e = effect(decide_change(
            &s,
            &ADMIN,
            &[7u8; 32],
            &change(op(1), &[1, 2, 3], None),
        ));
        let mut pending = e.intent.clone();
        // Only the last hop of the phase walk commits a new membership.
        pending.status_code = PHASE_CHANGING_VOTERS;
        s.intents.insert(op(1), pending);

        let done = (decide_progress(
            &s,
            &MembershipProgressCommand {
                operation_id: op(1),
                expected_phase: PHASE_CHANGING_VOTERS,
                new_phase: PHASE_COMPLETED,
                result_code: 0,
                observed_membership_log_id: Some(log(9)),
            },
        ))
        .expect("a pending intent advances")
        .intent_members;
        assert_eq!(done.len(), 3, "three voters, no learners");
        let mut first = [0u8; 32];
        first[0] = 1;
        assert!(done.contains(&first));
    }

    #[test]
    fn an_exhausted_budget_refuses_the_intent() {
        let mut s = base();
        // Saturate growth so the intent and route rows cannot be funded.
        s.growth_bytes = crate::coordination::model::limits::GROWTH_BUDGET;
        assert_eq!(
            reason(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &change(op(1), &[1, 2, 3], None)
            )),
            Reason::StateBudgetExhausted,
            "intent and route rows are growth-funded like any other write",
        );
    }

    // ── Acceptance ──────────────────────────────────────────────────────────

    #[test]
    fn only_an_administrator_may_change_membership() {
        let s = base();
        assert_eq!(
            reason(decide_change(
                &s,
                &OTHER,
                &[7u8; 32],
                &change(op(1), &[1, 2, 3], None)
            )),
            Reason::PermissionChanged
        );
    }

    /// The whole plan is stored, not a digest of it: a failover controller has
    /// to re-run the plan after a restart, and it cannot reconstruct desired
    /// voters or member seeds from a hash.
    #[test]
    fn an_accepted_intent_stores_the_whole_plan() {
        let s = base();
        let e = effect(decide_change(
            &s,
            &ADMIN,
            &[7u8; 32],
            &change(op(1), &[1, 2, 3], None),
        ));

        assert_eq!(e.intent.status_code, PHASE_PENDING);
        assert_eq!(e.intent.plan.desired_voters.len(), 3);
        assert_eq!(
            e.intent.plan.member_seeds.len(),
            3,
            "every new node is seeded, which is what §9 requires"
        );
        assert_eq!(e.intent.plan.member_seeds[0].ticket, b"aster1ticket");
        assert_eq!(e.intent.actor, ADMIN);
        assert_eq!(e.intent.request_hash, [7u8; 32]);
        assert_eq!(e.outcome, Outcome::MembershipAccepted);
    }

    /// A member seed's ticket becomes a retained route, so the controller can
    /// dial the node it is about to add.
    #[test]
    fn an_accepted_intent_retains_its_member_seed_routes() {
        let s = base();
        let e = effect(decide_change(
            &s,
            &ADMIN,
            &[7u8; 32],
            &change(op(1), &[1, 2, 3], None),
        ));
        assert_eq!(e.routes.len(), 3, "one route per seeded new node");
        let (node, route) = &e.routes[0];
        let mut first = [0u8; 32];
        first[0] = 1;
        assert_eq!(*node, first);
        assert_eq!(route.ticket, b"aster1ticket");
        assert_eq!(route.source_operation_id, op(1));
    }

    /// The expectation is the committed membership **log id**, and `None` --
    /// "nothing committed yet" -- is a value in its own right.
    #[test]
    fn the_expectation_is_a_log_id_and_none_is_distinct() {
        let mut s = base();
        // Nothing committed: `None` matches.
        assert!(matches!(
            decide_change(&s, &ADMIN, &[7u8; 32], &change(op(1), &[1, 2, 3], None)),
            Decision::Apply(_)
        ));

        // Once membership exists, `None` no longer matches it.
        s.membership_log_id = Some(log(5));
        assert_eq!(
            reason(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &change(op(1), &[1, 2, 3], None)
            )),
            Reason::MembershipRevisionMismatch
        );
        assert!(matches!(
            decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &change(op(1), &[1, 2, 3], Some(log(5)))
            ),
            Decision::Apply(_)
        ));
        // And a *different* committed id does not match either.
        assert_eq!(
            reason(decide_change(
                &s,
                &ADMIN,
                &[7u8; 32],
                &change(op(1), &[1, 2, 3], Some(log(4)))
            )),
            Reason::MembershipRevisionMismatch
        );
    }

    #[test]
    fn resubmitting_the_identical_request_returns_the_existing_intent() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_ADDING_LEARNERS, None));
        assert_eq!(
            decide_change(&s, &ADMIN, &[7u8; 32], &change(op(1), &[9], Some(log(99)))),
            Decision::Existing {
                phase: PHASE_ADDING_LEARNERS
            },
            "resolved from the id and hash alone"
        );
    }

    #[test]
    fn a_different_request_under_the_same_id_conflicts() {
        let mut s = base();
        s.admins.insert(OTHER);
        s.intents.insert(op(1), intent(PHASE_PENDING, None));
        for actor in [ADMIN, OTHER] {
            assert_eq!(
                reason(decide_change(
                    &s,
                    &actor,
                    &[8u8; 32],
                    &change(op(1), &[1], None)
                )),
                Reason::OperationIdConflict
            );
        }
    }

    #[test]
    fn only_one_intent_may_be_active() {
        for phase in [
            PHASE_PENDING,
            PHASE_ADDING_LEARNERS,
            PHASE_CHANGING_VOTERS,
            PHASE_CANCEL_REQUESTED,
        ] {
            let mut s = base();
            s.intents.insert(op(1), intent(phase, None));
            assert_eq!(
                reason(decide_change(
                    &s,
                    &ADMIN,
                    &[8u8; 32],
                    &change(op(2), &[1], None)
                )),
                Reason::MembershipIntentActive,
                "phase {phase} is active"
            );
        }
    }

    #[test]
    fn a_terminal_intent_does_not_block_a_new_one() {
        for phase in [PHASE_COMPLETED, PHASE_FAILED, PHASE_CANCELLED] {
            let mut s = base();
            s.intents.insert(op(1), intent(phase, Some(log(2))));
            assert!(
                matches!(
                    decide_change(&s, &ADMIN, &[8u8; 32], &change(op(2), &[1], None)),
                    Decision::Apply(_)
                ),
                "phase {phase} is terminal"
            );
        }
    }

    // ── Cancellation ────────────────────────────────────────────────────────

    /// A **pending** intent cancels immediately and terminally: nothing has been
    /// done, so there is nothing to unwind. Routing it through
    /// `CancelRequested` would leave an active intent nobody will ever advance,
    /// blocking every future membership change.
    #[test]
    fn cancelling_a_pending_intent_completes_immediately() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_PENDING, None));

        let e = effect(decide_cancel(
            &s,
            &ADMIN,
            &CancelMembershipAction {
                operation_id: op(1),
                expected_phase_code: PHASE_PENDING,
            },
        ));
        assert_eq!(e.intent.status_code, PHASE_CANCELLED);
        assert!(is_terminal_phase(e.intent.status_code));
    }

    /// A cancellation reports `10 MembershipCancelled`, not `8
    /// MembershipAccepted` -- telling a client its change was *accepted* when it
    /// was cancelled is the opposite of what happened.
    #[test]
    fn a_cancellation_reports_membership_cancelled() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_PENDING, None));
        let d = decide_cancel(
            &s,
            &ADMIN,
            &CancelMembershipAction {
                operation_id: op(1),
                expected_phase_code: PHASE_PENDING,
            },
        );
        assert_eq!(d.outcome(), Some(Outcome::MembershipCancelled));
    }

    /// Mid-flight, the controller must unwind, so the cancellation is
    /// *requested* rather than immediate.
    #[test]
    fn cancelling_while_adding_learners_is_requested_not_immediate() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_ADDING_LEARNERS, None));

        let e = effect(decide_cancel(
            &s,
            &ADMIN,
            &CancelMembershipAction {
                operation_id: op(1),
                expected_phase_code: PHASE_ADDING_LEARNERS,
            },
        ));
        assert_eq!(e.intent.status_code, PHASE_CANCEL_REQUESTED);
        assert!(!is_terminal_phase(e.intent.status_code));
        assert!(e.evict.is_empty(), "still active, so nothing is evictable");
    }

    /// Once the voter set is changing it is past the point of no return:
    /// unwinding a partially applied membership change is not something v1 can
    /// do safely, so the cancellation is **refused**.
    #[test]
    fn cancelling_while_changing_voters_is_refused() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_CHANGING_VOTERS, None));
        assert_eq!(
            reason(decide_cancel(
                &s,
                &ADMIN,
                &CancelMembershipAction {
                    operation_id: op(1),
                    expected_phase_code: PHASE_CHANGING_VOTERS,
                }
            )),
            Reason::MembershipCannotCancel
        );
    }

    #[test]
    fn a_terminal_or_unknown_intent_cannot_be_cancelled() {
        let mut s = base();
        s.intents
            .insert(op(1), intent(PHASE_COMPLETED, Some(log(2))));
        for (id, phase) in [(op(1), PHASE_COMPLETED), (op(99), PHASE_PENDING)] {
            assert_eq!(
                reason(decide_cancel(
                    &s,
                    &ADMIN,
                    &CancelMembershipAction {
                        operation_id: id,
                        expected_phase_code: phase,
                    }
                )),
                Reason::MembershipCannotCancel
            );
        }
    }

    #[test]
    fn cancelling_from_the_wrong_phase_is_refused() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_ADDING_LEARNERS, None));
        assert_eq!(
            reason(decide_cancel(
                &s,
                &ADMIN,
                &CancelMembershipAction {
                    operation_id: op(1),
                    expected_phase_code: PHASE_PENDING,
                }
            )),
            Reason::MembershipRevisionMismatch
        );
    }

    // ── Progress ────────────────────────────────────────────────────────────

    /// Both ends of a transition are validated. Checking only the expected
    /// phase would let a report drive an intent from `Pending` straight to
    /// `Cancelled`, or out of a terminal phase entirely.
    #[test]
    fn only_legal_phase_transitions_are_accepted() {
        let legal = [
            (PHASE_PENDING, PHASE_ADDING_LEARNERS),
            (PHASE_ADDING_LEARNERS, PHASE_CHANGING_VOTERS),
            (PHASE_CHANGING_VOTERS, PHASE_COMPLETED),
            (PHASE_CANCEL_REQUESTED, PHASE_CANCELLED),
            (PHASE_PENDING, PHASE_FAILED),
            (PHASE_ADDING_LEARNERS, PHASE_FAILED),
        ];
        for (from, to) in legal {
            assert!(is_legal_transition(from, to), "{from} -> {to} is legal");
        }

        let illegal = [
            (PHASE_PENDING, PHASE_COMPLETED),
            (PHASE_PENDING, PHASE_CANCELLED),
            (PHASE_PENDING, PHASE_CHANGING_VOTERS),
            (PHASE_ADDING_LEARNERS, PHASE_COMPLETED),
            (PHASE_CHANGING_VOTERS, PHASE_CANCELLED),
            // Nothing leaves a terminal phase.
            (PHASE_COMPLETED, PHASE_PENDING),
            (PHASE_FAILED, PHASE_ADDING_LEARNERS),
            (PHASE_CANCELLED, PHASE_COMPLETED),
            (PHASE_COMPLETED, PHASE_FAILED),
        ];
        for (from, to) in illegal {
            assert!(!is_legal_transition(from, to), "{from} -> {to} is illegal");
        }
    }

    #[test]
    fn an_illegal_transition_is_ignored_like_a_stale_report() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_PENDING, None));
        assert_eq!(
            decide_progress(
                &s,
                &MembershipProgressCommand {
                    operation_id: op(1),
                    expected_phase: PHASE_PENDING,
                    new_phase: PHASE_CANCELLED,
                    observed_membership_log_id: None,
                    result_code: SYS_MEMBERSHIP_ADVANCED,
                }
            ),
            None,
            "Pending cannot jump straight to Cancelled"
        );
    }

    /// A progress report's observed membership log id and result code are
    /// **retained**, not discarded: a controller resuming after failover reads
    /// them to decide what is left to do.
    #[test]
    fn a_progress_report_retains_its_observed_log_id_and_result_code() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_CHANGING_VOTERS, None));

        let e = decide_progress(
            &s,
            &MembershipProgressCommand {
                operation_id: op(1),
                expected_phase: PHASE_CHANGING_VOTERS,
                new_phase: PHASE_COMPLETED,
                observed_membership_log_id: Some(log(42)),
                result_code: SYS_MEMBERSHIP_ADVANCED,
            },
        )
        .expect("a legal transition");

        assert_eq!(e.intent.observed_membership_log_id, Some(log(42)));
        assert_eq!(e.intent.result_code, SYS_MEMBERSHIP_ADVANCED);
        assert_eq!(
            e.membership_log_id,
            Some(log(42)),
            "completing a change advances committed membership"
        );
    }

    #[test]
    fn a_progress_report_from_the_wrong_phase_changes_nothing() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_ADDING_LEARNERS, None));
        assert_eq!(
            decide_progress(
                &s,
                &MembershipProgressCommand {
                    operation_id: op(1),
                    expected_phase: PHASE_PENDING,
                    new_phase: PHASE_CHANGING_VOTERS,
                    observed_membership_log_id: None,
                    result_code: 0,
                }
            ),
            None
        );
    }

    #[test]
    fn a_progress_report_for_an_unknown_intent_changes_nothing() {
        let s = base();
        assert!(decide_progress(
            &s,
            &MembershipProgressCommand {
                operation_id: op(9),
                expected_phase: PHASE_PENDING,
                new_phase: PHASE_ADDING_LEARNERS,
                observed_membership_log_id: None,
                result_code: 0,
            }
        )
        .is_none());
    }

    // ── Eviction ────────────────────────────────────────────────────────────

    /// Eviction orders by **completion** log id, not acceptance. Two intents
    /// accepted in one order can complete in the other, and evicting by
    /// acceptance would discard the more recently finished one.
    #[test]
    fn eviction_orders_by_completion_not_by_acceptance() {
        let mut s = base();
        for i in 0..MAX_RETAINED_INTENTS {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&i.to_be_bytes());
            let mut row = intent(PHASE_COMPLETED, Some(log(1_000 - i)));
            // Accepted in ascending order, completed in *descending* order, so
            // the two orderings disagree everywhere.
            row.accepted_log_id = log(i + 1);
            s.intents.insert(id, row);
        }

        s.intents
            .insert(op(200), intent(PHASE_CHANGING_VOTERS, None));
        let e = decide_progress(
            &s,
            &MembershipProgressCommand {
                operation_id: op(200),
                expected_phase: PHASE_CHANGING_VOTERS,
                new_phase: PHASE_COMPLETED,
                observed_membership_log_id: None,
                result_code: SYS_MEMBERSHIP_ADVANCED,
            },
        )
        .expect("a legal transition");

        // The oldest *completion* is the one accepted last.
        let mut expected = [0u8; 16];
        expected[..8].copy_from_slice(&(MAX_RETAINED_INTENTS - 1).to_be_bytes());
        assert_eq!(
            e.evict,
            vec![expected],
            "the earliest completion goes, not the earliest acceptance"
        );
    }

    /// The cap bounds terminal history only: with all 64 slots terminal and one
    /// active, a new intent is still accepted and evicts nothing.
    #[test]
    fn the_cap_bounds_terminal_history_only() {
        let mut s = base();
        for i in 0..MAX_RETAINED_INTENTS {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&i.to_be_bytes());
            s.intents
                .insert(id, intent(PHASE_COMPLETED, Some(log(i + 1))));
        }
        let e = effect(decide_change(
            &s,
            &ADMIN,
            &[8u8; 32],
            &change(op(200), &[1], None),
        ));
        assert!(e.evict.is_empty());
    }

    #[test]
    fn every_phase_code_matches_the_spec_list() {
        assert_eq!(
            [
                PHASE_PENDING,
                PHASE_ADDING_LEARNERS,
                PHASE_CHANGING_VOTERS,
                PHASE_COMPLETED,
                PHASE_FAILED,
                PHASE_CANCEL_REQUESTED,
                PHASE_CANCELLED
            ],
            [1, 2, 3, 4, 5, 6, 7]
        );
        for phase in [PHASE_COMPLETED, PHASE_FAILED, PHASE_CANCELLED] {
            assert!(is_terminal_phase(phase));
        }
        for phase in [
            PHASE_PENDING,
            PHASE_ADDING_LEARNERS,
            PHASE_CHANGING_VOTERS,
            PHASE_CANCEL_REQUESTED,
        ] {
            assert!(!is_terminal_phase(phase), "phase {phase} is still active");
        }
    }
}
