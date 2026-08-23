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
    Reconcile { detail: String },
    /// End this leader generation without touching the intent. The next
    /// generation reacquires and re-observes.
    EndGeneration { detail: String },
    /// An invariant this build guarantees does not hold. Not a coordination
    /// result, and never an intent outcome.
    Invariant { detail: String },
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
            ClientWriteError::ChangeMembershipError(change) => match change {
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

/// Commit one `MembershipProgress` record.
///
/// The **only** way a phase advances. `decide_progress` checks both ends of the
/// transition and, for `Completed`, that the native membership it claims has
/// already been applied — so a controller cannot talk an intent forward. This
/// function's job is to say what was observed; apply decides whether it counts.
///
/// A stale report commits harmlessly, like a stale `Expire`: every voter
/// evaluates the same `decide_progress` and reaches `None`.
pub(crate) async fn report_progress(
    raft: &Raft<CoordinationTypeConfig>,
    operation_id: &crate::coordination::model::state::OperationId,
    expected_phase: i32,
    new_phase: i32,
    observed: Option<crate::coordination::contract::LogIdV1>,
    result_code: i32,
) -> Result<(), ChangeOutcome> {
    use crate::coordination::contract::{CommandV1, MembershipProgressCommandV1};

    let command = CommandV1::MembershipProgress(MembershipProgressCommandV1 {
        operation_id: operation_id.to_vec(),
        expected_phase,
        new_phase,
        observed_membership_log_id: observed,
        result_code,
    });
    raft.client_write(command)
        .await
        .map(|_| ())
        .map_err(|err| classify_change(&err))
}

/// One pass of the §7.3 phase table.
///
/// Every arm re-reads committed membership rather than trusting anything
/// carried from a previous pass — a pass that resumed from a remembered
/// position would be a third source of truth, and §7.3 has exactly two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Nothing to drive: no active intent, or a terminal phase.
    Idle,
    /// Add these learners, then re-observe.
    AddLearners(BTreeSet<CoordinationNodeId>),
    /// Every node is present; wait for each desired voter to reach `barrier`.
    AwaitCatchUp {
        barrier: u64,
        outstanding: BTreeSet<CoordinationNodeId>,
    },
    /// Advance the phase and call `change_membership` (§7.3 step 4).
    ChangeVoters,
    /// The uniform voter entry landed: replace the node map (step 5).
    ReplaceNodes,
    /// The observed membership is exactly the plan's: report completion.
    ReportComplete,
    /// A cancel was requested: remove only the learners this intent
    /// introduced, then report `Cancelled`.
    CancelCleanup(BTreeSet<CoordinationNodeId>),
}

