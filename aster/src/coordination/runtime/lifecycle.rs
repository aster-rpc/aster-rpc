//! The §7.6 runtime lifecycle: the six §5.2 codes, and nothing else produces
//! them.
//!
//! ## The predicate, and the one function that owns it
//!
//! [`classify`] is a pure function of a metrics snapshot plus the two local
//! facts OpenRaft does not know (which start corridor is still running, and
//! whether shutdown has begun). Everything else — `wait_ready`, the status
//! reply, the health surface — reads its output. A second reading of "are we
//! ready?" elsewhere is the failure this module exists to prevent, because the
//! two would disagree exactly during a partition.
//!
//! ## `Raft::current_leader()` is not in the predicate
//!
//! It reads `metrics().current_leader`, which OpenRaft derives from the
//! committed vote and does **not** clear when a leader loses contact with its
//! followers. An isolated leader keeps reporting itself, so a predicate built
//! on it returns `Ready` during exactly the partition it exists to detect. The
//! field is reported in status — an operator wants to know who this node thinks
//! the leader is — and it is never an input to readiness.
//!
//! ## Leader freshness is OpenRaft's own metric, and there is no second tracker
//!
//! `RaftMetrics::millis_since_quorum_ack` is computed through the effective
//! membership's `QuorumSet`, which during a **joint** configuration requires a
//! majority in *each* config. A hand-rolled "majority of the voter union"
//! disagrees with that exactly when it matters: partition `[{1,2,3},{3,4,5}]`
//! so the leader reaches only `{1,2,3}` and the union rule reports healthy for
//! a membership that cannot commit. It also folds in the leader's own clock
//! before reading, and correctly ignores a leader that is not itself a voter.
//! Writing a second tracker would reintroduce, one level down, the
//! false-healthy report this rule removes.
//!
//! `None` is **not** fresh. It means either "not leader" or "not yet
//! acknowledged by a quorum", and a newly elected leader must not report
//! `Ready` before its first ack.
//!
//! ## A leader's `Ready` is quorum-backed; a follower's is advisory
//!
//! On a follower the metric is always `None`, so the runtime keeps its own
//! `last_leader_contact`, updated by any accepted `append_entries` from the
//! current leader. **That observation is not evidence of quorum**, and v1 does
//! not pretend otherwise: partition five voters into `{old leader, follower}`
//! and `{three voters}` and the minority leader keeps appending, so the
//! follower's contact never goes stale even though its partition cannot commit.
//!
//! This is safe only because of what a follower is allowed to do. Every
//! authoritative operation redirects to a leader and is satisfied through the
//! leader's quorum-verified path, so an advisory follower `Ready` cannot
//! produce an unauthorised answer. A follower MUST NOT call
//! `ensure_linearizable()` — leader-only by signature — and MUST NOT answer an
//! authoritative read locally.
//!
//! Consequently [`LifecycleView::quorum_available`] is `Option<bool>`: `Some`
//! on a leader, `None` on a follower. A plain `bool` would force the stranded
//! follower above to claim a quorum it cannot observe.

use std::sync::Arc;
use std::time::Duration;

use openraft::{RaftMetrics, ServerState, StoredMembership};

use crate::coordination::ids::CoordinationNodeId;
use crate::coordination::typeconfig::CoordinationNode;

/// A metrics snapshot for this type config.
pub(crate) type Metrics = RaftMetrics<CoordinationNodeId, CoordinationNode>;

/// The §5.2 lifecycle codes. The numbers are wire surface and are pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i32)]
pub(crate) enum LifecycleCode {
    Bootstrapping = 1,
    Joining = 2,
    Ready = 3,
    NoQuorum = 4,
    Draining = 5,
    Fatal = 6,
}

impl LifecycleCode {
    /// Every code, in §5.2 declaration order.
    pub(crate) const ALL: &'static [LifecycleCode] = &[
        LifecycleCode::Bootstrapping,
        LifecycleCode::Joining,
        LifecycleCode::Ready,
        LifecycleCode::NoQuorum,
        LifecycleCode::Draining,
        LifecycleCode::Fatal,
    ];

    pub(crate) fn code(self) -> i32 {
        self as i32
    }
}

/// The §5.2 local-role codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub(crate) enum LocalRole {
    None = 0,
    Learner = 1,
    Voter = 2,
}

impl LocalRole {
    pub(crate) fn code(self) -> i32 {
        self as i32
    }
}

/// Which start corridor is still running, if any.
///
/// Not derived from Raft state, because neither corridor is a Raft concept: a
/// bootstrapping node has committed nothing yet, and a joining node is waiting
/// for a membership entry another group's leader must propose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Corridor {
    /// `Bootstrap`, until initialization commits.
    Bootstrapping,
    /// `Join`, until a committed membership contains the local node.
    Joining,
    /// Neither — `Existing`, or a corridor that has completed.
    Running,
}

