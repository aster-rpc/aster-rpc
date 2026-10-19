//! §7.2 graduation, against a leadership change the joiner was not told about.
//!
//! ## The wedge this closes
//!
//! A candidate joins on a signed permit that names its `bootstrap_senders` —
//! the only nodes it will accept Raft from until §7.2's graduation replaces
//! that list with the committed membership. An operator mints that permit
//! before the candidate exists, naming whichever voters they expect it to talk
//! to; the group's leadership is free to move afterwards, and nothing tells the
//! permit.
//!
//! §7.3 adds a candidate as a **learner** and promotes it only after catch-up,
//! so the whole interval that matters is one where the candidate is a member
//! and is not a voter. Graduation was reading `voter_ids()`, which never
//! contains it during exactly that interval: the candidate stayed on its seed
//! list, and a leadership change to a voter outside `bootstrap_senders` left
//! the node responsible for catching it up unable to speak to it at all. The
//! joiner then sits at `Joining` forever with nothing in its own state
//! explaining why — the group is healthy, its permit is valid, and its
//! allowlist is silently refusing the only peer that matters.
//!
//! §7.2 says the event is a committed membership containing the node. In
//! whichever role.

#![cfg(feature = "coordination-testing")]

use std::collections::BTreeSet;
use std::time::Duration;

use aster::coordination::contract::{
    ActionV1, ChangeMembershipActionV1, ClientCommandV1, CommandReplyV1, CommandV1, MemberSeedV1,
    NodeIdV1,
};
use aster::coordination::ids::CoordinationNodeId;
use aster::coordination::testing::{Committed, Schedule};

mod common;
use common::{
    await_leader, await_leader_other_than, cross_register, isolate_everywhere, shutdown_all,
    ChaosFixture, ChaoticVoter,
};

const DOMAIN: &str = "join.test";
const PATIENCE: Duration = Duration::from_secs(90);

/// §5.2's `MembershipAccepted`.
const MEMBERSHIP_ACCEPTED: i32 = 8;

/// The candidate's identity. Outside the genesis, so it joins on a permit.
const CANDIDATE: [u8; 32] = [0x4Au8; 32];

fn outcome_of(committed: &Committed) -> (i32, i32) {
    match &committed.reply {
        CommandReplyV1::Client(reply) => (reply.outcome_code, reply.reason_code),
        other => panic!("a membership plan must answer with a Client reply, got {other:?}"),
    }
}