/// Decide the next step from durable intent and committed membership.
///
/// A pure function of the two sources, so the phase table is testable without a
/// cluster — and, more importantly, so there is exactly one place the table
/// lives. The controller loop is only sequencing and cancellation.
pub(crate) fn next_step(
    intent: &crate::coordination::model::state::Intent,
    metrics: &super::lifecycle::Metrics,
    catch_up_barrier: Option<u64>,
) -> Step {
    use crate::coordination::model::membership as phases;

    let desired: Vec<CoordinationNodeId> = intent
        .plan
        .desired_voters
        .iter()
        .map(|n| CoordinationNodeId::from_array(*n))
        .collect();
    let retained: Vec<CoordinationNodeId> = intent
        .plan
        .retained_learners
        .iter()
        .map(|n| CoordinationNodeId::from_array(*n))
        .collect();

    let membership = metrics.membership_config.membership();
    let current: BTreeSet<CoordinationNodeId> = membership.nodes().map(|(id, _)| *id).collect();

    match intent.status_code {
        // §7.3 step 1: commit the transition to `AddingLearners` **before** any
        // external learner work, so a crash between the two resumes rather than
        // repeating a step whose effects are already visible. Route import is
        // the caller's — it touches the address book, not Raft.
        phases::PHASE_PENDING => Step::ChangeVoters,

        phases::PHASE_ADDING_LEARNERS => {
            let missing = missing_learners(&desired, &retained, &current);
            if !missing.is_empty() {
                return Step::AddLearners(missing);
            }
            // Every node is present. Wait for the **desired voters** to catch
            // up: a retained learner does not vote, so its lag cannot block a
            // membership the group can commit, and waiting for it would let one
            // slow non-voter stall a change indefinitely.
            let Some(barrier) = catch_up_barrier else {
                // §7.3 step 3 measures against the committed index observed
                // *when the promotion step began*. No barrier yet means this
                // pass is that beginning — a barrier re-read every pass is a
                // moving target no learner ever reaches.
                return Step::AwaitCatchUp {
                    barrier: metrics.last_log_index.unwrap_or(0),
                    outstanding: desired.iter().copied().collect(),
                };
            };
            let outstanding: BTreeSet<CoordinationNodeId> = desired
                .iter()
                .filter(|node| {
                    // A node is trivially caught up with itself, and OpenRaft
                    // reports no replication entry for the leader.
                    **node != metrics.id && !caught_up(metrics, node, barrier)
                })
                .copied()
                .collect();
            if outstanding.is_empty() {
                Step::ChangeVoters
            } else {
                Step::AwaitCatchUp {
                    barrier,
                    outstanding,
                }
            }
        }

        phases::PHASE_CHANGING_VOTERS => {
            if completion_is_visible(metrics, &desired, &retained) {
                return Step::ReportComplete;
            }
            let voters: BTreeSet<CoordinationNodeId> = membership.voter_ids().collect();
            if voters == desired.iter().copied().collect()
                && membership.get_joint_config().len() == 1
            {
                // The uniform voter entry landed; only the node map is owed.
                Step::ReplaceNodes
            } else {
                Step::ChangeVoters
            }
        }

        phases::PHASE_CANCEL_REQUESTED => {
            // Only the learners **this intent introduced**, and only those the
            // plan does not retain. Removing every learner would strip nodes
            // another intent added; removing a retained one would undo the
            // plan's own intent in reverse.
            let introduced: BTreeSet<CoordinationNodeId> = intent
                .plan
                .member_seeds
                .iter()
                .map(|seed| CoordinationNodeId::from_array(seed.node_id))
                .filter(|node| current.contains(node) && !desired.contains(node))
                .collect();
            Step::CancelCleanup(introduced)
        }

        // Terminal, or a phase this build does not implement. Either way there
        // is nothing to drive, and inventing a step for one would be acting on
        // a state the model does not define.
        _ => Step::Idle,
    }
}

