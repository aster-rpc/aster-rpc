//! `set_admin` (§8.1) -- one decision function, shared by preflight and apply.
//!
//! The administrator set is the root of the group's authorization: only an
//! administrator may change ACL, admin or membership state, and genesis
//! installs a non-empty exact set. Two rules follow from that and are enforced
//! here rather than left to callers.
//!
//! **The final administrator cannot be removed.** A group with zero
//! administrators can never grant again, and no later command can rescue it:
//! adding an administrator is itself a `set_admin`, which requires an
//! administrator to issue. The refusal is `24 LastAdministrator`, and it is
//! **retryable** rather than terminal: the last administrator can add a second
//! one and then reissue the removal unchanged, so a v1 command clears the
//! condition -- which is §10's test. (The removal must be reissued on a
//! different client id, because a pre-proposal `NotConsumed` consumed no
//! sequence and the original request may not be altered at that sequence.)
//!
//! **The revision advances exactly when the set changes.** Enabling an existing
//! administrator, or disabling one that is not there, is a no-op returning
//! `7 AdminUpdated` with the revision unmoved -- the same rule `set_grant`
//! follows, and for the same reason: a revision that moved without a change
//! would invalidate every other client's `expected_admin_revision` for a command
//! that did nothing.

use super::limits::{MAX_ADMINS_PER_GROUP, MAX_COUNTER};
use super::state::{NodeId, State};
use crate::coordination::reason::{Outcome, RefusalReason};

/// A `set_admin` request (§5.2 method 8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminAction {
    /// The node whose administrator status changes. Never the caller.
    pub target: NodeId,
    pub enabled: bool,
    pub expected_admin_revision: u64,
}

/// What a `set_admin` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminEffect {
    pub target: NodeId,
    pub enabled: bool,
    /// The administrator revision after the change.
    pub admin_revision: u64,
}

/// The outcome of deciding one `set_admin`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Apply(AdminEffect),
    /// Legal and changes nothing: `7 AdminUpdated`, revision unmoved.
    NoOp {
        admin_revision: u64,
    },
    Refuse(RefusalReason),
}

impl Decision {
    pub fn outcome(&self) -> Option<Outcome> {
        match self {
            Decision::Apply(_) | Decision::NoOp { .. } => Some(Outcome::AdminUpdated),
            Decision::Refuse(_) => None,
        }
    }
}

