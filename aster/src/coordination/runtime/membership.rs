//! The §7.3 membership controller: one leader-generation task reconciling
//! durable intent with native membership.
//!
//! ## Everything comes from two sources, and neither is local
//!
//! The **durable intent** says what was asked for; the **native membership**
//! says what the group has agreed. The controller's whole job is to move the
//! second toward the first, and it re-reads both before every side effect.
//!
//! There is deliberately no third source. No retry counter, no local phase
//! cache, no "we already did step 2" flag — because every one of those lives in
//! one leader generation, resets on failover, and would make an intent's fate
//! depend on which node was leader and for how long. §7.3 says so about
//! `LearnerNotFound` specifically, and the same reasoning covers the rest: "the
//! controller compares state after every retry, so losing a response or leader
//! is idempotent" is only true if there is nothing to lose.
//!
//! ## Losing leadership ends the generation, without touching the intent
//!
//! A generation that has lost leadership must not perform one more side effect.
//! Every asynchronous result is rechecked against current leadership *and*
//! current durable state before the next one, and `ForwardToLeader` ends the
//! generation rather than marking anything — the next generation reacquires and
//! re-observes.
//!
//! ## The three exhaustive error levels
//!
//! `change_membership` returns
//! `Result<_, RaftError<NID, ClientWriteError<NID, N>>>`, and §7.3 maps it at
//! three levels with **no wildcard at any of them**. [`classify_change`] is
//! those three matches; it decides whether an intent keeps retrying, reconciles
//! an earlier phase, or raises an invariant failure. It is not the §5.4
//! transport mapping and must not be confused with one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use openraft::error::{ChangeMembershipError, ClientWriteError, RaftError};
use openraft::{ChangeMembers, Raft};

use crate::coordination::ids::CoordinationNodeId;
use crate::coordination::model::shape::MEMBERSHIP_RETAIN;
use crate::coordination::typeconfig::{CoordinationNode, CoordinationTypeConfig};

/// What the controller does next after a `change_membership` failure.
///
/// Four outcomes, and each is a different thing to do — which is the reason
/// §7.3 refuses a single "retry" bucket. Collapsing them turns an ordinary
/// joint transition into a failed intent, or a lost leadership into a spurious
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChangeOutcome {
    /// Wait, re-observe, retry the same phase. The intent is untouched.
    Retry {
        /// Why, for the operator-facing status.
        detail: String,
    },
    /// Go back to learner reconciliation and retry against the same durable
    /// intent.
    Reconcile {
        detail: String,
    },
    /// End this leader generation without touching the intent. The next
    /// generation reacquires and re-observes.
    EndGeneration {
        detail: String,
    },
    /// An invariant this build guarantees does not hold. Not a coordination
    /// result, and never an intent outcome.
    Invariant {
        detail: String,
    },
}

