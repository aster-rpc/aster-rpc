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
use openraft::{ChangeMembers, Membership, Raft};

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
/// Against the **committed** membership. This read was
/// `metrics.membership_config`, which is the effective membership, so an intent
/// could be reported complete against a uniform entry that had been appended
/// and not accepted by any quorum — the one thing §S3-E's completion rule
/// exists to forbid.
///
/// A controller reply, a local node-map edit, or a combined member set is not
/// enough: the committed membership must be non-joint, its voters exactly the
/// desired set, and its learners exactly the retained set. `decide_progress`
/// refuses a `Completed` report that does not match, so a controller that
/// reported early would simply not advance the intent — but reporting early is
/// still wrong, and this is where it is not done.
pub(crate) fn completion_is_visible(
    membership: &Membership<CoordinationNodeId, CoordinationNode>,
    desired_voters: &[CoordinationNodeId],
    retained_learners: &[CoordinationNodeId],
) -> bool {
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
    /// Every node is present and no barrier has been taken: confirm leadership
    /// with a quorum and record the committed cut (§7.3 step 3).
    ///
    /// A step rather than a value the table computes, because the committed cut
    /// is not in `RaftMetrics` and cannot be. `last_log_index` is the log head,
    /// which includes entries no quorum has accepted — a learner could be told
    /// to reach a position that never commits. `last_applied` is only a *lower
    /// bound* on the committed state, not the contract's committed cut. Only
    /// `Raft::ensure_linearizable` produces that: it heartbeats a quorum, which
    /// is the only thing that proves this node still holds authority *now*, and
    /// then waits until the state machine has applied up to the read log id.
    ///
    /// Costing one round trip per intent per generation, not per polling pass:
    /// the barrier is taken once and remembered for the life of the generation.
    EstablishCatchUpBarrier,
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
///
/// **`committed` is the state machine's membership, not
/// `metrics.membership_config`.** That field is
/// `membership_state.effective()`: the newest membership entry this node has
/// *seen*. Reading it here made the table act on entries no quorum had
/// accepted — treating an appended joint entry as "the joint entry landed", and
/// an appended uniform entry as licence to replace the node map. Both can be
/// overwritten by a new leader, and the corridor's own rule is the opposite
/// one: while effective is ahead of committed, wait and retry. Asking the same
/// change again is what does that waiting — OpenRaft answers `InProgress`,
/// which `classify_change` turns into `ChangeOutcome::Retry`.
///
/// `metrics` is still needed, and only for what it is authoritative about:
/// this node's id, and per-peer replication progress. Not `last_log_index` —
/// the table read it for the catch-up barrier and no longer does, because the
/// barrier is a committed cut that costs a quorum round trip
/// ([`Step::EstablishCatchUpBarrier`]).
pub(crate) fn next_step(
    intent: &crate::coordination::model::state::Intent,
    metrics: &super::lifecycle::Metrics,
    committed: &Membership<CoordinationNodeId, CoordinationNode>,
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

    let membership = committed;
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
                // §7.3 step 3 measures against the **committed cut** observed
                // *when the promotion step began*. No barrier yet means this
                // pass is that beginning — a barrier re-read every pass is a
                // moving target no learner ever reaches — and taking it needs a
                // quorum round trip the table cannot make. See
                // [`Step::EstablishCatchUpBarrier`].
                return Step::EstablishCatchUpBarrier;
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
            if completion_is_visible(membership, &desired, &retained) {
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

/// Confirm leadership with a quorum and take the committed cut (§7.3 step 3).
///
/// `ensure_linearizable` is the only call that produces it. It heartbeats a
/// quorum — which is what proves this node still holds authority *now*, and is
/// exactly what `still_leading` cannot prove: an isolated leader goes on
/// reporting `ServerState::Leader` indefinitely, and the resume gates depend on
/// that being true. It then waits until the state machine has applied up to the
/// read log id, so the value returned is a position the group has agreed on and
/// this node has.
///
/// The three failures are three different things and are answered as such:
/// leadership that has moved ends the generation, a quorum this node cannot
/// currently reach is a retry that leaves the intent untouched, and a fatal
/// runtime is a lifecycle state rather than an intent outcome.
async fn establish_barrier(raft: &Raft<CoordinationTypeConfig>) -> Result<u64, ChangeOutcome> {
    use openraft::error::CheckIsLeaderError;

    match raft.ensure_linearizable().await {
        // `None` is a leader that has committed nothing at all, whose committed
        // cut is the empty prefix. Zero is that, not a fallback.
        Ok(read) => Ok(read.map(|id| id.index).unwrap_or(0)),
        Err(RaftError::Fatal(fatal)) => Err(ChangeOutcome::Invariant {
            detail: format!("the Raft runtime is fatal: {fatal}"),
        }),
        Err(RaftError::APIError(CheckIsLeaderError::ForwardToLeader(to))) => {
            Err(ChangeOutcome::EndGeneration {
                detail: format!(
                    "leadership moved while taking the catch-up barrier: {:?}",
                    to.leader_id
                ),
            })
        }
        Err(RaftError::APIError(CheckIsLeaderError::QuorumNotEnough(err))) => {
            Err(ChangeOutcome::Retry {
                detail: format!("no quorum answered the barrier heartbeat: {err}"),
            })
        }
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

        // The state machine's membership, which is the committed one. Re-read
        // every pass for the same reason everything else here is: the point of
        // §7.3's two sources is that neither is remembered.
        let committed = state.committed_membership();
        let step = next_step(
            &intent,
            &metrics,
            committed.membership(),
            barrier.map(|(_, at)| at),
        );
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
            Step::EstablishCatchUpBarrier => {
                // Re-checked immediately before the round trip, like every
                // other side effect: a generation that has been superseded must
                // not spend a quorum's heartbeats on an intent it no longer
                // drives.
                if !still_leading(
                    &raft.metrics().borrow().clone(),
                    &local,
                    generation,
                    Arc::clone(&current_generation),
                ) {
                    return;
                }
                match establish_barrier(&raft).await {
                    Ok(cut) => {
                        barrier = Some((operation_id, cut));
                        Ok(())
                    }
                    Err(outcome) => Err(outcome),
                }
            }
            Step::AwaitCatchUp {
                barrier: _,
                outstanding: _,
            } => {
                tokio::time::sleep(IDLE_POLL).await;
                Ok(())
            }
            Step::ChangeVoters => {
                // Re-checked before the side effect, exactly like
                // `AddLearners` and the barrier: a superseded or shut-down
                // generation must not commit a phase report or a voter
                // change it no longer drives.
                if !still_leading(
                    &raft.metrics().borrow().clone(),
                    &local,
                    generation,
                    Arc::clone(&current_generation),
                ) {
                    return;
                }
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
                if !still_leading(
                    &raft.metrics().borrow().clone(),
                    &local,
                    generation,
                    Arc::clone(&current_generation),
                ) {
                    return;
                }
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
                if !still_leading(
                    &raft.metrics().borrow().clone(),
                    &local,
                    generation,
                    Arc::clone(&current_generation),
                ) {
                    return;
                }
                // The **committed** membership's log id, which is what
                // `decide_progress` compares a `Completed` report against. From
                // the effective one it could name an entry no quorum accepted,
                // and the report would be refused for naming a membership the
                // group does not have — or, worse, accepted before it did.
                let observed = committed
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

        // **A generation that has committed its own removal stops here.**
        //
        // Once a membership excluding this node commits and applies, OpenRaft
        // steps the leader down of its own accord; driving one more step from
        // this generation is a side effect performed by a leader the group has
        // already voted out. §7.3 hands the rest — `ReplaceAllNodes` and the
        // completion report — to the next generation, which reaches the same
        // step from the same durable intent because that is the whole point of
        // deciding from observation.
        //
        // Checked *after* the step, not before: refusing to act at all would
        // stall, because the membership that removes this node is the very
        // entry this controller has to commit, and no election can be caused
        // while it still leads. `trigger().elect()` is a no-op on a leader.
        if !state
            .committed_membership()
            .membership()
            .voter_ids()
            .any(|id| id == local)
        {
            return;
        }

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
        use openraft::Membership;

        // A **membership**, not a metrics snapshot: completion is decided
        // against the committed membership the state machine holds, and the
        // metrics field this used to read is the effective one.
        let view = |configs: Vec<BTreeSet<CoordinationNodeId>>, learners: &[u8]| {
            let mut nodes: BTreeMap<CoordinationNodeId, CoordinationNode> = configs
                .iter()
                .flatten()
                .map(|id| (*id, CoordinationNode))
                .collect();
            for l in learners {
                nodes.insert(node(*l), CoordinationNode);
            }
            Membership::new(configs, nodes)
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
        // The head runs **ahead of** the applied index on purpose: 100 against
        // the 7 the fixture applies. §7.3's catch-up barrier is the committed
        // index, and a fixture where the two agreed could not tell a correct
        // reading from the log head — which is exactly what this locked in
        // while the barrier read `last_log_index`.
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

    /// `next_step` where the effective and committed membership agree.
    ///
    /// Every row of the table below describes a **settled** state, so the two
    /// coincide and saying so once keeps the rows about the table. The case
    /// where they differ is the whole point of reading the committed one, and
    /// is pinned on its own by
    /// [`an_uncommitted_entry_does_not_advance_the_table`].
    fn step(
        intent: &Intent,
        metrics: &super::super::lifecycle::Metrics,
        barrier: Option<u64>,
    ) -> Step {
        next_step(
            intent,
            metrics,
            metrics.membership_config.membership(),
            barrier,
        )
    }

    /// **The catch-up barrier is not computed from metrics at all.**
    ///
    /// §7.3 step 3 measures promotion against the committed cut observed when
    /// the step began, and no field of `RaftMetrics` is that. `last_log_index`
    /// is the log head, which includes entries no quorum has accepted — a
    /// learner could be told to reach a position that never commits, and the
    /// promotion would wait forever for an index the group had abandoned.
    /// `last_applied` is only a *lower bound* on committed state. Both were
    /// used here in turn, and both were wrong for the same reason: the value
    /// requires a quorum round trip, so the table asks for one instead of
    /// guessing.
    ///
    /// The fixture's head deliberately runs far ahead of its applied index, so
    /// a table that reached for either would produce a number and this would
    /// catch it.
    #[test]
    fn the_catch_up_barrier_is_taken_by_a_quorum_round_trip_not_read_from_metrics() {
        let plan = intent(phases::PHASE_ADDING_LEARNERS, &[1, 2], &[], &[]);
        let mut diverged = view(&[&[1]], &[2], &[]);
        diverged.last_log_index = Some(9_000);
        diverged.last_applied = Some(openraft::LogId::new(
            openraft::CommittedLeaderId::new(5, node(0)),
            42,
        ));

        assert_eq!(
            step(&plan, &diverged, None),
            Step::EstablishCatchUpBarrier,
            "with every node present and no barrier recorded, the table asks for the \
             committed cut rather than inventing one from the head (9000) or from the \
             applied index (42)"
        );

        // And once a barrier exists it is *used*, not re-derived: a learner
        // that reached it is caught up whatever the head says.
        assert_eq!(
            step(&plan, &view(&[&[1]], &[2], &[(2, 42)]), Some(42)),
            Step::ChangeVoters,
            "a recorded barrier is the measure, and re-reading one every pass would be \
             a moving target no learner reaches"
        );
    }

    /// **An entry that is only *effective* does not advance the table.**
    ///
    /// The regression for the defect this whole reading exists to prevent.
    /// `RaftMetrics::membership_config` is `membership_state.effective()`: the
    /// newest membership entry seen, committed or not. A controller reading it
    /// treats a joint entry one leader appended as "the joint entry landed" and
    /// a uniform entry as licence to replace the node map — both of which a new
    /// leader can overwrite.
    ///
    /// Here the effective membership is already the finished uniform shape
    /// while the committed one is still the old five. The table must decide
    /// from the committed one and ask for the voter change again; OpenRaft
    /// answers `InProgress` while effective is ahead, which `classify_change`
    /// turns into a retry — the corridor's own "wait and retry" rule.
    #[test]
    fn an_uncommitted_entry_does_not_advance_the_table() {
        let plan = intent(phases::PHASE_CHANGING_VOTERS, &[1, 2, 3], &[9], &[]);
        let ahead = view(&[&[1, 2, 3]], &[9], &[]);
        let behind = Membership::new(
            vec![members(&[1, 2, 3, 4, 5])],
            [1u8, 2, 3, 4, 5, 9]
                .iter()
                .map(|n| (node(*n), CoordinationNode))
                .collect::<BTreeMap<_, _>>(),
        );

        assert_eq!(
            next_step(&plan, &ahead, &behind, None),
            Step::ChangeVoters,
            "an uncommitted uniform entry is not a landed one; the corridor asks again"
        );
        // And read from the effective membership — as it was — the same state
        // reports the change finished, which is the bug.
        assert_eq!(
            next_step(&plan, &ahead, ahead.membership_config.membership(), None),
            Step::ReportComplete,
            "this is what reading the effective membership did, and why it was wrong"
        );
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
            step(
                &intent(phases::PHASE_PENDING, &[1, 2, 3], &[], &[]),
                &view(&[&[1]], &[], &[]),
                None
            ),
            Step::ChangeVoters
        );

        // AddingLearners with nodes missing: add exactly the missing ones.
        assert_eq!(
            step(
                &intent(phases::PHASE_ADDING_LEARNERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1]], &[], &[]),
                None
            ),
            Step::AddLearners(members(&[2, 3, 9]))
        );

        // All present, no barrier yet: this pass asks for one, and asking is a
        // quorum round trip rather than a metrics read. See
        // `the_catch_up_barrier_is_taken_by_a_quorum_round_trip_not_read_from_metrics`.
        assert_eq!(
            step(
                &intent(phases::PHASE_ADDING_LEARNERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1]], &[2, 3, 9], &[]),
                None
            ),
            Step::EstablishCatchUpBarrier
        );

        // A barrier, and one voter still behind.
        assert_eq!(
            step(
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
            step(
                &intent(phases::PHASE_ADDING_LEARNERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1]], &[2, 3, 9], &[(2, 100), (3, 100)]),
                Some(100)
            ),
            Step::ChangeVoters
        );

        // ChangingVoters, mid-joint: keep calling step 4.
        assert_eq!(
            step(
                &intent(phases::PHASE_CHANGING_VOTERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1], &[1, 2, 3]], &[9], &[]),
                None
            ),
            Step::ChangeVoters
        );

        // The uniform voter entry landed, the node map is still wrong: step 5.
        assert_eq!(
            step(
                &intent(phases::PHASE_CHANGING_VOTERS, &[1, 2, 3], &[9], &[]),
                &view(&[&[1, 2, 3]], &[], &[]),
                None
            ),
            Step::ReplaceNodes
        );

        // Exactly the plan: report.
        assert_eq!(
            step(
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
                step(
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
            step(&intent, &advanced, Some(100)),
            Step::ChangeVoters,
            "a learner that reached the observed barrier is caught up, whatever the head is"
        );

        // Against a barrier taken from the current head it would still be
        // behind — which is the stall this rule prevents.
        assert_eq!(
            step(&intent, &advanced, Some(5_000)),
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
            step(
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
        let step = step(
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
            step(
                &intent(phases::PHASE_CANCEL_REQUESTED, &[1, 2], &[], &[4]),
                &view(&[&[1, 2]], &[7], &[]),
                None
            ),
            Step::CancelCleanup(members(&[]))
        );
    }

    /// **`S3-JOINT-RESUME`: the four proof boundaries.**
    ///
    /// OpenRaft's blocking `change_membership` writes a joint entry and then a
    /// uniform one. Leadership loss or process death **between** them leaves
    /// the group joint, so a new generation must begin by *observing* rather
    /// than by assuming a uniform membership.
    ///
    /// Each boundary below is a state a new generation could wake up in, and
    /// each is driven against the **same durable intent** — which is the point:
    /// nothing is carried across the loss, so the same function reaches the
    /// right step from the state alone.
    ///
    /// The voter transition and the node-map replacement are two operations and
    /// the pair is **not** atomic. `ReplaceAllNodes` is one entry and atomic on
    /// its own, which is what makes it safe *after* the uniform entry is
    /// verified and wrong before or merged with it.
    #[test]
    fn a_new_generation_resumes_the_joint_corridor_from_observation() {
        let plan = intent(phases::PHASE_CHANGING_VOTERS, &[1, 2, 3], &[9], &[]);

        // 1. Lost **before the joint entry committed**: membership is still the
        //    old uniform set. Call the same desired change again; OpenRaft's
        //    coherent next step emits the joint entry.
        assert_eq!(
            step(&plan, &view(&[&[1, 2]], &[3, 9], &[]), None),
            Step::ChangeVoters,
            "before the joint entry, the corridor restarts from the same desired change"
        );

        // 2. Lost **after the joint entry committed**: the group is joint. The
        //    same call again is what makes OpenRaft emit the uniform entry —
        //    not a different call, and not an assumption that it already did.
        assert_eq!(
            step(&plan, &view(&[&[1, 2], &[1, 2, 3]], &[9], &[]), None),
            Step::ChangeVoters,
            "a joint membership resumes by repeating the desired change"
        );

        // 3. Lost **after the uniform entry committed**, node map not yet
        //    replaced: the voters are right and a stale learner remains. Only
        //    step 5 is owed, and it is a separate operation.
        assert_eq!(
            step(&plan, &view(&[&[1, 2, 3]], &[7, 9], &[]), None),
            Step::ReplaceNodes,
            "after the uniform entry, only the node map is owed"
        );

        // 4. Lost **after the node-map replacement**: everything matches, and
        //    all that remains is to report what has already been observed.
        assert_eq!(
            step(&plan, &view(&[&[1, 2, 3]], &[9], &[]), None),
            Step::ReportComplete,
            "after the replacement, completion is reported from observation"
        );

        // And the boundaries are genuinely different: a resume that answered
        // the same thing everywhere would pass three of the four above by
        // accident.
        let answers = [
            step(&plan, &view(&[&[1, 2]], &[3, 9], &[]), None),
            step(&plan, &view(&[&[1, 2, 3]], &[7, 9], &[]), None),
            step(&plan, &view(&[&[1, 2, 3]], &[9], &[]), None),
        ];
        let distinct: BTreeSet<String> = answers.iter().map(|a| format!("{a:?}")).collect();
        assert_eq!(
            distinct.len(),
            3,
            "the boundaries must be distinguishable, or the resume is not observing"
        );
    }

    /// The corridor never merges the voter change with the node-map
    /// replacement.
    ///
    /// `ReplaceNodes` is reachable **only** once the voters already match and
    /// the membership is uniform — so no state produces both in one pass, and
    /// nothing can be written as though the pair were atomic.
    #[test]
    fn the_node_map_replacement_never_runs_before_the_uniform_entry() {
        let plan = intent(phases::PHASE_CHANGING_VOTERS, &[1, 2, 3], &[9], &[]);
        for membership in [
            // Old uniform.
            view(&[&[1, 2]], &[3, 9], &[]),
            // Joint.
            view(&[&[1, 2], &[1, 2, 3]], &[9], &[]),
            // Joint, with the target already in one config.
            view(&[&[1, 2, 3], &[1, 2]], &[9], &[]),
        ] {
            assert_ne!(
                step(&plan, &membership, None),
                Step::ReplaceNodes,
                "the node map may only be replaced after the uniform voter entry"
            );
        }
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
    /// **Every side-effecting step re-checks leadership and generation
    /// immediately before acting** — `AddLearners`, the catch-up barrier,
    /// `ChangeVoters`, `ReplaceNodes`, `ReportComplete`, and the cancel
    /// cleanup. The third review found three arms without the recheck: a
    /// superseded or shut-down generation could commit a phase report or a
    /// voter change it no longer drove. A source guard, because the window
    /// is scheduler-shaped: the recheck's absence only matters in the
    /// milliseconds between supersession and the arm's round trip.
    #[test]
    fn every_side_effecting_step_rechecks_leadership() {
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
        let rechecks = body.matches("still_leading(").count();
        // Exactly seven: the per-pass check plus one in each of the six
        // side-effecting arms. Exact rather than a floor — the mutation
        // campaign proved a `>= 6` floor vacuous, because removing one arm's
        // recheck still left six.
        assert_eq!(
            rechecks, 7,
            "the pass check and all six side-effecting arms must recheck \
             still_leading; found {rechecks}"
        );
    }

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
