//! **Snapshot catch-up over the real network** (§S3-F, §11.4).
//!
//! ## The one path nothing else reaches
//!
//! `coordination_snapshot_stream.rs` drives an `install_snapshot` stream by
//! hand: it proves the receiver, the check order and the handoff. What it
//! cannot prove is that OpenRaft ever *decides* to send one, that the sender
//! builds the stream the receiver expects, or that a follower which took that
//! route ends up in the right state. Those need a follower the leader cannot
//! catch up any other way.
//!
//! Which is what this arranges, using the pinned §7.5 policy rather than a
//! relaxed one: `LogsSinceLast(10_000)` triggers the snapshot,
//! `max_in_snapshot_log_to_keep = 1_000` bounds what survives it, and a
//! follower stopped near index 1 is then behind the leader's purge point with
//! no log to replay. Weakening either constant to make the test cheaper would
//! be testing a configuration nothing runs.
//!
//! ## Why the applied index alone is not the assertion
//!
//! A catch-up by snapshot and a catch-up by log replay end identically: same
//! applied index, same state. The evidence that separates them is the
//! follower's own `snapshot` metric — it has one, and it covers the region it
//! could never have replayed — together with the leader having purged past
//! where the follower stopped, which is what made replay impossible.
//!
//! ## It takes about six minutes
//!
//! Twelve thousand real committed entries through a real Raft over real QUIC,
//! in a debug build with a durable log. That is the price of the pinned policy,
//! and it is not a hang. Every alternative costs the property: a smaller
//! trigger tests a configuration nothing runs, and writing entries straight to
//! the log would skip the replication this exists to exercise.
//!
//! ## It is also the S5-E measurement of a whole snapshot catch-up
//!
//! The gate prints the elapsed time of each phase under `--nocapture`, and the
//! catch-up phase is the operator-visible cost of a snapshot install: it starts
//! when the returned voter's process starts and stops when its applied index
//! reaches the leader's, with the snapshot-covers-the-purged-region assertion
//! standing behind it. It is the only measurement in the checkpoint that
//! includes OpenRaft *deciding* to send a snapshot, the sender building the
//! stream and the QUIC transfer, so it is the whole operation and not a
//! component.
//!
//! It is one sample, and it is reported as one. A run costs six minutes and
//! reshapes a real group, so there is no distribution here — the per-shape
//! distribution of the build and the install themselves is in
//! `coordination::storage::bench`, which prices the same two production calls
//! over every §12.1 shape without the network in front of them. Neither
//! measurement subsumes the other and the checkpoint quotes both.

#![cfg(feature = "coordination-testing")]

use std::time::{Duration, Instant};

use aster::coordination::contract::{
    AcquireActionV1, ActionV1, ClientCommandV1, CommandV1, ResourceKeyV1,
};
use aster::coordination::testing::GroupFixture;
use aster::rpc::AsterServer;

const DOMAIN: &str = "catchup.test";
const SEED: u8 = 0x2D;
const SIZE: usize = 3;

/// Past `SNAPSHOT_LOGS_SINCE_LAST` (10,000) plus `MAX_IN_SNAPSHOT_LOG_TO_KEEP`
/// (1,000), so the leader has both snapshotted and purged beyond a follower
/// that stopped at the start.
const ENTRIES: u64 = 12_000;

/// Proposals in flight at once.
///
/// Sequential writes would each cost a full round trip and this would take
/// minutes. OpenRaft batches concurrent proposals into append entries
/// (`max_payload_entries` is 28), so the concurrency is what makes the pinned
/// 10,000-entry policy testable at all rather than a reason to weaken it.
const IN_FLIGHT: usize = 64;

const PATIENCE: Duration = Duration::from_secs(120);

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

fn acquire(fixture: &GroupFixture, index: usize, n: u64) -> CommandV1 {
    let action = ActionV1::Acquire(AcquireActionV1 {
        resource: ResourceKeyV1 {
            namespace: "catchup".into(),
            key: n.to_be_bytes().to_vec(),
        },
        ttl_ms: 600_000,
    });
    CommandV1::Client(ClientCommandV1 {
        actor_node_id: fixture.node_id(index),
        client_id: aster::coordination::testing::id16(&n.to_be_bytes()),
        sequence: 1,
        request_hash: aster::coordination::hash::request_hash(&action)
            .expect("a canonical action hashes")
            .to_vec(),
        action,
    })
}

