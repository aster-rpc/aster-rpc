//! §S3-F: a real group driven through a **seeded, deterministic** faulty
//! network — drops, delays, duplicates, reorders, partitions and lost
//! leadership — with the seed recorded on every failure.
//!
//! ## The schedule is shared; the injectors are not
//!
//! Every voter runs its own [`ChaosNetwork`], because ordinals and the applied
//! tally are per-sender. They all run the **same** [`Schedule`], and that is
//! load-bearing rather than tidy: a partition is a property of a *pair*, so if
//! two voters drew from different seeds one could believe it was cut off from a
//! peer that still believed it was connected. That is a one-way link, which is
//! a different fault from a partition and one no consensus protocol promises
//! anything about. One seed makes `same_side` symmetric by construction.
//!
//! ## What makes these gates non-vacuous
//!
//! A fault schedule that happened to deliver everything would let a broken
//! implementation pass, and nothing in the run would say so. Each gate
//! therefore asserts against [`FaultTally`] — the count of what was *actually*
//! applied — and requires that every kind fired before it accepts a success.
//! A seed is recorded in every panic message. Note what that does and does not
//! buy: the schedule is a pure function, so replaying a seed reproduces the
//! verdict for message *n* to a given peer — but a live cluster's message
//! *count* depends on timing, so a rerun is not the same run. The seed narrows
//! an investigation and makes the schedule itself auditable; it is not a
//! promise that a failure repeats on demand. The pure-function property is
//! pinned separately by the unit tests in `network::chaos`.
//!
//! `FaultTally` is what a run has instead of replay, and both halves matter:
//! `reordered` counts only messages a **later** one actually overtook, and
//! `reorder_expired` counts the ones nothing did. An earlier version of this
//! harness slept for a fixed interval and counted the verdict, so the number
//! recorded how often the schedule had said the word "reorder" and never once
//! that two messages had changed places.
//!
//! ## Why this drives the public surface
//!
//! The injector lives in the production Raft client, behind an `Option` that
//! only a `coordination-testing` build can populate. A harness that wrapped a
//! different client would prove things about the wrapper: the §5.3 admission
//! checks, the envelope encoding and the error classification are exactly what
//! a lossy network has to be survived *through*.
//!
//! ## Lost leadership is placed, not waited for
//!
//! `trigger().elect()` on the current leader is a **no-op** in the pinned
//! OpenRaft — it says so, and `raft_core` ignores the command outright — so the
//! first version of this gate deposed nobody and its "commit after failover"
//! was just another commit. Leadership is now taken away by cutting the leader
//! off from every peer, and the gate requires a *different* voter leading at a
//! *higher* term before it accepts renewed progress.
//!
//! `coordination_resume` uses the same instrument to place a loss at each of
//! the four `S3-JOINT-RESUME` boundaries individually. This file is about a
//! group under a faulty network; that one is about a group under a chosen
//! failure.

#![cfg(feature = "coordination-testing")]

use std::time::Duration;

use aster::coordination::contract::{
    ActionV1, ClientCommandV1, CommandReplyV1, CommandV1, ResourceKeyV1, SetGrantActionV1,
};
use aster::coordination::ids::CoordinationNodeId;
use aster::coordination::testing::Schedule;

mod common;
use common::{
    await_leader, await_leader_other_than, combined_tally, every_core_is_alive, isolate_everywhere,
    rejoin_everywhere, shutdown_all, ChaosFixture, ChaoticVoter, PhaseClock,
};

const DOMAIN: &str = "chaos.test";

/// The recorded seed. Changing it is changing the test: a different seed is a
/// different network, and a gate that passes only on one seed has found a bug
/// worth keeping rather than a seed worth replacing.
const SEED: u64 = 0x5EED_C0FF_EE00_1234;

/// Out of 100. High enough that every kind fires within a run of this length,
/// low enough that a majority still makes progress — a network that loses most
/// messages tests a partition, and there is a separate gate for that.
///
/// The rate and [`COMMIT_ROUNDS`] are chosen together against the rarest kind.
/// A duplicate is one eighth of the faulty share, so at this rate roughly one
/// message in 36 is duplicated: a run of a couple of dozen messages would
/// plausibly contain none, and the gate would then be asserting nothing about
/// duplicates while appearing to. Sizing the run is the fix; loosening the
/// assertion would only hide the same gap.
const FAULT_PERCENT: u8 = 28;