/// Propose `command` at whichever voter currently leads (outside `excluded`),
/// resending the identical command on any error until `PATIENCE`.
///
/// A single attempt with a fixed timeout is a wait on one leader staying put.
/// Resending is safe because a membership plan is idempotent: the same
/// operation id with an unchanged request returns the existing intent
/// (`model/membership.rs`), so a copy that already committed is not a second
/// change. Same shape as `commit_at_leader` in `coordination_membership.rs`,
/// which does not share this file's fixture.
async fn commit_at_leader(
    voters: &[ChaoticVoter],
    excluded: &[CoordinationNodeId],
    command: CommandV1,
    what: &str,
) -> Committed {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let leader = await_leader_other_than(voters, excluded, PATIENCE, what).await;
        match voters[leader]
            .coordination()
            .propose_for_testing_within(command.clone(), Duration::from_secs(20))
            .await
        {
            Ok(committed) => return committed,
            Err(error) if tokio::time::Instant::now() < deadline => {
                eprintln!("{what}: {error:?}; retrying at the current leader");
            }
            Err(error) => panic!("{what}: {error:?}"),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// **A joiner survives leadership moving outside its bootstrap senders.**
///
/// The permit names exactly one voter, the group then commits a membership that
/// adds the candidate as a learner, and the voter the permit named is cut off.
/// A different voter takes over, and the candidate must accept it — which it
/// can only do if graduation has already happened, because that voter is not on
/// its seed list and never will be.
///
/// The assertion is the candidate's **applied log**, not its lifecycle code: a
/// node can report `Running` from a corridor transition while still refusing
/// every message. What proves the allowlist opened is entries arriving from a
/// leader the permit never mentioned.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_joiner_survives_leadership_moving_outside_its_bootstrap_senders() {
    let fixture = ChaosFixture::new(DOMAIN, 0x4B, 3).admitting(CANDIDATE);
    let voters = fixture.start(Schedule::perfect()).await;
    let leader = await_leader(&voters, PATIENCE, "the election before a join").await;

    // The permit names **only** the current leader. That is the narrow case an
    // operator produces without meaning to: they mint the permit against
    // whichever voter they can see, and leadership moves later.
    let joiner = fixture
        .start_joiner(CANDIDATE, &[leader], Schedule::perfect())
        .await;
    let everyone: Vec<&ChaoticVoter> = voters.iter().chain(std::iter::once(&joiner)).collect();
    cross_register(&everyone);

    assert_eq!(
        joiner.coordination().lifecycle_code(),
        2,
        "a permit grants no membership, so the candidate starts Joining"
    );

    // Add the candidate as a **learner**: the state §7.3 promotes from, and the
    // one graduation used to miss. The seed carries the same signed permit,
    // because §9 requires a valid one for a node the group does not yet have.
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
    let candidate_key = *joiner.id.as_bytes();
    let candidate_id = candidate_key.to_vec();
    let action = ActionV1::ChangeMembership(ChangeMembershipActionV1 {
        operation_id: aster::coordination::testing::id16(b"join-learner"),
        expected_membership_log_id: voters[leader]
            .coordination()
            .membership_log_id_for_testing(),
        desired_voters: named(&[0, 1, 2]),
        retained_learners: vec![NodeIdV1 {
            node_id: candidate_id.clone(),
        }],
        member_seeds: vec![MemberSeedV1 {
            node_id: candidate_id.clone(),
            ticket: aster::coordination::model::authority::canonical_candidate_ticket(
                &candidate_key,
            )
            .expect("a canonical candidate ticket"),
            signed_join_permit: Some(fixture.join_permit(CANDIDATE, &[leader])),
        }],
    });
    let plan = CommandV1::Client(ClientCommandV1 {
        actor_node_id: fixture.node_id(leader),
        client_id: aster::coordination::testing::id16(b"join-admin"),
        sequence: 1,
        request_hash: aster::coordination::hash::request_hash(&action)
            .expect("a canonical action hashes")
            .to_vec(),
        action,
    });
    let accepted = commit_at_leader(&voters, &[], plan, "the add-learner plan").await;
    assert_eq!(
        outcome_of(&accepted),
        (MEMBERSHIP_ACCEPTED, 0),
        "the group accepts a plan that adds the permitted candidate as a learner"
    );

    // Wait until the committed membership actually contains the candidate.
    // That is §7.2's event, and until it happens the candidate is legitimately
    // still on its seed list.
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let nodes: BTreeSet<Vec<u8>> = joiner
            .coordination()
            .committed_nodes_for_testing()
            .into_iter()
            .map(|n| n.as_bytes().to_vec())
            .collect();
        if nodes.contains(&candidate_id) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the candidate never appeared in its own committed membership; it saw {}",
            nodes.len()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let caught_up_under_the_named_sender = joiner
        .coordination()
        .last_applied_for_testing()
        .expect("the candidate applied the membership that named it");

    // Now take away the only sender its permit ever named.
    isolate_everywhere(&voters, voters[leader].id);
    joiner.chaos.isolate(voters[leader].id);
    let successor = await_leader_other_than(
        &voters,
        &[voters[leader].id],
        PATIENCE,
        "a leader outside the candidate's bootstrap senders",
    )
    .await;
    assert_ne!(voters[successor].id, voters[leader].id);

    // Commit something under the new leader, and require the candidate to
    // receive it. This is the assertion the whole gate exists for: the entries
    // arrive from a voter the permit never named, which is only possible once
    // the seed list has been replaced by the committed membership.
    let follow_on = ActionV1::ChangeMembership(ChangeMembershipActionV1 {
        operation_id: aster::coordination::testing::id16(b"join-followon"),
        expected_membership_log_id: voters[successor]
            .coordination()
            .membership_log_id_for_testing(),
        desired_voters: named(&[0, 1, 2]),
        retained_learners: vec![NodeIdV1 {
            node_id: candidate_id.clone(),
        }],
        // Already a member now, so no seed — and §9 refuses one for a node the
        // group already has.
        member_seeds: Vec::new(),
    });
    let second = CommandV1::Client(ClientCommandV1 {
        actor_node_id: fixture.node_id(successor),
        client_id: aster::coordination::testing::id16(b"join-admin"),
        sequence: 2,
        request_hash: aster::coordination::hash::request_hash(&follow_on)
            .expect("a canonical action hashes")
            .to_vec(),
        action: follow_on,
    });
    let committed = commit_at_leader(
        &voters,
        &[voters[leader].id],
        second,
        "the successor's follow-on plan",
    )
    .await;

    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let applied = joiner.coordination().last_applied_for_testing();
        if applied >= Some(committed.index) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the joiner never accepted entries from a leader outside its bootstrap \
             senders: applied {applied:?}, target {}, and it had reached {} while the \
             permitted sender still led — which is the wedge §7.2 graduation exists to \
             prevent",
            committed.index,
            caught_up_under_the_named_sender
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    shutdown_all(voters).await;
    joiner.server.shutdown().await;
}