/// A condition no later message repairs.
///
/// The test is **repairability**, not severity: a group with no leader is
/// `NoQuorum` however alarming it looks, and an entry this build cannot
/// interpret is `Fatal` however quiet it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FatalCause {
    /// Durable storage latched fatal.
    Storage(String),
    /// OpenRaft itself reported `Fatal`.
    Raft(String),
    /// The committed membership names a voter count this profile forbids.
    ProfileViolation { profile: i32, voters: usize },
    /// Storage identity did not match what was configured.
    IdentityMismatch(String),
}

impl FatalCause {
    /// The §5.2 `fatal_code` projection: one nonzero code per variant (`0`
    /// on the wire means "not fatal" and is never a variant's code). The
    /// authoritative declaration the client's restated domain is pinned to.
    pub(crate) fn code(&self) -> i32 {
        match self {
            FatalCause::Storage(_) => 1,
            FatalCause::Raft(_) => 2,
            FatalCause::ProfileViolation { .. } => 3,
            FatalCause::IdentityMismatch(_) => 4,
        }
    }
}

impl std::fmt::Display for FatalCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FatalCause::Storage(d) => write!(f, "durable storage latched fatal: {d}"),
            FatalCause::Raft(d) => write!(f, "the Raft runtime reported fatal: {d}"),
            FatalCause::ProfileViolation { profile, voters } => write!(
                f,
                "the committed membership has {voters} voters, which deployment profile \
                 {profile} forbids"
            ),
            FatalCause::IdentityMismatch(d) => write!(f, "storage identity mismatch: {d}"),
        }
    }
}

/// The local inputs §7.6 needs that a metrics snapshot does not carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalFacts {
    pub(crate) corridor: Corridor,
    /// Shutdown has begun. Takes precedence over everything except `Fatal`,
    /// because a draining node's readiness is about to stop being true.
    pub(crate) draining: bool,
    /// Latched, never cleared: a `Fatal` that could be recovered from by a
    /// later observation would not be `Fatal`.
    pub(crate) fatal: Option<FatalCause>,
    /// How long since this node last accepted an `append_entries` from the
    /// leader it currently recognises. `None` means it never has.
    ///
    /// **Advisory.** See the module docs: this is not quorum evidence, and the
    /// only reason it is sound to report at all is that no authoritative
    /// operation completes through a follower.
    pub(crate) follower_contact_age: Option<Duration>,
    /// §7.6's derived timeout, from
    /// [`config::leader_liveness_timeout`](super::config::leader_liveness_timeout).
    pub(crate) leader_liveness_timeout: Duration,
    /// Whether this node has **applied** the group's signed genesis.
    ///
    /// A voter can be a healthy member of a quorum and still be unable to
    /// authorize anything, because the delegated administrator set arrives in a
    /// committed entry like any other state. Reporting `Ready` before it lands
    /// would tell a caller the group is usable at the one moment every
    /// administrative action is guaranteed to be refused.
    ///
    /// Uniform across start modes on purpose: a bootstrapping voter reaches it
    /// by applying the entry its leader proposed, and a joining one by replay
    /// or by the snapshot that subsumes it. Neither has a different question to
    /// answer.
    pub(crate) genesis_installed: bool,
    /// The **committed** membership, from the state actor.
    ///
    /// Not from `metrics.membership_config`, which is
    /// `membership_state.effective()` — the newest membership entry seen,
    /// committed or not. §7.6 asks what the *group has agreed* this node is,
    /// and an entry one leader appended and no quorum accepted is not an
    /// agreement: a new leader can overwrite it, and a node that had already
    /// been reported a voter on the strength of it would have been reported
    /// something that never became true.
    pub(crate) committed: Arc<StoredMembership<CoordinationNodeId, CoordinationNode>>,
}

/// What this node reports about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LifecycleView {
    pub(crate) code: LifecycleCode,
    /// This node's role in the committed membership.
    pub(crate) local_role: LocalRole,
    /// "This node holds direct evidence that a quorum is reachable."
    ///
    /// `Some` on a leader, from `millis_since_quorum_ack`; `None` on a
    /// follower, which holds no such evidence. A leader whose metric is itself
    /// `None` reports `Some(false)`, not `None`: "not yet acknowledged by a
    /// quorum" is evidence, and "no evidence" is a different answer.
    pub(crate) quorum_available: Option<bool>,
    /// Set exactly when `code` is `Fatal`.
    pub(crate) fatal: Option<FatalCause>,
}

