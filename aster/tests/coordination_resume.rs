//! §S3-F / `S3-JOINT-RESUME`: **forced joint leader loss at each of the four
//! resume boundaries**, against a real group.
//!
//! ## What this adds to the unit table
//!
//! `runtime::membership`'s `a_new_generation_resumes_the_joint_corridor_from_observation`
//! already proves that `next_step` reaches the right step from each of the four
//! observed states. That is the *table*. It says nothing about whether a real
//! group ever reaches those states, nor about whether a new leader generation
//! actually drives the change to completion from one — a pure function cannot
//! lose leadership.
//!
//! ## Why the loss is placed rather than waited for
//!
//! A schedule that drops enough traffic will eventually move leadership
//! somewhere in the corridor, and a gate built on that proves whatever the run
//! happened to do. §S3-F asks for the loss at **each** of the four boundaries,
//! individually, and a run that hit one says nothing about the other three.
//!
//! So each gate here watches for its own boundary and, the moment it sees it,
//! cuts the leader off from every peer — an instruction to the injector, not a
//! draw from the seed. Two properties make that a *placed* loss rather than a
//! hopeful one:
//!
//! - once isolated, the deposed leader cannot commit anything more, so its own
//!   view is frozen at the state it held when leadership was lost; and
//! - the gate re-reads that frozen view and **asserts** it is the intended
//!   boundary. A gate that raced past its boundary fails, rather than quietly
//!   proving something about a different one.
//!
//! ## Why one voter departs without being retained
//!
//! Five voters shrink to three; one of the two departing is kept as a learner
//! and the other **leaves the group entirely**. Without that last one, the node
//! map after the uniform voter entry would already equal `desired ∪ retained`,
//! and boundaries 3 and 4 — "before the node-map replacement" and "after it" —
//! would be the same observable state. §7.3 makes the voter transition and
//! `ReplaceAllNodes` two operations precisely because they are not one, and a
//! gate that could not tell them apart could not prove that.
//!
//! Which voter plays which part is decided once the leader is known; see
//! [`Shape`].

#![cfg(feature = "coordination-testing")]

use std::collections::BTreeSet;
use std::time::Duration;

use aster::coordination::contract::{
    ActionV1, ChangeMembershipActionV1, ClientCommandV1, CommandReplyV1, CommandV1, MemberSeedV1,
    NodeIdV1,
};
use aster::coordination::testing::Schedule;

mod common;
use common::{
    await_leader, await_leader_other_than, combined_tally, every_core_is_alive, isolate_everywhere,
    rejoin_everywhere, shutdown_all, ChaosFixture, ChaoticVoter,
};

const DOMAIN: &str = "resume.test";

/// §5.2's `MembershipAccepted`.
const MEMBERSHIP_ACCEPTED: i32 = 8;

/// Which voter plays which part in the plan.
///
/// Chosen **around the observed leader**, so the leader is always one of the
/// three voters the change keeps. That is setup, not the property: this gate
/// places its own leader loss, and a plan that removed the sitting leader would
/// cause an *unplaced* one first — OpenRaft steps a removed leader down as soon
/// as its own removal commits, and the corridor's next state then passes inside
/// a leaderless gap that no watcher can sample. Three of these gates failed
/// that way before the shape followed the leader.
///
/// Moving leadership instead does not work: `trigger().elect()` on a follower
/// is refused while the sitting leader's lease is live, so the election simply
/// never happens.
struct Shape {
    /// The three voters that remain, the leader among them.
    desired: Vec<usize>,
    /// The one departing voter the plan keeps as a learner.
    retained: Vec<usize>,
    /// The one voter that leaves the group entirely, so the node map after the
    /// uniform entry differs from the final one — without which boundaries 3
    /// and 4 would be the same observable state.
    departing: usize,
}

impl Shape {
    fn around(leader: usize, size: usize) -> Self {
        let others: Vec<usize> = (0..size).filter(|i| *i != leader).collect();
        assert_eq!(others.len(), 4, "this shape is a five-voter group");
        Self {
            desired: vec![leader, others[0], others[1]],
            retained: vec![others[2]],
            departing: others[3],
        }
    }

    fn final_nodes(&self) -> Vec<usize> {
        self.desired.iter().chain(&self.retained).copied().collect()
    }
}

const PATIENCE: Duration = Duration::from_secs(90);

