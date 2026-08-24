//! §S4-D/§S4-E: the public `CoordinationClient` — journaled mutations,
//! durable redelivery across a process restart, the closing corridor over a
//! live group, and redirect routing from a follower seed.
//!
//! These are the client-side halves of the properties `coordination_store`
//! proves server-side. Every gate runs against real voters over real Iroh
//! RPC; the "restart" in the redelivery gate is a real drop-and-rebuild of
//! the client over the same journal directory, which is what a process
//! restart is to the journal.

#![cfg(feature = "coordination-testing")]

use std::time::Duration;

use aster::coordination::client::CoordinationClient;
use aster::coordination::contract::ResourceKeyV1;
use aster::coordination::ids::CoordinationGroupId;
use aster::coordination::model::limits::{
    RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE, RIGHT_RENEW,
};
use aster::coordination::testing::GroupFixture;
use aster::rpc::AsterServer;
use aster::{AsterConfig, Node, PublicKey, RelayMode, SecretKey};

const PATIENCE: Duration = Duration::from_secs(60);

async fn within<T, F: std::future::Future<Output = T>>(what: &str, f: F) -> T {
    match tokio::time::timeout(PATIENCE, f).await {
        Ok(value) => value,
        Err(_) => panic!("{what}: no answer within {PATIENCE:?}"),
    }
}

fn resource(key: &[u8]) -> ResourceKeyV1 {
    ResourceKeyV1 {
        namespace: "client.gate".into(),
        key: key.to_vec(),
    }
}

async fn start_voter(fixture: &GroupFixture, index: usize, dir: &std::path::Path) -> AsterServer {
    let voter = AsterServer::builder()
        .config(fixture.aster_config(index))
        .builtin_coordination(fixture.config(dir, index))
        .start()
        .await
        .expect("the voter starts");
    within(
        "voter readiness",
        voter.coordination().expect("hosted").wait_ready(),
    )
    .await
    .expect("the group becomes ready");
    voter
}

async fn client_node(secret: [u8; 32], fixture: &GroupFixture, voter: &AsterServer) -> Node {
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
    node.add_node_addr(voter.node().addr())
        .expect("register the voter's address");
    node
}

/// The canonical addressless ticket for voter `index` — exactly what an
/// operator would hand a client, and exactly what a permit carries: NodeId
/// only, routes from discovery.
fn voter_ticket(fixture: &GroupFixture, index: usize) -> String {
    let id: [u8; 32] = fixture.node_id(index).try_into().expect("32 bytes");
    aster::coordination::model::authority::canonical_candidate_ticket(&id)
        .expect("a canonical ticket")
}

fn group(fixture: &GroupFixture) -> CoordinationGroupId {
    fixture.group_id()
}