/// The §7.6 table. The **only** producer of a lifecycle code.
///
/// Precedence is strict and evaluated in this order:
/// `Fatal > Draining > Joining > Bootstrapping > NoQuorum > Ready`. A node that
/// has not completed `Join` reports `Joining` even when a leader exists and the
/// group is healthy, because a joining node is not yet part of the group whose
/// health is being reported — which is also why "membership does not yet
/// contain self" is **not** a `NoQuorum` condition.
pub(crate) fn classify(metrics: &Metrics, local: &LocalFacts) -> LifecycleView {
    let local_role = role_of(metrics, local);
    let quorum_available = quorum_evidence(metrics, local);

    // 1. Fatal. Both the latched local cause and OpenRaft's own running state,
    //    because either alone would miss half the conditions: storage latches
    //    without telling OpenRaft, and OpenRaft's state-machine worker can die
    //    without storage noticing.
    let fatal = local.fatal.clone().or_else(|| {
        metrics
            .running_state
            .as_ref()
            .err()
            .map(|e| FatalCause::Raft(e.to_string()))
    });
    if let Some(cause) = fatal {
        return LifecycleView {
            code: LifecycleCode::Fatal,
            local_role,
            quorum_available,
            fatal: Some(cause),
        };
    }

    // 2. Draining.
    if local.draining {
        return view(LifecycleCode::Draining, local_role, quorum_available);
    }

    // 3, 4. The corridors, before any health question is asked.
    match local.corridor {
        Corridor::Joining => return view(LifecycleCode::Joining, local_role, quorum_available),
        Corridor::Bootstrapping => {
            return view(LifecycleCode::Bootstrapping, local_role, quorum_available)
        }
        Corridor::Running => {}
    }

    // 5, 6. `Ready` requires four live facts, all of them. Anything short of
    //       all four is `NoQuorum`, which is every condition a later message
    //       repairs — including a genesis that has not arrived yet.
    let ready = local_role == LocalRole::Voter
        && leader_observation_is_fresh(metrics, local)
        && applied_through_the_leaders_blank_entry(metrics)
        && local.genesis_installed;

    view(
        if ready {
            LifecycleCode::Ready
        } else {
            LifecycleCode::NoQuorum
        },
        local_role,
        quorum_available,
    )
}

fn view(
    code: LifecycleCode,
    local_role: LocalRole,
    quorum_available: Option<bool>,
) -> LifecycleView {
    LifecycleView {
        code,
        local_role,
        quorum_available,
        fatal: None,
    }
}

/// This node's role in the **committed** membership.
///
/// From the state actor rather than from `ServerState`: `ServerState::Leader`
/// says what this node is doing, and the question here is what the group has
/// agreed this node is. And from the state actor rather than from
/// `metrics.membership_config`, which despite this comment's earlier wording is
/// the *effective* membership — the newest entry seen, committed or not.
fn role_of(metrics: &Metrics, local: &LocalFacts) -> LocalRole {
    let membership = local.committed.membership();
    // `voter_ids` unions every configuration, which is the right reading during
    // a joint change: a node that is a voter in either config votes, and the
    // question here is what the group has agreed this node is, not which half
    // of a transition it belongs to.
    if membership.voter_ids().any(|id| id == metrics.id) {
        LocalRole::Voter
    } else if membership.nodes().any(|(id, _)| id == &metrics.id) {
        LocalRole::Learner
    } else {
        LocalRole::None
    }
}

/// §7.6's `quorum_available`: direct evidence, or the honest absence of it.
///
/// The question is whether a quorum is **reachable**, not whether one was ever
/// reached, so a leader whose last acknowledgement is older than
/// `leader_liveness_timeout` answers `Some(false)`. An earlier revision here
/// returned `Some(millis_since_quorum_ack.is_some())`, which reported `true`
/// for a leader whose last ack was ten minutes old — the isolated leader, which
/// is precisely the case the field exists to expose. `Some` and the freshness
/// rule therefore share one definition: a leader reports `Some(true)` exactly
/// when [`leader_observation_is_fresh`] holds for it.
///
/// `None` from the metric still yields `Some(false)` rather than `None`: "not
/// yet acknowledged by a quorum" is evidence, and "no evidence" is a different
/// answer.
fn quorum_evidence(metrics: &Metrics, local: &LocalFacts) -> Option<bool> {
    match metrics.state {
        ServerState::Leader => Some(
            metrics
                .millis_since_quorum_ack
                .is_some_and(|ms| Duration::from_millis(ms) <= local.leader_liveness_timeout),
        ),
        // Everything else holds no direct evidence at all. A follower's leader
        // contact is a liveness hint about one peer, not a quorum observation.
        _ => None,
    }
}

