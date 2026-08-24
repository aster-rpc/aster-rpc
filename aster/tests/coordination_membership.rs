//! **Membership, reconciled end to end by the real controller** (§7.3, §S3-E).
//!
//! ## What was and was not proven before
//!
//! Every decision the controller makes had a component test: the three
//! exhaustive error levels, the phase table, the barrier rule, the step-5
//! replacement, `MEMBERSHIP_RETAIN`. What none of them could show is that the
//! controller *runs* — that an accepted intent reaches it, that it drives
//! OpenRaft, and that the group ends in the shape the plan asked for. A phase
//! table is a pure function; it is right or wrong on its own, and it says
//! nothing about whether anything calls it.
//!
//! The intent is seeded the way the contract matrix says to seed it: through
//! the internal harness, as a real `ChangeMembership` command committed by a
//! real Raft. Not by writing to the state machine, and not through a partial
//! Store RPC, which §6.2 forbids before Slice 4.
//!
//! ## The oracle is the committed membership, not the controller's report
//!
//! §S3-E is explicit that completion means "a non-joint membership with exact
//! desired voters and exact retained learners under the observed membership
//! LogId", and that "a controller reply, local node-map edit, or combined
//! member set is not enough". So every assertion here reads `RaftMetrics` —
//! the group's own agreed answer — and checks the *exact* sets rather than
//! containment, because a membership that merely contains the desired voters is
//! how a half-finished change looks.

#![cfg(feature = "coordination-testing")]

use std::collections::BTreeSet;
use std::time::Duration;

use aster::coordination::contract::{
    ActionV1, ChangeMembershipActionV1, ClientCommandV1, CommandV1, MemberSeedV1, NodeIdV1,
};
use aster::coordination::ids::CoordinationNodeId;
use aster::coordination::testing::GroupFixture;
use aster::rpc::AsterServer;

const DOMAIN: &str = "membership.test";
const SEED: u8 = 0x4E;
const SIZE: usize = 5;
const PATIENCE: Duration = Duration::from_secs(90);

struct Voter {
    server: AsterServer,
    index: usize,
    _dir: tempfile::TempDir,
}

impl Voter {
    fn coordination(&self) -> aster::coordination::Coordination {
        self.server.coordination().expect("hosted")
    }
}

async fn start_group(fixture: &GroupFixture) -> Vec<Voter> {
    let mut voters = Vec::new();
    for index in 0..SIZE {
        let dir = tempfile::tempdir().expect("a storage directory");
        let server = AsterServer::builder()
            .config(fixture.aster_config(index))
            .builtin_coordination(fixture.config(dir.path(), index))
            .start()
            .await
            .expect("a voter starts");
        voters.push(Voter {
            server,
            index,
            _dir: dir,
        });
    }

    let addrs: Vec<_> = voters.iter().map(|v| v.server.node().addr()).collect();
    for voter in &voters {
        for addr in &addrs {
            if addr.node_id != voter.server.node().addr().node_id {
                voter
                    .server
                    .node()
                    .add_node_addr(addr.clone())
                    .expect("register a peer address");
            }
        }
    }
    voters
}