/// How often the boundary watcher looks.
///
/// Every boundary here is separated from the next by at least one quorum round
/// trip — a membership entry has to be replicated and acknowledged before the
/// controller can take the following step — so a millisecond sample cannot step
/// over one on loopback. The assertion that the boundary was actually reached
/// is what makes that a claim rather than an assumption.
const WATCH: Duration = Duration::from_millis(1);

/// The four states `S3-JOINT-RESUME` names, in the order the corridor passes
/// through them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boundary {
    /// The intent is durable and no joint entry has committed: the membership
    /// is still the original uniform five.
    BeforeJoint,
    /// The joint entry has committed; the uniform one has not.
    AfterJoint,
    /// The uniform voter entry has committed; the node map still names the
    /// departing voter.
    AfterUniform,
    /// The node map has been replaced; the intent has not yet been completed.
    AfterReplacement,
}

/// One voter's reading of the corridor, from the **committed** membership.
///
/// Committed rather than effective, and the difference is the whole gate: the
/// interval between "the uniform voter entry committed" and "the node map was
/// replaced" does not exist in the effective membership, because the
/// `ReplaceAllNodes` entry is already effective before the uniform one has
/// committed. Written against the effective membership this gate watched for a
/// state it had made unobservable, and timed out having passed through
/// `3v/3n committed=false` straight to `3v/4n committed=true`.
#[derive(Debug)]
struct View {
    voters: BTreeSet<Vec<u8>>,
    nodes: BTreeSet<Vec<u8>>,
    joint: bool,
    intent_active: bool,
}

/// Everything but the intent.
///
/// Split out because the watcher samples at [`WATCH`] and the intent is the one
/// field that costs a round trip to the state actor. The membership does not:
/// the actor publishes it through a watch channel, so reading it is a borrow.
/// Asking for the *intent* a thousand times a second would put the harness's
/// own load on the actor that is applying the entries it is watching for, which
/// is a way to change the thing being measured.
fn metrics_view(voter: &ChaoticVoter, intent_active: bool) -> View {
    let coordination = voter.coordination();
    View {
        voters: coordination
            .committed_voters_for_testing()
            .into_iter()
            .map(|n| n.as_bytes().to_vec())
            .collect(),
        nodes: coordination
            .committed_nodes_for_testing()
            .into_iter()
            .map(|n| n.as_bytes().to_vec())
            .collect(),
        joint: coordination.committed_membership_is_joint_for_testing(),
        intent_active,
    }
}

async fn view_of(voter: &ChaoticVoter) -> View {
    let intent = voter
        .coordination()
        .membership_intent_active_for_testing()
        .await;
    metrics_view(voter, intent)
}

/// Whether `view` is `boundary`, given the group this plan runs against.
///
/// Written as one predicate over a full reading rather than as four ad-hoc
/// checks, so no boundary can be recognised by a condition another one also
/// satisfies — which is how a gate ends up placing every loss at the same
/// place while naming four.
fn is_boundary(view: &View, boundary: Boundary, fixture: &ChaosFixture, shape: &Shape) -> bool {
    let ids = |indices: &[usize]| -> BTreeSet<Vec<u8>> {
        indices.iter().map(|i| fixture.node_id(*i)).collect()
    };
    let everyone = ids(&(0..fixture.size()).collect::<Vec<_>>());
    let desired = ids(&shape.desired);
    let final_nodes = ids(&shape.final_nodes());

    match boundary {
        Boundary::BeforeJoint => view.intent_active && !view.joint && view.voters == everyone,
        Boundary::AfterJoint => view.intent_active && view.joint,
        Boundary::AfterUniform => {
            view.intent_active && !view.joint && view.voters == desired && view.nodes != final_nodes
        }
        Boundary::AfterReplacement => {
            view.intent_active && !view.joint && view.voters == desired && view.nodes == final_nodes
        }
    }
}