/// **The full journaled lifecycle, with redelivery across a restart.**
///
/// A mutation resolves, the process "dies" without acknowledging, and the
/// rebuilt client — same node identity, same journal directory — redelivers
/// the exact saved reply. Until it is acknowledged no new sequence is
/// permitted; after it, the next mutation proceeds on the same client id at
/// the next sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resolved_result_survives_a_client_restart_until_acknowledged() {
    let fixture = GroupFixture::new("client.test", 0xE1, 1);
    let dir = tempfile::tempdir().expect("a directory");
    let voter = start_voter(&fixture, 0, dir.path()).await;

    // ROOT grants the worker over its own durable client.
    let root_node = client_node([0xE1 ^ 0xFF; 32], &fixture, &voter).await;
    let root_journal = tempfile::tempdir().expect("a directory");
    let root = CoordinationClient::builder(root_node, group(&fixture))
        .seed_tickets([voter_ticket(&fixture, 0)])
        .journal_dir(root_journal.path())
        .build()
        .expect("the ROOT client builds");

    let worker_secret = [0xE1 ^ 0x41; 32];
    let worker_id = ed25519_dalek::SigningKey::from_bytes(&worker_secret)
        .verifying_key()
        .to_bytes();
    let granted = within(
        "grant",
        root.set_grant(
            resource(b"head"),
            worker_id,
            RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
            0,
        ),
    )
    .await
    .expect("ROOT grants");
    assert_eq!(granted.reply().outcome_code, 6);
    within("grant ack", granted.acknowledge())
        .await
        .expect("acknowledge");

    // The worker acquires and does NOT acknowledge.
    let worker_node = client_node(worker_secret, &fixture, &voter).await;
    let journal_dir = tempfile::tempdir().expect("a directory");
    let saved_reply;
    {
        let worker = CoordinationClient::builder(worker_node.clone(), group(&fixture))
            .seed_tickets([voter_ticket(&fixture, 0)])
            .journal_dir(journal_dir.path())
            .build()
            .expect("the worker client builds");

        let resolved = within("acquire", worker.acquire(resource(b"head"), 30_000))
            .await
            .expect("the acquire resolves");
        assert_eq!(resolved.reply().outcome_code, 1, "Granted");
        assert_eq!(resolved.reply().sequence, 1);
        saved_reply = resolved.reply().clone();

        // A second mutation while unacknowledged is refused by contract.
        let refused = worker.acquire(resource(b"other"), 30_000).await;
        assert!(
            refused.is_err(),
            "no new sequence until the application acknowledges"
        );
        // The process dies here: the handle drops unacknowledged.
        drop(resolved);
    }

    // "Restart": a new client over the same journal, same identity.
    let worker = CoordinationClient::builder(worker_node, group(&fixture))
        .seed_tickets([voter_ticket(&fixture, 0)])
        .journal_dir(journal_dir.path())
        .build()
        .expect("the journal reopens");

    let redelivered = within("unacknowledged", worker.unacknowledged())
        .await
        .expect("recovery reads the journal")
        .expect("the unacknowledged result is redelivered");
    assert_eq!(
        redelivered.reply(),
        &saved_reply,
        "redelivery is the exact saved reply"
    );
    within("redelivery ack", redelivered.acknowledge())
        .await
        .expect("acknowledge");

    // The journal is free: the next mutation runs at sequence 2 on the same
    // client id.
    let snapshot = saved_reply.snapshot.as_ref().expect("a snapshot");
    let fence = aster::coordination::contract::FenceV1 {
        group_id: snapshot.group_id.clone(),
        resource: snapshot.resource.clone(),
        epoch: snapshot.epoch,
        holder: snapshot.holder.clone().expect("held"),
    };
    let renewed = within("renew", worker.renew(fence))
        .await
        .expect("the renew resolves");
    assert_eq!(renewed.reply().outcome_code, 2, "Renewed");
    assert_eq!(renewed.reply().sequence, 2, "the same journal continued");
    within("renew ack", renewed.acknowledge())
        .await
        .expect("acknowledge");

    voter.shutdown().await;
}

/// **The closing corridor over a live group.** A terminal typed refusal —
/// ROOT targeting itself with `set_admin`, the immutable 26 — retires the
/// client id, replays on the tombstone, delivers the *original* terminal
/// reason, rotates, and the next mutation runs on a fresh client id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_refusal_retires_replays_delivers_and_rotates() {
    let fixture = GroupFixture::new("client.corridor", 0xE2, 1);
    let dir = tempfile::tempdir().expect("a directory");
    let voter = start_voter(&fixture, 0, dir.path()).await;

    let root_secret = [0xE2 ^ 0xFF; 32];
    let root_id = ed25519_dalek::SigningKey::from_bytes(&root_secret)
        .verifying_key()
        .to_bytes();
    let node = client_node(root_secret, &fixture, &voter).await;
    let journal_dir = tempfile::tempdir().expect("a directory");
    let root = CoordinationClient::builder(node, group(&fixture))
        .seed_tickets([voter_ticket(&fixture, 0)])
        .journal_dir(journal_dir.path())
        .build()
        .expect("the ROOT client builds");

    let before = root.client_id().await;

    // The §5.2 immutable-ROOT refusal: typed, NotConsumed, terminal (26).
    let resolved = within("set_admin(ROOT)", root.set_admin(root_id, false, 1))
        .await
        .expect("the corridor resolves the terminal refusal");
    assert_eq!(
        (
            resolved.reply().disposition_code,
            resolved.reply().outcome_code,
            resolved.reply().reason_code
        ),
        (2, 100, 26),
        "the delivered answer is the original terminal reason, not the \
         tombstone's ClientRetired: {:?}",
        resolved.reply()
    );
    within("corridor ack", resolved.acknowledge())
        .await
        .expect("acknowledgement rotates");

    let after = root.client_id().await;
    assert_ne!(before, after, "the corridor rotated to a fresh client id");

    // The old id is a tombstone the group remembers.
    let probe = within("probe", root.reads().probe_client(root_id, before))
        .await
        .expect("ROOT probes its own retired session");
    assert!(probe.exists);
    assert!(probe.retired);

    // And the fresh id mutates from sequence 1.
    let granted = within(
        "grant after rotation",
        root.set_grant(resource(b"fresh"), root_id, RIGHT_READ, 0),
    )
    .await
    .expect("the rotated journal mutates");
    assert_eq!(granted.reply().sequence, 1, "a fresh id starts at 1");
    assert_eq!(granted.reply().outcome_code, 6);
    within("ack", granted.acknowledge()).await.expect("ack");

    voter.shutdown().await;
}

