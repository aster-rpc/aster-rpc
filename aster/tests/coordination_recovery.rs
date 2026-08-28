//! §13 end-to-end new-group recovery (S5-B).
//!
//! The real corridor, with no seams: a source group serves a canonical
//! export over the wire, its quorum is then lost for good, and a **new**
//! group with a new random group id — whose ROOT-signed genesis binds the
//! export's BLAKE3 — bootstraps every initial voter from an empty directory
//! by importing the same file. What these gates then prove is the §13
//! contract itself: values and revisions survive, holders do not, every
//! old-group fence dies at the door, the administrator union holds, the
//! signed Genesis *confirms* the import rather than overwriting it, and a
//! tampered or mismatched export never founds a group at all.

use std::time::Duration;

use aster::coordination::client::CoordinationClient;
use aster::coordination::contract::ResourceKeyV1;
use aster::coordination::model::limits::{RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE};
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
        namespace: "recovery.gate".into(),
        key: key.to_vec(),
    }
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

fn voter_ticket(fixture: &GroupFixture, index: usize) -> String {
    let id: [u8; 32] = fixture.node_id(index).try_into().expect("32 bytes");
    aster::coordination::model::authority::canonical_candidate_ticket(&id)
        .expect("a canonical ticket")
}

async fn connect(
    secret: [u8; 32],
    fixture: &GroupFixture,
    voter: &AsterServer,
    journal: &std::path::Path,
) -> CoordinationClient {
    let node = client_node(secret, fixture, voter).await;
    CoordinationClient::builder(node, fixture.group_id())
        .seed_tickets([voter_ticket(fixture, 0)])
        .journal_dir(journal)
        .build()
        .expect("the client builds")
}

/// Resolve a mutation and acknowledge it, returning the reply.
macro_rules! mutate {
    ($what:expr, $call:expr) => {{
        let resolved = within($what, $call).await.expect("the mutation resolves");
        let reply = resolved.reply().clone();
        within(concat!($what, " ack"), resolved.acknowledge())
            .await
            .expect("acknowledge");
        reply
    }};
}

