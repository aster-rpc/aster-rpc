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

use super::limits::MAX_RETAINED_INTENTS;
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
}

impl Decision {
    pub fn outcome(&self) -> Option<Outcome> {
        match self {
            Decision::Apply(e) => Some(e.outcome),
            Decision::Existing { .. } => Some(Outcome::MembershipAccepted),
            Decision::Refuse(_) => None,
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
        routes: routes_from(action),
        evict_routes: Vec::new(),
        membership_log_id: None,
    }))
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

    fn plan(voters: &[u8]) -> MembershipPlan {
        MembershipPlan {
            desired_voters: voters
                .iter()
                .map(|b| {
                    let mut id = [0u8; 32];
                    id[0] = *b;
                    id
                })
                .collect(),
            retained_learners: Vec::new(),
            member_seeds: vec![MemberSeed {
                node_id: [9u8; 32],
                ticket: b"aster1ticket".to_vec(),
                join_permit: None,
            }],
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
        assert_eq!(e.intent.plan.member_seeds.len(), 1);
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
        assert_eq!(e.routes.len(), 1);
        let (node, route) = &e.routes[0];
        assert_eq!(*node, [9u8; 32]);
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
