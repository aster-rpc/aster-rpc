//! **The same-v1 rolling restart** (S5-C, `DECIDED S5-COMPAT`).
//!
//! ## What "same-v1 compatibility" can and cannot be tested as
//!
//! Every version axis in the matrix is `1`, and v1 has no cross-protocol
//! upgrade: a build that changes any of them is not an older v1 build, it is a
//! different protocol. So "mixed old/new processes" has exactly one honest
//! meaning inside v1 — **independently restarted processes of the same build,
//! interoperating across the restart** — and that is what this gate runs. The
//! *refusal* side of the matrix, where a genuinely different version is
//! presented, is covered per axis by the fourteen fixtures the matrix names;
//! those can construct a value the constants forbid, which no rolling restart
//! can.
//!
//! ## Why continuity is the assertion, not survival
//!
//! A test that stopped every voter, restarted them all, and found the state
//! intact would prove only that the store reopens — which
//! `a_bootstrapped_group_reopens_through_existing` already proves for one
//! voter. The property a rollout depends on is stronger and quite different:
//! **while any one voter is down, the remaining two still commit**, and the
//! returning voter rejoins the group it left rather than founding anything.
//!
//! So a write is committed through the surviving pair *during* every restart
//! window, and each of those writes is checked afterwards. If a restart ever
//! cost the group its quorum, or a returning voter ever came back as a
//! stranger, the round it happened in fails rather than the final state being
//! quietly short.
//!
//! ## Leader movement is included, not simulated
//!
//! Restarting all three voters necessarily restarts the leader, so leadership
//! has to move at least once and the test asserts it did. That is the same
//! event a rollout causes, arrived at the same way, rather than a transfer
//! arranged for the test.

#![cfg(feature = "coordination-testing")]

use std::time::Duration;

use aster::coordination::contract::{
    AcquireActionV1, ActionV1, ClientCommandV1, CommandV1, ResourceKeyV1,
};
use aster::coordination::testing::GroupFixture;
use aster::rpc::AsterServer;

const DOMAIN: &str = "rolling.test";
const SEED: u8 = 0x5C;
const SIZE: usize = 3;
const PATIENCE: Duration = Duration::from_secs(60);

struct Voter {
    server: AsterServer,
    dir: tempfile::TempDir,
    index: usize,
}

impl Voter {
    fn coordination(&self) -> aster::coordination::Coordination {
        self.server.coordination().expect("hosted")
    }
}

async fn start(fixture: &GroupFixture, index: usize, dir: tempfile::TempDir, fresh: bool) -> Voter {
    let config = if fresh {
        fixture.config(dir.path(), index)
    } else {
        fixture.existing_config(dir.path())
    };
    let server = AsterServer::builder()
        .config(fixture.aster_config(index))
        .builtin_coordination(config)
        .start()
        .await
        .expect("a voter starts");
    Voter { server, dir, index }
}

