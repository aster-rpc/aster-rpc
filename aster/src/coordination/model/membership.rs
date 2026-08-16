//! Membership intent state (§9) -- the replicated record of a membership
//! change, with **no network execution**.
//!
//! Slice 1 owns the state machine: accepting an intent, advancing its phase,
//! cancelling it, and evicting terminal history. Actually adding a learner,
//! waiting for it to catch up, and calling OpenRaft's `change_membership` is
//! Slice 3's controller, and deliberately not here -- apply must be a pure
//! function of replicated state, and a controller is neither pure nor
//! replicated.
//!
//! ## `operation_id` is group-global, and retention makes that finite
//!
//! Reusing an `operation_id` with the identical canonical plan returns the
//! existing intent; another plan returns `17 OperationIdConflict`, even from a
//! different administrator or client session. But only 64 terminal intents are
//! retained, so "group-global forever" cannot be literally true -- an unbounded
//! intent table is exactly the growth §12.1 exists to stop.
//!
//! An evicted `operation_id` is therefore treated as **unknown**: a
//! resubmission with any plan is accepted as new. That is safe in a way that
//! evicting a client tombstone would not be, because a membership plan is
//! idempotent against committed membership -- re-running an evicted plan
//! converges rather than double-applying. A client tombstone has no such
//! property, which is why tombstones are permanent and these are not.

use super::limits::{MAX_COUNTER, MAX_RETAINED_INTENTS};
use super::state::{Intent, LogId, NodeId, OperationId, State};
use crate::coordination::reason::{Outcome, RefusalReason};

/// Membership-intent status codes (§5.2).
pub const PHASE_PENDING: i32 = 1;
pub const PHASE_ADDING_LEARNERS: i32 = 2;
pub const PHASE_CHANGING_VOTERS: i32 = 3;
pub const PHASE_COMPLETED: i32 = 4;
pub const PHASE_FAILED: i32 = 5;
pub const PHASE_CANCEL_REQUESTED: i32 = 6;
pub const PHASE_CANCELLED: i32 = 7;

/// Is this phase terminal -- eligible for eviction, and no longer the one
/// active intent?
///
/// `CancelRequested` is deliberately **not** terminal: the cancellation has been
/// asked for but the controller has not yet unwound, so the intent is still the
/// active one and must not be evicted out from under it.
pub fn is_terminal_phase(phase: i32) -> bool {
    matches!(phase, PHASE_COMPLETED | PHASE_FAILED | PHASE_CANCELLED)
}

/// A `change_membership` request (§5.2 method 9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeMembershipAction {
    pub operation_id: OperationId,
    pub expected_membership_revision: u64,
    /// A canonical digest of the desired plan. The model compares plans by this
    /// rather than by their contents: §9 says an identical plan resubmitted
    /// under the same `operation_id` returns the existing intent, and "identical"
    /// has to mean canonical bytes, not field-by-field equality of a decoded
    /// structure.
    pub plan_hash: [u8; 32],
    /// Encoded length of the desired plan, for the §12.1 intent charge.
    pub encoded_plan_len: usize,
}

/// A `cancel_membership` request (§5.2 method 11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelMembershipAction {
    pub operation_id: OperationId,
    pub expected_phase: i32,
}

/// A system `MembershipProgress` command (§5.3): the controller reporting that
/// an intent advanced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipProgressCommand {
    pub operation_id: OperationId,
    pub expected_phase: i32,
    pub new_phase: i32,
    pub observed_membership_log_id: Option<LogId>,
}

/// What a membership command writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Store or replace the intent.
    Apply(Box<IntentEffect>),
    /// The identical plan was already accepted under this id: return the
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
    /// Terminal intents to evict, oldest first, so a 65th never accumulates.
    pub evict: Vec<OperationId>,
    pub membership_revision: u64,
}

