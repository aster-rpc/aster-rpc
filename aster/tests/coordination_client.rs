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

    // §14: the expiry that freed the abandoned lease is exactly one client
    // self-limit event — the lease reached the TTL its own holder chose —
    // and it rode an `Expire` proposal from the leader sweep.
    let metrics = voter.coordination().expect("hosted").store_metrics();
    assert!(
        metrics.expire_proposed >= 1 && metrics.self_limit_expirations >= 1,
        "the applied expiry must land in the §14 self-limit counter: {metrics:?}"
    );

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

// ── The inbound-reply boundary, over live RPC (the fourth review's P0) ──────

/// A counterfeit `aster.coordination.Store`: it answers with **validly
/// encoded** replies that echo the caller's client id, sequence and exact
/// request hash — everything a pre-boundary client checked — while naming a
/// configurable group and protocol version. The one thing it cannot forge
/// is what the boundary checks.
/// How the counterfeit deforms an otherwise fully valid grant — each one a
/// §5.2 violation only the request-aware validity boundary can catch, since
/// ownership (protocol/group/client/sequence/hash) stays perfect.
#[derive(Clone, Copy, PartialEq)]
enum Deformation {
    None,
    /// `Granted` with no snapshot at all — the fifth review's own example.
    NoSnapshot,
    /// A perfect snapshot about some other resource.
    WrongResource,
    /// `held: true` with no holder — a self-contradiction.
    HolderWithoutHeld,
    /// A success wearing a terminal refusal reason.
    TerminalReasonOnSuccess,
    /// `Consumed` + `100 Rejected` + `21 ActiveClientCap` — the sixth
    /// review's durable wedge: the classifier answers 21 only pre-write,
    /// so a Consumed 21 claims a session no path creates, and
    /// acknowledging it would advance the journal past a sequence the
    /// group still expects.
    ConsumedActiveClientCap,
    /// `NotConsumed` + `6 AclRevisionMismatch` — a consumed conflict
    /// answered pre-write, §8.2.1's forbidden wedge.
    NotConsumedConflict,
    /// A perfect grant decorated with a membership operation id.
    OperationIdOnSuccess,
}

/// What the counterfeit's streams serve.
#[derive(Clone, Copy, PartialEq)]
enum StreamMode {
    /// Honest §5.2 streams: ascending audit rows within the limit, a
    /// bounded export.
    Honest,
    /// One more audit row than the caller's limit.
    AuditOverLimit,
    /// Audit rows out of order.
    AuditDescending,
    /// An export manifest claiming more than the §11.4 body ceiling.
    ExportOverCeiling,
    /// An honest manifest followed by a chunk over the §5.2 chunk bound.
    ExportOversizedChunk,
}

struct ForeignStore {
    group_id: std::sync::Mutex<Vec<u8>>,
    protocol_version: std::sync::Mutex<i32>,
    /// When set, `acquire` answers an entry-owned §10-terminal refusal
    /// instead of a grant — the corridor opener for the retirement gate.
    terminal: std::sync::Mutex<bool>,
    /// The group stamped onto `retire_client` confirmations, separately
    /// switchable from the mutation stamp.
    retire_group: std::sync::Mutex<Vec<u8>>,
    /// The active grant deformation.
    deformation: std::sync::Mutex<Deformation>,
    /// The `(outcome, reason)` pair `retire_client` answers.
    retire_pair: std::sync::Mutex<(i32, i32)>,
    /// What the streaming methods serve.
    stream_mode: std::sync::Mutex<StreamMode>,
}

impl ForeignStore {
    fn stamp(&self) -> (Vec<u8>, i32) {
        (
            self.group_id.lock().expect("stamp").clone(),
            *self.protocol_version.lock().expect("stamp"),
        )
    }
}