/// The freshness half of the `Ready` predicate.
///
/// Two different facts wearing one name, and the split is deliberate rather
/// than a special case: on a leader this is quorum evidence, and on a follower
/// it is a liveness hint about one peer. They share a timeout because they are
/// measured against the same election window, not because they mean the same
/// thing.
fn leader_observation_is_fresh(metrics: &Metrics, local: &LocalFacts) -> bool {
    match metrics.state {
        ServerState::Leader => metrics
            .millis_since_quorum_ack
            .is_some_and(|ms| Duration::from_millis(ms) <= local.leader_liveness_timeout),
        ServerState::Follower => local
            .follower_contact_age
            .is_some_and(|age| age <= local.leader_liveness_timeout),
        // A candidate is mid-election and a learner is not a voter, so neither
        // reaches `Ready` regardless; saying so here keeps the match total
        // without a wildcard that would silently absorb a future state.
        ServerState::Candidate | ServerState::Learner | ServerState::Shutdown => false,
    }
}

/// The third `Ready` fact: this node has applied through the current leader's
/// initial blank entry.
///
/// A leader appends a blank entry at the start of its term, so once it is
/// applied the applied log id's term equals the current term. `CommittedLeaderId`
/// carries only the term in this feature configuration, which is why this
/// compares terms rather than leader identities — and it is sufficient, because
/// two leaders cannot share a term.
///
/// Without this a node reports `Ready` while its state machine still reflects
/// the previous term, and a read taken then is answered from state the new
/// leader has not yet caught up to.
fn applied_through_the_leaders_blank_entry(metrics: &Metrics) -> bool {
    metrics
        .last_applied
        .as_ref()
        .is_some_and(|id| id.leader_id.term == metrics.current_term)
}

#[cfg(test)]
pub(crate) mod testing {
    //! Synthesizing metrics snapshots.
    //!
    //! The lifecycle rule is a function of a snapshot, so the honest way to
    //! test every branch is to build the snapshots. What this **cannot**
    //! establish is that OpenRaft produces the snapshot a real partition would
    //! — that is the real-Raft partition test in the gate suite, and these
    //! cases are the exhaustive table beneath it.

    use super::*;
    use openraft::{Membership, StoredMembership};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    pub(crate) fn node(n: u8) -> CoordinationNodeId {
        CoordinationNodeId::from_array([n; 32])
    }

    /// A committed membership with `voters` (possibly joint) and `learners`.
    pub(crate) fn membership(
        configs: &[&[u8]],
        learners: &[u8],
    ) -> Arc<StoredMembership<CoordinationNodeId, CoordinationNode>> {
        let configs: Vec<BTreeSet<CoordinationNodeId>> = configs
            .iter()
            .map(|c| c.iter().map(|n| node(*n)).collect())
            .collect();
        let mut nodes: BTreeMap<CoordinationNodeId, CoordinationNode> = configs
            .iter()
            .flatten()
            .map(|id| (*id, CoordinationNode))
            .collect();
        for l in learners {
            nodes.insert(node(*l), CoordinationNode);
        }
        Arc::new(StoredMembership::new(
            Some(openraft::LogId::new(
                openraft::CommittedLeaderId::new(1, node(0)),
                1,
            )),
            Membership::new(configs, nodes),
        ))
    }

    /// A snapshot with the fields this module reads, and defaults elsewhere.
    pub(crate) fn metrics(
        id: u8,
        state: ServerState,
        term: u64,
        applied_term: Option<u64>,
        millis_since_quorum_ack: Option<u64>,
        membership: Arc<StoredMembership<CoordinationNodeId, CoordinationNode>>,
    ) -> Metrics {
        Metrics {
            running_state: Ok(()),
            id: node(id),
            current_term: term,
            vote: openraft::Vote::new(term, node(id)),
            last_log_index: Some(7),
            last_applied: applied_term
                .map(|t| openraft::LogId::new(openraft::CommittedLeaderId::new(t, node(0)), 7)),
            snapshot: None,
            purged: None,
            state,
            current_leader: Some(node(1)),
            millis_since_quorum_ack,
            membership_config: membership,
            replication: None,
        }
    }

    /// Local facts whose **committed** membership is taken from `metrics`.
    ///
    /// For the cases that are about `local_role`: production reads the role
    /// from the state actor's committed membership, so a fixture that left the
    /// two unrelated would be asserting about a membership the classifier never
    /// sees. Everything else uses [`local`], whose default already makes this
    /// node a voter.
    pub(crate) fn local_of(metrics: &Metrics, corridor: Corridor) -> LocalFacts {
        LocalFacts {
            committed: Arc::clone(&metrics.membership_config),
            ..local(corridor)
        }
    }