/// The full §13 corridor over real voters and a real exported body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_group_recovers_from_a_real_export_end_to_end() {
    // ── The source group, with state worth recovering ───────────────────────
    let source = GroupFixture::new("recovery.src", 0xF1, 1);
    let source_dir = tempfile::tempdir().expect("a directory");
    let source_voter = AsterServer::builder()
        .config(source.aster_config(0))
        .builtin_coordination(source.config(source_dir.path(), 0))
        .start()
        .await
        .expect("the source voter starts");
    within(
        "source readiness",
        source_voter.coordination().expect("hosted").wait_ready(),
    )
    .await
    .expect("the source group becomes ready");

    let worker_secret = [0xF1 ^ 0x41; 32];
    let worker_id = ed25519_dalek::SigningKey::from_bytes(&worker_secret)
        .verifying_key()
        .to_bytes();

    let root_journal = tempfile::tempdir().expect("a directory");
    let root = connect(
        [0xF1 ^ 0xFF; 32],
        &source,
        &source_voter,
        root_journal.path(),
    )
    .await;

    // A worker grant that must survive recovery, a self grant, a held lease,
    // and a written value.
    mutate!(
        "grant the worker",
        root.set_grant(
            resource(b"tree-1"),
            worker_id,
            RIGHT_READ | RIGHT_ACQUIRE | RIGHT_MUTATE | RIGHT_RELEASE,
            0,
        )
    );
    mutate!(
        "grant ROOT itself",
        root.set_grant(
            resource(b"tree-1"),
            root.local_id().expect("the local id"),
            RIGHT_ACQUIRE | RIGHT_MUTATE,
            1,
        )
    );
    let acquired = within("acquire", root.acquire(resource(b"tree-1"), 30_000))
        .await
        .expect("the acquire resolves");
    let snapshot = acquired.reply().snapshot.clone().expect("a snapshot");
    within("acquire ack", acquired.acknowledge())
        .await
        .expect("acknowledge");
    let source_epoch = snapshot.epoch;
    assert!(snapshot.held, "the source lease is held at export time");
    let fence = aster::coordination::contract::FenceV1 {
        group_id: snapshot.group_id.clone(),
        resource: snapshot.resource.clone(),
        epoch: snapshot.epoch,
        holder: snapshot.holder.clone().expect("held"),
    };
    let written = mutate!(
        "write the value",
        root.compare_and_set(fence.clone(), 0, vec![0xAB; 64])
    );
    let value_revision = written
        .snapshot
        .as_ref()
        .expect("a snapshot")
        .value_revision;

    // ── The export, over the wire, while the lease is still held ────────────
    let (manifest, body) = within("recovery_export", root.reads().recovery_export())
        .await
        .expect("the export streams");
    assert_eq!(
        blake3::hash(&body).as_bytes().to_vec(),
        manifest.blake3_checksum
    );
    let export_hash: [u8; 32] = blake3::hash(&body).into();
    let export_dir = tempfile::tempdir().expect("a directory");
    let export_path = export_dir.path().join("source.recovery");
    std::fs::write(&export_path, &body).expect("the export file writes");

    // ── The disaster: the source quorum is gone for good ────────────────────
    source_voter.shutdown().await;

    // ── The new group: three voters, empty directories, one bound export ────
    // The old worker is named an extra administrator, so the union is
    // observable; it is also admitted so it can connect at all.
    let recovered = GroupFixture::new("recovery.new", 0xF2, 3)
        .admitting(worker_secret)
        .recovering(export_hash, &[worker_id]);
    let dirs: Vec<tempfile::TempDir> = (0..3)
        .map(|_| tempfile::tempdir().expect("a directory"))
        .collect();
    let mut voters = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        voters.push(
            AsterServer::builder()
                .config(recovered.aster_config(index))
                .builtin_coordination(recovered.recovery_config(dir.path(), index, &export_path))
                .start()
                .await
                .expect("a recovered voter starts"),
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
    // Readiness itself is evidence: `Ready` now requires the confirming
    // Genesis command to have applied on a recovered store, not merely the
    // imported admin set.
    for voter in &voters {
        within(
            "recovered readiness",
            voter.coordination().expect("hosted").wait_ready(),
        )
        .await
        .expect("the recovered group becomes ready");
    }

    // ── The recovered state, observed through the public API ────────────────
    let worker_journal = tempfile::tempdir().expect("a directory");
    let worker = connect(worker_secret, &recovered, &voters[0], worker_journal.path()).await;

    let read = within(
        "read",
        worker.reads().read_linearizable(resource(b"tree-1")),
    )
    .await
    .expect("the imported register reads");
    assert_eq!(
        read.snapshot.value,
        Some(vec![0xAB; 64]),
        "the chosen value survives recovery"
    );
    assert_eq!(
        read.snapshot.value_revision, value_revision,
        "the value revision is preserved, not restarted"
    );
    assert_eq!(
        read.snapshot.epoch,
        source_epoch + 1,
        "the epoch advances exactly once across recovery"
    );
    assert!(!read.snapshot.held, "recovery frees every lease");
    assert_eq!(read.snapshot.holder, None);

    // Provenance over the status API, and the confirming Genesis in the
    // audit history.
    let new_root_journal = tempfile::tempdir().expect("a directory");
    let new_root = connect(
        [0xF2 ^ 0xFF; 32],
        &recovered,
        &voters[0],
        new_root_journal.path(),
    )
    .await;
    let status = within("group_status", new_root.reads().group_status())
        .await
        .expect("the status serves");
    let provenance = status.recovered_from.expect("a recovered group says so");
    assert_eq!(provenance.source_group_id, [0xF1u8; 32].to_vec());
    assert_eq!(provenance.export_hash, export_hash.to_vec());
    assert_eq!(provenance.source_authority_domain, "recovery.src");
    let audit = within("audit", new_root.reads().audit_stream(None, 100))
        .await
        .expect("the audit streams");
    assert!(
        audit.iter().any(|record| record.operation_code == 102),
        "the confirming Genesis command applied and left its audit row"
    );

    // The administrator union: the old worker, never in the new voter set,
    // was carried as an administrator — an admin-only call succeeds.
    let admins = within("inspect_admins", worker.reads().inspect_admins())
        .await
        .expect("the union made the old worker an administrator");
    assert!(
        admins
            .admin_snapshot
            .administrators
            .iter()
            .any(|admin| admin.node_id == worker_id.to_vec()),
        "the exported-and-genesis union names the worker"
    );

    // ── Every old-group fence dies at the door ──────────────────────────────
    // 1. A client still configured for the old group id: refused per-request.
    let old_group_journal = tempfile::tempdir().expect("a directory");
    let old_node = client_node(worker_secret, &recovered, &voters[0]).await;
    let old_group_client = CoordinationClient::builder(old_node, source.group_id())
        .seed_tickets([voter_ticket(&recovered, 0)])
        .journal_dir(old_group_journal.path())
        .build()
        .expect("the old-group client builds");
    let refused = within(
        "an old-group read",
        old_group_client
            .reads()
            .read_linearizable(resource(b"tree-1")),
    )
    .await
    .expect_err("the recovered group refuses the old group id");
    assert!(
        refused.to_string().contains("different coordination group"),
        "got: {refused}"
    );

    // 2. The old lease's epoch at the new group: recovery advanced the epoch
    //    and freed the row, so a fence carrying the source epoch is dead even
    //    for an actor the new group admits and even naming itself as holder —
    //    the row has no holder to be.
    let stale = within(
        "a stale-fence mutation",
        worker.compare_and_set(
            aster::coordination::contract::FenceV1 {
                group_id: recovered.group_id().as_bytes().to_vec(),
                resource: resource(b"tree-1"),
                epoch: source_epoch,
                holder: worker_id.to_vec(),
            },
            value_revision,
            b"stale".to_vec(),
        ),
    )
    .await
    .expect("the stale mutation resolves");
    assert_eq!(
        stale.reply().outcome_code,
        100,
        "an old-group fence commits a refusal, never an effect"
    );
    within("stale ack", stale.acknowledge())
        .await
        .expect("acknowledge");
    let after = within(
        "re-read",
        worker.reads().read_linearizable(resource(b"tree-1")),
    )
    .await
    .expect("the register still reads");
    assert_eq!(
        after.snapshot.value,
        Some(vec![0xAB; 64]),
        "the stale fence changed nothing"
    );

    // ── And the recovered group is live: a fresh acquire works ──────────────
    let acquired = within("acquire", worker.acquire(resource(b"tree-1"), 30_000))
        .await
        .expect("the recovered group grants a fresh lease");
    let snapshot = acquired.reply().snapshot.clone().expect("a snapshot");
    within("acquire ack", acquired.acknowledge())
        .await
        .expect("acknowledge");
    assert_eq!(
        snapshot.epoch,
        source_epoch + 2,
        "the fresh lease is one epoch past the imported row"
    );

    for voter in voters {
        voter.shutdown().await;
    }
}