/// The three exhaustive levels of §7.3, as one function.
///
/// ## Why `LearnerNotFound` never produces `Failed`
///
/// The ordinary case is a race — a learner was removed, or never landed,
/// between observation and submission — and reconciling from current committed
/// membership fixes it. The *other* case is not a coordination result either:
/// an accepted intent's seeds were validated at acceptance, so a plan naming a
/// member whose seed is absent from durable state is an invariant violation or
/// corruption.
///
/// Distinguishing the two would need a durable per-node reconciliation fact,
/// and v1 does not have one. It must not be inferred from a local retry count
/// (which resets on failover) or from `Intent.observed_membership_log_id`
/// (which is a single id recorded for status, carrying no per-node content and
/// explicitly not read as authority anywhere). So this always reconciles, and
/// the absent-seed case surfaces where reconciliation actually fails to
/// construct the plan — with the state that proves it, rather than a guess.
pub(crate) fn classify_change(
    err: &RaftError<CoordinationNodeId, ClientWriteError<CoordinationNodeId, CoordinationNode>>,
) -> ChangeOutcome {
    match err {
        // Level 1: `RaftError`. `Fatal` follows the §7.6 lifecycle — it is a
        // lifecycle state, never an intent outcome, and is not retried.
        RaftError::Fatal(fatal) => ChangeOutcome::Invariant {
            detail: format!("the Raft runtime is fatal: {fatal}"),
        },
        RaftError::APIError(api) => match api {
            // Level 2: `ClientWriteError`.
            //
            // A generation that has lost leadership must not perform one more
            // side effect — including marking the intent. The next generation
            // reacquires and re-observes.
            ClientWriteError::ForwardToLeader(to) => ChangeOutcome::EndGeneration {
                detail: format!("no longer leader; {to}"),
            },
            // Level 3: `ChangeMembershipError`.
            ClientWriteError::ChangeMembership(change) => match change {
                // The effective membership log id is ahead of the committed
                // one: the ordinary state during a joint transition, and
                // indistinguishable from healthy progress. Never `Failed`.
                ChangeMembershipError::InProgress(e) => ChangeOutcome::Retry {
                    detail: format!("a joint transition is settling: {e}"),
                },
                // Unreachable from any validated v1 plan: every profile permits
                // only a non-empty voter set, and `decide_change` refuses an
                // empty one at acceptance. Handled exhaustively as the
                // invariant failure it is, rather than by inventing a
                // coordination reason for a state the model forbids.
                ChangeMembershipError::EmptyMembership(e) => ChangeOutcome::Invariant {
                    detail: format!(
                        "a validated plan produced an empty membership, which no profile \
                         permits: {e}"
                    ),
                },
                ChangeMembershipError::LearnerNotFound(e) => ChangeOutcome::Reconcile {
                    detail: format!("a learner is absent; reconciling from committed state: {e}"),
                },
            },
        },
    }
}

/// The learners a plan needs added before its voter change.
///
/// Every node the plan names — desired voter or retained learner — that the
/// **committed** membership does not already contain. Derived from current
/// state on every call rather than remembered, so a restart or a failover
/// recomputes rather than resuming from a stale list.
pub(crate) fn missing_learners(
    desired_voters: &[CoordinationNodeId],
    retained_learners: &[CoordinationNodeId],
    current_members: &BTreeSet<CoordinationNodeId>,
) -> BTreeSet<CoordinationNodeId> {
    desired_voters
        .iter()
        .chain(retained_learners)
        .filter(|node| !current_members.contains(*node))
        .copied()
        .collect()
}

/// §7.3 step 5's node map: **exactly** the desired voters plus the retained
/// learners.
///
/// One operation, and that is normative. `retain = false` removes a departing
/// voter even when the plan names it in `retained_learners`, so step 4 can drop
/// a node step 5 has to put back — and any sequence that *adds* before it
/// removes has an intermediate node map wider than either end. Five voters with
/// six stranded learners, shrinking to three while retaining two of the
/// departing ones, momentarily reaches seven learners and trips §9's envelope
/// on every voter at once.
///
/// Replacing the whole map has no intermediate to get wrong.
pub(crate) fn replacement_node_map(
    desired_voters: &[CoordinationNodeId],
    retained_learners: &[CoordinationNodeId],
) -> BTreeMap<CoordinationNodeId, CoordinationNode> {
    desired_voters
        .iter()
        .chain(retained_learners)
        .map(|node| (*node, CoordinationNode))
        .collect()
}

/// Step 4: promote to the exact desired voter set.
///
/// `retain` is [`MEMBERSHIP_RETAIN`] — **consumed, not passed as a literal**.
/// §9 declares it and the gate on this controller is to read it: a controller
/// that could be *asked* to retain could commit a membership no voter can load,
/// because `retain = true` turns every member absent from the new voter set
/// into a learner and §9's envelope is checked against the nodes a plan
/// *introduces*, which cannot see them.
pub(crate) async fn change_voters(
    raft: &Raft<CoordinationTypeConfig>,
    desired_voters: &[CoordinationNodeId],
) -> Result<(), ChangeOutcome> {
    let voters: BTreeSet<CoordinationNodeId> = desired_voters.iter().copied().collect();
    raft.change_membership(voters, MEMBERSHIP_RETAIN)
        .await
        .map(|_| ())
        .map_err(|err| classify_change(&err))
}