/// The controller task: one leader generation, driving [`next_step`] until the
/// generation ends.
///
/// ## What ends a generation
///
/// Losing leadership, being superseded by a newer generation, or a
/// [`ChangeOutcome::EndGeneration`]. In every case the intent is **untouched**:
/// a generation that has lost leadership must not perform one more side effect,
/// including marking an intent it can no longer see the outcome of.
///
/// An invariant failure latches the runtime fatal rather than failing the
/// intent, because §7.3 reserves `Failed` for "a deterministic
/// invalid/invariant/storage outcome that cannot succeed unchanged" as an
/// *intent* outcome — and a broken invariant in this build is not a fact about
/// the operator's plan.
pub(crate) async fn run_generation(
    raft: Raft<CoordinationTypeConfig>,
    state: crate::coordination::storage::machine::StateHandle,
    local: CoordinationNodeId,
    generation: Generation,
    current_generation: Arc<std::sync::atomic::AtomicU64>,
    facts: Arc<super::owner::RuntimeFacts>,
) {
    use crate::coordination::model::membership as phases;

    // The catch-up barrier is per generation and per intent, and it is the one
    // value the loop legitimately carries: §7.3 measures against the index
    // observed *when the promotion step began*, and re-reading it each pass
    // makes it a moving target no learner reaches. A new generation observes a
    // fresh one, which is correct — it has re-observed everything else too.
    let mut barrier: Option<(crate::coordination::model::state::OperationId, u64)> = None;

    loop {
        let metrics = raft.metrics().borrow().clone();
        if !still_leading(
            &metrics,
            &local,
            generation,
            Arc::clone(&current_generation),
        ) {
            return;
        }

        let Ok(active) = state.active_intent().await else {
            // The store cannot say what it holds. The lifecycle surface already
            // reports that; this generation simply stops driving.
            return;
        };
        let Some((operation_id, intent)) = active else {
            // Nothing to do. Sleep a beat rather than spinning: the next intent
            // arrives through apply, which the metrics watch does not observe.
            tokio::time::sleep(IDLE_POLL).await;
            continue;
        };
        if barrier.as_ref().map(|(id, _)| *id) != Some(operation_id) {
            barrier = None;
        }

        let step = next_step(&intent, &metrics, barrier.map(|(_, at)| at));
        let outcome = match step {
            Step::Idle => {
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            }
            Step::AddLearners(missing) => {
                let mut result = Ok(());
                for node in missing {
                    // Re-checked before **every** side effect, not once per
                    // pass: a generation that lost leadership mid-sequence
                    // stops there rather than completing a step the new leader
                    // will redo.
                    if !still_leading(
                        &raft.metrics().borrow().clone(),
                        &local,
                        generation,
                        Arc::clone(&current_generation),
                    ) {
                        return;
                    }
                    result = add_learner(&raft, node).await;
                    if result.is_err() {
                        break;
                    }
                }
                result
            }
            Step::AwaitCatchUp {
                barrier: observed,
                outstanding: _,
            } => {
                barrier = Some((operation_id, observed));
                tokio::time::sleep(IDLE_POLL).await;
                Ok(())
            }
            Step::ChangeVoters => {
                if intent.status_code == phases::PHASE_PENDING {
                    // The transition commits **before** external learner work,
                    // so a crash between the two resumes rather than repeating
                    // a step whose effects are already visible.
                    report_progress(
                        &raft,
                        &operation_id,
                        phases::PHASE_PENDING,
                        phases::PHASE_ADDING_LEARNERS,
                        None,
                        0,
                    )
                    .await
                } else if intent.status_code == phases::PHASE_ADDING_LEARNERS {
                    report_progress(
                        &raft,
                        &operation_id,
                        phases::PHASE_ADDING_LEARNERS,
                        phases::PHASE_CHANGING_VOTERS,
                        None,
                        0,
                    )
                    .await
                } else {
                    let desired: Vec<CoordinationNodeId> = intent
                        .plan
                        .desired_voters
                        .iter()
                        .map(|n| CoordinationNodeId::from_array(*n))
                        .collect();
                    change_voters(&raft, &desired).await
                }
            }
            Step::ReplaceNodes => {
                let desired: Vec<CoordinationNodeId> = intent
                    .plan
                    .desired_voters
                    .iter()
                    .map(|n| CoordinationNodeId::from_array(*n))
                    .collect();
                let retained: Vec<CoordinationNodeId> = intent
                    .plan
                    .retained_learners
                    .iter()
                    .map(|n| CoordinationNodeId::from_array(*n))
                    .collect();
                replace_node_map(&raft, &desired, &retained).await
            }
            Step::ReportComplete => {
                let observed = metrics
                    .membership_config
                    .log_id()
                    .map(crate::coordination::contract::LogIdV1::from);
                report_progress(
                    &raft,
                    &operation_id,
                    phases::PHASE_CHANGING_VOTERS,
                    phases::PHASE_COMPLETED,
                    observed,
                    crate::coordination::contract::SYS_MEMBERSHIP_ADVANCED,
                )
                .await
            }
            Step::CancelCleanup(introduced) => {
                let mut result = Ok(());
                for node in introduced {
                    if !still_leading(
                        &raft.metrics().borrow().clone(),
                        &local,
                        generation,
                        Arc::clone(&current_generation),
                    ) {
                        return;
                    }
                    result = raft
                        .change_membership(
                            ChangeMembers::RemoveNodes([node].into()),
                            MEMBERSHIP_RETAIN,
                        )
                        .await
                        .map(|_| ())
                        .map_err(|err| classify_change(&err));
                    if result.is_err() {
                        break;
                    }
                }
                if result.is_ok() {
                    result = report_progress(
                        &raft,
                        &operation_id,
                        phases::PHASE_CANCEL_REQUESTED,
                        phases::PHASE_CANCELLED,
                        None,
                        0,
                    )
                    .await;
                }
                result
            }
        };

        match outcome {
            Ok(()) => {}
            // Wait, re-observe, retry the same phase. The intent is untouched.
            Err(ChangeOutcome::Retry { .. }) => tokio::time::sleep(IDLE_POLL).await,
            // Back to learner reconciliation against the same durable intent.
            // The barrier is dropped: it described a set that has changed.
            Err(ChangeOutcome::Reconcile { .. }) => {
                barrier = None;
                tokio::time::sleep(IDLE_POLL).await;
            }
            Err(ChangeOutcome::EndGeneration { .. }) => return,
            Err(ChangeOutcome::Invariant { detail }) => {
                facts.latch_fatal(super::lifecycle::FatalCause::Raft(detail));
                return;
            }
        }
    }
}