/// Decide one `set_admin` against committed state.
pub fn decide(state: &State, actor: &NodeId, action: &AdminAction) -> Decision {
    // Administrator authority: ROOT, or the committed delegated set.
    if !state.is_administrator(actor) {
        return Decision::Refuse(RefusalReason::PermissionChanged);
    }

    // **ROOT is not a row in this table and cannot be made one.**
    //
    // Immutable means immutable in both directions. Enabling ROOT would put it
    // in the delegated set, where a later `set_admin` could remove it and the
    // final-administrator rule would start counting it — so a group could
    // "remove ROOT" and reach a state where the key that founded it no longer
    // authorizes anything. Disabling it is the same request said plainly.
    // Neither is a permission question, so neither is answered as one: the
    // caller may hold every authority there is — ROOT itself does — and what is
    // refused is the *target*.
    if state.is_root(&action.target) {
        return Decision::Refuse(RefusalReason::RootAuthorityImmutable);
    }

    // **Revision 0 is the pre-Genesis state, and no ordinary command moves it.**
    //
    // Revision 0 with no delegated rows means the signed genesis has not been
    // applied yet. The authorization seed comes from that document and from
    // nowhere else — "seeding fresh admins outside Raft" is exactly the
    // shortcut §7.1 forbids — and ROOT passing the predicate above is what
    // makes the hole reachable: without this, ROOT could install delegates
    // before the genesis arrived, and the genesis would then find a revision it
    // did not write.
    //
    // Reported as a revision mismatch because that is what it is: the command
    // is revision-fenced, and 0 is not a revision at which any `set_admin` may
    // apply.
    if state.admin_revision == 0 {
        return Decision::Refuse(RefusalReason::AdminRevisionMismatch);
    }

    // The revision check precedes every other decision, for the same reason it
    // does in `set_grant`: a stale request was never going to apply, so any
    // other answer would misdirect the client.
    if action.expected_admin_revision != state.admin_revision {
        return Decision::Refuse(RefusalReason::AdminRevisionMismatch);
    }

    let is_admin = state.admins.contains(&action.target);

    // Change detection precedes the capacity and ceiling checks: a command that
    // changes nothing can exhaust nothing, and cannot empty the set either.
    if is_admin == action.enabled {
        return Decision::NoOp {
            admin_revision: state.admin_revision,
        };
    }

    if !action.enabled {
        // §8.1: the final administrator cannot be removed. Checked before the
        // ceiling, because it is the more specific answer and both are
        // terminal -- telling a client its counter is exhausted when the real
        // problem is that it is the last administrator would send it to fix the
        // wrong thing.
        if state.admins.len() <= 1 {
            return Decision::Refuse(RefusalReason::LastAdministrator);
        }
    } else if !state.growth_fits(super::limits::charge_admin()) {
        // §12.1: an admin row is growth-funded like any other, and adding one
        // must be refused when the budget is full. An earlier revision checked
        // the byte budget for resource values only, so this table wrote past a
        // saturated budget.
        return Decision::Refuse(RefusalReason::StateBudgetExhausted);
    } else if state.admins.len() as u64 >= MAX_ADMINS_PER_GROUP {
        // §12.1's 64-administrator cap, reported as `23 AclSlotsExhausted`:
        // §8.1 groups ACL and admin state together, and the terminality matches
        // exactly -- removing an administrator frees a slot, so this is
        // retryable like the grant caps.
        return Decision::Refuse(RefusalReason::AclSlotsExhausted);
    }

    if state.admin_revision >= MAX_COUNTER {
        return Decision::Refuse(RefusalReason::CounterExhausted);
    }

    Decision::Apply(AdminEffect {
        target: action.target,
        enabled: action.enabled,
        admin_revision: state.admin_revision + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::reason::Reason;

    const A: NodeId = [1u8; 32];
    const B: NodeId = [2u8; 32];
    const C: NodeId = [3u8; 32];

    /// Genesis installs a non-empty set, and a fresh genesis admin revision is
    /// `1` (§8.1).
    fn genesis(admins: &[NodeId]) -> State {
        let mut s = State::default();
        for a in admins {
            s.admins.insert(*a);
        }
        s.admin_revision = 1;
        s.growth_bytes = s.recompute_growth();
        s
    }

    fn action(target: NodeId, enabled: bool, expected: u64) -> AdminAction {
        AdminAction {
            target,
            enabled,
            expected_admin_revision: expected,
        }
    }

    fn reason(d: Decision) -> Reason {
        match d {
            Decision::Refuse(r) => r.get(),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn only_an_administrator_may_change_the_administrator_set() {
        let s = genesis(&[A]);
        assert_eq!(
            reason(decide(&s, &B, &action(C, true, 1))),
            Reason::PermissionChanged
        );
    }

    #[test]
    fn a_stale_revision_is_refused_before_anything_else() {
        let s = genesis(&[A]);
        // Even a command that would otherwise be refused for capacity or as the
        // last administrator answers the stale revision first.
        assert_eq!(
            reason(decide(&s, &A, &action(A, false, 0))),
            Reason::AdminRevisionMismatch
        );
    }

    #[test]
    fn adding_and_removing_advance_the_revision() {
        let s = genesis(&[A]);
        let added = match decide(&s, &A, &action(B, true, 1)) {
            Decision::Apply(e) => e,
            other => panic!("expected an applied change, got {other:?}"),
        };
        assert_eq!(added.target, B);
        assert!(added.enabled);
        assert_eq!(added.admin_revision, 2);

        let two = genesis(&[A, B]);
        let removed = match decide(&two, &A, &action(B, false, 1)) {
            Decision::Apply(e) => e,
            other => panic!("expected an applied change, got {other:?}"),
        };
        assert!(!removed.enabled);
        assert_eq!(removed.admin_revision, 2);
    }

    /// The revision advances exactly when the set changes.
    #[test]
    fn a_no_op_leaves_the_revision_where_it_was() {
        let s = genesis(&[A, B]);
        // Enabling an existing administrator.
        assert_eq!(
            decide(&s, &A, &action(B, true, 1)),
            Decision::NoOp { admin_revision: 1 }
        );
        // Disabling one that is not there.
        assert_eq!(
            decide(&s, &A, &action(C, false, 1)),
            Decision::NoOp { admin_revision: 1 }
        );
    }

    /// §8.1: the final administrator cannot be removed, and the refusal is
    /// **terminal for the request that received it**.
    ///
    /// The escape — "add a second administrator, then re-send" — clears the
    /// *condition* but not for that request. `set_admin` carries
    /// `expected_admin_revision`, and adding the replacement is itself a
    /// `set_admin` that advances the revision, so the unchanged removal can
    /// then only fail `AdminRevisionMismatch`. §10's test is whether a v1
    /// command makes *this* request apply unchanged; none does.
    ///
    /// Asserted by construction rather than argued: the second half of this
    /// test performs the escape and shows the original request still fails.
    #[test]
    fn the_final_administrator_cannot_be_removed_and_the_refusal_is_terminal() {
        let s = genesis(&[A]);
        let removal = action(A, false, 1);
        let d = decide(&s, &A, &removal);
        assert_eq!(reason(d.clone()), Reason::LastAdministrator);
        match d {
            Decision::Refuse(r) => {
                assert!(
                    r.is_terminal_when_not_consumed(),
                    "the replacement advances admin_revision, so this exact \
                     revision-fenced request can never apply"
                );
                assert!(r.is_admission_denial(), "denied pre-proposal at stage 3");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        // The escape, performed: add B, and the *unchanged* removal is now
        // refused for a different reason — which is the whole point.
        let mut after = s.clone();
        let Decision::Apply(effect) = decide(&s, &A, &action(B, true, 1)) else {
            panic!("adding a second administrator is legal");
        };
        after.admins.insert(effect.target);
        after.admin_revision = effect.admin_revision;
        assert_eq!(
            reason(decide(&after, &A, &removal)),
            Reason::AdminRevisionMismatch,
            "the same request cannot apply after the replacement advanced the \
             revision it was fenced against"
        );
    }

    /// With two administrators, removing either is legal -- the rule is about
    /// emptying the set, not about self-removal.
    #[test]
    fn either_of_two_administrators_may_be_removed_including_the_caller() {
        let s = genesis(&[A, B]);
        for target in [A, B] {
            assert!(
                matches!(
                    decide(&s, &A, &action(target, false, 1)),
                    Decision::Apply(_)
                ),
                "removing {target:?} leaves one administrator"
            );
        }
    }

    /// The 64-administrator cap, reported as `23` and retryable.
    #[test]
    fn the_administrator_cap_is_enforced_and_retryable() {
        let mut admins = Vec::new();
        for i in 0..MAX_ADMINS_PER_GROUP {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&i.to_be_bytes());
            admins.push(id);
        }
        let s = genesis(&admins);
        assert_eq!(s.admins.len() as u64, MAX_ADMINS_PER_GROUP);

        let d = decide(&s, &admins[0], &action(C, true, 1));
        assert_eq!(reason(d.clone()), Reason::AclSlotsExhausted);
        match d {
            Decision::Refuse(r) => assert!(
                !r.is_terminal_when_not_consumed(),
                "removing an administrator frees a slot"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }

        // Removing one frees the slot.
        let mut freed = s.clone();
        freed.admins.remove(&admins[1]);
        freed.admin_revision = 2;
        assert!(matches!(
            decide(&freed, &admins[0], &action(C, true, 2)),
            Decision::Apply(_)
        ));
    }

    /// A no-op is unaffected by the cap: it adds no row.
    #[test]
    fn a_no_op_succeeds_at_the_administrator_cap() {
        let mut admins = Vec::new();
        for i in 0..MAX_ADMINS_PER_GROUP {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&i.to_be_bytes());
            admins.push(id);
        }
        let s = genesis(&admins);
        assert_eq!(
            decide(&s, &admins[0], &action(admins[3], true, 1)),
            Decision::NoOp { admin_revision: 1 }
        );
    }

    #[test]
    fn an_exhausted_revision_refuses_the_command() {
        let mut s = genesis(&[A, B]);
        s.admin_revision = MAX_COUNTER;
        assert_eq!(
            reason(decide(&s, &A, &action(C, true, MAX_COUNTER))),
            Reason::CounterExhausted
        );
    }

    /// The last-administrator rule outranks the counter ceiling. Naming the
    /// ceiling would send the client to fix something it cannot, when adding a
    /// second administrator is what actually unblocks it.
    #[test]
    fn the_last_administrator_rule_outranks_the_counter_ceiling() {
        let mut s = genesis(&[A]);
        s.admin_revision = MAX_COUNTER;
        assert_eq!(
            reason(decide(&s, &A, &action(A, false, MAX_COUNTER))),
            Reason::LastAdministrator
        );
    }

    #[test]
    fn deciding_twice_gives_the_same_answer() {
        let s = genesis(&[A, B]);
        let a = action(C, true, 1);
        assert_eq!(decide(&s, &A, &a), decide(&s, &A, &a));
    }
}