/// Step 5: replace the node map in one operation.
pub(crate) async fn replace_node_map(
    raft: &Raft<CoordinationTypeConfig>,
    desired_voters: &[CoordinationNodeId],
    retained_learners: &[CoordinationNodeId],
) -> Result<(), ChangeOutcome> {
    let map = replacement_node_map(desired_voters, retained_learners);
    raft.change_membership(ChangeMembers::ReplaceAllNodes(map), MEMBERSHIP_RETAIN)
        .await
        .map(|_| ())
        .map_err(|err| classify_change(&err))
}

/// Step 2: add one missing node as a learner.
pub(crate) async fn add_learner(
    raft: &Raft<CoordinationTypeConfig>,
    node: CoordinationNodeId,
) -> Result<(), ChangeOutcome> {
    // `blocking = false`: §7.3 step 3 waits for catch-up *separately*, against a
    // freshly observed committed index. A blocking add would wait against
    // whatever index OpenRaft chose internally, which is not the observation
    // the phase transition is recorded from.
    raft.add_learner(node, CoordinationNode, false)
        .await
        .map(|_| ())
        .map_err(|err| classify_change(&err))
}

/// §7.3 step 3: has `node` applied through `barrier`?
///
/// The barrier is the committed index observed **when the promotion step
/// began**, per §7.3 — not the current one, which advances continuously and
/// would make catch-up a moving target no learner ever reaches.
pub(crate) fn caught_up(
    metrics: &super::lifecycle::Metrics,
    node: &CoordinationNodeId,
    barrier: u64,
) -> bool {
    metrics
        .replication
        .as_ref()
        .and_then(|replication| replication.get(node).copied())
        .flatten()
        .is_some_and(|matched| matched.index >= barrier)
}

/// Completion is **observed**, never claimed (§7.3).
///
/// A controller reply, a local node-map edit, or a combined member set is not
/// enough: the committed membership must be non-joint, its voters exactly the
/// desired set, and its learners exactly the retained set. `decide_progress`
/// refuses a `Completed` report that does not match, so a controller that
/// reported early would simply not advance the intent — but reporting early is
/// still wrong, and this is where it is not done.
pub(crate) fn completion_is_visible(
    metrics: &super::lifecycle::Metrics,
    desired_voters: &[CoordinationNodeId],
    retained_learners: &[CoordinationNodeId],
) -> bool {
    let membership = metrics.membership_config.membership();
    // A joint entry is the *middle* of a change. An intent that reported
    // itself complete during one has not observed the uniform membership it
    // claims.
    if membership.get_joint_config().len() != 1 {
        return false;
    }
    let voters: BTreeSet<CoordinationNodeId> = membership.voter_ids().collect();
    if voters != desired_voters.iter().copied().collect() {
        return false;
    }
    let learners: BTreeSet<CoordinationNodeId> = membership.learner_ids().collect();
    learners == retained_learners.iter().copied().collect()
}

/// The one active intent this controller is working on, if any.
///
/// Read from durable state on every pass. `None` means nothing to do — not
/// "we finished", which is a different fact the intent's own phase records.
pub(crate) fn active_intent(
    state: &crate::coordination::storage::rows::DurableState,
) -> Option<(
    crate::coordination::model::state::OperationId,
    crate::coordination::model::state::Intent,
)> {
    state
        .model
        .intents
        .iter()
        .find(|(_, intent)| {
            !crate::coordination::model::membership::is_terminal_phase(intent.status_code)
        })
        .map(|(id, intent)| (*id, intent.clone()))
}

/// The leader generation a controller task belongs to.
///
/// A plain counter, incremented by the runtime each time this node acquires
/// leadership. It is **not** state the controller reasons from — it is how the
/// runtime cancels a generation that has been superseded, which is the one
/// thing a local value is legitimately for here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Generation(pub(crate) u64);