/// How long the controller waits when there is nothing to drive.
///
/// Short enough that a new intent is picked up promptly, long enough that an
/// idle group is not spinning. Not a timeout: §7.3 forbids a wall-clock
/// transition to `Failed`, and nothing here counts elapsed time.
const IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::error::{EmptyMembership, ForwardToLeader, InProgress, LearnerNotFound};

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
            RaftError::APIError(ClientWriteError::ChangeMembershipError(
                ChangeMembershipError::InProgress(InProgress {
                    committed: None,
                    membership_log_id: None,
                }),
            ));
        assert!(matches!(
            classify_change(&in_progress),
            ChangeOutcome::Retry { .. }
        ));

        let empty: RaftError<CoordinationNodeId, ClientWriteError<_, _>> =
            RaftError::APIError(ClientWriteError::ChangeMembershipError(
                ChangeMembershipError::EmptyMembership(EmptyMembership {}),
            ));
        assert!(matches!(
            classify_change(&empty),
            ChangeOutcome::Invariant { .. }
        ));

        let missing: RaftError<CoordinationNodeId, ClientWriteError<_, _>> =
            RaftError::APIError(ClientWriteError::ChangeMembershipError(
                ChangeMembershipError::LearnerNotFound(LearnerNotFound { node_id: node(3) }),
            ));
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
            let err: RaftError<CoordinationNodeId, ClientWriteError<_, _>> =
                RaftError::APIError(ClientWriteError::ChangeMembershipError(
                    ChangeMembershipError::LearnerNotFound(LearnerNotFound {
                        node_id: node(target),
                    }),
                ));
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
            RaftError::APIError(ClientWriteError::ChangeMembershipError(
                ChangeMembershipError::InProgress(InProgress {
                    committed: None,
                    membership_log_id: None,
                }),
            ));
        let missing: RaftError<CoordinationNodeId, ClientWriteError<_, _>> =
            RaftError::APIError(ClientWriteError::ChangeMembershipError(
                ChangeMembershipError::LearnerNotFound(LearnerNotFound { node_id: node(3) }),
            ));
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
        assert!(
            !calls.is_empty(),
            "the controller must call change_membership"
        );

        let literals: Vec<usize> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let code = line.split("//").next().unwrap_or("");
                code.contains("change_membership(")
                    && (code.contains(", false") || code.contains(", true"))
            })
            .map(|(n, _)| n + 1)
            .collect();
        assert!(
            literals.is_empty(),
            "the retain flag must be MEMBERSHIP_RETAIN, not a literal; found at {literals:?}"
        );
        // Counted over **code** lines: the doc comments name the constant
        // constantly, which is the point of them, and counting prose would
        // make this assertion about how much was written rather than about
        // what is called.
        let uses = production
            .lines()
            .filter(|line| {
                let code = line.split("//").next().unwrap_or("");
                code.contains("MEMBERSHIP_RETAIN")
            })
            .count();
        assert_eq!(
            uses,
            // The import, plus one per call site.
            1 + calls.len(),
            "every change_membership call must pass the constant"
        );

        // And the constant is what §9 says it is, so a change to it is visible
        // from the controller that depends on it. Read through a runtime value
        // because a direct assertion on a `const bool` is a constant
        // expression the lints reject — the fact under test is the same.
        let retain: bool = std::hint::black_box(MEMBERSHIP_RETAIN);
        assert!(
            !retain,
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
                Arc::new(StoredMembership::new(None, Membership::new(configs, nodes))),
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

    // ── the §7.3 phase table ────────────────────────────────────────────────

    use crate::coordination::model::membership as phases;
    use crate::coordination::model::state::{Intent, MemberSeed, MembershipPlan};
    use openraft::{Membership, StoredMembership};

    fn intent(phase: i32, desired: &[u8], retained: &[u8], seeds: &[u8]) -> Intent {
        Intent {
            actor: [1u8; 32],
            request_hash: [0u8; 32],
            expected_membership: None,
            plan: MembershipPlan {
                desired_voters: desired.iter().map(|n| [*n; 32]).collect(),
                retained_learners: retained.iter().map(|n| [*n; 32]).collect(),
                member_seeds: seeds
                    .iter()
                    .map(|n| MemberSeed {
                        node_id: [*n; 32],
                        ticket: Vec::new(),
                        join_permit: None,
                    })
                    .collect(),
            },
            status_code: phase,
            result_code: 0,
            accepted_log_id: crate::coordination::model::state::LogId {
                term: 1,
                leader_node_id: [1u8; 32],
                index: 1,
            },
            completed_log_id: None,
            observed_membership_log_id: None,
        }
    }

    /// A metrics snapshot with `voters`, `learners`, and per-peer replication
    /// progress.
    fn view(
        voters: &[&[u8]],
        learners: &[u8],
        replication: &[(u8, u64)],
    ) -> super::super::lifecycle::Metrics {
        let configs: Vec<BTreeSet<CoordinationNodeId>> = voters
            .iter()
            .map(|c| c.iter().copied().map(node).collect())
            .collect();
        let mut nodes: BTreeMap<CoordinationNodeId, CoordinationNode> = configs
            .iter()
            .flatten()
            .map(|id| (*id, CoordinationNode))
            .collect();
        for l in learners {
            nodes.insert(node(*l), CoordinationNode);
        }
        let mut metrics = super::super::lifecycle::testing::metrics(
            1,
            openraft::ServerState::Leader,
            5,
            Some(5),
            Some(10),
            Arc::new(StoredMembership::new(None, Membership::new(configs, nodes))),
        );
        metrics.last_log_index = Some(100);
        metrics.replication = Some(
            replication
                .iter()
                .map(|(n, index)| {
                    (
                        node(*n),
                        Some(openraft::LogId::new(
                            openraft::CommittedLeaderId::new(5, node(1)),
                            *index,
                        )),
                    )
                })
                .collect(),
        );
        metrics
    }

    /// **The whole phase table**, one row at a time, from the two sources it is
    /// allowed to read.
    ///
    /// Each row is decided from durable intent plus committed membership and
    /// nothing else — no carried position, no retry count. That is why the same
    /// function answers correctly after a restart: there is nothing to restore.
    #[test]
    fn each_phase_selects_its_step_from_intent_and_membership() {
        // Pending: commit the transition before any external learner work.
        assert_eq!(
            next_step(
                &intent(phases::PHASE_PENDING, &[1, 2, 3], &[], &[]),
                &view(&[&[1]], &[], &[]),
                None
            ),
            Step::ChangeVoters
        );

        // AddingLearners with nodes missing: add exactly the missing ones.
        assert_eq!(
            next_step(
                &intent(phases::PHASE_ADDING_LEARNERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1]], &[], &[]),
                None
            ),
            Step::AddLearners(members(&[2, 3, 9]))
        );

        // All present, no barrier yet: this pass observes one.
        assert_eq!(
            next_step(
                &intent(phases::PHASE_ADDING_LEARNERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1]], &[2, 3, 9], &[]),
                None
            ),
            Step::AwaitCatchUp {
                barrier: 100,
                outstanding: members(&[1, 2, 3]),
            }
        );

        // A barrier, and one voter still behind.
        assert_eq!(
            next_step(
                &intent(phases::PHASE_ADDING_LEARNERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1]], &[2, 3, 9], &[(2, 100), (3, 40)]),
                Some(100)
            ),
            Step::AwaitCatchUp {
                barrier: 100,
                outstanding: members(&[3]),
            }
        );

        // Everyone caught up: promote.
        assert_eq!(
            next_step(
                &intent(phases::PHASE_ADDING_LEARNERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1]], &[2, 3, 9], &[(2, 100), (3, 100)]),
                Some(100)
            ),
            Step::ChangeVoters
        );

        // ChangingVoters, mid-joint: keep calling step 4.
        assert_eq!(
            next_step(
                &intent(phases::PHASE_CHANGING_VOTERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1], &[1, 2, 3]], &[9], &[]),
                None
            ),
            Step::ChangeVoters
        );

        // The uniform voter entry landed, the node map is still wrong: step 5.
        assert_eq!(
            next_step(
                &intent(phases::PHASE_CHANGING_VOTERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1, 2, 3]], &[], &[]),
                None
            ),
            Step::ReplaceNodes
        );

        // Exactly the plan: report.
        assert_eq!(
            next_step(
                &intent(phases::PHASE_CHANGING_VOTERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1, 2, 3]], &[9], &[]),
                None
            ),
            Step::ReportComplete
        );

        // Terminal phases drive nothing.
        for terminal in [
            phases::PHASE_COMPLETED,
            phases::PHASE_FAILED,
            phases::PHASE_CANCELLED,
        ] {
            assert_eq!(
                next_step(
                    &intent(terminal, &[1, 2, 3], &[], &[]),
                    &view(&[&[1, 2, 3]], &[], &[]),
                    None
                ),
                Step::Idle,
                "phase {terminal}"
            );
        }
    }

    /// **The catch-up barrier is observed once**, not re-read every pass.
    ///
    /// §7.3 measures against the committed index observed *when the promotion
    /// step began*. A barrier taken fresh each pass advances with the log, so a
    /// learner catching up at any finite rate never reaches it — the change
    /// would stall forever while looking like progress.
    #[test]
    fn the_catch_up_barrier_does_not_move_under_the_learner() {
        let intent = intent(phases::PHASE_ADDING_LEARNERS, &[1, 2], &[], &[]);

        // The log has advanced well past the barrier, and the learner has
        // reached the barrier but not the head.
        let mut advanced = view(&[&[1]], &[2], &[(2, 100)]);
        advanced.last_log_index = Some(5_000);

        assert_eq!(
            next_step(&intent, &advanced, Some(100)),
            Step::ChangeVoters,
            "a learner that reached the observed barrier is caught up, whatever the head is"
        );

        // Against a barrier taken from the current head it would still be
        // behind — which is the stall this rule prevents.
        assert_eq!(
            next_step(&intent, &advanced, Some(5_000)),
            Step::AwaitCatchUp {
                barrier: 5_000,
                outstanding: members(&[2]),
            }
        );
    }

    /// A **retained learner's** lag never blocks the voter change.
    ///
    /// It does not vote, so a membership the group can commit does not depend
    /// on it — and waiting would let one slow non-voter stall a change
    /// indefinitely.
    #[test]
    fn a_lagging_retained_learner_does_not_block_the_change() {
        assert_eq!(
            next_step(
                &intent(phases::PHASE_ADDING_LEARNERS, &[1, 2], &[9], &[]),
                // Node 9 is present but far behind; the voters are current.
                &view(&[&[1]], &[2, 9], &[(2, 100), (9, 1)]),
                Some(100)
            ),
            Step::ChangeVoters
        );
    }

    /// **Cancellation removes only what this intent introduced**, and never a
    /// node the plan retains.
    ///
    /// Removing every learner would strip nodes another intent added; removing
    /// a retained one would undo the plan's own intent in reverse.
    #[test]
    fn a_cancel_removes_only_this_intents_introductions() {
        // Seeds name 4 and 5; 5 is a desired voter, so only 4 is an
        // introduction to clean up. Node 7 is a learner this intent never
        // touched.
        let step = next_step(
            &intent(phases::PHASE_CANCEL_REQUESTED, &[1, 2, 5], &[], &[4, 5]),
            &view(&[&[1, 2]], &[4, 5, 7], &[]),
            None,
        );
        assert_eq!(
            step,
            Step::CancelCleanup(members(&[4])),
            "only the introduced, non-desired node is removed"
        );
    }

    /// A cancel cleans up nothing when its introductions are already gone,
    /// rather than removing something else.
    #[test]
    fn a_cancel_with_nothing_left_to_undo_removes_nothing() {
        assert_eq!(
            next_step(
                &intent(phases::PHASE_CANCEL_REQUESTED, &[1, 2], &[], &[4]),
                &view(&[&[1, 2]], &[7], &[]),
                None
            ),
            Step::CancelCleanup(members(&[]))
        );
    }

    /// **A new generation carries nothing but the catch-up barrier.**
    ///
    /// This is what makes "membership resumes across leader loss" true rather
    /// than hoped for: a generation that started mid-intent derives every
    /// decision from durable intent and committed membership, so it reaches the
    /// same step the old one would have. `next_step` is a pure function of
    /// those two, and the tests above cover its table; what this adds is that
    /// the *loop* has nothing else to restore.
    ///
    /// The barrier is the one exception, and it is deliberate: §7.3 measures
    /// catch-up against the index observed when the promotion step began, so a
    /// new generation observes a fresh one — correct, because it has
    /// re-observed everything else too.
    ///
    /// A source guard, because the property is about state that does not exist.
    /// No behavioural test can observe a field nobody wrote.
    #[test]
    fn a_generation_carries_no_state_but_its_barrier() {
        let source = include_str!("membership.rs");
        let end = crate::coordination::guards::production_end(source);
        let production = &source[..end];

        let at = production
            .find("pub(crate) async fn run_generation(")
            .expect("the controller loop exists");
        let body_end = production[at..]
            .find("\n/// How long the controller waits")
            .expect("the loop is followed by IDLE_POLL");
        let body = &production[at..at + body_end];

        // Only the **prologue** — what is declared before `loop {` and
        // therefore survives a pass. A `let mut` inside a match arm is a
        // per-pass local and carries nothing; counting those would make the
        // guard fail for code that is exactly right.
        let loop_at = body.find("\n    loop {").expect("the controller loops");
        let prologue = &body[..loop_at];
        let carried: Vec<&str> = prologue
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .filter(|code| code.trim_start().starts_with("let mut "))
            .collect();
        assert_eq!(
            carried.len(),
            1,
            "the loop must carry exactly one value across passes — the catch-up \
             barrier; found {carried:?}"
        );
        assert!(
            carried[0].contains("barrier"),
            "the one carried value must be the barrier: {}",
            carried[0].trim()
        );

        // And it is dropped whenever the intent changes or reconciliation
        // restarts, so it cannot describe a set that has moved.
        assert!(
            body.contains("barrier = None"),
            "the barrier must be dropped when the intent changes or a \
             reconciliation restarts"
        );

        // Nothing counts attempts. §7.3 forbids deciding an intent's fate from
        // a local retry count, which resets on failover.
        for forbidden in ["retries", "attempt", "retry_count", "elapsed"] {
            assert!(
                !body.contains(forbidden),
                "the loop must not count {forbidden:?}: a counter lives in one leader \
                 generation and would make an intent's fate depend on which node was \
                 leader and for how long"
            );
        }
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