/// Commits driven through the faulty network. Each is a proposal plus its
/// replication to two peers, so this is the run's message volume.
const COMMIT_ROUNDS: u8 = 20;

/// Further commits allowed while a fault kind has not fired yet.
const TOP_UP_ROUNDS: u16 = 200;

/// Generous: every retry under this schedule costs at least one election
/// timeout, and `election_timeout_max` is 3 s. A hang still fails rather than
/// waits forever.
const PATIENCE: Duration = Duration::from_secs(90);

/// A grant on a fresh resource `key`, from **one** client at `sequence`.
///
/// One client id for the whole run, not one per command. A new client id per
/// round is not what a client does, and it is not free: §12.1 caps the active
/// sessions a group retains, so a twenty-round gate written that way stops
/// being about the network at round seven and starts being about the cap. That
/// is how this was first written, and `ActiveClientCap` said so.
///
/// A fixed `(client_id, sequence)` also makes the retry in [`commit_somewhere`]
/// meaningful rather than merely tolerated: a proposal whose reply the schedule
/// dropped is re-proposed as an **exact replay**, which the state machine must
/// answer from its saved result instead of applying twice.
fn grant(actor: &[u8], client: &[u8], sequence: u64, key: &[u8]) -> CommandV1 {
    use aster::coordination::model::limits::{RIGHT_ACQUIRE, RIGHT_READ, RIGHT_RENEW};

    let action = ActionV1::SetGrant(SetGrantActionV1 {
        resource: ResourceKeyV1 {
            namespace: "chaos-gate".into(),
            key: key.to_vec(),
        },
        target_node_id: actor.to_vec(),
        rights: RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW,
        // Each round grants on its own resource key, so every one is a fresh
        // ACL at revision 0.
        expected_acl_revision: 0,
    });
    CommandV1::Client(ClientCommandV1 {
        actor_node_id: actor.to_vec(),
        client_id: aster::coordination::testing::id16(client),
        sequence,
        request_hash: aster::coordination::hash::request_hash(&action)
            .expect("a canonical action hashes")
            .to_vec(),
        action,
    })
}

/// §5.2 outcome 6, reason 0: a grant was written.
const GRANT_UPDATED: i32 = 6;

fn outcome_of(committed: &aster::coordination::testing::Committed) -> (i32, i32) {
    match &committed.reply {
        CommandReplyV1::Client(reply) => (reply.outcome_code, reply.reason_code),
        other => panic!("a client command must answer with a Client reply, got {other:?}"),
    }
}

/// Commit `command` at whichever voter currently leads, retrying across a
/// leadership change.
///
/// Retry is not leniency here: under this schedule the leader that accepted a
/// proposal may lose leadership before it commits, and a client that gave up on
/// the first `ForwardToLeader` would be testing its own impatience.
async fn commit_somewhere(
    voters: &[ChaoticVoter],
    command: CommandV1,
    what: &str,
) -> aster::coordination::testing::Committed {
    commit_avoiding(voters, &[], command, what).await
}