/// The refusals that keep a wrong export from founding a group, at the real
/// boot corridor: a hash the genesis does not bind, a **correctly signed**
/// genesis binding the hash of a noncanonical body (the S5-A obligation:
/// only the reader can catch it, and the production import must), a
/// non-empty directory, and the legal restart afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_or_tampered_export_never_founds_a_group() {
    // A small real source.
    let source = GroupFixture::new("recovery.badsrc", 0xF5, 1);
    let source_dir = tempfile::tempdir().expect("a directory");
    let source_voter = AsterServer::builder()
        .config(source.aster_config(0))
        .builtin_coordination(source.config(source_dir.path(), 0))
        .start()
        .await
        .expect("the source voter starts");
    within(
        "source readiness",
        source_voter.coordination().expect("hosted").wait_ready(),
    )
    .await
    .expect("ready");
    let root_journal = tempfile::tempdir().expect("a directory");
    let root = connect(
        [0xF5 ^ 0xFF; 32],
        &source,
        &source_voter,
        root_journal.path(),
    )
    .await;
    mutate!(
        "seed a grant",
        root.set_grant(resource(b"seed"), [0x42; 32], RIGHT_READ, 0)
    );
    let (_, body) = within("export", root.reads().recovery_export())
        .await
        .expect("the export streams");
    source_voter.shutdown().await;

    let export_dir = tempfile::tempdir().expect("a directory");

    // 1. The genesis binds a hash the file does not have.
    let intact_path = export_dir.path().join("intact.recovery");
    std::fs::write(&intact_path, &body).expect("write");
    let wrong = GroupFixture::new("recovery.wrong", 0xF6, 1).recovering([0x99; 32], &[]);
    let dir = tempfile::tempdir().expect("a directory");
    let err = match AsterServer::builder()
        .config(wrong.aster_config(0))
        .builtin_coordination(wrong.recovery_config(dir.path(), 0, &intact_path))
        .start()
        .await
    {
        Ok(_) => panic!("a mismatched export must not bootstrap"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("recovery import"), "got: {err}");

    // 2. A correctly signed genesis binding the hash of a *noncanonical*
    //    body: append a byte inside the final row's Fory frame and adjust
    //    its length varint, then bind exactly those bytes. The hash matches;
    //    the canonical reader is the only line left, and it holds.
    let tampered = {
        use aster::coordination::rowstream::{
            read_body, write_varint, BodyError, BodyRow, BodyVisitor, RecoveryBodyRowV1,
            RecoveryHeaderV1,
        };
        // Decode the real body to learn the final row's exact canonical
        // frame, then rebuild the tail as `varint(len + 1) || row || 0x00` —
        // the same mutation the S5-A canonicality guard exists for, here
        // driven through the full production boot corridor.
        #[derive(Default)]
        struct Rows(Vec<RecoveryBodyRowV1>);
        impl BodyVisitor<RecoveryBodyRowV1> for Rows {
            fn header(&mut self, _h: &RecoveryHeaderV1) -> Result<(), BodyError> {
                Ok(())
            }
            fn row(&mut self, _s: u64, row: RecoveryBodyRowV1) -> Result<(), BodyError> {
                self.0.push(row);
                Ok(())
            }
        }
        let mut rows = Rows::default();
        read_body::<_, RecoveryBodyRowV1>(&mut &body[..], &mut rows)
            .expect("the real export parses");
        let last = RecoveryBodyRowV1::encode(
            &<RecoveryBodyRowV1 as BodyRow>::fory(),
            rows.0.last().expect("the export has rows"),
        )
        .expect("encode");
        let mut frame = Vec::new();
        write_varint(&mut frame, last.len() as u64);
        frame.extend_from_slice(&last);
        assert!(body.ends_with(&frame), "the last row frame ends the body");
        let mut bytes = body[..body.len() - frame.len()].to_vec();
        write_varint(&mut bytes, last.len() as u64 + 1);
        bytes.extend_from_slice(&last);
        bytes.push(0x00);
        bytes
    };
    let tampered_hash: [u8; 32] = blake3::hash(&tampered).into();
    let tampered_path = export_dir.path().join("tampered.recovery");
    std::fs::write(&tampered_path, &tampered).expect("write");
    let bound_to_tampered =
        GroupFixture::new("recovery.tampered", 0xF7, 1).recovering(tampered_hash, &[]);
    let dir = tempfile::tempdir().expect("a directory");
    let err = match AsterServer::builder()
        .config(bound_to_tampered.aster_config(0))
        .builtin_coordination(bound_to_tampered.recovery_config(dir.path(), 0, &tampered_path))
        .start()
        .await
    {
        Ok(_) => panic!("a noncanonical body must not bootstrap even correctly bound"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("recovery import"), "got: {err}");

    // 3. A valid recovery bootstrap, once — then the same directory refuses a
    //    second recovery bootstrap, and accepts an ordinary restart.
    let export_hash: [u8; 32] = blake3::hash(&body).into();
    let good = GroupFixture::new("recovery.good", 0xF8, 1).recovering(export_hash, &[]);
    let dir = tempfile::tempdir().expect("a directory");
    let voter = AsterServer::builder()
        .config(good.aster_config(0))
        .builtin_coordination(good.recovery_config(dir.path(), 0, &intact_path))
        .start()
        .await
        .expect("the valid recovery bootstraps");
    within(
        "recovered readiness",
        voter.coordination().expect("hosted").wait_ready(),
    )
    .await
    .expect("ready");
    voter.shutdown().await;

    let err = match AsterServer::builder()
        .config(good.aster_config(0))
        .builtin_coordination(good.recovery_config(dir.path(), 0, &intact_path))
        .start()
        .await
    {
        Ok(_) => panic!("a founded directory must not import again"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("recovery import") || err.to_string().contains("directory"),
        "got: {err}"
    );

    let voter = AsterServer::builder()
        .config(good.aster_config(0))
        .builtin_coordination(good.existing_config(dir.path()))
        .start()
        .await
        .expect("the ordinary restart works");
    within(
        "restart readiness",
        voter.coordination().expect("hosted").wait_ready(),
    )
    .await
    .expect("ready again");
    voter.shutdown().await;
}