/// **Redirects: a client seeded only with a follower finds the leader.**
/// The §5.4 loop: the follower's authenticated `UNAVAILABLE` hint names the
/// leader, the engine follows it within the cap, and the mutation commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_client_seeded_with_a_follower_follows_the_hint_to_the_leader() {
    let fixture = GroupFixture::new("client.redirect", 0xE3, 3);
    let dirs: Vec<tempfile::TempDir> = (0..3)
        .map(|_| tempfile::tempdir().expect("a directory"))
        .collect();
    let mut voters = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        voters.push(
            AsterServer::builder()
                .config(fixture.aster_config(index))
                .builtin_coordination(fixture.config(dir.path(), index))
                .start()
                .await
                .expect("a voter starts"),
        );
    }
    let addrs: Vec<_> = voters.iter().map(|v| v.node().addr()).collect();
    for voter in &voters {
        for addr in &addrs {
            if addr.node_id != voter.node().addr().node_id {
                voter
                    .node()
                    .add_node_addr(addr.clone())
                    .expect("register a peer");
            }
        }
    }
    for voter in &voters {
        within(
            "readiness",
            voter.coordination().expect("hosted").wait_ready(),
        )
        .await
        .expect("the group becomes ready");
    }

    let leader_id = voters[0]
        .coordination()
        .expect("hosted")
        .current_leader_for_testing()
        .expect("a ready group leads");
    let follower_index = (0..3)
        .find(|i| {
            aster::coordination::ids::CoordinationNodeId::from_slice(&fixture.node_id(*i))
                .expect("a node id")
                != leader_id
        })
        .expect("two of three follow");

    // The ROOT client knows only the follower.
    let root_secret = [0xE3 ^ 0xFF; 32];
    let root_id = ed25519_dalek::SigningKey::from_bytes(&root_secret)
        .verifying_key()
        .to_bytes();
    let node = client_node(root_secret, &fixture, &voters[follower_index]).await;
    // The leader must be dialable once the hint names it: same-host discovery
    // substitution, as everywhere in these gates.
    for addr in &addrs {
        let _ = node.add_node_addr(addr.clone());
    }
    let journal_dir = tempfile::tempdir().expect("a directory");
    let root = CoordinationClient::builder(node, group(&fixture))
        // Only the follower is seeded; the leader is reached by its hint.
        .seed_tickets([voter_ticket(&fixture, follower_index)])
        .journal_dir(journal_dir.path())
        .build()
        .expect("the client builds");

    let granted = within(
        "grant via redirect",
        root.set_grant(resource(b"via-follower"), root_id, RIGHT_READ, 0),
    )
    .await
    .expect("the client follows the authenticated hint to the leader");
    assert_eq!(granted.reply().outcome_code, 6);
    within("ack", granted.acknowledge()).await.expect("ack");

    for voter in voters {
        voter.shutdown().await;
    }
}

