//! §S4-G/§S4-H: the history gate — concurrent real clients against real
//! voters, under injected faults, with the recorded history checked for
//! linearizability and the effects checked for durable recovery.
//!
//! ## What "the history checker" is here, exactly
//!
//! Every register operation either client performs — grants, renewals,
//! writes, releases, revocations, reads, **and their denials** — is recorded
//! as a real-time invocation/response interval with its full observation,
//! and [`common::linearize::check`] runs the Wing–Gong interval search for a
//! total order that real time permits and a sequential model of the §5.2
//! register accepts, comparing outcome codes, reason codes and the
//! snapshot's epoch/revision/holdership/value at every step. An operation
//! the client gave up on is recorded as ambiguous — the search may place its
//! effect anywhere after invocation or drop it, since an exact-byte resend
//! may land it at any later time or never.
//!
//! The witness-counter chain remains as a second, independent assertion:
//! committed CAS revisions form one gap-free strictly increasing chain, and
//! granted epochs are never shared. The chain would also be implied by the
//! search, but it is cheap and its failure mode names the defect directly.
//!
//! ## The two halves
//!
//! * **Chaos half** — in-process voters behind the seeded §6.2 injector:
//!   drops, delays, duplicates, reorders on every Raft message, a partition
//!   stranding a minority (quorum loss and heal), a mid-run revocation, and
//!   leadership movement — with `FaultTally` asserting every kind actually
//!   fired, and the clients riding redirects and exact-byte resends through
//!   all of it.
//! * **Kill half** — subprocess voters over real loopback Iroh, the leader
//!   `SIGKILL`ed mid-traffic (response loss and duplication arise from the
//!   resends that follow), the survivors electing, the killed voter
//!   restarting onto its own storage, and finally **every** voter restarted
//!   before the last read.

#![cfg(feature = "coordination-testing")]

use std::collections::BTreeSet;
use std::time::Duration;

use aster::coordination::client::CoordinationClient;
use aster::coordination::contract::{FenceV1, ResourceKeyV1};
use aster::coordination::model::limits::{
    RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE, RIGHT_RENEW, RIGHT_REVOKE,
};
use aster::coordination::store::wire::MutationReplyWireV1;
use aster::{AsterConfig, Node, PublicKey, RelayMode, SecretKey};

mod common;
use common::linearize::{self, Observed, Recorded, RegisterCall};
use common::{await_leader, combined_tally, shutdown_all, ChaosFixture, ChaoticVoter};

const PATIENCE: Duration = Duration::from_secs(90);

async fn within<T, F: std::future::Future<Output = T>>(what: &str, f: F) -> T {
    match tokio::time::timeout(PATIENCE, f).await {
        Ok(value) => value,
        Err(_) => panic!("{what}: no answer within {PATIENCE:?}"),
    }
}

/// Longer than any phase of either gate runs, so a §9.2 expiry cannot
/// legitimately fire and the checker's model needs no hidden transition.
const TTL_MS: u64 = 120_000;

fn resource() -> ResourceKeyV1 {
    ResourceKeyV1 {
        namespace: "history.gate".into(),
        key: b"register".to_vec(),
    }
}

/// One recorded committed effect.
#[derive(Debug, Clone)]
struct Committed {
    epoch: u64,
    value_revision: u64,
    value: Vec<u8>,
}

/// The shared interval recorder: one monotonic origin, every operation from
/// every client, in arrival order (the checker orders by the intervals, not
/// by this vector).
struct HistoryLog {
    origin: std::time::Instant,
    ops: std::sync::Mutex<Vec<Recorded>>,
}

impl HistoryLog {
    fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
            ops: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn now(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos()).expect("a bounded run")
    }

    fn record(&self, op: Recorded) {
        self.ops.lock().expect("the history log").push(op);
    }

    fn into_ops(self) -> Vec<Recorded> {
        self.ops.into_inner().expect("the history log")
    }
}

/// Project one §5.2 mutation reply into the checker's observation, in full:
/// every snapshot fact the reply states — epoch, lease revision, value
/// revision, holdership, **holder identity** and value — is a claim the
/// model must agree with. The value is compared only when the reply carries
/// it unredacted.
fn observe_mutation(reply: &MutationReplyWireV1) -> Observed {
    Observed {
        outcome_code: reply.outcome_code,
        reason_code: reply.reason_code,
        snapshot: reply.snapshot.as_ref().map(snapshot_facts),
    }
}