impl Decision {
    pub fn outcome(&self) -> Option<Outcome> {
        match self {
            Decision::Apply(_) | Decision::Existing { .. } => Some(Outcome::MembershipAccepted),
            Decision::Refuse(_) => None,
        }
    }
}

/// Decide one `change_membership`.
pub fn decide_change(state: &State, actor: &NodeId, action: &ChangeMembershipAction) -> Decision {
    if !state.admins.contains(actor) {
        return Decision::Refuse(RefusalReason::PermissionChanged);
    }

    // An `operation_id` that is still known resolves before anything else: the
    // whole point of a group-global id is that resubmitting it is answerable
    // without re-deciding.
    if let Some(existing) = state.intents.get(&action.operation_id) {
        if existing.plan_hash == action.plan_hash {
            return Decision::Existing {
                phase: existing.phase,
            };
        }
        // Same id, different plan -- even from another administrator or client
        // session.
        return Decision::Refuse(RefusalReason::OperationIdConflict);
    }

    if action.expected_membership_revision != state.membership_revision {
        return Decision::Refuse(RefusalReason::MembershipRevisionMismatch);
    }

    // Only one intent may be pending. A committed intent is not retroactively
    // cancelled by a later administrator revocation, so this is the only gate.
    if state.intents.values().any(|i| !is_terminal_phase(i.phase)) {
        return Decision::Refuse(RefusalReason::MembershipIntentActive);
    }

    if state.membership_revision >= MAX_COUNTER {
        return Decision::Refuse(RefusalReason::CounterExhausted);
    }

    Decision::Apply(Box::new(IntentEffect {
        operation_id: action.operation_id,
        intent: Intent {
            encoded_plan_len: action.encoded_plan_len,
            plan_hash: action.plan_hash,
            phase: PHASE_PENDING,
            introduced_index: 0, // stamped by apply, which knows the log id
        },
        // A new intent is `Pending`, so it is not terminal and displaces
        // nothing: the cap bounds terminal history only. With all 64 slots
        // terminal and one active, a new intent is still accepted.
        evict: Vec::new(),
        membership_revision: state.membership_revision + 1,
    }))
}

/// Decide one `cancel_membership`.
pub fn decide_cancel(state: &State, actor: &NodeId, action: &CancelMembershipAction) -> Decision {
    if !state.admins.contains(actor) {
        return Decision::Refuse(RefusalReason::PermissionChanged);
    }

    // An evicted or never-seen id is unknown, and cancelling an unknown intent
    // cannot be expressed as an intent transition.
    let Some(existing) = state.intents.get(&action.operation_id) else {
        return Decision::Refuse(RefusalReason::MembershipCannotCancel);
    };

    if existing.phase != action.expected_phase {
        return Decision::Refuse(RefusalReason::MembershipRevisionMismatch);
    }

    // A terminal intent cannot be cancelled: there is nothing left to unwind,
    // and moving it out of a terminal phase would resurrect an evictable row as
    // the active one.
    if is_terminal_phase(existing.phase) {
        return Decision::Refuse(RefusalReason::MembershipCannotCancel);
    }

    let mut intent = existing.clone();
    intent.phase = PHASE_CANCEL_REQUESTED;
    Decision::Apply(Box::new(IntentEffect {
        operation_id: action.operation_id,
        intent,
        // Still non-terminal, so nothing becomes evictable.
        evict: Vec::new(),
        membership_revision: state.membership_revision,
    }))
}

/// Decide one system `MembershipProgress`.
///
/// Returns the new phase, or `None` when the reported phase does not match --
/// a stale controller report, which applies harmlessly like a stale `Expire`.
pub fn decide_progress(state: &State, command: &MembershipProgressCommand) -> Option<IntentEffect> {
    let existing = state.intents.get(&command.operation_id)?;
    if existing.phase != command.expected_phase {
        return None;
    }

    let mut intent = existing.clone();
    intent.phase = command.new_phase;

    // Becoming terminal makes this row evictable and may push the retained set
    // over the cap.
    let evict = if is_terminal_phase(command.new_phase) && !is_terminal_phase(existing.phase) {
        evictions(state, 1)
    } else {
        Vec::new()
    };

    Some(IntentEffect {
        operation_id: command.operation_id,
        intent,
        evict,
        membership_revision: state.membership_revision,
    })
}