/// **The §9.2/§9.4 lease surface end to end**: `CoordinationLease` drives the
/// existing renewal engine through committed commands; a holder that stops
/// renewing loses its fence at the self-limit *first*, and the leader's
/// conservative timer then expires the lease so a successor can acquire —
/// takeover latency, never overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_abandoned_lease_expires_and_a_successor_acquires() {
    use aster::coordination::client::lease::{CoordinationLease, LeaseAcquireOutcome};
    use aster_transport_core::lease::LeaseOptions;

    let fixture = GroupFixture::new("client.expiry", 0xE4, 1);
    let dir = tempfile::tempdir().expect("a directory");
    let voter = start_voter(&fixture, 0, dir.path()).await;

    // ROOT grants two workers on one register.
    let root_node = client_node([0xE4 ^ 0xFF; 32], &fixture, &voter).await;
    let root_journal = tempfile::tempdir().expect("a directory");
    let root = CoordinationClient::builder(root_node, group(&fixture))
        .seed_tickets([voter_ticket(&fixture, 0)])
        .journal_dir(root_journal.path())
        .build()
        .expect("the ROOT client builds");
    let holder_secret = [0xE4 ^ 0x41; 32];
    let successor_secret = [0xE4 ^ 0x42; 32];
    for (sequence, secret) in [(1u64, holder_secret), (2, successor_secret)] {
        let subject = ed25519_dalek::SigningKey::from_bytes(&secret)
            .verifying_key()
            .to_bytes();
        let granted = within(
            "grant",
            root.set_grant(
                resource(b"reg"),
                subject,
                RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE,
                sequence - 1,
            ),
        )
        .await
        .expect("ROOT grants");
        assert_eq!(granted.reply().outcome_code, 6);
        within("ack", granted.acknowledge()).await.expect("ack");
    }

    let build_client = |secret: [u8; 32], journal: std::path::PathBuf| {
        let fixture_ticket = voter_ticket(&fixture, 0);
        let group_id = group(&fixture);
        let voter = &voter;
        let fixture = &fixture;
        async move {
            let node = client_node(secret, fixture, voter).await;
            CoordinationClient::builder(node, group_id)
                .seed_tickets([fixture_ticket])
                .journal_dir(&journal)
                .build()
                .expect("the client builds")
        }
    };

    // The §12.1 minimum TTL, renewals that would keep it alive, and the
    // default 1s holder margin.
    let options = LeaseOptions {
        ttl: Duration::from_millis(5_000),
        renew_interval: Duration::from_millis(1_500),
        holder_margin: Duration::from_millis(1_000),
    };

    let holder_journal = tempfile::tempdir().expect("a directory");
    let holder = build_client(holder_secret, holder_journal.path().to_path_buf()).await;
    let lease = match within(
        "acquire",
        CoordinationLease::acquire(holder.clone(), resource(b"reg"), options),
    )
    .await
    .expect("the acquire round-trips")
    {
        LeaseAcquireOutcome::Granted(lease) => lease,
        LeaseAcquireOutcome::Denied(reason, snapshot) => {
            panic!("the first acquire must grant, got {reason:?} over {snapshot:?}")
        }
    };
    let fence = lease.fence().expect("a fresh lease presents its fence");
    assert_eq!(
        fence.holder,
        holder.local_id().expect("a local id").to_vec()
    );

    // While held, a competing acquire is denied — no live steal.
    let successor_journal = tempfile::tempdir().expect("a directory");
    let successor = build_client(successor_secret, successor_journal.path().to_path_buf()).await;
    match within(
        "competing acquire",
        CoordinationLease::acquire(successor.clone(), resource(b"reg"), options),
    )
    .await
    .expect("the competing acquire round-trips")
    {
        LeaseAcquireOutcome::Denied(reason, _) => {
            assert_eq!(reason, aster_transport_core::lease::DenyReason::HeldByOther)
        }
        LeaseAcquireOutcome::Granted(_) => panic!("a live lease must not be stolen"),
    }

    // The holder abandons: renewals stop, and the self-limit strips the fence
    // strictly before the server timer can transfer ownership.
    let abandoned_at = std::time::Instant::now();
    drop(lease);

    // The successor polls for takeover. The §9.2 promise is an order, not a
    // deadline: the acquire that eventually succeeds must happen only after
    // the abandoned holder's self-limit (ttl - margin) has passed.
    let deadline = std::time::Instant::now() + PATIENCE;
    let successor_lease = loop {
        match within(
            "takeover acquire",
            CoordinationLease::acquire(successor.clone(), resource(b"reg"), options),
        )
        .await
        .expect("the takeover attempt round-trips")
        {
            LeaseAcquireOutcome::Granted(lease) => break lease,
            LeaseAcquireOutcome::Denied(_, _) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the expiry loop never freed the abandoned lease"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    assert!(
        abandoned_at.elapsed() >= Duration::from_millis(4_000),
        "a takeover before the abandoned holder's self-limit would overlap: {:?}",
        abandoned_at.elapsed()
    );
    let successor_fence = successor_lease
        .fence()
        .expect("the successor presents its fence");
    assert!(
        successor_fence.epoch > fence.epoch,
        "a takeover advances the epoch: {} then {}",
        fence.epoch,
        successor_fence.epoch
    );

    // The old fence is dead: a mutation under it is a committed EpochMismatch.
    let stale = within(
        "stale mutation",
        holder.compare_and_set(fence, 0, b"stale".to_vec()),
    )
    .await
    .expect("the stale mutation resolves");
    assert_eq!(stale.reply().outcome_code, 100);
    assert!(
        matches!(stale.reply().reason_code, 3 | 4),
        "the survived race is a committed fence refusal — NotHolder or          EpochMismatch, whichever §9 rule the row met first: {:?}",
        stale.reply()
    );
    within("stale ack", stale.acknowledge()).await.expect("ack");

    within("release", successor_lease.release())
        .await
        .expect("the successor releases cleanly");

    voter.shutdown().await;
}