/// The plan: five voters down to three, one of the two departing kept as a
/// learner and the other removed outright.
fn plan(
    fixture: &ChaosFixture,
    shape: &Shape,
    actor: usize,
    expected: Option<aster::coordination::contract::LogIdV1>,
) -> CommandV1 {
    let named = |indices: &[usize]| {
        let mut ids: Vec<NodeIdV1> = indices
            .iter()
            .map(|i| NodeIdV1 {
                node_id: fixture.node_id(*i),
            })
            .collect();
        // Sorted unique by node id, which is a public key: fixture order and
        // byte order are unrelated, and an unsorted plan is refused as
        // shapeless before any controller sees it.
        ids.sort_by(|l, r| l.node_id.cmp(&r.node_id));
        ids
    };

    let action = ActionV1::ChangeMembership(ChangeMembershipActionV1 {
        // Fixed, so a plan that committed while its reply was dropped is the
        // *same* intent when it arrives again rather than a second one racing
        // the first.
        operation_id: aster::coordination::testing::id16(b"resume-shrink"),
        expected_membership_log_id: expected,
        desired_voters: named(&shape.desired),
        retained_learners: named(&shape.retained),
        // Every node named is already a member, so none needs a route seed.
        member_seeds: Vec::<MemberSeedV1>::new(),
    });
    CommandV1::Client(ClientCommandV1 {
        actor_node_id: fixture.node_id(actor),
        client_id: aster::coordination::testing::id16(b"resume-admin"),
        sequence: 1,
        request_hash: aster::coordination::hash::request_hash(&action)
            .expect("a canonical action hashes")
            .to_vec(),
        action,
    })
}

fn outcome_of(committed: &aster::coordination::testing::Committed) -> (i32, i32) {
    match &committed.reply {
        CommandReplyV1::Client(reply) => (reply.outcome_code, reply.reason_code),
        other => panic!("a membership plan must answer with a Client reply, got {other:?}"),
    }
}