/// The counterfeit's stream frames per mode — audit rows that are honest,
/// too many, or out of order; export manifests over the §11.4 ceiling or
/// chunks over the §5.2 chunk bound.
fn counterfeit_stream_frames(method: &str, mode: StreamMode, group_id: &[u8]) -> Vec<Vec<u8>> {
    use aster::coordination::contract::payload_fory;
    use aster::coordination::store::wire::{
        AuditRecordWireV1, RecoveryExportChunkWireV1, RecoveryExportFrameV1,
        RecoveryExportManifestWireV1, STORE_PROTOCOL_VERSION,
    };

    let audit_row = |index: u64| AuditRecordWireV1 {
        protocol_version: STORE_PROTOCOL_VERSION,
        group_id: group_id.to_vec(),
        log_id: aster::coordination::contract::LogIdV1 {
            term: 1,
            leader_node_id: vec![0x11; 32],
            index,
        },
        actor_node_id: Some(vec![0x0B; 32]),
        operation_code: 1,
        resource: Some(aster::coordination::contract::ResourceKeyV1 {
            namespace: "client.gate".into(),
            key: b"stream".to_vec(),
        }),
        result_code: 1,
        epoch: 1,
        value_revision: 0,
    };
    let encode_row = |row: &AuditRecordWireV1| {
        payload_fory::<AuditRecordWireV1>()
            .serialize(row)
            .expect("a row encodes")
    };
    let encode_frame = |frame: &RecoveryExportFrameV1| {
        payload_fory::<RecoveryExportFrameV1>()
            .serialize(frame)
            .expect("a frame encodes")
    };

    match (method, mode) {
        ("audit_stream", StreamMode::AuditOverLimit) => {
            (1..=3).map(|i| encode_row(&audit_row(i))).collect()
        }
        ("audit_stream", StreamMode::AuditDescending) => {
            vec![encode_row(&audit_row(2)), encode_row(&audit_row(1))]
        }
        ("audit_stream", _) => (1..=2).map(|i| encode_row(&audit_row(i))).collect(),
        ("recovery_export", StreamMode::ExportOverCeiling) => {
            vec![encode_frame(&RecoveryExportFrameV1::Manifest(
                RecoveryExportManifestWireV1 {
                    protocol_version: STORE_PROTOCOL_VERSION,
                    group_id: group_id.to_vec(),
                    total_len: aster::coordination::rowstream::MAX_SNAPSHOT_BODY_BYTES + 1,
                    blake3_checksum: vec![0x33; 32],
                    applied_log_id: None,
                },
            ))]
        }
        ("recovery_export", _) => {
            // An honest, in-bounds manifest — followed by one chunk a full
            // §5.2 bound and one byte long, which the client must refuse
            // before writing.
            let oversized = vec![0xAB; 1024 * 1024 + 1];
            vec![
                encode_frame(&RecoveryExportFrameV1::Manifest(
                    RecoveryExportManifestWireV1 {
                        protocol_version: STORE_PROTOCOL_VERSION,
                        group_id: group_id.to_vec(),
                        total_len: 4 * 1024 * 1024,
                        blake3_checksum: vec![0x33; 32],
                        applied_log_id: None,
                    },
                )),
                encode_frame(&RecoveryExportFrameV1::Chunk(RecoveryExportChunkWireV1 {
                    offset: 0,
                    data: oversized,
                    final_chunk: false,
                })),
            ]
        }
        _ => Vec::new(),
    }
}

/// The registered wrapper: the builder's `service` takes ownership, so the
/// test keeps the shared state through this delegating handle.
struct ForeignStoreService(std::sync::Arc<ForeignStore>);