/// **The streamed recovery export is exact and incremental** (the reopen's
/// blocker 5). The body is written straight into a caller-supplied `File`
/// writer — never assembled by the client — and the file's bytes match the
/// convenience `Vec` path byte for byte, with the manifest's length and
/// checksum holding over real state: resources, grants and an admin row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_recovery_export_streams_into_a_writer_exactly() {
    let fixture = GroupFixture::new("client.test", 0xE5, 1);
    let dir = tempfile::tempdir().expect("a directory");
    let voter = start_voter(&fixture, 0, dir.path()).await;

    let root_node = client_node([0xE5 ^ 0xFF; 32], &fixture, &voter).await;
    let root_journal = tempfile::tempdir().expect("a directory");
    let root = CoordinationClient::builder(root_node, group(&fixture))
        .seed_tickets([voter_ticket(&fixture, 0)])
        .journal_dir(root_journal.path())
        .build()
        .expect("the ROOT client builds");

    // Give the export a body worth checking: a grant, a held register with a
    // written value, and the ROOT admin row that §13 always carries.
    let worker_id = ed25519_dalek::SigningKey::from_bytes(&[0xE5 ^ 0x41; 32])
        .verifying_key()
        .to_bytes();
    let granted = within(
        "grant",
        root.set_grant(
            resource(b"exported"),
            worker_id,
            RIGHT_READ | RIGHT_ACQUIRE | RIGHT_MUTATE | RIGHT_RELEASE,
            0,
        ),
    )
    .await
    .expect("ROOT grants");
    within("grant ack", granted.acknowledge())
        .await
        .expect("acknowledge");
    let self_grant = within(
        "self grant",
        root.set_grant(
            resource(b"exported"),
            root.local_id().expect("the local id"),
            RIGHT_ACQUIRE | RIGHT_MUTATE,
            1,
        ),
    )
    .await
    .expect("ROOT grants itself");
    within("self grant ack", self_grant.acknowledge())
        .await
        .expect("acknowledge");
    let acquired = within("acquire", root.acquire(resource(b"exported"), 30_000))
        .await
        .expect("the acquire resolves");
    let snapshot = acquired.reply().snapshot.clone().expect("a snapshot");
    within("acquire ack", acquired.acknowledge())
        .await
        .expect("acknowledge");
    let fence = aster::coordination::contract::FenceV1 {
        group_id: snapshot.group_id.clone(),
        resource: snapshot.resource.clone(),
        epoch: snapshot.epoch,
        holder: snapshot.holder.clone().expect("held"),
    };
    let written = within(
        "compare_and_set",
        root.compare_and_set(fence, 0, vec![0xAB; 4096]),
    )
    .await
    .expect("the value writes");
    within("cas ack", written.acknowledge())
        .await
        .expect("acknowledge");

    let reads = root.reads();

    // The streaming path, into a real file writer.
    let export_dir = tempfile::tempdir().expect("a directory");
    let path = export_dir.path().join("recovery.body");
    let file = std::fs::File::create(&path).expect("the export file opens");
    let (manifest, file) = within(
        "recovery_export_to",
        reads.recovery_export_to(std::io::BufWriter::new(file)),
    )
    .await
    .expect("the export streams");
    drop(file);

    let streamed = std::fs::read(&path).expect("the export file reads back");
    assert_eq!(
        streamed.len() as u64,
        manifest.total_len,
        "the file holds exactly the manifest's length"
    );
    assert!(
        manifest.total_len > 0,
        "the seeded state produced a non-empty body"
    );
    assert_eq!(
        blake3::hash(&streamed).as_bytes().to_vec(),
        manifest.blake3_checksum,
        "the file re-hashes to the manifest checksum"
    );

    // The convenience path yields the same bytes: one pinned body, two
    // consumers.
    let (again, body) = within("recovery_export", reads.recovery_export())
        .await
        .expect("the convenience export succeeds");
    assert_eq!(body, streamed, "both paths deliver identical bytes");
    assert_eq!(again.total_len, manifest.total_len);

    voter.shutdown().await;
}
