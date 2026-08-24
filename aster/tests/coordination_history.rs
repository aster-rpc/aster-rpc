//! §S4-G/§S4-H: the history gate — concurrent real clients against real
//! voters, under injected faults, with the recorded history checked for
//! linearizability and the effects checked for durable recovery.
//!
//! ## What "the history checker" is here, exactly
//!
//! The §5.2 register exposes its linearization witness in every reply: a
//! committed `compare_and_set` carries the `value_revision` it produced, an
//! `acquire` carries the `epoch` it was granted at, and both counters are
//! replicated state that only committed commands move. Linearizability of
//! this object is therefore checkable **exactly**, not approximately:
//!
//! * the committed CAS revisions across every client form a strictly
//!   increasing, gap-free chain — one total order, no lost effect, no doubly
//!   applied effect (a duplicated or resent command that applied twice would
//!   mint a second revision and break the count);
//! * granted epochs strictly increase across grants and revocations — two
//!   holders can never share an epoch;
//! * the final linearizable read equals the highest-revision write — and
//!   still does after every voter is restarted onto its own storage, which is
//!   the durable-recovery half of the claim.
//!
//! A general interval-order search (Wing–Gong) is unnecessary for an object
//! that returns its linearization point; asserting the chain **is** the
//! linearizability check, and it is sensitive to exactly the failures the
//! §S4-H list names.
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
use common::{await_leader, combined_tally, shutdown_all, ChaosFixture, ChaoticVoter};

const PATIENCE: Duration = Duration::from_secs(90);

async fn within<T, F: std::future::Future<Output = T>>(what: &str, f: F) -> T {
    match tokio::time::timeout(PATIENCE, f).await {
        Ok(value) => value,
        Err(_) => panic!("{what}: no answer within {PATIENCE:?}"),
    }
}

fn resource() -> ResourceKeyV1 {
    ResourceKeyV1 {
        namespace: "history.gate".into(),
        key: b"register".to_vec(),
    }
}

fn fence_of(reply: &MutationReplyWireV1) -> FenceV1 {
    let snapshot = reply.snapshot.as_ref().expect("a snapshot");
    FenceV1 {
        group_id: snapshot.group_id.clone(),
        resource: snapshot.resource.clone(),
        epoch: snapshot.epoch,
        holder: snapshot.holder.clone().expect("held"),
    }
}

/// One recorded committed effect.
#[derive(Debug, Clone)]
struct Committed {
    epoch: u64,
    value_revision: u64,
    value: Vec<u8>,
}

/// One worker's contention loop: acquire → CAS → release, retrying denied
/// acquires, riding redirects and resends, recording every committed effect.
///
/// Bounded by rounds, not by time: a round that cannot commit keeps retrying
/// inside the client's own drive loop, and the gate's outer patience is what
/// catches a wedged group.
async fn contend(client: &CoordinationClient, label: &[u8], rounds: usize) -> Vec<Committed> {
    let mut committed = Vec::new();
    let mut round = 0usize;
    let deadline = std::time::Instant::now() + PATIENCE;
    while round < rounds {
        assert!(
            std::time::Instant::now() < deadline,
            "the contention loop starved: {round}/{rounds} rounds"
        );
        // Acquire, tolerating HeldByOther and transient unavailability.
        let acquired = match client.acquire(resource(), 30_000).await {
            Ok(resolved) => resolved,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let reply = acquired.reply().clone();
        acquired.acknowledge().await.expect("acknowledge");
        if reply.outcome_code != 1 {
            // Denied — someone else holds it, or it was revoked mid-flight.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        let epoch = reply.snapshot.as_ref().expect("a snapshot").epoch;
        let base_revision = reply.snapshot.as_ref().expect("a snapshot").value_revision;
        let fence = fence_of(&reply);

        // One CAS under the fence.
        let value = [label, b"-", round.to_string().as_bytes()].concat();
        let mutated = match client
            .compare_and_set(fence.clone(), base_revision, value.clone())
            .await
        {
            Ok(resolved) => resolved,
            Err(_) => {
                // Ambiguity exhausted the drive budget; resume until resolved.
                match client.resume().await {
                    Ok(Some(resolved)) => resolved,
                    _ => continue,
                }
            }
        };
        let cas_reply = mutated.reply().clone();
        mutated.acknowledge().await.expect("acknowledge");
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

        // Release, so the other workers can take the register. A refusal is
        // fine — a revocation may have fenced us first.
        if let Ok(resolved) = client.release(fence).await {
            let _ = resolved.acknowledge().await;
        }
    }
    committed
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
    // mid-run — the revocation fences whichever holder is current.
    let contention = {
        let w0 = workers[0].clone();
        let w1 = workers[1].clone();
        let revoker = root.clone();
        async move {
            let (a, b, _) =
                tokio::join!(contend(&w0, b"w0", 6), contend(&w1, b"w1", 6), async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if let Ok(resolved) = revoker.revoke(resource(), None).await {
                        let _ = resolved.acknowledge().await;
                    }
                });
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
    let mid_partition = within("mutation across the partition", async {
        loop {
            match workers[0].acquire(resource(), 30_000).await {
                Ok(resolved) => break resolved,
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    })
    .await;
    let mid_reply = mid_partition.reply().clone();
    within("ack", mid_partition.acknowledge())
        .await
        .expect("ack");
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

    // The history: one chain, no sharing, no gaps.
    let all: Vec<Committed> = committed_a.iter().chain(&committed_b).cloned().collect();
    let grant_epochs: Vec<u64> = all.iter().map(|c| c.epoch).collect();
    check_history(&all, &grant_epochs);

    // The final read reflects the highest committed revision that carried a
    // value this run wrote (the mid-partition acquire holds the register but
    // wrote nothing).
    let final_read = within("final read", root_read(&workers[0], resource())).await;
    let best = all
        .iter()
        .max_by_key(|c| c.value_revision)
        .expect("something committed");
    assert_eq!(
        final_read.snapshot.value.as_deref(),
        Some(best.value.as_slice()),
        "the linearizable read is the highest-revision write"
    );

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

async fn root_read(
    client: &CoordinationClient,
    resource: ResourceKeyV1,
) -> aster::coordination::store::wire::ReadReplyWireV1 {
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        match client.reads().read_linearizable(resource.clone()).await {
            Ok(reply) => return reply,
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

    // Traffic before the kill.
    let mut all = contend(&worker, b"pre", 3).await;

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
    all.extend(contend(&worker, b"post-kill", 3).await);

    // The killed voter restarts onto its own storage and rejoins.
    children[leader] = Some(spawn(leader));

    all.extend(contend(&worker, b"post-restart", 2).await);

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

    let final_read = within("final read", root_read(&worker, resource())).await;
    assert_eq!(
        final_read.snapshot.value.as_deref(),
        Some(best.value.as_slice()),
        "every committed effect survived the kill, the restart, and the full restart"
    );

    for child in children.iter_mut().flatten() {
        let _ = child.kill();
        let _ = child.wait();
    }
}