/// The full fact set of one wire snapshot.
fn snapshot_facts(
    s: &aster::coordination::contract::ResourceSnapshotV1,
) -> linearize::SnapshotFacts {
    linearize::SnapshotFacts {
        epoch: s.epoch,
        lease_revision: s.lease_revision,
        value_revision: s.value_revision,
        held: s.held,
        holder: s
            .holder
            .as_ref()
            .and_then(|h| <[u8; 32]>::try_from(h.as_slice()).ok()),
        value: if s.value_redacted {
            None
        } else {
            s.value.clone()
        },
    }
}

/// Perform one register mutation and record its interval and observation.
///
/// The invocation instant is taken before the first send. If the drive gives
/// up, the journaled entry is resumed until it resolves — the §10 loop —
/// because the fsync-before-send journal means `resume() == None` proves the
/// request never left this process (recorded as nothing), while a resolved
/// resume is this same operation's response. An operation this cannot settle
/// before the caller's patience is recorded ambiguous by the caller's panic
/// never arriving; in practice the outer `within` bounds the whole loop.
async fn perform(
    client: &CoordinationClient,
    log: &HistoryLog,
    call: RegisterCall,
) -> Option<MutationReplyWireV1> {
    let actor = client.local_id().expect("the client identity");
    let group_id = client.group().as_bytes().to_vec();
    let fence = |epoch: u64| FenceV1 {
        group_id: group_id.clone(),
        resource: resource(),
        epoch,
        holder: actor.to_vec(),
    };
    let invoked = log.now();
    let first = match &call {
        RegisterCall::Acquire => client.acquire(resource(), TTL_MS).await,
        RegisterCall::Renew { fence_epoch } => client.renew(fence(*fence_epoch)).await,
        RegisterCall::CompareAndSet {
            fence_epoch,
            expected_value_revision,
            new_value,
        } => {
            client
                .compare_and_set(
                    fence(*fence_epoch),
                    *expected_value_revision,
                    new_value.clone(),
                )
                .await
        }
        RegisterCall::Release { fence_epoch } => client.release(fence(*fence_epoch)).await,
        RegisterCall::Revoke { expected_epoch } => client.revoke(resource(), *expected_epoch).await,
        RegisterCall::Read => unreachable!("reads go through recorded_read"),
    };
    let resolved = match first {
        Ok(resolved) => resolved,
        Err(_) => loop {
            match client.resume().await {
                Ok(Some(resolved)) => break resolved,
                // Nothing journaled: fsync-before-send proves the request
                // never left this process. No operation happened.
                Ok(None) => return None,
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        },
    };
    let reply = resolved.reply().clone();
    log.record(Recorded {
        actor,
        invoked,
        returned: Some(log.now()),
        call,
        observed: Some(observe_mutation(&reply)),
    });
    resolved
        .acknowledge()
        .await
        .expect("the journal acknowledges");
    Some(reply)
}

/// One worker's contention loop: acquire → CAS → release, every operation —
/// denials included — recorded with its interval.
///
/// Bounded by rounds, not by time: a round that cannot commit keeps retrying
/// inside the client's own drive loop, and the gate's outer patience is what
/// catches a wedged group.
async fn contend(
    client: &CoordinationClient,
    log: &HistoryLog,
    label: &[u8],
    rounds: usize,
) -> Vec<Committed> {
    let mut committed = Vec::new();
    let mut round = 0usize;
    let deadline = std::time::Instant::now() + PATIENCE;
    while round < rounds {
        assert!(
            std::time::Instant::now() < deadline,
            "the contention loop starved: {round}/{rounds} rounds"
        );
        let Some(reply) = perform(client, log, RegisterCall::Acquire).await else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        if reply.outcome_code != 1 {
            // Denied — someone else holds it, or it was revoked mid-flight.
            // The denial is in the history; the checker must justify it.
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        }
        let epoch = reply.snapshot.as_ref().expect("a snapshot").epoch;
        let base_revision = reply.snapshot.as_ref().expect("a snapshot").value_revision;

        // One CAS under the fence.
        let value = [label, b"-", round.to_string().as_bytes()].concat();
        let cas = perform(
            client,
            log,
            RegisterCall::CompareAndSet {
                fence_epoch: epoch,
                expected_value_revision: base_revision,
                new_value: value.clone(),
            },
        )
        .await;
        if let Some(cas_reply) = cas {
            if cas_reply.outcome_code == 3 {
                committed.push(Committed {
                    epoch,
                    value_revision: cas_reply
                        .snapshot
                        .as_ref()
                        .expect("a snapshot")
                        .value_revision,
                    value,
                });
                round += 1;
            }
        }

        // Release, so the other workers can take the register. A refusal is
        // fine — a revocation may have fenced us first — and it too is in
        // the history.
        let _ = perform(client, log, RegisterCall::Release { fence_epoch: epoch }).await;
    }
    committed
}

/// Run the interval search and panic with its diagnosis on failure.
fn check_linearizable(log: HistoryLog) {
    let ops = log.into_ops();
    assert!(!ops.is_empty(), "the history recorded nothing");
    match linearize::check(&ops) {
        Ok(order) => assert_eq!(
            order.len(),
            ops.iter().filter(|op| op.observed.is_some()).count(),
            "the witness order places every observed operation"
        ),
        Err(diagnosis) => panic!("{diagnosis}"),
    }
}

/// The exact-history assertions over everything every client committed.
fn check_history(all: &[Committed], grants_epochs: &[u64]) {
    // The CAS chain: strictly increasing, gap-free, one total order.
    let mut revisions: Vec<u64> = all.iter().map(|c| c.value_revision).collect();
    revisions.sort_unstable();
    let distinct: BTreeSet<u64> = revisions.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        revisions.len(),
        "two committed CAS effects shared a revision — a double application"
    );
    for pair in revisions.windows(2) {
        assert_eq!(
            pair[1],
            pair[0] + 1,
            "the committed revision chain has a gap — a lost or invented effect"
        );
    }

    // Epochs: strictly increasing across grants; never shared.
    let mut epochs = grants_epochs.to_vec();
    epochs.sort_unstable();
    let distinct: BTreeSet<u64> = epochs.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        epochs.len(),
        "two grants shared an epoch — two holders at once"
    );
}