async fn await_leader(voters: &[Voter], what: &str) -> usize {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        for (i, voter) in voters.iter().enumerate() {
            let c = voter.coordination();
            if c.lifecycle_code() == 3 && c.quorum_available() == Some(true) {
                return i;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: no leader within {PATIENCE:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn ids(fixture: &GroupFixture, indices: &[usize]) -> BTreeSet<CoordinationNodeId> {
    indices
        .iter()
        .map(|i| CoordinationNodeId::from_slice(&fixture.node_id(*i)).expect("a node id"))
        .collect()
}

/// One `change_membership` intent, as a client command.
fn plan(
    fixture: &GroupFixture,
    actor: usize,
    operation: &[u8],
    expected: Option<aster::coordination::contract::LogIdV1>,
    desired_voters: &[usize],
    retained_learners: &[usize],
) -> CommandV1 {
    // §9 requires both vectors **sorted unique** by node id, and a fixture index
    // is not a node id — the ids are public keys, so index order and byte order
    // are unrelated. Sorting here rather than expecting the caller to is what
    // keeps every plan in this file well-shaped; an unsorted one is refused
    // before any controller sees it, and the refusal reads as a shapeless
    // request rather than as anything about membership.
    let named = |indices: &[usize]| {
        let mut ids: Vec<NodeIdV1> = indices
            .iter()
            .map(|i| NodeIdV1 {
                node_id: fixture.node_id(*i),
            })
            .collect();
        ids.sort_by(|l, r| l.node_id.cmp(&r.node_id));
        ids
    };

    let action = ActionV1::ChangeMembership(ChangeMembershipActionV1 {
        operation_id: aster::coordination::testing::id16(operation),
        expected_membership_log_id: expected,
        desired_voters: named(desired_voters),
        retained_learners: named(retained_learners),
        // Every node in this plan is already a member, so none of them needs a
        // seed: §7.3 requires a seed for a node the group cannot already reach,
        // and supplying one here would be describing a route the address book
        // already has.
        member_seeds: Vec::<MemberSeedV1>::new(),
    });
    CommandV1::Client(ClientCommandV1 {
        actor_node_id: fixture.node_id(actor),
        client_id: aster::coordination::testing::id16(operation),
        sequence: 1,
        request_hash: aster::coordination::hash::request_hash(&action)
            .expect("a canonical action hashes")
            .to_vec(),
        action,
    })
}

/// The §5.2 outcome a committed command answered with.
///
/// A `change_membership` that is *refused* still commits — apply must be total,
/// so a rejection is an applied entry with a rejecting reply. `Ok(_)` from the
/// proposal therefore says nothing about whether the plan was accepted, and a
/// gate that stopped there would wait for a change no controller was ever asked
/// to make. This is the difference, read from the state machine's own reply.
/// A `Client` reply, because a membership plan travels as a `ClientCommandV1`:
/// §5.2 pairs command variant 1 with reply variant 1, and a `System` reply
/// answers `Expire` and `MembershipProgress`, which are the controller's own
/// commands rather than an administrator's.
fn outcome_of(committed: &aster::coordination::testing::Committed) -> (i32, i32) {
    match &committed.reply {
        aster::coordination::contract::CommandReplyV1::Client(reply) => {
            (reply.outcome_code, reply.reason_code)
        }
        other => panic!("a client command must answer with a Client reply, got {other:?}"),
    }
}

/// §5.2's `MembershipAccepted`.
const MEMBERSHIP_ACCEPTED: i32 = 8;

/// Wait until the committed membership is exactly `voters` plus `learners`.
async fn await_shape(
    voter: &Voter,
    fixture: &GroupFixture,
    voters: &[usize],
    learners: &[usize],
    what: &str,
) {
    let want_voters = ids(fixture, voters);
    let want_nodes: BTreeSet<CoordinationNodeId> = want_voters
        .union(&ids(fixture, learners))
        .copied()
        .collect();

    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let c = voter.coordination();
        let have_voters: BTreeSet<_> = c.effective_voters_for_testing().into_iter().collect();
        let have_nodes: BTreeSet<_> = c.effective_nodes_for_testing().into_iter().collect();
        // Non-joint as well as correct. A joint configuration containing the
        // desired voters is what a change looks like *mid-flight*, and §S3-E
        // refuses to call that completion.
        if have_voters == want_voters
            && have_nodes == want_nodes
            && !c.membership_is_joint_for_testing()
            // And the durable intent has reached a terminal phase. The
            // committed membership reaches its final shape one entry *before*
            // the controller records completion, so a gate that stopped at the
            // shape would submit its next plan into a group that still has one
            // in flight and be refused `MembershipIntentActive` — correctly.
            && !c.membership_intent_active_for_testing().await
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: membership never settled; voters {have_voters:?} want \
             {want_voters:?}, nodes {have_nodes:?} want {want_nodes:?}, joint {}",
            c.membership_is_joint_for_testing()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// **Two voters are removed and retained as learners, then restored.**
///
/// The departing-voter case with step-5 replacement, run for real: five voters
/// down to three, keeping the two that left reachable. That is the shape §7.3's
/// last step exists for — the uniform voter change lands first, and the separate
/// one-entry `ReplaceAllNodes(desired ∪ retained)` is what leaves the departed
/// nodes present as learners rather than gone. A group that dropped them
/// entirely would still report the right *voters*, which is why the node set is
/// asserted too, and asserted exactly.
///
/// Five and three, not three and two: §7.1's profile 1 admits exactly those two
/// sizes, and a plan for an even group is refused by the shape check before any
/// controller sees it. Both endpoints of this change are shapes the profile
/// actually permits, which is what makes it a test of the controller rather
/// than of the shape guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn departing_voters_are_retained_as_learners_and_can_return() {
    let fixture = GroupFixture::new(DOMAIN, SEED, SIZE);
    let voters = start_group(&fixture).await;
    let leader = await_leader(&voters, "before the change").await;
    let all: Vec<usize> = (0..SIZE).collect();

    // Everyone starts a voter and nobody is a learner.
    await_shape(&voters[leader], &fixture, &all, &[], "at rest").await;

    // The departing pair are followers: removing the leader makes it step down,
    // which is a different property, and mixing the two would leave a failure
    // ambiguous between them.
    let departing: Vec<usize> = (0..SIZE)
        .filter(|i| *i != voters[leader].index)
        .take(2)
        .collect();
    let staying: Vec<usize> = (0..SIZE).filter(|i| !departing.contains(i)).collect();

    let coordination = voters[leader].coordination();
    let accepted = coordination
        .propose_for_testing_within(
            plan(
                &fixture,
                voters[leader].index,
                b"shrink",
                coordination.membership_log_id_for_testing(),
                &staying,
                &departing,
            ),
            Duration::from_secs(20),
        )
        .await
        .expect("the plan commits");
    assert_eq!(
        outcome_of(&accepted).0,
        MEMBERSHIP_ACCEPTED,
        "{} (outcome, reason) = {:?}",
        "an administrator's well-shaped plan must be accepted, not merely \
         committed",
        outcome_of(&accepted)
    );

    await_shape(
        &voters[leader],
        &fixture,
        &staying,
        &departing,
        "after the shrink",
    )
    .await;

    // Three voters are a quorum of three; the two retained learners do not
    // count toward it. So the group must still be able to change itself back.
    let coordination = voters[leader].coordination();
    let before = coordination
        .last_applied_for_testing()
        .expect("an applied index");
    let accepted = coordination
        .propose_for_testing_within(
            plan(
                &fixture,
                voters[leader].index,
                b"grow",
                coordination.membership_log_id_for_testing(),
                &all,
                &[],
            ),
            Duration::from_secs(20),
        )
        .await
        .expect("the restoring plan commits");
    assert_eq!(
        outcome_of(&accepted).0,
        MEMBERSHIP_ACCEPTED,
        "{} (outcome, reason) = {:?}",
        "restoring the original shape must be accepted",
        outcome_of(&accepted)
    );

    await_shape(&voters[leader], &fixture, &all, &[], "after the grow").await;
    assert!(
        coordination
            .last_applied_for_testing()
            .expect("an applied index")
            > before,
        "the group must have committed the restoring change"
    );

    for voter in voters {
        voter.server.shutdown().await;
    }
}

/// **A second plan is refused while one is in flight, and the first still
/// finishes.**
///
/// §S3-E's `InProgress` branch, and the reason it must never produce `Failed`:
/// a controller that treated "another change is in flight" as a failure would
/// abandon an operation that was about to succeed. The evidence is that the
/// membership still reaches the *first* plan's shape after the refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_second_plan_is_refused_without_disturbing_the_first() {
    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x01, SIZE);
    let voters = start_group(&fixture).await;
    let leader = await_leader(&voters, "before the change").await;
    let coordination = voters[leader].coordination();

    let all: Vec<usize> = (0..SIZE).collect();
    let departing: Vec<usize> = (0..SIZE)
        .filter(|i| *i != voters[leader].index)
        .take(2)
        .collect();
    let staying: Vec<usize> = (0..SIZE).filter(|i| !departing.contains(i)).collect();
    let at = coordination.membership_log_id_for_testing();

    let first = coordination
        .propose_for_testing_within(
            plan(
                &fixture,
                voters[leader].index,
                b"first",
                at.clone(),
                &staying,
                &departing,
            ),
            Duration::from_secs(20),
        )
        .await
        .expect("the first plan commits");
    assert_eq!(
        outcome_of(&first).0,
        MEMBERSHIP_ACCEPTED,
        "{} (outcome, reason) = {:?}",
        "the first plan must be accepted, or the rest of this proves nothing",
        outcome_of(&first)
    );

    // A second, different operation planned from the same starting point. It
    // must be refused — but as a *decision*, not a fault: the command commits
    // and the reply says no. Apply is total, so a rejection is an applied entry.
    let second = coordination
        .propose_for_testing_within(
            plan(&fixture, voters[leader].index, b"second", at, &all, &[]),
            Duration::from_secs(20),
        )
        .await
        .expect("a refused plan is still a committed command, not an error");
    assert_ne!(
        outcome_of(&second).0,
        MEMBERSHIP_ACCEPTED,
        "a second plan must not be admitted while one is in flight"
    );

    // And the first plan still lands. This is the assertion that matters: a
    // refusal that derailed the change in flight would leave the group short of
    // its target with nothing running to finish it.
    await_shape(
        &voters[leader],
        &fixture,
        &staying,
        &departing,
        "after the refused second plan",
    )
    .await;

    for voter in voters {
        voter.server.shutdown().await;
    }
}

/// **An exact genesis replay survives a membership change.**
///
/// The discriminating case for the check ordering. `apply_genesis` compares the
/// genesis's `initial_voters` with the committed membership, and that comparison
/// belongs to *first installation* only: running it on every arrival made
/// idempotence expire the moment the group legally changed shape, so the
/// founding document would be `GenesisRefused` — fatal, on every voter, for the
/// one document they all agree on.
///
/// A replay that only followed an *administrator* change would still pass with
/// the old ordering, because the voter set would be unchanged. This one shrinks
/// five voters to three first, so the sets genuinely differ.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_genesis_replay_survives_a_membership_change() {
    use aster::coordination::contract::GenesisCommandV1;

    let fixture = GroupFixture::new(DOMAIN, SEED ^ 0x02, SIZE);
    let voters = start_group(&fixture).await;
    let leader = await_leader(&voters, "before the change").await;

    let departing: Vec<usize> = (0..SIZE)
        .filter(|i| *i != voters[leader].index)
        .take(2)
        .collect();
    let staying: Vec<usize> = (0..SIZE).filter(|i| !departing.contains(i)).collect();

    let coordination = voters[leader].coordination();
    let accepted = coordination
        .propose_for_testing_within(
            plan(
                &fixture,
                voters[leader].index,
                b"shrink-before-replay",
                coordination.membership_log_id_for_testing(),
                &staying,
                &departing,
            ),
            Duration::from_secs(20),
        )
        .await
        .expect("the plan commits");
    assert_eq!(outcome_of(&accepted).0, MEMBERSHIP_ACCEPTED);
    await_shape(
        &voters[leader],
        &fixture,
        &staying,
        &departing,
        "before the replay",
    )
    .await;

    // The committed voter set is now three; the genesis names five. Under the
    // old ordering this is the point at which the founding document became
    // fatal.
    let coordination = voters[leader].coordination();
    let replay = coordination
        .propose_for_testing_within(
            CommandV1::Genesis(GenesisCommandV1 {
                signed_genesis: fixture.signed_genesis(),
            }),
            Duration::from_secs(20),
        )
        .await
        .expect("an exact replay commits after the membership changed");
    match &replay.reply {
        aster::coordination::contract::CommandReplyV1::System(reply) => assert_eq!(
            reply.system_result_code, 8,
            "SYS_GENESIS_ALREADY_INSTALLED: recognised, not reinstalled"
        ),
        other => panic!("a genesis answers with a System reply, got {other:?}"),
    }

    // And nothing moved: the replay is a business-state no-op, not a
    // reinstallation that would have restored the genesis's five voters.
    await_shape(
        &voters[leader],
        &fixture,
        &staying,
        &departing,
        "after the replay",
    )
    .await;
    assert_eq!(
        voters[leader].coordination().lifecycle_code(),
        3,
        "still Ready"
    );

    for voter in voters {
        voter.server.shutdown().await;
    }
}