#[async_trait::async_trait]
impl aster::rpc::ServiceDispatch for ForeignStoreService {
    fn name(&self) -> &str {
        aster::coordination::service::STORE_SERVICE
    }
    fn version(&self) -> i32 {
        aster::coordination::service::STORE_VERSION
    }
    fn methods(&self) -> &[&str] {
        aster::coordination::service::STORE_METHODS
    }
    async fn dispatch(&self, method: &str, mut call: aster::rpc::Call) {
        use aster::coordination::contract::{payload_fory, AcquireActionV1, ActionV1};
        use aster::coordination::store::wire::{
            AcquireRequestV1, GroupStatusReplyWireV1, MutationReplyWireV1, STORE_PROTOCOL_VERSION,
        };
        let Some(frame) = call.recv_request().await else {
            return;
        };
        let (group_id, protocol_version) = self.0.stamp();

        // The streaming methods: frames via `send`, then an OK finish.
        if method == "audit_stream" || method == "recovery_export" {
            let mode = *self.0.stream_mode.lock().expect("stamp");
            let frames = counterfeit_stream_frames(method, mode, &group_id);
            for frame in frames {
                let _ = call.send(frame).await;
            }
            let _ = call.finish(&aster::rpc::RpcStatus::ok()).await;
            return;
        }

        let reply_bytes = match method {
            "acquire" => {
                let request: AcquireRequestV1 = payload_fory::<AcquireRequestV1>()
                    .deserialize(&frame)
                    .expect("the counterfeit decodes the request");
                // Echo exactly what a pre-boundary client verified: the
                // client id, the sequence, and the true canonical hash of
                // the requested action.
                let hash =
                    aster::coordination::hash::request_hash(&ActionV1::Acquire(AcquireActionV1 {
                        resource: request.resource.clone(),
                        ttl_ms: request.ttl_ms,
                    }))
                    .expect("the action hashes");
                let terminal = *self.0.terminal.lock().expect("stamp");
                let deformation = *self.0.deformation.lock().expect("stamp");
                let (mut disposition_code, mut outcome_code, mut reason_code) = if terminal {
                    // NotConsumed with a §10-terminal reason: the corridor
                    // opener, honestly owned by the entry.
                    (2, 100, 24)
                } else {
                    (1, 1, 0)
                };
                // A grant is otherwise **fully valid**: complete snapshot
                // about the requested resource, held by the caller — so a
                // refusal downstream can only be the deformation's.
                let holder: Vec<u8> = {
                    let peer = call.peer();
                    aster::coordination::ids::CoordinationNodeId::parse_hex(peer)
                        .map(|id| id.as_bytes().to_vec())
                        .unwrap_or_else(|_| vec![0xEE; 32])
                };
                let mut snapshot =
                    (!terminal).then(|| aster::coordination::contract::ResourceSnapshotV1 {
                        group_id: group_id.clone(),
                        resource: request.resource.clone(),
                        epoch: 1,
                        holder: Some(holder),
                        held: true,
                        ttl_ms: request.ttl_ms,
                        lease_revision: 1,
                        value_revision: 0,
                        acl_revision: 1,
                        value: Some(Vec::new()),
                        value_redacted: false,
                    });
                let mut operation_id = None;
                match deformation {
                    Deformation::None => {}
                    Deformation::ConsumedActiveClientCap => {
                        disposition_code = 1;
                        outcome_code = 100;
                        reason_code = 21;
                        snapshot = None;
                    }
                    Deformation::NotConsumedConflict => {
                        disposition_code = 2;
                        outcome_code = 100;
                        reason_code = 6;
                        snapshot = None;
                    }
                    Deformation::OperationIdOnSuccess => {
                        operation_id = Some(vec![0x55; 16]);
                    }
                    Deformation::NoSnapshot => snapshot = None,
                    Deformation::WrongResource => {
                        if let Some(s) = snapshot.as_mut() {
                            s.resource = aster::coordination::contract::ResourceKeyV1 {
                                namespace: "client.gate".into(),
                                key: b"some-other-register".to_vec(),
                            };
                        }
                    }
                    Deformation::HolderWithoutHeld => {
                        // The holder is still this client — the grant-holder
                        // fact passes — but the held flag contradicts it, so
                        // only the coherence check can refuse this shape.
                        if let Some(s) = snapshot.as_mut() {
                            s.held = false;
                        }
                    }
                    Deformation::TerminalReasonOnSuccess => reason_code = 24,
                }
                // §5.2.1 copies the ACL revision from the snapshot's row.
                let acl_revision = snapshot.as_ref().map_or(0, |s| s.acl_revision);
                payload_fory::<MutationReplyWireV1>()
                    .serialize(&MutationReplyWireV1 {
                        protocol_version,
                        group_id,
                        client_id: request.client_id,
                        sequence: request.sequence,
                        request_hash: hash.to_vec(),
                        disposition_code,
                        outcome_code,
                        reason_code,
                        snapshot,
                        acl_revision,
                        operation_id,
                        ..Default::default()
                    })
                    .expect("the counterfeit encodes")
            }
            "retire_client" => {
                use aster::coordination::store::wire::RetireClientReplyWireV1;
                let (outcome_code, reason_code) = *self.0.retire_pair.lock().expect("stamp");
                payload_fory::<RetireClientReplyWireV1>()
                    .serialize(&RetireClientReplyWireV1 {
                        protocol_version: STORE_PROTOCOL_VERSION,
                        group_id: self.0.retire_group.lock().expect("stamp").clone(),
                        outcome_code,
                        reason_code,
                    })
                    .expect("the counterfeit encodes")
            }
            "group_status" => payload_fory::<GroupStatusReplyWireV1>()
                .serialize(&GroupStatusReplyWireV1 {
                    protocol_version: STORE_PROTOCOL_VERSION,
                    group_id,
                    lifecycle_code: 3,
                    local_node_id: vec![0xEE; 32],
                    genesis_hash: vec![0xEE; 32],
                    // A malformed nested identifier: 31 bytes where a voter
                    // id must be 32. Outer envelope perfect once the group
                    // stamp is honest — only nested validation can refuse.
                    voters: vec![aster::coordination::contract::NodeIdV1 {
                        node_id: vec![0xEE; 31],
                    }],
                    ..Default::default()
                })
                .expect("the counterfeit encodes"),
            _ => {
                let _ = call
                    .finish(&aster::rpc::RpcStatus::error(
                        aster::rpc::StatusCode::Unimplemented,
                        "the counterfeit serves acquire and group_status only",
                    ))
                    .await;
                return;
            }
        };
        let _ = call
            .respond(reply_bytes, &aster::rpc::RpcStatus::ok())
            .await;
    }
}