/// Whether this generation still holds leadership.
///
/// Checked before **every** side effect, not once per pass: the whole point of
/// §7.3's ordering is that a generation which lost leadership mid-sequence
/// stops there rather than completing a step the new leader will redo.
pub(crate) fn still_leading(
    metrics: &super::lifecycle::Metrics,
    local: &CoordinationNodeId,
    generation: Generation,
    current: Arc<std::sync::atomic::AtomicU64>,
) -> bool {
    if current.load(std::sync::atomic::Ordering::SeqCst) != generation.0 {
        return false;
    }
    matches!(metrics.state, openraft::ServerState::Leader) && &metrics.id == local
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::error::{
        EmptyMembership, ForwardToLeader, InProgress, LearnerNotFound,
    };

    fn node(n: u8) -> CoordinationNodeId {
        CoordinationNodeId::from_array([n; 32])
    }

    fn members(ids: &[u8]) -> BTreeSet<CoordinationNodeId> {
        ids.iter().copied().map(node).collect()
    }

    /// **All three levels, exhaustively**, with the outcome each one selects.
    ///
    /// Written as a table with the expectations stated independently, so it is
    /// an oracle rather than a restatement. The failure it guards is a single
    /// "retry" bucket: `InProgress` and `LearnerNotFound` both retry, and they
    /// retry *different phases*.
    #[test]
    fn every_change_membership_failure_selects_its_own_outcome() {
        // Level 1.
        let fatal: RaftError<CoordinationNodeId, ClientWriteError<_, _>> =
            RaftError::Fatal(openraft::error::Fatal::Stopped);
        assert!(matches!(
            classify_change(&fatal),
            ChangeOutcome::Invariant { .. }
        ));

        // Level 2.
        let forward: RaftError<CoordinationNodeId, ClientWriteError<_, _>> =
            RaftError::APIError(ClientWriteError::ForwardToLeader(ForwardToLeader {
                leader_id: Some(node(2)),
                leader_node: Some(CoordinationNode),
            }));
        assert!(matches!(
            classify_change(&forward),
            ChangeOutcome::EndGeneration { .. }
        ));

        // Level 3.
        let in_progress: RaftError<CoordinationNodeId, ClientWriteError<_, _>> =
            RaftError::APIError(ClientWriteError::ChangeMembership(
                ChangeMembershipError::InProgress(InProgress {
                    committed: None,
                    membership_log_id: None,
                }),
            ));
        assert!(matches!(
            classify_change(&in_progress),
            ChangeOutcome::Retry { .. }
        ));

        let empty: RaftError<CoordinationNodeId, ClientWriteError<_, _>> = RaftError::APIError(
            ClientWriteError::ChangeMembership(ChangeMembershipError::EmptyMembership(
                EmptyMembership {},
            )),
        );
        assert!(matches!(
            classify_change(&empty),
            ChangeOutcome::Invariant { .. }
        ));

        let missing: RaftError<CoordinationNodeId, ClientWriteError<_, _>> = RaftError::APIError(
            ClientWriteError::ChangeMembership(ChangeMembershipError::LearnerNotFound(
                LearnerNotFound { node_id: node(3) },
            )),
        );
        assert!(matches!(
            classify_change(&missing),
            ChangeOutcome::Reconcile { .. }
        ));
    }

    /// **`LearnerNotFound` never produces `Failed`**, and the reason is that no
    /// state exists to justify one.
    ///
    /// `ChangeOutcome` has no `Failed` variant at all, which is the structural
    /// half; this asserts the classification half. An implementation that
    /// inferred `Failed` from a retry count would have had to add the variant
    /// first — and a counter lives in one leader generation and resets on
    /// failover, so an intent's fate would depend on which node was leader and
    /// for how long.
    #[test]
    fn a_missing_learner_always_reconciles_and_never_fails_the_intent() {
        for target in [1u8, 2, 3, 200] {
            let err: RaftError<CoordinationNodeId, ClientWriteError<_, _>> = RaftError::APIError(
                ClientWriteError::ChangeMembership(ChangeMembershipError::LearnerNotFound(
                    LearnerNotFound {
                        node_id: node(target),
                    },
                )),
            );
            assert!(
                matches!(classify_change(&err), ChangeOutcome::Reconcile { .. }),
                "node {target}"
            );
        }
    }

    /// `InProgress` and `LearnerNotFound` both keep going and go to
    /// **different** places.
    ///
    /// The distinction a single retry bucket destroys: one waits for a joint
    /// transition to settle, the other rebuilds the learner set from committed
    /// state. Retrying the voter change against a missing learner loops
    /// forever.
    #[test]
    fn waiting_out_a_joint_change_is_not_the_same_as_rebuilding_learners() {
        let in_progress: RaftError<CoordinationNodeId, ClientWriteError<_, _>> =
            RaftError::APIError(ClientWriteError::ChangeMembership(
                ChangeMembershipError::InProgress(InProgress {
                    committed: None,
                    membership_log_id: None,
                }),
            ));
        let missing: RaftError<CoordinationNodeId, ClientWriteError<_, _>> = RaftError::APIError(
            ClientWriteError::ChangeMembership(ChangeMembershipError::LearnerNotFound(
                LearnerNotFound { node_id: node(3) },
            )),
        );
        assert_ne!(classify_change(&in_progress), classify_change(&missing));
    }

    /// Learners to add come from **current** committed membership, so a
    /// restart or failover recomputes rather than resuming a stale list.
    #[test]
    fn missing_learners_are_derived_from_committed_membership() {
        let desired = [node(1), node(2), node(3)];
        let retained = [node(9)];

        // Nothing committed yet: everything the plan names is missing.
        assert_eq!(
            missing_learners(&desired, &retained, &members(&[])),
            members(&[1, 2, 3, 9])
        );

        // Two already members: only the rest.
        assert_eq!(
            missing_learners(&desired, &retained, &members(&[1, 2])),
            members(&[3, 9])
        );

        // All present: nothing to do, and that is different from "we already
        // did it" — the same answer is recomputed every pass.
        assert_eq!(
            missing_learners(&desired, &retained, &members(&[1, 2, 3, 9])),
            members(&[])
        );
    }

    /// **Step 5 replaces the map**; it never adds before it removes.
    ///
    /// The counterexample §7.3 records: five voters with six stranded learners,
    /// shrinking to three while retaining two of the departing ones. An
    /// add-then-remove sequence momentarily holds seven learners and trips §9's
    /// envelope on every voter at once. The replacement has no intermediate.
    #[test]
    fn the_replacement_map_is_exactly_the_voters_and_retained_learners() {
        let desired = [node(1), node(2), node(3)];
        let retained = [node(4), node(5)];
        let map = replacement_node_map(&desired, &retained);

        assert_eq!(
            map.keys().copied().collect::<BTreeSet<_>>(),
            members(&[1, 2, 3, 4, 5]),
            "exactly the voters plus the retained learners"
        );
        assert_eq!(map.len(), 5, "and no stranded learner survives it");

        // The shrink that motivated the rule: six stranded learners are simply
        // absent from the result, with no intermediate that holds them.
        let stranded = [6u8, 7, 8, 9, 10, 11];
        for node_id in stranded {
            assert!(
                !map.contains_key(&node(node_id)),
                "a learner the plan does not retain must not survive step 5"
            );
        }
    }

    /// The controller **consumes** `MEMBERSHIP_RETAIN` rather than passing a
    /// literal.
    ///
    /// §9 declares the constant and its gate on this controller is to read it.
    /// A source guard, because a literal `false` behaves identically today — the
    /// difference is only visible if the constant ever changes, which is
    /// precisely when a literal would be wrong.
    #[test]
    fn the_retain_flag_is_read_from_the_constant() {
        let source = include_str!("membership.rs");
        let end = crate::coordination::guards::production_end(source);
        let production = &source[..end];

        let calls: Vec<&str> = production
            .lines()
            .filter(|line| line.contains("change_membership("))
            .collect();
        assert!(!calls.is_empty(), "the controller must call change_membership");

        let literals: Vec<usize> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let code = line.split("//").next().unwrap_or("");
                code.contains("change_membership(") && (code.contains(", false") || code.contains(", true"))
            })
            .map(|(n, _)| n + 1)
            .collect();
        assert!(
            literals.is_empty(),
            "the retain flag must be MEMBERSHIP_RETAIN, not a literal; found at {literals:?}"
        );
        assert_eq!(
            production.matches("MEMBERSHIP_RETAIN").count(),
            // The import, plus one per call site.
            1 + calls.len(),
            "every change_membership call must pass the constant"
        );

        // And the constant is what §9 says it is. Asserted here so a change to
        // it is visible from the controller that depends on it.
        assert!(
            !MEMBERSHIP_RETAIN,
            "retain = true adds learners the plan never named, which §9's envelope cannot see"
        );
    }

    /// **Completion is observed.** A joint entry, a wrong voter set, or a
    /// wrong learner set each refuses it.
    #[test]
    fn completion_requires_the_exact_uniform_membership() {
        use openraft::{Membership, StoredMembership};
        use std::sync::Arc;

        let view = |configs: Vec<BTreeSet<CoordinationNodeId>>, learners: &[u8]| {
            let mut nodes: BTreeMap<CoordinationNodeId, CoordinationNode> = configs
                .iter()
                .flatten()
                .map(|id| (*id, CoordinationNode))
                .collect();
            for l in learners {
                nodes.insert(node(*l), CoordinationNode);
            }
            super::super::lifecycle::testing::metrics(
                1,
                openraft::ServerState::Leader,
                5,
                Some(5),
                Some(10),
                Arc::new(StoredMembership::new(
                    None,
                    Membership::new(configs, nodes),
                )),
            )
        };

        let desired = [node(1), node(2), node(3)];
        let retained = [node(4)];

        // The one that counts.
        assert!(completion_is_visible(
            &view(vec![members(&[1, 2, 3])], &[4]),
            &desired,
            &retained
        ));

        // A joint entry is the middle of a change, not the end of one.
        assert!(
            !completion_is_visible(
                &view(vec![members(&[1, 2, 3]), members(&[3, 4, 5])], &[4]),
                &desired,
                &retained
            ),
            "a joint membership is not a completed one"
        );

        // The wrong voters.
        assert!(!completion_is_visible(
            &view(vec![members(&[1, 2])], &[4]),
            &desired,
            &retained
        ));

        // The right voters, the wrong learners — which a "combined member set"
        // check would accept, and §7.3 explicitly says is not enough.
        assert!(
            !completion_is_visible(&view(vec![members(&[1, 2, 3])], &[9]), &desired, &retained),
            "the retained learners must be exact, not merely present"
        );
        assert!(
            !completion_is_visible(&view(vec![members(&[1, 2, 3])], &[]), &desired, &retained),
            "a missing retained learner is not completion"
        );
        assert!(
            !completion_is_visible(
                &view(vec![members(&[1, 2, 3])], &[4, 5]),
                &desired,
                &retained
            ),
            "an extra learner is not completion either"
        );
    }

    /// A generation that has been superseded stops, whatever the metrics say.
    #[test]
    fn a_superseded_generation_stops_leading() {
        use std::sync::atomic::AtomicU64;

        let leader = super::super::lifecycle::testing::metrics(
            1,
            openraft::ServerState::Leader,
            5,
            Some(5),
            Some(10),
            super::super::lifecycle::testing::membership(&[&[1, 2, 3]], &[]),
        );
        let current = Arc::new(AtomicU64::new(7));

        assert!(still_leading(
            &leader,
            &node(1),
            Generation(7),
            Arc::clone(&current)
        ));

        // A newer generation exists: this one stops even though the node is
        // still leader.
        current.store(8, std::sync::atomic::Ordering::SeqCst);
        assert!(!still_leading(
            &leader,
            &node(1),
            Generation(7),
            Arc::clone(&current)
        ));

        // And a node that is no longer leader stops even at the right
        // generation.
        let follower = super::super::lifecycle::testing::metrics(
            1,
            openraft::ServerState::Follower,
            5,
            Some(5),
            None,
            super::super::lifecycle::testing::membership(&[&[1, 2, 3]], &[]),
        );
        assert!(!still_leading(
            &follower,
            &node(1),
            Generation(8),
            Arc::clone(&current)
        ));
    }
}