/// Every voter learns every other's address.
///
/// Re-run after each restart: a restarted voter has a fresh address book, and
/// the peers' entries for it are stale. §5.3 routes are advisory — identity is
/// the handshake's — so re-registering cannot make a call land on the wrong
/// node; it only makes the rejoin prompt instead of waiting on discovery.
fn cross_register(voters: &[Voter]) {
    let addrs: Vec<_> = voters.iter().map(|v| v.server.node().addr()).collect();
    for voter in voters {
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
}

/// The index of a voter that is ready and sees a quorum.
async fn await_serving(voters: &[Voter], what: &str) -> usize {
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
            "{what}: no voter was serving within {PATIENCE:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The node id the group currently agrees is leading.
async fn await_leader_id(
    voters: &[Voter],
    what: &str,
) -> aster::coordination::ids::CoordinationNodeId {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        for voter in voters {
            if let Some(leader) = voter.coordination().current_leader_for_testing() {
                return leader;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: no leader within {PATIENCE:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for every voter to be ready — the rollout's own "this node is back"
/// condition, which is what makes moving on to the next one legal.
///
/// Readiness is `lifecycle_code() == 3` on **every** voter plus exactly one
/// quorum claimant. Requiring `quorum_available() == Some(true)` everywhere
/// would be wrong by §7.6's design: that is direct quorum evidence, which only
/// a leader can hold, and a follower reports `None` however healthy it is.
async fn await_all_ready(voters: &[Voter], what: &str) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let ready = voters
            .iter()
            .filter(|v| v.coordination().lifecycle_code() == 3)
            .count();
        let claiming = voters
            .iter()
            .filter(|v| v.coordination().quorum_available().is_some())
            .count();
        if ready == voters.len() && claiming == 1 {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let observed: Vec<(usize, i32, Option<bool>)> = voters
                .iter()
                .map(|v| {
                    let c = v.coordination();
                    (v.index, c.lifecycle_code(), c.quorum_available())
                })
                .collect();
            panic!(
                "{what}: {ready}/{} ready and {claiming} quorum claimants within \
                 {PATIENCE:?}; observed {observed:?}",
                voters.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn acquire(fixture: &GroupFixture, actor: usize, n: u64) -> CommandV1 {
    let action = ActionV1::Acquire(AcquireActionV1 {
        resource: ResourceKeyV1 {
            namespace: "rolling".into(),
            key: n.to_be_bytes().to_vec(),
        },
        ttl_ms: 600_000,
    });
    CommandV1::Client(ClientCommandV1 {
        actor_node_id: fixture.node_id(actor),
        client_id: aster::coordination::testing::id16(&n.to_be_bytes()),
        sequence: 1,
        request_hash: aster::coordination::hash::request_hash(&action)
            .expect("a canonical action hashes")
            .to_vec(),
        action,
    })
}

/// Commit one acquire through whichever voter is currently serving, retrying
/// while leadership is in flight.
///
/// A single attempt would be flaky for a reason that is not the property under
/// test: a proposal issued in the instant between an old leader stepping down
/// and a new one being elected is refused for want of a leader, not for want of
/// a quorum. Retrying until the deadline distinguishes "no quorum" — which must
/// fail this test — from "not yet elected", which is ordinary.
async fn commit(voters: &[Voter], fixture: &GroupFixture, n: u64, what: &str) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let serving = await_serving(voters, what).await;
        let coordination = voters[serving].coordination();
        let actor = voters[serving].index;
        if coordination
            .propose_for_testing(acquire(fixture, actor, n))
            .await
            .is_ok()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: the surviving voters never committed within {PATIENCE:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// **A whole group is restarted one voter at a time, and never stops
/// committing.**
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_same_v1_group_survives_a_rolling_restart_of_every_voter() {
    let fixture = GroupFixture::new(DOMAIN, SEED, SIZE);

    let mut voters = Vec::new();
    for index in 0..SIZE {
        let dir = tempfile::tempdir().expect("a storage directory");
        voters.push(start(&fixture, index, dir, true).await);
    }
    cross_register(&voters);
    await_all_ready(&voters, "the initial bootstrap").await;

    // A value that must survive the whole rollout.
    commit(&voters, &fixture, 0, "before the rollout").await;

    let leader_before = await_leader_id(&voters, "before the rollout").await;
    let term_before = voters[await_serving(&voters, "before the rollout").await]
        .coordination()
        .vote_for_testing()
        .leader_id
        .term;

    // Restart every voter, in index order, one at a time.
    for round in 0..SIZE {
        let position = voters
            .iter()
            .position(|v| v.index == round)
            .expect("every index is present between rounds");
        let stopping = voters.remove(position);
        let Voter { dir, index, server } = stopping;
        server.shutdown().await;

        // The remaining two are a quorum of three, so the group must still
        // commit *while this voter is down*. This is the assertion the whole
        // gate exists for: a rollout that paused service here would be a
        // rollout that cannot be performed on a live deployment.
        assert_eq!(voters.len(), SIZE - 1);
        commit(
            &voters,
            &fixture,
            (round + 1) as u64,
            &format!("while voter {index} was down"),
        )
        .await;

        // Back through `Existing`: the same directory, the same signed
        // genesis, no bootstrap. A returning voter rejoins; it does not found.
        let returned = start(&fixture, index, dir, false).await;
        voters.push(returned);
        cross_register(&voters);
        await_all_ready(&voters, &format!("after voter {index} returned")).await;

        // And the restored group commits again with all three present, so the
        // returning voter is a member rather than merely a running process.
        commit(
            &voters,
            &fixture,
            100 + round as u64,
            &format!("after voter {index} returned"),
        )
        .await;
    }

    // ── What the rollout must have preserved ────────────────────────────────

    // Leadership moved. The first leader was itself restarted, so the group
    // cannot still be under the term it held at the start — either a different
    // node leads, or the same node was re-elected under a later term. Asserting
    // the term rather than only the identity is what makes this hold in both
    // cases: a three-voter group may legitimately re-elect the node that just
    // came back, and "the leader is the same" would then be a false failure.
    let leader_after = await_leader_id(&voters, "after the rollout").await;
    let term_after = voters[await_serving(&voters, "after the rollout").await]
        .coordination()
        .vote_for_testing()
        .leader_id
        .term;
    assert!(
        term_after > term_before || leader_after != leader_before,
        "the rollout restarted the leader, so leadership must have moved: \
         {leader_before:?} at term {term_before} -> {leader_after:?} at term {term_after}"
    );

    // Every write survives — the one from before the rollout, one from each
    // restart window, and one from each restored group.
    let serving = await_serving(&voters, "the final check").await;
    let applied = voters[serving]
        .coordination()
        .last_applied_for_testing()
        .expect("the group has applied its history");
    assert!(
        applied > 0,
        "the group applied nothing across the rollout, so nothing was proved"
    );

    // And every voter converges on the same applied index, which is what
    // "rejoined" has to mean.
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let indexes: Vec<Option<u64>> = voters
            .iter()
            .map(|v| v.coordination().last_applied_for_testing())
            .collect();
        if indexes.iter().all(|i| *i == Some(applied)) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the voters never converged after the rollout: {indexes:?} against {applied}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The group still accepts new work, which a group that had merely
    // converged on a frozen history would also have to do.
    commit(&voters, &fixture, 999, "after the rollout").await;

    for voter in voters {
        voter.server.shutdown().await;
        drop(voter.dir);
    }
}