/// **A validly encoded foreign reply is refused at the boundary, live.**
/// The counterfeit store echoes the client id, sequence and true request
/// hash — the pre-boundary checks — while naming another group (then
/// another protocol version). The mutation must not resolve and nothing
/// may be persisted for delivery; a linearizable-status read must surface
/// the refusal rather than the counterfeit's answer. Flipping the
/// counterfeit to the honest stamps makes the same call succeed, pinning
/// every refusal to the envelope boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_reply_is_refused_at_the_inbound_boundary() {
    let fixture = GroupFixture::new("client.boundary", 0xE9, 1);
    let real_group = group(&fixture);

    let foreign = std::sync::Arc::new(ForeignStore {
        group_id: std::sync::Mutex::new(vec![0xEE; 32]),
        protocol_version: std::sync::Mutex::new(1),
        terminal: std::sync::Mutex::new(false),
        retire_group: std::sync::Mutex::new(vec![0xEE; 32]),
        deformation: std::sync::Mutex::new(Deformation::None),
        retire_pair: std::sync::Mutex::new((9, 0)),
        stream_mode: std::sync::Mutex::new(StreamMode::Honest),
    });
    let counterfeit = AsterServer::builder()
        .config(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes([0xE9 ^ 0x7A; 32]))
                .build(),
        )
        .service(ForeignStoreService(std::sync::Arc::clone(&foreign)))
        .start()
        .await
        .expect("the counterfeit starts");
    let counterfeit_id: [u8; 32] =
        *aster::coordination::ids::CoordinationNodeId::parse_hex(counterfeit.node().id().as_str())
            .expect("a 32-byte id")
            .as_bytes();

    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(SecretKey::from_bytes([0xE9 ^ 0x41; 32]))
            .build(),
    )
    .await
    .expect("a client node starts");
    node.add_node_addr(counterfeit.node().addr())
        .expect("register the counterfeit");
    let journal = tempfile::tempdir().expect("a directory");
    let client = CoordinationClient::builder(node, real_group)
        .seed_tickets([
            aster::coordination::model::authority::canonical_candidate_ticket(&counterfeit_id)
                .expect("a canonical ticket"),
        ])
        .journal_dir(journal.path())
        .build()
        .expect("the client builds");

    // Wrong group: the drive classifies every reply as not-this-entry's and
    // exhausts; nothing is delivered and nothing awaits acknowledgement.
    let refused = within(
        "wrong-group acquire",
        client.acquire(resource(b"boundary"), 60_000),
    )
    .await;
    let Err(err) = refused else {
        panic!("a foreign reply must not resolve")
    };
    assert!(
        format!("{err}").contains("no resolution"),
        "the driver classifies the foreign reply as not-this-entry's and \
         exhausts — it never reaches the journal: {err}"
    );
    assert!(
        within("unacknowledged", client.unacknowledged())
            .await
            .expect("the journal answers")
            .is_none(),
        "a foreign reply must never be persisted for delivery"
    );

    // The read path refuses a foreign group_status outright — checked while
    // the counterfeit still stamps the foreign group.
    let status = within("foreign status", client.reads().group_status()).await;
    let _ = status.expect_err("a foreign status reply must be refused");

    // Wrong protocol version under the true group: same refusal.
    *foreign.group_id.lock().expect("stamp") = real_group.as_bytes().to_vec();
    *foreign.protocol_version.lock().expect("stamp") = 99;
    let refused = within("wrong-version resume", client.resume()).await;
    let Err(err) = refused else {
        panic!("a wrong-version reply must not resolve")
    };
    assert!(
        format!("{err}").contains("no resolution"),
        "the driver, not the journal backstop, refuses the wrong version: {err}"
    );

    // The fifth review's half: **ownership-perfect but invalid** replies.
    // Honest stamps from here on — protocol, group, client, sequence and
    // the true hash all echo — and each §5.2 deformation must still be
    // refused by the request-aware boundary.
    *foreign.protocol_version.lock().expect("stamp") =
        aster::coordination::store::wire::STORE_PROTOCOL_VERSION;
    for (deformation, why) in [
        (Deformation::NoSnapshot, "a grant with no snapshot"),
        (
            Deformation::WrongResource,
            "a snapshot about another resource",
        ),
        (
            Deformation::HolderWithoutHeld,
            "a held register with no holder",
        ),
        (
            Deformation::TerminalReasonOnSuccess,
            "a success wearing a terminal reason",
        ),
        (
            Deformation::ConsumedActiveClientCap,
            "a Consumed rejection with the classifier-only 21",
        ),
        (
            Deformation::NotConsumedConflict,
            "a consumed conflict answered NotConsumed",
        ),
        (
            Deformation::OperationIdOnSuccess,
            "a grant decorated with a membership operation id",
        ),
    ] {
        *foreign.deformation.lock().expect("stamp") = deformation;
        let refused = within("deformed resume", client.resume()).await;
        let Err(err) = refused else {
            panic!("{why} must not resolve");
        };
        assert!(
            format!("{err}").contains("no resolution"),
            "{why} is refused at the boundary: {err}"
        );
    }
    *foreign.deformation.lock().expect("stamp") = Deformation::None;

    // With an honest envelope, the status is still refused — its nested
    // voter id is 31 bytes, a fact only nested validation can see.
    let nested = within("malformed status", client.reads().group_status()).await;
    let Err(err) = nested else {
        panic!("a malformed nested identifier must be refused")
    };
    assert!(
        format!("{err}").contains("malformed"),
        "the nested validator names the fault: {err}"
    );

    // Honest stamps: the same journaled request now resolves — the earlier
    // refusals were the boundary's, not some other fault's.
    *foreign.protocol_version.lock().expect("stamp") =
        aster::coordination::store::wire::STORE_PROTOCOL_VERSION;
    let resolved = within("honest resume", client.resume())
        .await
        .expect("the honest reply resolves")
        .expect("the journaled request resolves");
    assert_eq!(resolved.reply().outcome_code, 1);
    within("ack", resolved.acknowledge()).await.expect("ack");

    counterfeit.shutdown().await;
}