/// Which terminal intents to evict so that `retained + incoming` fits the cap.
///
/// Oldest first by the log index that introduced them, so every voter evicts
/// the same rows in the same order. The one active intent is never a candidate.
fn evictions(state: &State, incoming_terminal: u64) -> Vec<OperationId> {
    let mut terminal: Vec<(u64, OperationId)> = state
        .intents
        .iter()
        .filter(|(_, i)| is_terminal_phase(i.phase))
        .map(|(id, i)| (i.introduced_index, *id))
        .collect();
    // `(index, id)` rather than index alone: two intents can share a log index
    // only through corrupt state, but a total order must still be total, and
    // ties broken by id are the same on every voter.
    terminal.sort_unstable();

    let retained = terminal.len() as u64;
    let over = (retained + incoming_terminal).saturating_sub(MAX_RETAINED_INTENTS);
    terminal
        .into_iter()
        .take(over as usize)
        .map(|(_, id)| id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::reason::Reason;

    const ADMIN: NodeId = [1u8; 32];
    const OTHER: NodeId = [2u8; 32];

    fn op(n: u8) -> OperationId {
        [n; 16]
    }

    fn base() -> State {
        let mut s = State::default();
        s.admins.insert(ADMIN);
        s.admin_revision = 1;
        s.growth_bytes = s.recompute_growth();
        s
    }

    fn change(id: OperationId, hash: u8, expected: u64) -> ChangeMembershipAction {
        ChangeMembershipAction {
            operation_id: id,
            expected_membership_revision: expected,
            plan_hash: [hash; 32],
            encoded_plan_len: 500,
        }
    }

    fn intent(phase: i32, hash: u8, index: u64) -> Intent {
        Intent {
            encoded_plan_len: 500,
            plan_hash: [hash; 32],
            phase,
            introduced_index: index,
        }
    }

    fn reason(d: Decision) -> Reason {
        match d {
            Decision::Refuse(r) => r.get(),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn only_an_administrator_may_change_membership() {
        let s = base();
        assert_eq!(
            reason(decide_change(&s, &OTHER, &change(op(1), 1, 0))),
            Reason::PermissionChanged
        );
    }

    #[test]
    fn a_first_intent_is_accepted_as_pending() {
        let s = base();
        match decide_change(&s, &ADMIN, &change(op(1), 1, 0)) {
            Decision::Apply(e) => {
                assert_eq!(e.intent.phase, PHASE_PENDING);
                assert_eq!(e.membership_revision, 1);
                assert!(e.evict.is_empty());
            }
            other => panic!("expected acceptance, got {other:?}"),
        }
    }

    /// Reusing an id with the **identical** plan returns the existing intent
    /// rather than creating a second one -- the property that makes an
    /// `operation_id` usable as a retry key.
    #[test]
    fn resubmitting_the_identical_plan_returns_the_existing_intent() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_ADDING_LEARNERS, 7, 3));

        assert_eq!(
            decide_change(&s, &ADMIN, &change(op(1), 7, 99)),
            Decision::Existing {
                phase: PHASE_ADDING_LEARNERS
            },
            "resolved from the id alone, without re-checking the revision"
        );
    }

    /// A different plan under the same id conflicts, even from another
    /// administrator.
    #[test]
    fn a_different_plan_under_the_same_id_conflicts() {
        let mut s = base();
        s.admins.insert(OTHER);
        s.intents.insert(op(1), intent(PHASE_PENDING, 7, 3));

        for actor in [ADMIN, OTHER] {
            assert_eq!(
                reason(decide_change(&s, &actor, &change(op(1), 8, 0))),
                Reason::OperationIdConflict
            );
        }
    }

    #[test]
    fn only_one_intent_may_be_active() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_ADDING_LEARNERS, 7, 3));
        assert_eq!(
            reason(decide_change(&s, &ADMIN, &change(op(2), 9, 0))),
            Reason::MembershipIntentActive
        );
    }

    /// `CancelRequested` is not terminal: the controller has not unwound yet,
    /// so it still counts as the active intent.
    #[test]
    fn a_cancel_requested_intent_still_blocks_a_new_one() {
        let mut s = base();
        s.intents
            .insert(op(1), intent(PHASE_CANCEL_REQUESTED, 7, 3));
        assert_eq!(
            reason(decide_change(&s, &ADMIN, &change(op(2), 9, 0))),
            Reason::MembershipIntentActive
        );
    }

    #[test]
    fn a_terminal_intent_does_not_block_a_new_one() {
        for phase in [PHASE_COMPLETED, PHASE_FAILED, PHASE_CANCELLED] {
            let mut s = base();
            s.intents.insert(op(1), intent(phase, 7, 3));
            assert!(
                matches!(
                    decide_change(&s, &ADMIN, &change(op(2), 9, 0)),
                    Decision::Apply(_)
                ),
                "phase {phase} is terminal and must not block"
            );
        }
    }

    #[test]
    fn a_stale_membership_revision_is_refused() {
        let mut s = base();
        s.membership_revision = 4;
        assert_eq!(
            reason(decide_change(&s, &ADMIN, &change(op(1), 1, 3))),
            Reason::MembershipRevisionMismatch
        );
    }

    // ── Eviction ────────────────────────────────────────────────────────────

    fn fill_terminal(s: &mut State, n: u64) {
        for i in 0..n {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&i.to_be_bytes());
            s.intents.insert(id, intent(PHASE_COMPLETED, i as u8, i));
        }
    }

    /// Producing a 65th terminal intent evicts the oldest, by log order, so
    /// every voter evicts the same row.
    #[test]
    fn a_sixty_fifth_terminal_intent_evicts_the_oldest_by_log_order() {
        let mut s = base();
        fill_terminal(&mut s, MAX_RETAINED_INTENTS);

        let progress = MembershipProgressCommand {
            operation_id: op(200),
            expected_phase: PHASE_PENDING,
            new_phase: PHASE_COMPLETED,
            observed_membership_log_id: None,
        };
        s.intents.insert(op(200), intent(PHASE_PENDING, 200, 9_999));

        let effect = decide_progress(&s, &progress).expect("the intent exists");
        let mut oldest = [0u8; 16];
        oldest[..8].copy_from_slice(&0u64.to_be_bytes());
        assert_eq!(
            effect.evict,
            vec![oldest],
            "the lowest log index goes first"
        );
    }

    /// The cap bounds **terminal history only**: with all 64 slots terminal and
    /// one active, a new intent is still accepted.
    #[test]
    fn the_cap_bounds_terminal_history_and_not_the_active_intent() {
        let mut s = base();
        fill_terminal(&mut s, MAX_RETAINED_INTENTS);
        assert_eq!(s.intents.len() as u64, MAX_RETAINED_INTENTS);

        match decide_change(&s, &ADMIN, &change(op(200), 1, 0)) {
            Decision::Apply(e) => assert!(
                e.evict.is_empty(),
                "a new intent is not terminal, so it evicts nothing"
            ),
            other => panic!("expected acceptance, got {other:?}"),
        }
    }

    /// The active intent is never a candidate for eviction.
    #[test]
    fn the_active_intent_is_never_evicted() {
        let mut s = base();
        fill_terminal(&mut s, MAX_RETAINED_INTENTS);
        s.intents.insert(op(200), intent(PHASE_PENDING, 200, 0));

        let progress = MembershipProgressCommand {
            operation_id: op(200),
            expected_phase: PHASE_PENDING,
            new_phase: PHASE_COMPLETED,
            observed_membership_log_id: None,
        };
        let effect = decide_progress(&s, &progress).expect("exists");
        assert!(
            !effect.evict.contains(&op(200)),
            "the intent being completed is not evicted by its own completion"
        );
    }

    // ── Cancellation ────────────────────────────────────────────────────────

    #[test]
    fn cancelling_an_active_intent_moves_it_to_cancel_requested() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_ADDING_LEARNERS, 7, 3));

        let action = CancelMembershipAction {
            operation_id: op(1),
            expected_phase: PHASE_ADDING_LEARNERS,
        };
        match decide_cancel(&s, &ADMIN, &action) {
            Decision::Apply(e) => {
                assert_eq!(e.intent.phase, PHASE_CANCEL_REQUESTED);
                assert!(e.evict.is_empty(), "still active, so nothing is evictable");
            }
            other => panic!("expected acceptance, got {other:?}"),
        }
    }

    #[test]
    fn a_terminal_or_unknown_intent_cannot_be_cancelled() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_COMPLETED, 7, 3));

        assert_eq!(
            reason(decide_cancel(
                &s,
                &ADMIN,
                &CancelMembershipAction {
                    operation_id: op(1),
                    expected_phase: PHASE_COMPLETED,
                }
            )),
            Reason::MembershipCannotCancel
        );

        // An evicted or never-seen id is unknown.
        assert_eq!(
            reason(decide_cancel(
                &s,
                &ADMIN,
                &CancelMembershipAction {
                    operation_id: op(99),
                    expected_phase: PHASE_PENDING,
                }
            )),
            Reason::MembershipCannotCancel
        );
    }

    #[test]
    fn cancelling_from_the_wrong_phase_is_refused() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_ADDING_LEARNERS, 7, 3));
        assert_eq!(
            reason(decide_cancel(
                &s,
                &ADMIN,
                &CancelMembershipAction {
                    operation_id: op(1),
                    expected_phase: PHASE_PENDING,
                }
            )),
            Reason::MembershipRevisionMismatch
        );
    }

    // ── Progress ────────────────────────────────────────────────────────────

    /// A stale controller report applies harmlessly, like a stale `Expire`.
    #[test]
    fn a_progress_report_from_the_wrong_phase_changes_nothing() {
        let mut s = base();
        s.intents.insert(op(1), intent(PHASE_ADDING_LEARNERS, 7, 3));
        assert_eq!(
            decide_progress(
                &s,
                &MembershipProgressCommand {
                    operation_id: op(1),
                    expected_phase: PHASE_PENDING,
                    new_phase: PHASE_COMPLETED,
                    observed_membership_log_id: None,
                }
            ),
            None
        );
    }

    #[test]
    fn a_progress_report_for_an_unknown_intent_changes_nothing() {
        let s = base();
        assert_eq!(
            decide_progress(
                &s,
                &MembershipProgressCommand {
                    operation_id: op(9),
                    expected_phase: PHASE_PENDING,
                    new_phase: PHASE_COMPLETED,
                    observed_membership_log_id: None,
                }
            ),
            None
        );
    }

    /// An evicted id is treated as unknown, so a resubmission with **any** plan
    /// is accepted as new. Safe because a membership plan is idempotent against
    /// committed membership -- unlike a client tombstone, which is why those are
    /// permanent and these are not.
    #[test]
    fn an_evicted_operation_id_is_accepted_as_new_with_any_plan() {
        let s = base();
        // op(1) was evicted, so it is simply absent.
        match decide_change(&s, &ADMIN, &change(op(1), 200, 0)) {
            Decision::Apply(e) => assert_eq!(e.intent.plan_hash, [200u8; 32]),
            other => panic!("expected acceptance, got {other:?}"),
        }
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