/// Submit the plan until a leader accepts it.
///
/// Rebuilt on every attempt because two of its inputs go stale:
/// `expected_membership_log_id` must be the membership the group currently has,
/// and the actor must be a voter that is currently leading. A `None`
/// expectation is not a wildcard — it is a mismatch.
async fn submit_plan(voters: &[ChaoticVoter], fixture: &ChaosFixture, shape: &Shape) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let at = await_leader(voters, PATIENCE, "a leader to accept the membership plan").await;
        let coordination = voters[at].coordination();
        let expected = coordination.membership_log_id_for_testing();
        match coordination
            .propose_for_testing_within(plan(fixture, shape, at, expected), Duration::from_secs(20))
            .await
        {
            Ok(committed) if outcome_of(&committed) == (MEMBERSHIP_ACCEPTED, 0) => return,
            // Leadership or the membership log id can move between reading and
            // committing; the honest response is to re-read both and ask again.
            // Never being accepted at all is what the deadline catches.
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the membership plan was never accepted within {PATIENCE:?}; faults {:?}",
            combined_tally(voters)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Watch until some voter reports `boundary`, cut that group's **leader** off,
/// and return which voter was cut.
///
/// The cut is what freezes the evidence: an isolated leader has no quorum, so
/// it can commit nothing further and the view it holds is the view it held when
/// leadership was lost. The caller asserts against that frozen view.
async fn lose_the_leader_at(
    voters: &[ChaoticVoter],
    fixture: &ChaosFixture,
    shape: &Shape,
    boundary: Boundary,
) -> usize {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    let mut seen: Vec<String> = Vec::new();
    loop {
        for (index, voter) in voters.iter().enumerate() {
            let coordination = voter.coordination();
            // Only the leader's own reading is worth acting on: it is the node
            // whose loss is being placed, and the only one whose view freezes
            // when it is cut.
            if coordination.lifecycle_code() != 3 || coordination.quorum_available() != Some(true) {
                continue;
            }
            // The cheap half first, assuming the intent is still open. Every
            // boundary requires that, so a reading the metrics already rule out
            // is ruled out without touching the state actor; only a candidate
            // pays for the confirmation.
            let view = metrics_view(voter, true);
            if is_boundary(&view, boundary, fixture, shape)
                && coordination.membership_intent_active_for_testing().await
            {
                isolate_everywhere(voters, voter.id);
                return index;
            }
            let sketch = format!(
                "{}v/{}n joint={} intent={}",
                view.voters.len(),
                view.nodes.len(),
                view.joint,
                view.intent_active
            );
            if seen.last() != Some(&sketch) {
                seen.push(sketch);
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the corridor never reached {boundary:?} within {PATIENCE:?}; the leader \
             passed through {seen:?}; faults {:?}",
            combined_tally(voters)
        );
        tokio::time::sleep(WATCH).await;
    }
}

/// Wait for the change to finish, asked of every surviving voter.
///
/// Completion is read from the **committed membership and the durable intent**,
/// not from a controller's reply: an accepted plan says the controller was
/// asked, and the whole question here is whether a *new* generation finished
/// what an old one started.
async fn await_settled(
    voters: &[ChaoticVoter],
    fixture: &ChaosFixture,
    shape: &Shape,
    excluding: &[usize],
) {
    let want_voters: BTreeSet<Vec<u8>> =
        shape.desired.iter().map(|i| fixture.node_id(*i)).collect();
    let want_nodes: BTreeSet<Vec<u8>> = shape
        .final_nodes()
        .iter()
        .map(|i| fixture.node_id(*i))
        .collect();

    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let mut observed = Vec::new();
        for (index, voter) in voters.iter().enumerate() {
            if excluding.contains(&index) {
                continue;
            }
            let view = view_of(voter).await;
            if view.voters == want_voters
                && view.nodes == want_nodes
                && !view.joint
                && !view.intent_active
            {
                return;
            }
            let coordination = voter.coordination();
            observed.push(format!(
                "v{index}: {}v/{}n joint={} intent={} lifecycle={} quorum={:?}",
                view.voters.len(),
                view.nodes.len(),
                view.joint,
                view.intent_active,
                coordination.lifecycle_code(),
                coordination.quorum_available(),
            ));
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the membership change never settled after the loss, within {PATIENCE:?}. \
             Wanted {}v/{}n non-joint with the intent closed; the survivors are \
             {observed:?}; faults {:?}",
            want_voters.len(),
            want_nodes.len(),
            combined_tally(voters)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One boundary, end to end.
///
/// The schedule delays every message and loses none, which is not the same as
/// a faulty network and is chosen for a specific reason. These gates are about
/// a leader loss placed at a chosen point, so a drop rate would add a second,
/// *unplaced* source of leadership churn and make a failure ambiguous between
/// the placed loss and the schedule's. But a perfect network is too fast to
/// watch: on loopback the interval between the uniform voter entry committing
/// and `ReplaceAllNodes` committing is under a millisecond, and the
/// `AfterUniform` gate timed out claiming the corridor never reached a state it
/// had passed straight through — committed went `joint` to `3v/4n` with the
/// `3v/3n` in between never sampled. Stretching every round trip makes the
/// state observable without making the group unstable.
///
/// `coordination_chaos` drives the same controller through a faulty network;
/// this drives it through a chosen failure over a slow one.
async fn a_leader_lost_at(boundary: Boundary, seed: u8) {
    let fixture = ChaosFixture::new(DOMAIN, seed, 5);
    let voters = fixture.start(Schedule::slow(u64::from(seed))).await;

    let elected = await_leader(&voters, PATIENCE, "the election before a membership change").await;

    // The plan is built **around the leader**, which keeps it. See [`Shape`]:
    // a plan that removed the sitting leader would cause an unplaced leadership
    // change before this gate could place its own.
    let shape = Shape::around(elected, fixture.size());
    submit_plan(&voters, &fixture, &shape).await;

    let lost = lose_the_leader_at(&voters, &fixture, &shape, boundary).await;
    let deposed = voters[lost].id;

    // The evidence, read from the frozen view of the voter that was cut. An
    // isolated leader commits nothing further, so this is what it held at the
    // moment its leadership ended.
    let frozen = view_of(&voters[lost]).await;
    assert!(
        is_boundary(&frozen, boundary, &fixture, &shape),
        "the loss was meant to be placed at {boundary:?}, and the deposed leader's \
         frozen view is {frozen:?}"
    );

    // A new generation takes over and finishes from durable intent plus
    // observation — the whole claim of `S3-JOINT-RESUME`.
    let successor = await_leader_other_than(
        &voters,
        &[deposed],
        PATIENCE,
        "a new leader generation after the placed loss",
    )
    .await;
    assert_ne!(voters[successor].id, deposed);

    // **The loss is transient**, and it has to be. The plan keeps the deposed
    // voter, and §7.3 will not promote a desired voter it cannot replicate to:
    // the catch-up barrier waits for every desired voter to reach the index
    // observed when the promotion step began, and a voter that is cut off never
    // reaches it. A permanently isolated member is therefore a change that
    // legitimately never completes — the `BeforeJoint` gate sat at `5v/5n` with
    // a healthy leader and an open intent for the full ninety seconds, which is
    // the controller being right rather than stuck.
    //
    // So the loss ends here, after the evidence has been read and a successor
    // has been observed. What is being proved is that a *new generation*
    // resumed the corridor from durable intent and observation; that the
    // deposed node came back is what makes the group able to finish at all.
    rejoin_everywhere(&voters, &deposed);
    await_settled(&voters, &fixture, &shape, &[]).await;

    // And the deposed voter is carried along: if it is still a member it must
    // catch up on everything committed while it was away.
    if lost != shape.departing {
        let want = voters[successor]
            .coordination()
            .last_applied_for_testing()
            .expect("the successor has applied entries");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let caught_up = voters[lost].coordination().last_applied_for_testing();
            if caught_up >= Some(want) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the deposed voter never caught up after rejoining: applied \
                 {caught_up:?} against {want}; faults {:?}",
                combined_tally(&voters)
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    every_core_is_alive(&voters, &format!("after the loss at {boundary:?}")).await;
    shutdown_all(voters).await;
}

/// **Boundary 1: lost before the joint entry committed.**
///
/// The membership is still the original uniform five and the intent is durable.
/// A new generation must restart the corridor from the same desired change —
/// not assume the joint entry landed, and not treat the plan as finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_new_generation_finishes_a_change_lost_before_the_joint_entry() {
    a_leader_lost_at(Boundary::BeforeJoint, 0x31).await;
}

/// **Boundary 2: lost after the joint entry committed.**
///
/// The group is joint, which is the state OpenRaft's own `change_membership`
/// documents as what a crash between its two steps leaves behind. A new
/// generation must repeat the desired change so the coherent next step emits
/// the uniform configuration — a corridor that assumed uniform here would
/// report a completion the group never reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_new_generation_finishes_a_change_lost_after_the_joint_entry() {
    a_leader_lost_at(Boundary::AfterJoint, 0x32).await;
}

/// **Boundary 3: lost after the uniform entry committed, before the node map
/// was replaced.**
///
/// The voters are already right and the departing node is still in the node
/// map. Only step 5 is owed, and it is a separate one-entry operation — the
/// pair is not atomic, and a generation that treated it as one would either
/// redo the voter change or report completion against a node map it never
/// replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_new_generation_finishes_a_change_lost_after_the_uniform_entry() {
    a_leader_lost_at(Boundary::AfterUniform, 0x33).await;
}

/// **Boundary 4: lost after the node map was replaced, before completion was
/// recorded.**
///
/// Everything the plan asked for is committed and the intent is still open. A
/// new generation must report completion **from observation**; §S3-E's rule is
/// that completion is reported only against an exact, non-joint, role-correct
/// membership, and this is the one boundary where the only thing left to do is
/// say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_new_generation_finishes_a_change_lost_after_the_node_map_was_replaced() {
    a_leader_lost_at(Boundary::AfterReplacement, 0x34).await;
}

/// **A real OpenRaft answers `InProgress` while effective is ahead of
/// committed** — and that is what makes the corridor's "wait and retry" rule
/// work.
///
/// The controller reads the **committed** membership, so while a membership
/// entry is appended and not yet committed the table keeps asking for the same
/// voter change. That is only correct if OpenRaft refuses the repeat rather
/// than appending a second entry on top of the first. `classify_change`'s unit
/// table already maps a hand-built `ChangeMembershipError::InProgress` to
/// `ChangeOutcome::Retry`; what it cannot show is that a real OpenRaft on this
/// path produces one, and the existing "a second plan is refused" gate proves
/// something else entirely — Aster's own active-intent admission, which rejects
/// the *command* before OpenRaft is reached at all.
///
/// The state is built rather than waited for: three voters, two of them cut
/// off, so the leader can append a membership entry and can never commit it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn openraft_refuses_a_repeat_change_while_one_is_uncommitted() {
    use openraft::error::{ChangeMembershipError, ClientWriteError, RaftError};
    use openraft::ChangeMembers;

    let fixture = ChaosFixture::new(DOMAIN, 0x35, 3);
    let voters = fixture.start(Schedule::perfect()).await;
    let at = await_leader(&voters, PATIENCE, "the election before the corridor").await;

    // Strand the leader: it keeps leadership — nothing can reach it with a
    // higher term — but it cannot commit anything again.
    for index in 0..voters.len() {
        if index != at {
            isolate_everywhere(&voters, voters[index].id);
        }
    }

    let raft = voters[at].coordination();
    let raft = raft.raft_for_testing();
    let shrink: BTreeSet<_> = [voters[at].id].into_iter().collect();

    // The first call appends the entry and then waits for a commit that cannot
    // come. Abandoning the wait is the point: the entry is on this node's log,
    // so effective is now ahead of committed.
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            raft.change_membership(ChangeMembers::ReplaceAllVoters(shrink.clone()), false),
        )
        .await
        .is_err(),
        "a leader with no quorum must not be able to commit a membership change"
    );

    // The repeat the corridor would make. It must be refused as in progress,
    // not accepted as a second entry.
    let repeat = tokio::time::timeout(
        Duration::from_secs(5),
        raft.change_membership(ChangeMembers::ReplaceAllVoters(shrink), false),
    )
    .await
    .expect("a refused change answers at once rather than waiting for a quorum");

    match repeat {
        Err(RaftError::APIError(ClientWriteError::ChangeMembershipError(
            ChangeMembershipError::InProgress(in_progress),
        ))) => {
            // And it names the entry that is in the way, which is what makes
            // the retry a wait rather than a guess.
            println!("InProgress: {in_progress:?}");
        }
        other => panic!(
            "a repeat change while one is uncommitted must answer InProgress; it \
             answered {other:?}"
        ),
    }

    for index in 0..voters.len() {
        if index != at {
            rejoin_everywhere(&voters, &voters[index].id);
        }
    }
    shutdown_all(voters).await;
}