/// **A retirement confirmation from another group confirms nothing** (the
/// fourth review's P0, retirement half). The counterfeit opens the closing
/// corridor honestly — an entry-owned §10-terminal refusal under the true
/// group — and then answers `retire_client` with outcome 9 stamped with a
/// **foreign** group. The client must keep the journal `Closing` and
/// surface the exhausted confirmation attempt, not honor the foreign 9 and
/// replay on a tombstone that may not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_retirement_confirmation_is_not_honored() {
    let fixture = GroupFixture::new("client.boundary2", 0xEA, 1);
    let real_group = group(&fixture);

    let foreign = std::sync::Arc::new(ForeignStore {
        group_id: std::sync::Mutex::new(real_group.as_bytes().to_vec()),
        protocol_version: std::sync::Mutex::new(
            aster::coordination::store::wire::STORE_PROTOCOL_VERSION,
        ),
        terminal: std::sync::Mutex::new(true),
        retire_group: std::sync::Mutex::new(vec![0xEE; 32]),
        deformation: std::sync::Mutex::new(Deformation::None),
        retire_pair: std::sync::Mutex::new((9, 0)),
        stream_mode: std::sync::Mutex::new(StreamMode::Honest),
    });
    let counterfeit = AsterServer::builder()
        .config(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes([0xEA ^ 0x7A; 32]))
                .build(),
        )
        .service(ForeignStoreService(std::sync::Arc::clone(&foreign)))
        .start()
        .await
        .expect("the counterfeit starts");
    let counterfeit_id: [u8; 32] =
        *aster::coordination::ids::CoordinationNodeId::parse_hex(counterfeit.node().id().as_str())
            .expect("a 32-byte id")
            .as_bytes();

    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(SecretKey::from_bytes([0xEA ^ 0x41; 32]))
            .build(),
    )
    .await
    .expect("a client node starts");
    node.add_node_addr(counterfeit.node().addr())
        .expect("register the counterfeit");
    let journal = tempfile::tempdir().expect("a directory");
    let client = CoordinationClient::builder(node, real_group)
        .seed_tickets([
            aster::coordination::model::authority::canonical_candidate_ticket(&counterfeit_id)
                .expect("a canonical ticket"),
        ])
        .journal_dir(journal.path())
        .build()
        .expect("the client builds");

    // The terminal refusal opens the corridor; the foreign outcome-9 must
    // then be skipped until the confirmation budget exhausts, with the
    // journal still Closing.
    let refused = within(
        "corridor under a foreign confirmation",
        client.acquire(resource(b"boundary2"), 60_000),
    )
    .await;
    let Err(err) = refused else {
        panic!("the corridor cannot complete on a foreign confirmation")
    };
    assert!(
        format!("{err}").contains("retirement did not confirm"),
        "the foreign outcome 9 must be skipped, not honored: {err}"
    );

    // The sixth review's half: an honest-world confirmation whose **pair**
    // is contradictory. `(9, 18)` — retired, dressed with the fatal reason —
    // must neither confirm nor latch FatalJournal; `(100, 12)` — the
    // model's cross-actor refusal — cannot answer a self-targeted request.
    *foreign.retire_group.lock().expect("stamp") = real_group.as_bytes().to_vec();
    for (pair, why) in [
        ((9, 18), "outcome 9 dressed with reason 18"),
        ((100, 12), "the cross-actor pair on a self-targeted request"),
    ] {
        *foreign.retire_pair.lock().expect("stamp") = pair;
        let refused = within("contradictory retirement pair", client.resume()).await;
        let Err(err) = refused else {
            panic!("{why} must not be honored");
        };
        assert!(
            format!("{err}").contains("retirement did not confirm"),
            "{why} is skipped as not-this-request's answer, with the \
             journal still Closing: {err}"
        );
    }

    // The honest pair confirms, and the §10 corridor completes: retire,
    // replay the exact mutation on the tombstone, deliver. The counterfeit
    // now grants the replay, which is the tombstone's saved answer.
    *foreign.retire_pair.lock().expect("stamp") = (9, 0);
    *foreign.terminal.lock().expect("stamp") = false;
    let resolved = within("the corridor completes", client.resume())
        .await
        .expect("the corridor completes on the honest pair")
        .expect("the journaled request resolves");
    assert_eq!(
        resolved.reply().outcome_code,
        1,
        "the tombstone's saved answer"
    );
    within("rotation ack", resolved.acknowledge())
        .await
        .expect("the rotation acknowledges");

    counterfeit.shutdown().await;
}