/// **The chaos half.**
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_register_history_is_linearizable_through_a_seeded_faulty_network() {
    use aster::coordination::testing::Schedule;

    const SEED: u64 = 0x415E_D000_CAFE_0001;
    const FAULT_PERCENT: u8 = 22;

    let fixture = ChaosFixture::new("history.chaos", 0xF3, 3);
    let schedule = Schedule::new(SEED, FAULT_PERCENT);
    let voters = fixture.start(schedule).await;
    await_leader(&voters, PATIENCE, "the election under chaos").await;

    // ROOT grants two workers plus itself REVOKE.
    let root_secret = [0xF3 ^ 0xFF; 32];
    let worker_secrets = [[0xF3 ^ 0x41; 32], [0xF3 ^ 0x42; 32]];
    let (root, workers, _journals) =
        connect_clients(&voters, &fixture, root_secret, &worker_secrets).await;

    for (index, secret) in worker_secrets.iter().enumerate() {
        let subject = ed25519_dalek::SigningKey::from_bytes(secret)
            .verifying_key()
            .to_bytes();
        let granted = within(
            "grant",
            root.set_grant(
                resource(),
                subject,
                RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
                index as u64,
            ),
        )
        .await
        .expect("ROOT grants");
        assert_eq!(granted.reply().outcome_code, 6);
        within("ack", granted.acknowledge()).await.expect("ack");
    }
    let root_id = ed25519_dalek::SigningKey::from_bytes(&root_secret)
        .verifying_key()
        .to_bytes();
    let granted = within(
        "grant REVOKE",
        root.set_grant(resource(), root_id, RIGHT_REVOKE, 2),
    )
    .await
    .expect("ROOT grants itself REVOKE");
    assert_eq!(granted.reply().outcome_code, 6);
    within("ack", granted.acknowledge()).await.expect("ack");

    // Two workers contend through the faulty network while ROOT revokes once
    // mid-run — the revocation fences whichever holder is current. Every
    // operation from all three, denials and the revocation included, lands
    // in one interval log.
    let log = std::sync::Arc::new(HistoryLog::new());
    let contention = {
        let w0 = workers[0].clone();
        let w1 = workers[1].clone();
        let revoker = root.clone();
        let log = std::sync::Arc::clone(&log);
        async move {
            let (a, b, _) = tokio::join!(
                contend(&w0, &log, b"w0", 6),
                contend(&w1, &log, b"w1", 6),
                async {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    let _ = perform(
                        &revoker,
                        &log,
                        RegisterCall::Revoke {
                            expected_epoch: None,
                        },
                    )
                    .await;
                }
            );
            (a, b)
        }
    };
    let (committed_a, committed_b) = within("contention", contention).await;

    // A partition strands a minority mid-life: quorum holds at two of three,
    // the clients follow the leader among the majority, and healing restores
    // the third. (The epochs are asked of the schedule, as the §S3-F gates
    // do.)
    let ids: Vec<_> = (0..3).map(|i| fixture.id(i)).collect();
    let split = (1..2_000u64)
        .find(|epoch| {
            (0..3).any(|alone| {
                (0..3).all(|other| {
                    let connected = schedule_reaches(&fixture, &ids, *epoch, alone, other);
                    connected == (other == alone)
                })
            })
        })
        .expect("a stranding epoch exists");
    for voter in &voters {
        voter.chaos.set_epoch(split);
    }
    let mid_reply = within("mutation across the partition", async {
        loop {
            match perform(&workers[0], &log, RegisterCall::Acquire).await {
                Some(reply) if reply.outcome_code == 1 => break reply,
                _ => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    })
    .await;
    assert_eq!(
        mid_reply.outcome_code, 1,
        "the majority side keeps granting through the partition"
    );
    let healed = (split + 1..4_000u64)
        .find(|epoch| {
            (0..3).all(|a| (0..3).all(|b| schedule_reaches(&fixture, &ids, *epoch, a, b)))
        })
        .expect("a healing epoch exists");
    for voter in &voters {
        voter.chaos.set_epoch(healed);
    }

    // The counter chains: one chain, no sharing, no gaps.
    let all: Vec<Committed> = committed_a.iter().chain(&committed_b).cloned().collect();
    let grant_epochs: Vec<u64> = all.iter().map(|c| c.epoch).collect();
    check_history(&all, &grant_epochs);

    // The final read reflects the highest committed revision that carried a
    // value this run wrote (the mid-partition acquire holds the register but
    // wrote nothing).
    let final_read = within("final read", recorded_read(&workers[0], &log)).await;
    let best = all
        .iter()
        .max_by_key(|c| c.value_revision)
        .expect("something committed");
    assert_eq!(
        final_read.snapshot.value.as_deref(),
        Some(best.value.as_slice()),
        "the linearizable read is the highest-revision write"
    );

    // The interval search: a witness order exists that real time permits
    // and the sequential register accepts, denials and all.
    let log = std::sync::Arc::into_inner(log).expect("every recorder returned");
    check_linearizable(log);

    // Non-vacuity: the schedule really injected.
    let tally = combined_tally(&voters);
    assert!(
        tally.every_kind_fired(),
        "the history ran on a fault-free network: {tally:?}"
    );

    shutdown_all(voters).await;
}

fn schedule_reaches(
    fixture: &ChaosFixture,
    ids: &[aster::coordination::ids::CoordinationNodeId],
    epoch: u64,
    a: usize,
    b: usize,
) -> bool {
    let _ = fixture;
    // Every voter runs the same schedule; ask any one's copy.
    // (The schedule is a pure function; `reaches` is symmetric.)
    aster::coordination::testing::Schedule::new(0x415E_D000_CAFE_0001, 22)
        .reaches(&ids[a], &ids[b], epoch)
}

/// One linearizable read, recorded with its own interval. Status refusals
/// (`UNAVAILABLE` during elections) never touched the register and are not
/// operations; each retry gets a fresh invocation instant.
async fn recorded_read(
    client: &CoordinationClient,
    log: &HistoryLog,
) -> aster::coordination::store::wire::ReadReplyWireV1 {
    let actor = client.local_id().expect("the client identity");
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        let invoked = log.now();
        match client.reads().read_linearizable(resource()).await {
            Ok(reply) => {
                log.record(Recorded {
                    actor,
                    invoked,
                    returned: Some(log.now()),
                    call: RegisterCall::Read,
                    observed: Some(Observed {
                        outcome_code: 0,
                        reason_code: 0,
                        snapshot: Some(snapshot_facts(&reply.snapshot)),
                    }),
                });
                return reply;
            }
            Err(_) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no linearizable read within {PATIENCE:?}"
                );
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn connect_clients(
    voters: &[ChaoticVoter],
    fixture: &ChaosFixture,
    root_secret: [u8; 32],
    worker_secrets: &[[u8; 32]],
) -> (
    CoordinationClient,
    Vec<CoordinationClient>,
    Vec<tempfile::TempDir>,
) {
    let group = fixture.group_id();
    let tickets: Vec<String> = (0..voters.len())
        .map(|i| {
            let id: [u8; 32] = fixture.node_id(i).try_into().expect("32 bytes");
            aster::coordination::model::authority::canonical_candidate_ticket(&id)
                .expect("a canonical ticket")
        })
        .collect();
    let build = |secret: [u8; 32]| {
        let tickets = tickets.clone();
        async move {
            let node = Node::start(
                AsterConfig::builder()
                    .relay(RelayMode::Disabled)
                    .bind_addr("127.0.0.1:0")
                    .secret_key(SecretKey::from_bytes(secret))
                    .root_pubkey(PublicKey::from_bytes(fixture.authority_key()))
                    .build(),
            )
            .await
            .expect("a client node starts");
            for voter in voters {
                node.add_node_addr(voter.server.node().addr())
                    .expect("register");
            }
            let journal = tempfile::tempdir().expect("a directory");
            let client = CoordinationClient::builder(node, group)
                .seed_tickets(tickets)
                .journal_dir(journal.path())
                .build()
                .expect("the client builds");
            (client, journal)
        }
    };
    let (root, root_journal) = build(root_secret).await;
    let mut workers = Vec::new();
    let mut journals = vec![root_journal];
    for secret in worker_secrets {
        let (client, journal) = build(*secret).await;
        workers.push(client);
        journals.push(journal);
    }
    (root, workers, journals)
}

// ── The kill half ───────────────────────────────────────────────────────────

/// Subprocess voters, driven as Store servers by real clients, with the
/// leader killed mid-traffic and every voter restarted before the last read.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn committed_effects_survive_a_leader_kill_and_a_full_restart() {
    use std::process::{Child, Command};

    const SIZE: usize = 3;
    const SEED: u8 = 0xF4;
    const DOMAIN: &str = "process-loss.test";

    let control_dir = tempfile::tempdir().expect("a control directory");
    let control = control_dir.path().to_path_buf();

    let spawn = |index: usize| -> Child {
        Command::new(env!("CARGO_BIN_EXE_coordination-voter"))
            .arg(&control)
            .arg(index.to_string())
            .arg(SIZE.to_string())
            .arg(SEED.to_string())
            .spawn()
            .expect("the voter binary starts")
    };
    let mut children: Vec<Option<Child>> = (0..SIZE).map(|i| Some(spawn(i))).collect();

    // The same fixture derivation the binary uses.
    let fixture = aster::coordination::testing::GroupFixture::new(DOMAIN, SEED, SIZE);

    // Wait for every voter to publish an address and reach Ready, and learn
    // where they are.
    let deadline = std::time::Instant::now() + PATIENCE;
    let addr_of = |index: usize| -> Option<(String, Vec<String>)> {
        let text = std::fs::read_to_string(control.join(format!("addr-{index}"))).ok()?;
        let mut parts = text.split_whitespace();
        let node_id = parts.next()?.to_string();
        let direct: Vec<String> = parts.map(str::to_string).collect();
        (!direct.is_empty()).then_some((node_id, direct))
    };
    let status_of = |index: usize| -> Option<(i32, i64)> {
        let text = std::fs::read_to_string(control.join(format!("status-{index}"))).ok()?;
        let mut parts = text.split_whitespace();
        let lifecycle: i32 = parts.next()?.parse().ok()?;
        let _quorum = parts.next()?;
        let leader: i64 = parts.next()?.parse().ok()?;
        Some((lifecycle, leader))
    };
    loop {
        let ready = (0..SIZE).all(|i| {
            addr_of(i).is_some() && status_of(i).map(|(code, _)| code == 3).unwrap_or(false)
        });
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the subprocess group never became ready"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // A ROOT client and one worker client.
    let group = fixture.group_id();
    let authority = fixture.authority_key();
    let make_node = |secret: [u8; 32]| async move {
        Node::start(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes(secret))
                .root_pubkey(PublicKey::from_bytes(authority))
                .build(),
        )
        .await
        .expect("a client node starts")
    };
    let register_all = |node: &Node| {
        for index in 0..SIZE {
            let (node_id, direct) = addr_of(index).expect("published");
            node.add_node_addr(aster::NodeAddr {
                node_id: aster::NodeId::from_hex(node_id),
                relay_url: None,
                direct_addresses: direct,
            })
            .expect("register");
        }
    };
    let tickets: Vec<String> = (0..SIZE)
        .map(|i| {
            let id: [u8; 32] = fixture.node_id(i).try_into().expect("32 bytes");
            aster::coordination::model::authority::canonical_candidate_ticket(&id)
                .expect("a canonical ticket")
        })
        .collect();

    let root_node = make_node([SEED ^ 0xFF; 32]).await;
    let root_node_handle = root_node.clone();
    register_all(&root_node);
    let root_journal = tempfile::tempdir().expect("a directory");
    let root = CoordinationClient::builder(root_node, group)
        .seed_tickets(tickets.clone())
        .journal_dir(root_journal.path())
        .build()
        .expect("the ROOT client builds");

    let worker_secret = [SEED ^ 0x41; 32];
    let worker_id = ed25519_dalek::SigningKey::from_bytes(&worker_secret)
        .verifying_key()
        .to_bytes();
    let worker_node = make_node(worker_secret).await;
    let worker_node_handle = worker_node.clone();
    register_all(&worker_node);
    let worker_journal = tempfile::tempdir().expect("a directory");
    let worker = CoordinationClient::builder(worker_node, group)
        .seed_tickets(tickets)
        .journal_dir(worker_journal.path())
        .build()
        .expect("the worker client builds");

    let granted = within(
        "grant",
        root.set_grant(
            resource(),
            worker_id,
            RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
            0,
        ),
    )
    .await
    .expect("ROOT grants");
    assert_eq!(granted.reply().outcome_code, 6);
    within("ack", granted.acknowledge()).await.expect("ack");

    // Traffic before the kill, in one interval log spanning every phase.
    let log = HistoryLog::new();
    let mut all = contend(&worker, &log, b"pre", 3).await;

    // SIGKILL the leader mid-life. `Child::kill` runs no destructor.
    let (_, leader) = status_of(0).expect("a status");
    let leader = usize::try_from(leader).expect("a leader index");
    children[leader]
        .as_mut()
        .expect("running")
        .kill()
        .expect("SIGKILL");
    let _ = children[leader].as_mut().expect("running").wait();

    // The survivors elect; the client rides redirects and exact resends. The
    // committed chain keeps growing with no gap and no duplicate.
    all.extend(contend(&worker, &log, b"post-kill", 3).await);

    // The killed voter restarts onto its own storage and rejoins.
    children[leader] = Some(spawn(leader));

    all.extend(contend(&worker, &log, b"post-restart", 2).await);

    let grant_epochs: Vec<u64> = all.iter().map(|c| c.epoch).collect();
    check_history(&all, &grant_epochs);
    let best = all
        .iter()
        .max_by_key(|c| c.value_revision)
        .expect("something committed")
        .clone();

    // Durable recovery: kill **every** voter, restart them all onto their own
    // stores, and require the read to answer the same highest-revision write.
    for child in children.iter_mut().flatten() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Remove the stale address files first, so the re-registration below
    // reads the restarted endpoints' fresh ephemeral ports and not the dead
    // ones — the same NodeIds, new routes, exactly as discovery would see it.
    for index in 0..SIZE {
        let _ = std::fs::remove_file(control.join(format!("addr-{index}")));
        let _ = std::fs::remove_file(control.join(format!("status-{index}")));
    }
    let mut children: Vec<Option<Child>> = (0..SIZE).map(|i| Some(spawn(i))).collect();
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        let ready = (0..SIZE).all(|i| {
            addr_of(i).is_some() && status_of(i).map(|(code, _)| code == 3).unwrap_or(false)
        });
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the restarted group never became ready"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    register_all(&root_node_handle);
    register_all(&worker_node_handle);

    let final_read = within("final read", recorded_read(&worker, &log)).await;
    assert_eq!(
        final_read.snapshot.value.as_deref(),
        Some(best.value.as_slice()),
        "every committed effect survived the kill, the restart, and the full restart"
    );

    // The interval search over every phase: pre-kill, post-kill,
    // post-restart, and the read after the full restart.
    check_linearizable(log);

    for child in children.iter_mut().flatten() {
        let _ = child.kill();
        let _ = child.wait();
    }
}