/// **A follower behind the leader's purge point is caught up by a snapshot.**
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_stranded_follower_catches_up_by_snapshot() {
    let fixture = GroupFixture::new(DOMAIN, SEED, SIZE);

    let mut voters = Vec::new();
    for index in 0..SIZE {
        let dir = tempfile::tempdir().expect("a storage directory");
        voters.push(start(&fixture, index, dir, true).await);
    }
    cross_register(&voters);

    let leader = await_leader(&voters, "before the writes").await;
    let stranded = (0..SIZE).find(|i| *i != leader).expect("a follower");

    // Stop the follower and keep its directory. Graceful, deliberately: this
    // gate is about being *behind*, and the abrupt-loss case has its own gate
    // in `coordination_process_loss.rs`. Mixing them would leave a failure
    // ambiguous between the two.
    let stopped = voters.remove(stranded);
    let stopped_at = stopped.coordination().last_applied_for_testing();
    stopped.server.shutdown().await;
    let Voter { dir, index, .. } = stopped;

    // The leader is now `leader`'s old index shifted by the removal; find it
    // again rather than adjusting arithmetic that would silently go wrong.
    let leader = await_leader(&voters, "after the follower stopped").await;
    let coordination = voters[leader].coordination();

    // Enough traffic to snapshot and purge. Two of three is a quorum, so the
    // remaining pair commits without the stranded voter.
    let actor = voters[leader].index;
    let writes_began = Instant::now();
    let mut next = 0u64;
    while next < ENTRIES {
        let end = (next + IN_FLIGHT as u64).min(ENTRIES);
        let batch: Vec<_> = (next..end)
            .map(|n| {
                let command = acquire(&fixture, actor, n);
                let coordination = coordination.clone();
                tokio::spawn(async move { coordination.propose_for_testing(command).await })
            })
            .collect();
        next = end;
        for handle in batch {
            handle
                .await
                .expect("the proposal task does not panic")
                .expect("the majority commits");
        }
    }

    let writes_took = writes_began.elapsed();

    // The leader must have snapshotted *and* purged past where the stranded
    // voter stopped — otherwise a replay would still be possible and a
    // successful catch-up would prove nothing about snapshots.
    //
    // The wait is timed separately from the writes so it cannot be mistaken for
    // part of them. It is **not** a build measurement and must not be quoted as
    // one: the policy fires on the write that crosses the trigger, so the build
    // and the purge run concurrently with the remaining writes and this loop
    // usually finds the condition already true. The per-shape build cost is
    // measured in `coordination::storage::bench`.
    let snapshot_wait_began = Instant::now();
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let purged = coordination.purged_index_for_testing();
        let snapshot = coordination.snapshot_index_for_testing();
        if snapshot.is_some() && purged > stopped_at {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the leader never purged past the stranded follower: \
             snapshot {snapshot:?}, purged {purged:?}, stranded at {stopped_at:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let snapshot_wait_took = snapshot_wait_began.elapsed();
    let purged = coordination
        .purged_index_for_testing()
        .expect("the leader purged");
    let target = coordination
        .last_applied_for_testing()
        .expect("the leader applied");

    // The catch-up clock starts here, before the process does. What an operator
    // waits through is the whole thing: the voter starting, OpenRaft deciding a
    // snapshot is the only route, the sender building and streaming it, and the
    // receiver installing it. Starting the clock after the process was up would
    // exclude a cost the operator still pays.
    let catchup_began = Instant::now();

    // Bring it back onto its own store, and re-register addresses so the dial
    // succeeds without waiting for discovery.
    voters.push(start(&fixture, index, dir, false).await);
    cross_register(&voters);
    let returned = voters.last().expect("the returned voter");

    // The loop yields the elapsed time rather than assigning into a binding
    // declared outside it: the only way out is the branch that observed the
    // catch-up, so there is no path on which the duration is unset and none on
    // which it belongs to a different observation.
    let deadline = tokio::time::Instant::now() + PATIENCE;
    let catchup_took = loop {
        let c = returned.coordination();
        let applied = c.last_applied_for_testing();
        if applied >= Some(target) {
            let elapsed = catchup_began.elapsed();
            // The evidence that it came by snapshot: the follower holds one,
            // and it covers a region the leader had already purged, so no
            // replay could have delivered it.
            let snapshot = c
                .snapshot_index_for_testing()
                .expect("a follower caught up past a purge point holds a snapshot");
            assert!(
                snapshot >= purged,
                "the follower's snapshot must cover the purged region it could \
                 not have replayed: snapshot {snapshot}, purged {purged}"
            );
            break elapsed;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the returned follower never caught up: applied {applied:?}, target {target}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    // S5-E. One sample, labelled as one — see the module header for why there
    // is no distribution here and where the distribution lives.
    let installed = returned
        .coordination()
        .snapshot_index_for_testing()
        .expect("the caught-up follower holds a snapshot");
    println!(
        "\nS5-E — whole snapshot catch-up over the real network (n=1)\n\
         {}\n  \
         entries written                {ENTRIES} in {writes_took:.3?}\n  \
         leader snapshot + purge wait   {snapshot_wait_took:.3?} (near-zero means the leader had already \
snapshotted and purged before the writes finished — not a build measurement)\n  \
         stranded at index              {stopped_at:?}\n  \
         leader purged through          {purged}\n  \
         catch-up target index          {target}\n  \
         follower snapshot index        {installed}\n  \
         WHOLE CATCH-UP                 {catchup_took:.3?}  (process start → applied == target, by snapshot)\n",
        "-".repeat(78),
    );

    for voter in voters {
        voter.server.shutdown().await;
    }
}