/// **The stream contracts are enforced client-side, live** (the sixth
/// review's P0, stream half). The counterfeit serves validly encoded,
/// honestly stamped streams that violate exactly one contract word each:
/// one audit row over the requested limit, rows out of order, an export
/// manifest over the §11.4 body ceiling, and a chunk over the §5.2 chunk
/// bound. Each is refused at its own violation — and the honest stream,
/// served by the same counterfeit, is accepted, pinning every refusal to
/// the stream boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_contracts_are_enforced_at_the_boundary() {
    let fixture = GroupFixture::new("client.boundary3", 0xEB, 1);
    let real_group = group(&fixture);

    let foreign = std::sync::Arc::new(ForeignStore {
        group_id: std::sync::Mutex::new(real_group.as_bytes().to_vec()),
        protocol_version: std::sync::Mutex::new(
            aster::coordination::store::wire::STORE_PROTOCOL_VERSION,
        ),
        terminal: std::sync::Mutex::new(false),
        retire_group: std::sync::Mutex::new(real_group.as_bytes().to_vec()),
        deformation: std::sync::Mutex::new(Deformation::None),
        retire_pair: std::sync::Mutex::new((9, 0)),
        stream_mode: std::sync::Mutex::new(StreamMode::Honest),
    });
    let counterfeit = AsterServer::builder()
        .config(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes([0xEB ^ 0x7A; 32]))
                .build(),
        )
        .service(ForeignStoreService(std::sync::Arc::clone(&foreign)))
        .start()
        .await
        .expect("the counterfeit starts");
    let counterfeit_id: [u8; 32] =
        *aster::coordination::ids::CoordinationNodeId::parse_hex(counterfeit.node().id().as_str())
            .expect("a 32-byte id")
            .as_bytes();

    let node = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(SecretKey::from_bytes([0xEB ^ 0x41; 32]))
            .build(),
    )
    .await
    .expect("a client node starts");
    node.add_node_addr(counterfeit.node().addr())
        .expect("register the counterfeit");
    let journal = tempfile::tempdir().expect("a directory");
    let client = CoordinationClient::builder(node, real_group)
        .seed_tickets([
            aster::coordination::model::authority::canonical_candidate_ticket(&counterfeit_id)
                .expect("a canonical ticket"),
        ])
        .journal_dir(journal.path())
        .build()
        .expect("the client builds");
    let reads = client.reads();

    // The honest stream is accepted: two ascending rows within the limit.
    let rows = within("honest audit stream", reads.audit_stream(None, 2))
        .await
        .expect("an honest stream is accepted");
    assert_eq!(rows.len(), 2);
    assert!(rows[0].log_id.index < rows[1].log_id.index);

    // One row over the requested limit: refused at the third frame.
    *foreign.stream_mode.lock().expect("stamp") = StreamMode::AuditOverLimit;
    let over = within("over-limit audit stream", reads.audit_stream(None, 2)).await;
    let Err(err) = over else {
        panic!("a stream over the requested limit must be refused")
    };
    assert!(format!("{err}").contains("overran"), "{err}");

    // Out of order: refused at the violating row.
    *foreign.stream_mode.lock().expect("stamp") = StreamMode::AuditDescending;
    let unordered = within("descending audit stream", reads.audit_stream(None, 2)).await;
    let Err(err) = unordered else {
        panic!("a non-ascending stream must be refused")
    };
    assert!(format!("{err}").contains("ascending"), "{err}");

    // A manifest over the §11.4 body ceiling: refused before any chunk.
    *foreign.stream_mode.lock().expect("stamp") = StreamMode::ExportOverCeiling;
    let over = within("over-ceiling export", reads.recovery_export()).await;
    let Err(err) = over else {
        panic!("a manifest over the body ceiling must be refused")
    };
    assert!(format!("{err}").contains("ceiling"), "{err}");

    // A chunk over the §5.2 chunk bound: refused before it is written.
    *foreign.stream_mode.lock().expect("stamp") = StreamMode::ExportOversizedChunk;
    let oversized = within("oversized export chunk", reads.recovery_export()).await;
    let Err(err) = oversized else {
        panic!("an oversized chunk must be refused")
    };
    assert!(format!("{err}").contains("chunk exceeds"), "{err}");

    counterfeit.shutdown().await;
}