    pub(crate) fn local(corridor: Corridor) -> LocalFacts {
        LocalFacts {
            // A settled group in which this node — id 1 — is a voter, which is
            // what every case that is not about the role already assumed when
            // this was read from the metrics fixture.
            committed: membership(&[&[1, 2, 3]], &[]),
            corridor,
            draining: false,
            fatal: None,
            follower_contact_age: None,
            leader_liveness_timeout: Duration::from_millis(3_000),
            // The fixture's default is *installed*, so every existing case
            // keeps testing what it was written to test. The genesis condition
            // has its own cases, which set it false deliberately.
            genesis_installed: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const PROD: &[&[u8]] = &[&[1, 2, 3]];
    const JOINT: &[&[u8]] = &[&[1, 2, 3], &[3, 4, 5]];

    /// A healthy leader that has been acknowledged and has applied its own
    /// blank entry.
    fn healthy_leader() -> Metrics {
        metrics(
            1,
            ServerState::Leader,
            5,
            Some(5),
            Some(100),
            membership(PROD, &[]),
        )
    }

    #[test]
    fn a_quorum_acknowledged_leader_that_has_applied_its_blank_entry_is_ready() {
        let view = classify(&healthy_leader(), &local(Corridor::Running));
        assert_eq!(view.code, LifecycleCode::Ready);
        assert_eq!(view.local_role, LocalRole::Voter);
        assert_eq!(view.quorum_available, Some(true));
        assert_eq!(view.fatal, None);
    }

    /// §7.6's isolated leader. `millis_since_quorum_ack` stops advancing, so
    /// the node that *is* leader detects its own partition.
    ///
    /// The boundary is exercised in both directions: exactly at the timeout is
    /// fresh, one millisecond past it is not.
    #[test]
    fn an_isolated_leader_reports_no_quorum_once_the_timeout_passes() {
        let facts = local(Corridor::Running);
        let timeout = facts.leader_liveness_timeout.as_millis() as u64;

        let mut at_boundary = healthy_leader();
        at_boundary.millis_since_quorum_ack = Some(timeout);
        assert_eq!(
            classify(&at_boundary, &facts).code,
            LifecycleCode::Ready,
            "exactly at the timeout is still fresh"
        );

        let mut past = healthy_leader();
        past.millis_since_quorum_ack = Some(timeout + 1);
        let view = classify(&past, &facts);
        assert_eq!(view.code, LifecycleCode::NoQuorum);
        assert_eq!(
            view.quorum_available,
            Some(false),
            "a stale leader still holds evidence, and the evidence says no"
        );
    }

    /// A newly elected leader has `None` for the metric until its first ack.
    /// `None` is not fresh, and the reported evidence is `Some(false)` rather
    /// than `None`: "not yet acknowledged" is an answer.
    #[test]
    fn a_newly_elected_leader_is_not_ready_before_its_first_quorum_ack() {
        let mut fresh_leader = healthy_leader();
        fresh_leader.millis_since_quorum_ack = None;
        let view = classify(&fresh_leader, &local(Corridor::Running));
        assert_eq!(view.code, LifecycleCode::NoQuorum);
        assert_eq!(view.quorum_available, Some(false));
    }

    /// A leader in a joint configuration is classified from OpenRaft's own
    /// quorum-set-aware metric, so this node reports what the library computed
    /// rather than what a voter-union majority would suggest.
    ///
    /// The independent oracle is the union rule itself, computed here: under
    /// `[{1,2,3},{3,4,5}]` a leader reaching `{1,2,3}` has 3 of the 5-node
    /// union — a clear majority — while the joint quorum is not met. The
    /// classification must follow the metric and report `NoQuorum`.
    #[test]
    fn a_joint_configuration_is_judged_by_the_metric_not_by_the_voter_union() {
        let facts = local(Corridor::Running);
        let reached: std::collections::BTreeSet<u8> = [1, 2, 3].into_iter().collect();
        let union: std::collections::BTreeSet<u8> = [1, 2, 3, 4, 5].into_iter().collect();
        assert!(
            reached.len() * 2 > union.len(),
            "the union rule would call this healthy, which is the whole hazard"
        );

        // OpenRaft's metric, computed through the joint quorum set, stops
        // advancing. The classification follows it.
        let mut joint = metrics(
            1,
            ServerState::Leader,
            5,
            Some(5),
            None,
            membership(JOINT, &[]),
        );
        assert_eq!(
            classify(&joint, &facts).code,
            LifecycleCode::NoQuorum,
            "a joint leader with no quorum ack is not ready"
        );

        // And once both configs acknowledge, the same node is ready — so the
        // test is not merely asserting that joint membership is always unready.
        joint.millis_since_quorum_ack = Some(50);
        assert_eq!(classify(&joint, &facts).code, LifecycleCode::Ready);
    }

    /// A follower reports `quorum_available: None` in every state, because it
    /// holds no direct evidence — including when it is perfectly healthy.
    #[test]
    fn a_follower_never_claims_quorum_evidence() {
        let mut facts = local(Corridor::Running);
        facts.follower_contact_age = Some(Duration::from_millis(10));
        let follower = metrics(
            2,
            ServerState::Follower,
            5,
            Some(5),
            None,
            membership(PROD, &[]),
        );
        let view = classify(&follower, &facts);
        assert_eq!(view.code, LifecycleCode::Ready, "advisory readiness");
        assert_eq!(
            view.quorum_available, None,
            "a follower has no direct evidence either way"
        );
    }

    /// §7.6's 2/3 partition, and the honest statement of what v1 does about it.
    ///
    /// `{old leader, follower}` versus `{three voters}`: the minority leader
    /// keeps appending, so the follower's contact never goes stale and it
    /// reports `Ready`. That is **advisory** — the follower's `Ready` is a
    /// claim about recent leader contact, not about quorum, and it reports
    /// `quorum_available: None` rather than `Some(true)`.
    ///
    /// What makes it safe is not asserted here, because it cannot be: the
    /// property is that no authoritative operation completes through a
    /// follower, which is a fact about the Store service and is exercised by
    /// the real-cluster gate. This case pins the classification and the honest
    /// `None`; it does not claim to establish safety.
    #[test]
    fn a_stranded_follower_reports_ready_advisorily_and_claims_no_quorum() {
        let mut facts = local(Corridor::Running);
        facts.follower_contact_age = Some(Duration::from_millis(5));
        let stranded = metrics(
            2,
            ServerState::Follower,
            5,
            Some(5),
            None,
            membership(&[&[1, 2, 3, 4, 5]], &[]),
        );
        let view = classify(&stranded, &facts);
        assert_eq!(view.code, LifecycleCode::Ready);
        assert_eq!(view.quorum_available, None);

        // The minority *leader* in the same partition does detect it.
        let minority_leader = metrics(
            1,
            ServerState::Leader,
            5,
            Some(5),
            Some(60_000),
            membership(&[&[1, 2, 3, 4, 5]], &[]),
        );
        let leader_view = classify(&minority_leader, &facts);
        assert_eq!(leader_view.code, LifecycleCode::NoQuorum);
        assert_eq!(leader_view.quorum_available, Some(false));
    }

    /// A follower that has never heard from a leader, and one whose contact has
    /// gone stale, are both `NoQuorum`.
    #[test]
    fn a_follower_with_no_or_stale_contact_is_not_ready() {
        let follower = metrics(
            2,
            ServerState::Follower,
            5,
            Some(5),
            None,
            membership(PROD, &[]),
        );

        let never = local(Corridor::Running);
        assert_eq!(classify(&follower, &never).code, LifecycleCode::NoQuorum);

        let mut stale = local(Corridor::Running);
        stale.follower_contact_age = Some(Duration::from_millis(3_001));
        assert_eq!(classify(&follower, &stale).code, LifecycleCode::NoQuorum);

        let mut boundary = local(Corridor::Running);
        boundary.follower_contact_age = Some(Duration::from_millis(3_000));
        assert_eq!(classify(&follower, &boundary).code, LifecycleCode::Ready);
    }

    /// The third `Ready` fact on its own: a node whose applied entry is from
    /// the previous term is not ready, however fresh its leader observation.
    #[test]
    fn a_node_that_has_not_applied_the_leaders_blank_entry_is_not_ready() {
        let mut behind = healthy_leader();
        behind.last_applied = Some(openraft::LogId::new(
            openraft::CommittedLeaderId::new(4, node(0)),
            7,
        ));
        assert_eq!(
            classify(&behind, &local(Corridor::Running)).code,
            LifecycleCode::NoQuorum
        );

        let mut nothing_applied = healthy_leader();
        nothing_applied.last_applied = None;
        assert_eq!(
            classify(&nothing_applied, &local(Corridor::Running)).code,
            LifecycleCode::NoQuorum
        );
    }

    /// A learner is never `Ready`, whatever its contact looks like: the first
    /// `Ready` fact is that the committed membership makes this node a voter.
    #[test]
    fn a_learner_is_not_ready_and_reports_its_role() {
        let learner = metrics(
            9,
            ServerState::Learner,
            5,
            Some(5),
            None,
            membership(PROD, &[9]),
        );
        // The role comes from the **committed** membership, so the facts carry
        // it: a fixture whose facts and metrics named different memberships
        // would be asserting about one the classifier never reads.
        let mut facts = local_of(&learner, Corridor::Running);
        facts.follower_contact_age = Some(Duration::from_millis(1));
        let view = classify(&learner, &facts);
        assert_eq!(view.local_role, LocalRole::Learner);
        assert_eq!(view.code, LifecycleCode::NoQuorum);

        // And a node the membership does not name at all.
        let stranger = metrics(
            9,
            ServerState::Follower,
            5,
            Some(5),
            None,
            membership(PROD, &[]),
        );
        let mut stranger_facts = local_of(&stranger, Corridor::Running);
        stranger_facts.follower_contact_age = Some(Duration::from_millis(1));
        assert_eq!(
            classify(&stranger, &stranger_facts).local_role,
            LocalRole::None
        );
    }

    /// Precedence is strict, and each level is asserted to beat the one below
    /// it while the lower condition is simultaneously true.
    ///
    /// A table that only checked each state in isolation would pass with the
    /// order reversed — which is the whole thing precedence fixes.
    #[test]
    fn precedence_is_strict_from_fatal_down_to_ready() {
        // Start from a snapshot that would otherwise be `Ready`, so every case
        // below is a genuine override rather than a state reached by default.
        let healthy = healthy_leader();

        let mut draining = local(Corridor::Running);
        draining.draining = true;
        assert_eq!(classify(&healthy, &draining).code, LifecycleCode::Draining);

        let mut fatal_and_draining = draining.clone();
        fatal_and_draining.fatal = Some(FatalCause::Storage("disk".into()));
        assert_eq!(
            classify(&healthy, &fatal_and_draining).code,
            LifecycleCode::Fatal,
            "Fatal beats Draining"
        );

        let mut joining = local(Corridor::Joining);
        assert_eq!(classify(&healthy, &joining).code, LifecycleCode::Joining);
        joining.draining = true;
        assert_eq!(
            classify(&healthy, &joining).code,
            LifecycleCode::Draining,
            "Draining beats Joining"
        );

        let bootstrapping = local(Corridor::Bootstrapping);
        assert_eq!(
            classify(&healthy, &bootstrapping).code,
            LifecycleCode::Bootstrapping
        );

        // Joining beats Bootstrapping: they are mutually exclusive corridors in
        // practice, but the ordering is declared, so it is pinned.
        assert_eq!(
            classify(&healthy, &local(Corridor::Joining)).code,
            LifecycleCode::Joining
        );

        // And NoQuorum beats Ready, which is the ordering that matters most.
        let mut stale = healthy_leader();
        stale.millis_since_quorum_ack = Some(999_999);
        assert_eq!(
            classify(&stale, &local(Corridor::Running)).code,
            LifecycleCode::NoQuorum
        );
    }

    /// §7.6: "membership does not yet contain self" is `Joining`, not
    /// `NoQuorum`. The distinction is not cosmetic — an operator seeing
    /// `NoQuorum` looks for a partition, and the group is healthy.
    #[test]
    fn a_joining_node_reports_joining_even_while_the_group_is_healthy() {
        let candidate = metrics(
            9,
            ServerState::Learner,
            5,
            Some(5),
            None,
            membership(PROD, &[9]),
        );
        let mut facts = local_of(&candidate, Corridor::Joining);
        facts.follower_contact_age = Some(Duration::from_millis(1));
        let view = classify(&candidate, &facts);
        assert_eq!(view.code, LifecycleCode::Joining);
        assert_eq!(view.local_role, LocalRole::Learner);
    }

    /// OpenRaft's own fatal state produces `Fatal` without a local latch — the
    /// state-machine worker can die without storage noticing.
    #[test]
    fn openrafts_own_fatal_state_is_fatal_here_too() {
        let mut dead = healthy_leader();
        dead.running_state = Err(openraft::error::Fatal::Stopped);
        let view = classify(&dead, &local(Corridor::Running));
        assert_eq!(view.code, LifecycleCode::Fatal);
        assert!(matches!(view.fatal, Some(FatalCause::Raft(_))));
    }

    /// A `Fatal` view always carries its cause, and no other view does. An
    /// operator paged for `Fatal` needs to know which of the four it is.
    #[test]
    fn a_cause_accompanies_fatal_and_only_fatal() {
        let causes = [
            FatalCause::Storage("out of disk".into()),
            FatalCause::ProfileViolation {
                profile: 1,
                voters: 4,
            },
            FatalCause::IdentityMismatch("group id".into()),
            FatalCause::Raft("stopped".into()),
        ];
        for cause in causes {
            let mut facts = local(Corridor::Running);
            facts.fatal = Some(cause.clone());
            let view = classify(&healthy_leader(), &facts);
            assert_eq!(view.code, LifecycleCode::Fatal);
            assert_eq!(view.fatal, Some(cause.clone()));
            assert!(!cause.to_string().is_empty());
        }

        assert_eq!(
            classify(&healthy_leader(), &local(Corridor::Running)).fatal,
            None
        );
    }

    /// A leader's reported evidence and its own freshness are one rule, not
    /// two.
    ///
    /// The defect this pins: reporting `Some(true)` merely because an
    /// acknowledgement exists makes an isolated leader — the case the field is
    /// for — claim a reachable quorum from a ten-minute-old ack while
    /// simultaneously classifying itself `NoQuorum`. The two answers must agree
    /// across the whole range, so the check sweeps it rather than sampling.
    #[test]
    fn a_leaders_reported_evidence_agrees_with_its_own_freshness() {
        let facts = local(Corridor::Running);
        let timeout = facts.leader_liveness_timeout.as_millis() as u64;
        for ms in [
            None,
            Some(0),
            Some(1),
            Some(timeout - 1),
            Some(timeout),
            Some(timeout + 1),
            Some(600_000),
        ] {
            let mut leader = healthy_leader();
            leader.millis_since_quorum_ack = ms;
            let view = classify(&leader, &facts);
            assert_eq!(
                view.quorum_available,
                Some(view.code == LifecycleCode::Ready),
                "a leader at {ms:?} ms reported {:?} while classifying as {:?}",
                view.quorum_available,
                view.code
            );
        }
    }

    /// The wire codes are the §5.2 numbers, asserted as literals.
    #[test]
    fn the_wire_codes_are_the_numbers_section_five_two_declares() {
        assert_eq!(LifecycleCode::Bootstrapping.code(), 1);
        assert_eq!(LifecycleCode::Joining.code(), 2);
        assert_eq!(LifecycleCode::Ready.code(), 3);
        assert_eq!(LifecycleCode::NoQuorum.code(), 4);
        assert_eq!(LifecycleCode::Draining.code(), 5);
        assert_eq!(LifecycleCode::Fatal.code(), 6);
        assert_eq!(LifecycleCode::ALL.len(), 6);

        assert_eq!(LocalRole::None.code(), 0);
        assert_eq!(LocalRole::Learner.code(), 1);
        assert_eq!(LocalRole::Voter.code(), 2);
    }

    /// Every declared lifecycle code is actually produced by the table.
    ///
    /// The instrument against a code that exists on the wire and can never be
    /// reported — which is indistinguishable, from outside, from one the
    /// classifier forgot.
    #[test]
    fn every_declared_code_is_reachable_from_the_table() {
        let healthy = healthy_leader();
        let mut stale = healthy_leader();
        stale.millis_since_quorum_ack = None;

        let mut draining = local(Corridor::Running);
        draining.draining = true;
        let mut fatal = local(Corridor::Running);
        fatal.fatal = Some(FatalCause::Storage("x".into()));

        let produced: std::collections::BTreeSet<LifecycleCode> = [
            classify(&healthy, &local(Corridor::Bootstrapping)).code,
            classify(&healthy, &local(Corridor::Joining)).code,
            classify(&healthy, &local(Corridor::Running)).code,
            classify(&stale, &local(Corridor::Running)).code,
            classify(&healthy, &draining).code,
            classify(&healthy, &fatal).code,
        ]
        .into_iter()
        .collect();

        let declared: std::collections::BTreeSet<LifecycleCode> =
            LifecycleCode::ALL.iter().copied().collect();
        assert_eq!(produced, declared, "every declared code must be reachable");
    }

    /// `Raft::current_leader()` is not an input: a metrics snapshot whose
    /// `current_leader` says one thing and whose freshness says another is
    /// classified by the freshness.
    ///
    /// This is the isolated-leader hazard in its purest form — `current_leader`
    /// still names this node, which is exactly what makes it useless here.
    #[test]
    fn the_reported_leader_is_not_an_input_to_readiness() {
        let mut isolated = healthy_leader();
        isolated.current_leader = Some(node(1));
        isolated.millis_since_quorum_ack = Some(600_000);
        assert_eq!(
            classify(&isolated, &local(Corridor::Running)).code,
            LifecycleCode::NoQuorum,
            "current_leader still names this node; freshness is what decides"
        );

        // And the converse: a node with no reported leader but a fresh quorum
        // ack is still ready, so the field is not consulted in either direction.
        let mut leaderless_but_acked = healthy_leader();
        leaderless_but_acked.current_leader = None;
        assert_eq!(
            classify(&leaderless_but_acked, &local(Corridor::Running)).code,
            LifecycleCode::Ready
        );
    }
}