/// The four boundaries are **distinguishable**, so a gate cannot pass three of
/// them by recognising one.
///
/// The runtime half of the same guard the unit table carries: `is_boundary` is
/// the predicate every gate above places its loss with, and if two boundaries
/// accepted the same reading, two of those gates would silently be the same
/// test.
#[test]
fn no_two_boundaries_accept_the_same_reading() {
    let fixture = ChaosFixture::new(DOMAIN, 0x3F, 5);
    let shape = Shape::around(0, 5);
    let ids = |indices: &[usize]| -> BTreeSet<Vec<u8>> {
        indices.iter().map(|i| fixture.node_id(*i)).collect()
    };
    let readings = [
        (
            Boundary::BeforeJoint,
            View {
                voters: ids(&[0, 1, 2, 3, 4]),
                nodes: ids(&[0, 1, 2, 3, 4]),
                joint: false,
                intent_active: true,
            },
        ),
        (
            Boundary::AfterJoint,
            View {
                voters: ids(&[0, 1, 2, 3, 4]),
                nodes: ids(&[0, 1, 2, 3, 4]),
                joint: true,
                intent_active: true,
            },
        ),
        (
            Boundary::AfterUniform,
            View {
                voters: ids(&shape.desired),
                nodes: ids(&[0, 1, 2, 3, 4]),
                joint: false,
                intent_active: true,
            },
        ),
        (
            Boundary::AfterReplacement,
            View {
                voters: ids(&shape.desired),
                nodes: ids(&shape.final_nodes()),
                joint: false,
                intent_active: true,
            },
        ),
    ];

    for (expected, view) in &readings {
        let accepted: Vec<Boundary> = [
            Boundary::BeforeJoint,
            Boundary::AfterJoint,
            Boundary::AfterUniform,
            Boundary::AfterReplacement,
        ]
        .into_iter()
        .filter(|candidate| is_boundary(view, *candidate, &fixture, &shape))
        .collect();
        assert_eq!(
            accepted,
            vec![*expected],
            "the reading for {expected:?} must be recognised by that boundary and no \
             other; it was accepted by {accepted:?}"
        );
    }

    // An intent that is no longer active is none of them, whatever the shape:
    // every boundary is a state the corridor is *in*, and the corridor is over
    // once the intent reaches a terminal phase.
    for (_, view) in &readings {
        let finished = View {
            voters: view.voters.clone(),
            nodes: view.nodes.clone(),
            joint: view.joint,
            intent_active: false,
        };
        for candidate in [
            Boundary::BeforeJoint,
            Boundary::AfterJoint,
            Boundary::AfterUniform,
            Boundary::AfterReplacement,
        ] {
            assert!(
                !is_boundary(&finished, candidate, &fixture, &shape),
                "a finished intent is not {candidate:?}"
            );
        }
    }

    // "The joint entry was appended" is not a boundary at all here, and cannot
    // be: every reading comes from the committed membership, so an entry no
    // quorum has accepted is not in it. That is a property of where the gate
    // reads from rather than a case it has to exclude — and reading from the
    // effective membership instead is what made `AfterUniform` unobservable,
    // because the node-map entry is effective before the uniform one commits.
}