/// [`commit_somewhere`], but never asking a voter in `excluding`.
async fn commit_avoiding(
    voters: &[ChaoticVoter],
    excluding: &[CoordinationNodeId],
    command: CommandV1,
    what: &str,
) -> aster::coordination::testing::Committed {
    let started = std::time::Instant::now();
    let mut attempts = 0u32;
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let leader = await_leader_other_than(voters, excluding, PATIENCE, what).await;
        let coordination = voters[leader].coordination();
        attempts += 1;
        if let Ok(committed) = coordination
            .propose_for_testing_within(command.clone(), Duration::from_secs(20))
            .await
        {
            // The walltime ledger: a commit that needed more than one proposal
            // or more than a couple of seconds is exactly the event whose cost
            // S3-CHAOS-WALLTIME could not attribute.
            let elapsed = started.elapsed();
            if attempts > 1 || elapsed > Duration::from_secs(2) {
                println!(
                    "[chaos-timing] slow commit: {what} took {:.2}s over {attempts} proposal(s)",
                    elapsed.as_secs_f64()
                );
            }
            return committed;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: no commit within {PATIENCE:?} at seed {SEED:#x}; faults {:?}",
            combined_tally(voters)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// **A group elects, commits and survives lost leadership through a seeded
/// faulty network.**
///
/// Three voters, one recorded seed, every message subject to the schedule.
/// The gate requires a real commit *after* the elected leader is deliberately
/// stepped down, so the run covers election, replication and failover rather
/// than a single lucky window — and it requires the tally to show that drops,
/// delays, duplicates and reorders all actually happened, so a schedule that
/// silently delivered everything fails instead of passing quietly.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_group_makes_progress_through_a_seeded_faulty_network() {
    let mut clock = PhaseClock::start("progress-under-chaos");
    let fixture = ChaosFixture::new(DOMAIN, 0xA0, 3);
    let voters = fixture.start(Schedule::new(SEED, FAULT_PERCENT)).await;
    clock.mark("start 3 voters");

    // Election, through a network that is dropping and delaying votes.
    let leader = await_leader(&voters, PATIENCE, "the first election under chaos").await;
    clock.mark("first election");

    // Replication, likewise.
    let client = b"chaos-client";
    let mut sequence = 0u64;
    let mut next = |key: &[u8]| {
        sequence += 1;
        grant(&fixture.node_id(0), client, sequence, key)
    };

    let first = commit_somewhere(&voters, next(b"before"), "the first commit").await;
    assert_eq!(
        outcome_of(&first),
        (GRANT_UPDATED, 0),
        "a committed grant is accepted even through a faulty network (seed {SEED:#x})"
    );
    clock.mark("first commit");

    // Enough traffic for the schedule to be a schedule rather than a coin
    // flip. An election plus one commit is a couple of dozen messages, and at
    // this rate the rarer kinds would plausibly not come up at all. This is
    // exactly what the tally assertion is for: the first two versions of this
    // gate ran too little traffic, and the tally said so instead of passing.
    for round in 0..COMMIT_ROUNDS {
        let key = [b'r', round];
        let committed = commit_somewhere(&voters, next(&key), "a mid-run commit").await;
        assert_eq!(
            outcome_of(&committed),
            (GRANT_UPDATED, 0),
            "round {round} commits (seed {SEED:#x})"
        );
    }
    clock.mark("20 commit rounds");

    // The schedule is seeded per message slot, but which messages fill the
    // slots depends on timing, so a run can end its fixed rounds before a
    // rarer kind has fired (one candidate saw `reordered: 0`). Keep committing
    // through the same faults until every kind has, capped; the tally
    // assertion below still fails a schedule that never gets there.
    let mut top_up = 0u16;
    while !combined_tally(&voters).every_kind_fired() && top_up < TOP_UP_ROUNDS {
        let key = [b't', (top_up >> 8) as u8, top_up as u8];
        let committed = commit_somewhere(&voters, next(&key), "a top-up commit").await;
        assert_eq!(
            outcome_of(&committed),
            (GRANT_UPDATED, 0),
            "top-up round {top_up} commits (seed {SEED:#x})"
        );
        top_up += 1;
    }
    clock.mark("top-up commit rounds");

    // Lost leadership, at a moment the schedule did not choose — the phase
    // §S3-F names last, and the one a purely message-level fault never reaches.
    //
    // **Not** `trigger().elect()` on the leader. The pinned OpenRaft documents
    // that as a no-op for a node that already leads, and the first version of
    // this gate called it exactly there: the same voter kept leading, the term
    // never moved, and the "commit after failover" below was just another
    // commit. Cutting the leader off is a loss the group cannot decline.
    let lost = voters[leader].id;
    let term_before = voters[leader].term();
    isolate_everywhere(&voters, lost);

    let successor = await_leader_other_than(
        &voters,
        &[lost],
        PATIENCE,
        "an election after the leader was cut off",
    )
    .await;
    clock.mark("failover election");
    assert_ne!(
        voters[successor].id, lost,
        "the successor must be a different voter, or nothing was lost"
    );
    assert!(
        voters[successor].term() > term_before,
        "a successor at the same term would be the same leadership under another \
         name; {} then {} (seed {SEED:#x})",
        term_before,
        voters[successor].term()
    );

    // And progress again afterwards, which is the actual claim: the group
    // recovered rather than merely survived.
    let second = commit_avoiding(
        &voters,
        &[lost],
        next(b"after"),
        "a commit after lost leadership",
    )
    .await;
    assert_eq!(
        outcome_of(&second),
        (GRANT_UPDATED, 0),
        "the group commits again after leadership moved (seed {SEED:#x})"
    );
    clock.mark("post-failover commit");

    // The deposed voter rejoins and catches up, so the loss was a loss and not
    // a removal — a group that could only continue by forgetting a member has
    // not survived a leader failure, it has survived a smaller cluster.
    rejoin_everywhere(&voters, &lost);
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let recovered = voters[leader].coordination().last_applied_for_testing();
        if recovered >= Some(second.index) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the deposed voter never caught up: applied {recovered:?} against \
             {} at seed {SEED:#x}; faults {:?}",
            second.index,
            combined_tally(&voters)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    clock.mark("deposed voter catch-up");

    // Non-vacuity. Without this the gate would pass just as well on a perfect
    // network, and would therefore prove nothing about a faulty one.
    let tally = combined_tally(&voters);
    println!("seed {SEED:#x} at {FAULT_PERCENT}% applied {tally:?}");
    assert!(
        tally.every_kind_fired(),
        "the schedule must have applied every kind of fault, or this gate proves \
         nothing about a faulty network; seed {SEED:#x} produced {tally:?}"
    );

    every_core_is_alive(&voters, "after the gate").await;
    shutdown_all(voters).await;
    clock.mark("shutdown");
}

/// The lone voter at `epoch`, if the schedule separates exactly one from the
/// other two.
///
/// Asked of the schedule rather than guessed. A gate that stepped to the next
/// epoch and hoped would sometimes be testing a whole network while claiming a
/// partition — which is what the first version of this gate did.
fn minority_at(schedule: &Schedule, ids: &[CoordinationNodeId], epoch: u64) -> Option<usize> {
    (0..ids.len()).find(|alone| {
        (0..ids.len()).all(|other| {
            let connected = schedule.reaches(&ids[*alone], &ids[other], epoch);
            connected == (other == *alone)
        })
    })
}

/// Whether every pair can reach every other at `epoch`.
fn whole_at(schedule: &Schedule, ids: &[CoordinationNodeId], epoch: u64) -> bool {
    ids.iter()
        .all(|a| ids.iter().all(|b| schedule.reaches(a, b, epoch)))
}

/// **A partition strands a minority, and healing it lets that minority back
/// in.**
///
/// Distinct from the gate above because a partition is not a high drop rate:
/// dropping each message independently never leaves a *stable* minority, and a
/// group that only ever met independent drops would never exercise the case
/// where one side must refuse to commit for a sustained period.
///
/// Three claims, and the middle one is the one the earlier version of this gate
/// left out entirely — it advanced into one epoch, committed on whichever side
/// led, and stopped, so "heals across one" named something that never happened:
///
/// 1. while the split holds, the majority commits and the stranded voter's log
///    does not move at all;
/// 2. the epoch is then moved to one the schedule says connects **every** pair;
/// 3. after which the stranded voter catches up to the entries it missed, and
///    the whole group commits again.
///
/// Both epochs are chosen by asking the schedule, so neither is a hope.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_partition_strands_a_minority_and_heals() {
    let fixture = ChaosFixture::new(DOMAIN, 0xB0, 3);
    let schedule = Schedule::new(SEED ^ 0xBEEF, FAULT_PERCENT);
    let ids: Vec<CoordinationNodeId> = (0..fixture.size()).map(|i| fixture.id(i)).collect();

    // Chosen from the schedule before anything starts: an epoch that genuinely
    // strands one voter, and a later one that genuinely connects everybody.
    let (split_epoch, stranded) = (1..2_000u64)
        .find_map(|epoch| minority_at(&schedule, &ids, epoch).map(|alone| (epoch, alone)))
        .expect("this seed separates one voter at some epoch");
    let healed_epoch = (split_epoch + 1..4_000u64)
        .find(|epoch| whole_at(&schedule, &ids, *epoch))
        .expect("this seed reconnects the group at some later epoch");

    let mut clock = PhaseClock::start("partition-strand-heal");
    let voters = fixture.start(schedule).await;
    let client = b"chaos-client";

    await_leader(&voters, PATIENCE, "the election before any partition").await;
    clock.mark("start + pre-partition election");
    let before = commit_somewhere(
        &voters,
        grant(&fixture.node_id(0), client, 1, b"pre-partition"),
        "a commit before the partition",
    )
    .await;
    assert_eq!(outcome_of(&before), (GRANT_UPDATED, 0));
    clock.mark("pre-partition commit");

    // Move every voter to the split epoch at once. The sides come from the
    // shared seed, so all three agree on who can reach whom; two voters at
    // different epochs would be a one-way link.
    for voter in &voters {
        voter.chaos.set_epoch(split_epoch);
    }
    assert!(
        voters.iter().all(|v| v.chaos.epoch() == split_epoch),
        "every voter must be at the same epoch, or the split is one-way"
    );

    // 1. The majority commits; the stranded voter hears none of it.
    //
    // The mark is taken after a beat, not at the instant the epoch moved: the
    // schedule is consulted when a message is *sent*, so a message dispatched
    // microseconds before the split still lands, and the stranded voter can
    // legitimately apply one more entry from before it. Asserting against the
    // instant of the split failed on exactly that, and the failure was the
    // test's, not the group's.
    //
    // `Option` rather than an unwrapped index, and that is not tidiness: a
    // follower that was still behind when the split landed has applied nothing
    // at all, and `None` is the honest mark for it. Requiring `Some` failed
    // once for exactly that reason — a test asserting the group had done
    // something before it had finished doing it.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let marooned_at = voters[stranded].coordination().last_applied_for_testing();
    let after = commit_avoiding(
        &voters,
        &[ids[stranded]],
        grant(&fixture.node_id(0), client, 2, b"post-partition"),
        "a commit on the majority side of the partition",
    )
    .await;
    assert_eq!(
        outcome_of(&after),
        (GRANT_UPDATED, 0),
        "a majority side commits across a partition (seed {:#x})",
        SEED ^ 0xBEEF
    );
    assert!(
        Some(after.index) > marooned_at,
        "the majority's commit must be past where the stranded voter stopped: \
         {marooned_at:?} against {}",
        after.index
    );
    assert_eq!(
        voters[stranded].coordination().last_applied_for_testing(),
        marooned_at,
        "a stranded minority must not apply anything the majority commits; \
         seed {:#x} produced {:?}",
        SEED ^ 0xBEEF,
        combined_tally(&voters)
    );
    clock.mark("majority commit while stranded");

    // 2. Heal, to an epoch the schedule says connects every pair.
    for voter in &voters {
        voter.chaos.set_epoch(healed_epoch);
    }
    assert!(
        whole_at(&schedule, &ids, healed_epoch),
        "the healed epoch must connect every pair, or nothing healed"
    );

    // 3. The stranded voter catches up on what it missed, and the whole group
    //    commits again.
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let caught_up = voters[stranded].coordination().last_applied_for_testing();
        if caught_up >= Some(after.index) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the stranded voter never caught up after the partition healed: \
             applied {caught_up:?} against {} at seed {:#x}; faults {:?}",
            after.index,
            SEED ^ 0xBEEF,
            combined_tally(&voters)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    clock.mark("stranded voter catch-up after heal");
    let healed = commit_somewhere(
        &voters,
        grant(&fixture.node_id(0), client, 3, b"post-heal"),
        "a commit once the partition healed",
    )
    .await;
    assert_eq!(
        outcome_of(&healed),
        (GRANT_UPDATED, 0),
        "the whole group commits again once the split is gone (seed {:#x})",
        SEED ^ 0xBEEF
    );

    let tally = combined_tally(&voters);
    println!("partition at epochs {split_epoch}/{healed_epoch} applied {tally:?}");
    assert!(
        tally.partitioned > 0,
        "the partition must have actually dropped traffic; seed {:#x} produced {tally:?}",
        SEED ^ 0xBEEF
    );

    every_core_is_alive(&voters, "after the gate").await;
    shutdown_all(voters).await;
    clock.mark("post-heal commit + shutdown");
}

/// **A perfect schedule changes nothing.**
///
/// The control. A zero fault rate must leave the group behaving exactly as the
/// unchaosed cluster gates do — if this fails, the injector itself is breaking
/// the group and every result above is about the harness rather than the code.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_zero_fault_schedule_leaves_the_group_untouched() {
    let mut clock = PhaseClock::start("zero-fault-control");
    let fixture = ChaosFixture::new(DOMAIN, 0xC0, 3);
    let voters = fixture.start(Schedule::perfect()).await;

    await_leader(&voters, PATIENCE, "an election with no faults").await;
    clock.mark("start + election");
    let committed = commit_somewhere(
        &voters,
        grant(&fixture.node_id(0), b"chaos-client", 1, b"control"),
        "a commit with no faults",
    )
    .await;
    assert_eq!(outcome_of(&committed), (GRANT_UPDATED, 0));

    let tally = combined_tally(&voters);
    assert_eq!(
        (
            tally.dropped,
            tally.delayed,
            tally.duplicated,
            tally.reordered,
            tally.reorder_expired,
            tally.partitioned
        ),
        (0, 0, 0, 0, 0, 0),
        "a perfect schedule applies no faults at all, and delivered {tally:?}"
    );
    assert!(
        tally.delivered > 0,
        "the injector must actually be on the path, or this control proves nothing"
    );

    every_core_is_alive(&voters, "after the gate").await;
    shutdown_all(voters).await;
    clock.mark("commit + shutdown");
}

/// **A membership change completes through the faulty network.**
///
/// The phase §S3-F names that no message-level gate reaches. A membership
/// change is not one commit: the controller adds learners, waits for each to
/// catch up, enters a joint configuration and leaves it, and every one of those
/// steps is a separate round of replication that the schedule is dropping,
/// delaying, duplicating and reordering underneath.
///
/// Five voters shrinking to three, the two departing ones retained as learners
/// — the same shape `coordination_membership` proves on a healthy network, so
/// the difference between the two gates is the network and nothing else.
/// Profile 1 admits 3 or 5 voters and nothing between, which is why the change
/// is 5→3 rather than something smaller.
///
/// Completion is read from the **committed membership**, not from the
/// controller's reply: an accepted plan says the controller was asked, and this
/// gate is about whether it finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_membership_change_completes_through_a_seeded_faulty_network() {
    use std::collections::BTreeSet;

    // The **contract** `NodeIdV1`, not the `raft` one: a membership plan
    // travels as a client command, and the two types are distinct.
    use aster::coordination::contract::{
        ChangeMembershipActionV1, MemberSeedV1, NodeIdV1 as PlanNodeId,
    };

    let mut clock = PhaseClock::start("membership-under-chaos");
    let fixture = ChaosFixture::new(DOMAIN, 0xD0, 5);
    let voters = fixture
        .start(Schedule::new(SEED ^ 0x5171, FAULT_PERCENT))
        .await;
    // Wait for a first leader before planning: a plan proposed into a group
    // that has not elected is refused for a reason that is about the election
    // rather than about the network.
    await_leader(&voters, PATIENCE, "the election before a membership change").await;
    clock.mark("start 5 voters + election");

    // Sorted unique by node id, which is a public key: fixture order and byte
    // order are unrelated, and an unsorted plan is refused as shapeless before
    // any controller sees it.
    let named = |indices: &[usize]| {
        let mut ids: Vec<PlanNodeId> = indices
            .iter()
            .map(|i| PlanNodeId {
                node_id: fixture.node_id(*i),
            })
            .collect();
        ids.sort_by(|l, r| l.node_id.cmp(&r.node_id));
        ids
    };

    /// §5.2's `MembershipAccepted`.
    const MEMBERSHIP_ACCEPTED: i32 = 8;

    // The plan is rebuilt on **every** attempt, because two of its inputs go
    // stale under chaos and a fixed command would be refused rather than
    // retried: `expected_membership_log_id` must be the membership the group
    // currently has, and the actor must be a voter that is currently leading.
    // A `None` expectation is not a wildcard — it is a mismatch, and reason 13
    // is what the first version of this gate got.
    //
    // The `operation_id` is fixed, so a plan that committed while its reply was
    // dropped is the *same* intent when it arrives again rather than a second
    // one racing the first.
    let plan_at = |leader: usize, expected| {
        let action = ActionV1::ChangeMembership(ChangeMembershipActionV1 {
            operation_id: aster::coordination::testing::id16(b"chaos-shrink"),
            expected_membership_log_id: expected,
            desired_voters: named(&[0, 1, 2]),
            retained_learners: named(&[3, 4]),
            // Every node named is already a member, so none needs a route seed.
            member_seeds: Vec::<MemberSeedV1>::new(),
        });
        CommandV1::Client(ClientCommandV1 {
            actor_node_id: fixture.node_id(leader),
            client_id: aster::coordination::testing::id16(b"chaos-admin"),
            sequence: 1,
            request_hash: aster::coordination::hash::request_hash(&action)
                .expect("a canonical action hashes")
                .to_vec(),
            action,
        })
    };

    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let at = await_leader(&voters, PATIENCE, "a leader to accept the membership plan").await;
        let coordination = voters[at].coordination();
        let expected = coordination.membership_log_id_for_testing();
        match coordination
            .propose_for_testing_within(plan_at(at, expected), Duration::from_secs(20))
            .await
        {
            Ok(committed) if outcome_of(&committed) == (MEMBERSHIP_ACCEPTED, 0) => break,
            // A refusal here is not a failure of the gate: leadership or the
            // membership log id can move between reading it and committing,
            // and the honest response is to re-read both and ask again. What
            // would be a failure is never being accepted at all, which the
            // deadline below catches.
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the membership plan was never accepted within {PATIENCE:?} at seed {:#x}; \
             faults {:?}",
            SEED ^ 0x5171,
            combined_tally(&voters)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    clock.mark("plan accepted");

    // Settled, non-joint, and with the durable intent finished. A joint
    // configuration containing the desired voters is what a change looks like
    // *mid-flight*, and the committed shape reaches its final form one entry
    // before the controller records completion.
    let want_voters: BTreeSet<Vec<u8>> = [0, 1, 2].iter().map(|i| fixture.node_id(*i)).collect();
    let want_nodes: BTreeSet<Vec<u8>> = (0..5).map(|i| fixture.node_id(i)).collect();

    let settle_deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let mut settled = false;
        for voter in &voters {
            let c = voter.coordination();
            let have_voters: BTreeSet<Vec<u8>> = c
                .effective_voters_for_testing()
                .into_iter()
                .map(|n| n.as_bytes().to_vec())
                .collect();
            let have_nodes: BTreeSet<Vec<u8>> = c
                .effective_nodes_for_testing()
                .into_iter()
                .map(|n| n.as_bytes().to_vec())
                .collect();
            if have_voters == want_voters
                && have_nodes == want_nodes
                && !c.membership_is_joint_for_testing()
                && !c.membership_intent_active_for_testing().await
            {
                settled = true;
                break;
            }
        }
        if settled {
            break;
        }
        assert!(
            tokio::time::Instant::now() < settle_deadline,
            "the membership change never settled within {PATIENCE:?} at seed {:#x}; \
             faults {:?}",
            SEED ^ 0x5171,
            combined_tally(&voters)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    clock.mark("membership settled");

    let tally = combined_tally(&voters);
    println!("membership under chaos applied {tally:?}");
    assert!(
        tally.dropped > 0 && tally.delayed > 0,
        "the controller must have run through real faults; seed {:#x} produced {tally:?}",
        SEED ^ 0x5171
    );

    every_core_is_alive(&voters, "after the gate").await;
    shutdown_all(voters).await;
    clock.mark("shutdown");
}
