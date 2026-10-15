#![cfg(all(feature = "enrollment", feature = "docs", feature = "rpc"))]
//! A configured replica is trusted for fleet-namespace synchronization only
//! (enrollment design, "Replica trust"), through the high-level path:
//! `AsterServer` with `[enrollment] replicas`.
//!
//! The replica is a "daemon": an `AsterServer` without replicas that holds the
//! fleet namespace the root publishes into, admitted by its root registration
//! alone (no credential), as Portal's daemons are. The node under test is a
//! "coord-host": an `AsterServer` pointed at the daemon, with no credential.
//! The root is played with the low-level API.

use std::collections::BTreeSet;
use std::ops::AsyncFnMut;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::rpc::{
    AsterClient, AsterServer, Call, CapabilityRequirement, RpcConnection, RpcStatus,
    SerializationMode, ServiceDispatch, StreamHeader,
};
use aster::{
    AsterConfig, BlobFormat, Credential, Doc, EnrollmentConfig, EnrollmentLimits, FetchStrategy,
    Node, PublicKey, RelayMode, SecretKey, Ticket,
};

const FLEET: &str = "replica-trust";
const ROOT: u8 = 1;
const DAEMON: u8 = 3;
const HOST: u8 = 4;
const CLIENT: u8 = 5;
const SYNC_TIMEOUT_MS: u64 = 3000;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn key(seed: u8) -> SecretKey {
    SecretKey::from_bytes([seed; 32])
}
fn public(seed: u8) -> [u8; 32] {
    JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
}
fn claims() -> AdmissionClaims {
    AdmissionClaims {
        scopes: Some(BTreeSet::new()),
        tags: Some(BTreeSet::new()),
        attributes: Some(Default::default()),
    }
}
/// A root-issued credential for `seed`, expiring at `expires`.
fn credential(seed: u8, expires: u64) -> String {
    let mut request = EnrollmentIssueRequest::new(public(seed), FLEET, expires);
    request.claims = claims();
    issue_credential(&key(ROOT), &request, now())
        .unwrap()
        .into_chain()
}

/// `ping`, open to any admitted caller.
struct Ops;

#[async_trait::async_trait]
impl ServiceDispatch for Ops {
    fn name(&self) -> &str {
        "Ops"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["ping"]
    }
    fn method_requires(&self, _method: &str) -> Option<CapabilityRequirement> {
        None
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let _ = call.recv_request().await;
        let _ = call.respond(b"pong".to_vec(), &RpcStatus::ok()).await;
    }
}

async fn ping(rpc: &RpcConnection) -> aster::Result<()> {
    let header = StreamHeader {
        service: "Ops".into(),
        method: "ping".into(),
        version: 1,
        serialization_mode: SerializationMode::Xlang.as_i8(),
        ..Default::default()
    };
    tokio::time::timeout(Duration::from_secs(10), rpc.unary(&header, vec![1]))
        .await
        .map_err(|_| aster::Error::Connection("call timed out".into()))??;
    Ok(())
}

/// Whether `from` can reach `to`'s RPC service at all.
async fn rpc_reaches(from: &Node, to: &AsterServer) -> bool {
    from.add_peer(to.node()).unwrap();
    match from.rpc_connect(&to.id()).await {
        Ok(rpc) => ping(&rpc).await.is_ok(),
        Err(_) => false,
    }
}

/// Whether `from` can fetch `hash` from `to`'s blob store.
async fn blob_reaches(from: &Node, to: &AsterServer, hash: &aster::Hash) -> bool {
    from.add_peer(to.node()).unwrap();
    tokio::time::timeout(
        Duration::from_secs(15),
        from.blobs().download_hash_to_store_from(
            hash,
            &[to.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await
    .is_ok_and(|result| result.is_ok())
}

fn base(seed: u8) -> aster::AsterConfigBuilder {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(key(seed))
        .root_pubkey(PublicKey::from_bytes(public(ROOT)))
}

fn namespace(doc: &Doc) -> [u8; 32] {
    hex::decode(doc.id().unwrap().to_string())
        .unwrap()
        .try_into()
        .unwrap()
}

async fn wait_for_addr(node: &Node) {
    for _ in 0..100 {
        if !node.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn eventually(what: &str, mut condition: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while !condition().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The root: the writable fleet namespace, published to a daemon the way a
/// CLI publication session does.
struct Root {
    node: Node,
    fleet: Doc,
    enrollment: Enrollment,
    _files: tempfile::TempDir,
}

impl Root {
    async fn start() -> Self {
        Self::start_with_policy(Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )))
        .await
    }

    async fn start_with_policy(policy: Arc<dyn AdmissionPolicy>) -> Self {
        let files = tempfile::tempdir().unwrap();
        let node = Node::start_with_alpns(
            base(ROOT)
                .hooks(true)
                .hook_failure_mode(aster::HookFailureMode::FailClosed)
                .build(),
            vec![ENROLLMENT_ALPN.to_vec()],
        )
        .await
        .unwrap();
        wait_for_addr(&node).await;
        let fleet = node.docs().create().await.unwrap();
        let enrollment = Enrollment::install(
            &node,
            Arc::new(
                EnrollmentPolicyStore::open(
                    files.path().join("root.json"),
                    public(ROOT),
                    namespace(&fleet),
                    FLEET,
                )
                .unwrap(),
            ),
            JwtEnrollmentVerifier::new(public(ROOT), FLEET).unwrap(),
            Some(credential(ROOT, now() + 3600)),
            policy,
            EnrollmentOptions::default(),
        )
        .unwrap();
        enrollment.set_policy_document(&fleet).unwrap();
        let served = (node.clone(), enrollment.clone());
        tokio::spawn(async move {
            let (node, enrollment) = served;
            while let Ok((alpn, connection)) = node.accept().await {
                if alpn == ENROLLMENT_ALPN {
                    let enrollment = enrollment.clone();
                    tokio::spawn(async move {
                        if enrollment.accept(&connection).await.is_ok() {
                            connection.closed().await;
                        }
                    });
                }
            }
        });
        Self {
            node,
            fleet,
            enrollment,
            _files: files,
        }
    }

    fn namespace(&self) -> String {
        hex::encode(namespace(&self.fleet))
    }

    async fn register(&self, subject: [u8; 32]) {
        self.write(subject, NodeRegistrationStatus::Active, 1, None)
            .await;
    }

    async fn revoke(&self, subject: [u8; 32]) {
        self.write(subject, NodeRegistrationStatus::Revoked, 2, None)
            .await;
    }

    /// A registration record for `subject`, written by the root's own author
    /// or, with `author`, by another author holding the namespace's write
    /// capability.
    async fn write(
        &self,
        subject: [u8; 32],
        status: NodeRegistrationStatus,
        revision: u64,
        author: Option<&aster::AuthorId>,
    ) {
        let record = NodeRegistrationV1 {
            version: 1,
            issuer: hex::encode(public(ROOT)),
            subject: hex::encode(subject),
            audience: Audience::One(FLEET.into()),
            revision,
            issued_at: now() - 60,
            not_before: None,
            expires_at: None,
            status,
            claims: claims(),
            mandatory: vec![],
            reauthorizes_revision: None,
            reason: Some("test".into()),
        };
        let author = match author {
            Some(author) => author.clone(),
            None => self.node.docs().default_author().await.unwrap(),
        };
        self.fleet
            .set_bytes(
                &author,
                node_registration_key(&subject),
                serde_json::to_vec(&record).unwrap(),
            )
            .await
            .unwrap();
    }

    /// Deliver the fleet namespace into `daemon`, as `aster enroll … --peer`.
    async fn publish(&self, daemon: &AsterServer) {
        self.enrollment.connect(daemon.node().addr()).await.unwrap();
        self.fleet
            .sync_with_timeout(&[daemon.id()], Duration::from_secs(15))
            .await
            .unwrap();
    }
}

fn enrollment(root: &Root, state: &std::path::Path, replicas: Vec<String>) -> EnrollmentConfig {
    EnrollmentConfig {
        audience: Some(FLEET.into()),
        fleet_namespace: Some(root.namespace()),
        state_path: Some(state.to_path_buf()),
        replicas,
        limits: EnrollmentLimits {
            reconciliation_interval_ms: Some(200),
            sync_timeout_ms: Some(SYNC_TIMEOUT_MS),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The daemon: holds the fleet namespace for the root to publish into.
async fn start_daemon(
    root: &Root,
    files: &std::path::Path,
    credential: Option<String>,
) -> AsterServer {
    let mut config = base(DAEMON).enrollment(enrollment(
        root,
        &files.join("daemon-enrollment.json"),
        Vec::new(),
    ));
    if let Some(chain) = credential {
        config = config.enrollment_credential_chain(chain);
    }
    start(config.build()).await.unwrap()
}

fn ticket(server: &AsterServer) -> String {
    Ticket::from_node_addr(&server.node().addr(), Credential::Open)
        .unwrap()
        .to_base58()
        .unwrap()
}

/// The coord-host: no credential, pointed at the daemon.
fn host_config(root: &Root, files: &std::path::Path, daemon: &AsterServer) -> AsterConfig {
    host_builder(root, files, daemon).build()
}

/// The coord-host with a credential, so a client that holds no fleet copy can
/// verify it. The daemon still presents nothing.
fn credentialed_host_config(
    root: &Root,
    files: &std::path::Path,
    daemon: &AsterServer,
) -> AsterConfig {
    host_builder(root, files, daemon)
        .enrollment_credential_chain(credential(HOST, now() + 3600))
        .build()
}

fn host_builder(
    root: &Root,
    files: &std::path::Path,
    daemon: &AsterServer,
) -> aster::AsterConfigBuilder {
    base(HOST)
        .persistent(files.join("host"))
        .enrollment(enrollment(
            root,
            &files.join("host-enrollment.json"),
            vec![ticket(daemon)],
        ))
}

async fn start(config: AsterConfig) -> aster::Result<AsterServer> {
    let server = AsterServer::builder()
        .service(Ops)
        .config(config)
        .start()
        .await?;
    wait_for_addr(server.node()).await;
    Ok(server)
}

/// [`start`] for a restart in the same process: the previous instance's
/// tasks release the policy store's lock shortly after shutdown.
async fn restart(config: AsterConfig) -> aster::Result<AsterServer> {
    for _ in 0..100 {
        match start(config.clone()).await {
            Err(error) if error.to_string().contains("lock acquisition failed") => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            result => return result,
        }
    }
    start(config).await
}

fn enrolled(server: &AsterServer) -> &Enrollment {
    server.enrollment().unwrap()
}

fn session_source(server: &AsterServer, peer: u8) -> Option<AdmissionEvidenceSource> {
    enrolled(server)
        .peer_session(&public(peer))
        .map(|(_, evidence)| evidence.source())
}

/// A client with a credential, enrolled with `server`.
async fn client(root: &Root, server: &AsterServer) -> AsterClient {
    AsterClient::builder()
        .config(
            base(CLIENT)
                .enrollment_credential_chain(credential(CLIENT, now() + 3600))
                .enrollment(EnrollmentConfig {
                    audience: Some(FLEET.into()),
                    fleet_namespace: Some(root.namespace()),
                    // Its own sync from the server would count as the
                    // server's policy freshness; the tests read that.
                    synchronize: Some(false),
                    ..Default::default()
                })
                .build(),
        )
        .address(server.address().unwrap())
        .connect()
        .await
        .unwrap()
}

/// Test 1: a fresh coord-host starts from a daemon it holds no record for,
/// receives the fleet namespace including the daemon's own registration, and
/// then holds an ordinary admitted session with it: the daemon reaches its
/// RPC service, and its policy freshness rests on the daemon.
#[tokio::test]
async fn fresh_node_bootstraps_from_a_credential_less_registered_replica() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        root.register(public(DAEMON)).await;
        root.register(public(HOST)).await;
        let daemon = start_daemon(&root, files.path(), None).await;
        root.publish(&daemon).await;

        let host = start(host_config(&root, files.path(), &daemon))
            .await
            .expect("a fresh coord-host starts from its credential-less daemon");
        assert_eq!(
            enrolled(&host).subject_standing(&public(DAEMON)),
            SubjectStanding::Registered
        );
        assert_eq!(
            enrolled(&host).subject_standing(&public(HOST)),
            SubjectStanding::Registered
        );
        assert!(matches!(
            session_source(&host, DAEMON),
            Some(AdmissionEvidenceSource::RootAuthoredRecord { .. })
        ));
        let ready = enrolled(&host).check_peer(&public(DAEMON)).await.unwrap();
        assert_eq!(
            ready.policy_sync.map(|sync| sync.source),
            Some(SynchronizationSource::Replica(public(DAEMON)))
        );
        assert!(rpc_reaches(daemon.node(), &host).await);

        host.shutdown().await;
        daemon.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// Test 2: replica trust grants no application access. The daemon's
/// credential has expired and the root has not registered it, so after the
/// coord-host synchronized from it, local policy still has nothing admitting
/// it: its RPC and blob requests are refused. Once the root registers it, the
/// coord-host's replica maintenance learns that from the daemon itself and
/// the same requests succeed. The root admits only the daemon, so it can
/// never reach the coord-host: through the fleet document's gossip swarm,
/// HyParView would otherwise forward the coord-host's join to the root,
/// which then dials it and could deliver the registration itself.
#[tokio::test]
async fn replica_gets_no_application_access_until_registered() {
    tokio::time::timeout(Duration::from_secs(150), async {
        let root = Root::start_with_policy(Arc::new(DaemonOnly)).await;
        let files = tempfile::tempdir().unwrap();
        root.register(public(HOST)).await;
        // The root admits the daemon by a short credential to publish into
        // it, then the credential lapses and the daemon presents nothing.
        let expires = now() + 4;
        let daemon = start_daemon(&root, files.path(), Some(credential(DAEMON, expires))).await;
        root.publish(&daemon).await;
        while now() <= expires {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // The daemon admits the coord-host by its registration and serves the
        // fleet namespace; the coord-host cannot admit the daemon, but holds
        // root-authored records now, so it starts unsynchronized.
        let host = start(host_config(&root, files.path(), &daemon))
            .await
            .expect("starts from the root-authored records the daemon served");
        assert_eq!(
            enrolled(&host).subject_standing(&public(HOST)),
            SubjectStanding::Registered
        );
        assert_eq!(
            enrolled(&host).subject_standing(&public(DAEMON)),
            SubjectStanding::Unregistered
        );
        assert!(session_source(&host, DAEMON).is_none());
        let hash = host
            .node()
            .blobs()
            .add_bytes(b"host-only payload".to_vec())
            .await
            .unwrap();
        assert!(!rpc_reaches(daemon.node(), &host).await, "RPC refused");
        assert!(
            !blob_reaches(daemon.node(), &host, &hash).await,
            "blob refused"
        );

        // The root registers the daemon and publishes to it; nothing is
        // published to the coord-host.
        root.register(public(DAEMON)).await;
        root.publish(&daemon).await;
        eventually("the coord-host admits the registered daemon", async || {
            session_source(&host, DAEMON).is_some()
        })
        .await;
        // The registration can only have come from the daemon.
        assert!(root.enrollment.peer_session(&public(HOST)).is_none());
        assert!(rpc_reaches(daemon.node(), &host).await);
        assert!(blob_reaches(daemon.node(), &host, &hash).await);

        host.shutdown().await;
        daemon.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// Test 4: a record in the fleet namespace written by an author other than
/// the root (here a second author on the root's own node, which holds the
/// namespace's write capability) reaches the coord-host through the daemon
/// and is ignored: its subject stays unregistered and cannot enroll.
#[tokio::test]
async fn records_by_a_non_root_author_are_ignored() {
    tokio::time::timeout(Duration::from_secs(120), async {
        const FORGED: u8 = 7;
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        root.register(public(DAEMON)).await;
        root.register(public(HOST)).await;
        let other = root.node.docs().create_author().await.unwrap();
        root.write(
            public(FORGED),
            NodeRegistrationStatus::Active,
            1,
            Some(&other),
        )
        .await;
        let daemon = start_daemon(&root, files.path(), None).await;
        root.publish(&daemon).await;

        let host = start(host_config(&root, files.path(), &daemon))
            .await
            .unwrap();
        assert_eq!(
            enrolled(&host).subject_standing(&public(DAEMON)),
            SubjectStanding::Registered
        );
        assert_eq!(
            enrolled(&host).subject_standing(&public(FORGED)),
            SubjectStanding::Unregistered,
            "a non-root author's registration admits nothing"
        );

        // The forged subject, presenting no credential, is refused.
        let forged = Node::start_with_alpns(
            base(FORGED)
                .hooks(true)
                .hook_failure_mode(aster::HookFailureMode::FailClosed)
                .build(),
            vec![ENROLLMENT_ALPN.to_vec()],
        )
        .await
        .unwrap();
        let forged_files = tempfile::tempdir().unwrap();
        let forged_enrollment = Enrollment::install(
            &forged,
            Arc::new(
                EnrollmentPolicyStore::open(
                    forged_files.path().join("forged.json"),
                    public(ROOT),
                    namespace(&root.fleet),
                    FLEET,
                )
                .unwrap(),
            ),
            JwtEnrollmentVerifier::new(public(ROOT), FLEET).unwrap(),
            None,
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy,
            )),
            EnrollmentOptions::default(),
        )
        .unwrap();
        assert!(forged_enrollment.connect(host.node().addr()).await.is_err());
        assert!(session_source(&host, FORGED).is_none());

        forged.shutdown().await;
        host.shutdown().await;
        daemon.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// The root admits only the daemon, so it never holds a session with (or
/// gossips to) anyone else: whatever reaches the coord-host came through the
/// daemon.
struct DaemonOnly;

#[async_trait::async_trait]
impl AdmissionPolicy for DaemonOnly {
    async fn evaluate(
        &self,
        evidence: &VerifiedAdmissionEvidence,
        _: &AdmissionContext,
        _: u64,
    ) -> Result<AdmissionDecision, PolicyError> {
        Ok(if evidence.subject() == &public(DAEMON) {
            AdmissionDecision::Allow
        } else {
            AdmissionDecision::Deny(DenialReason::Consumer("root talks to the daemon".into()))
        })
    }
}

/// Test 5: after startup the fleet namespace stays synchronized with the
/// daemon. A revocation the root publishes to the daemon alone reaches the
/// coord-host and closes the revoked client's connection; the root admits
/// only the daemon, so it never holds a session with the coord-host.
#[tokio::test]
async fn revocations_published_to_the_replica_reach_the_node() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start_with_policy(Arc::new(DaemonOnly)).await;
        let files = tempfile::tempdir().unwrap();
        root.register(public(DAEMON)).await;
        root.register(public(HOST)).await;
        let daemon = start_daemon(&root, files.path(), None).await;
        root.publish(&daemon).await;
        let host = start(credentialed_host_config(&root, files.path(), &daemon))
            .await
            .unwrap();

        let client = client(&root, &host).await;
        ping(&client.rpc()).await.unwrap();
        let mut revoked = enrolled(&host).revocations();

        root.revoke(public(CLIENT)).await;
        root.publish(&daemon).await;
        tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                if revoked.recv().await.unwrap().peer == public(CLIENT) {
                    break;
                }
            }
        })
        .await
        .expect("the coord-host learns the revocation from the daemon");
        assert!(ping(&client.rpc()).await.is_err(), "connection closed");
        assert!(root.enrollment.peer_session(&public(HOST)).is_none());

        client.close().await;
        host.shutdown().await;
        daemon.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// Test 6: a coord-host whose daemon is down restarts from its persisted
/// fleet copy, unsynchronized, and synchronizes again once the daemon is back.
/// A fresh node with nothing persisted still fails to start.
#[tokio::test]
async fn restart_with_a_local_copy_proceeds_while_the_replica_is_down() {
    tokio::time::timeout(Duration::from_secs(150), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        root.register(public(DAEMON)).await;
        root.register(public(HOST)).await;
        let daemon = start_daemon(&root, files.path(), None).await;
        root.publish(&daemon).await;
        let config = credentialed_host_config(&root, files.path(), &daemon);
        let host = start(config.clone()).await.unwrap();
        host.shutdown().await;
        daemon.shutdown().await;

        let host = restart(config)
            .await
            .expect("a persisted fleet copy keeps the coord-host starting");
        assert_eq!(
            enrolled(&host).subject_standing(&public(DAEMON)),
            SubjectStanding::Registered
        );
        let client = client(&root, &host).await;
        ping(&client.rpc()).await.unwrap();
        let ready = enrolled(&host).check_peer(&public(CLIENT)).await.unwrap();
        assert!(ready.policy_sync.is_none(), "reported unsynchronized");

        // Nothing persisted: no replica, no start.
        let fresh = start(
            base(6)
                .enrollment(enrollment(
                    &root,
                    &files.path().join("fresh-enrollment.json"),
                    vec![ticket(&host)],
                ))
                .build(),
        )
        .await;
        drop(fresh.err().expect("an empty fleet copy needs a replica"));

        // The daemon comes back (same identity, fresh state, a new port the
        // coord-host learns as any address update); the root publishes to it,
        // and the coord-host synchronizes again.
        let daemon = restart(
            base(DAEMON)
                .enrollment(enrollment(
                    &root,
                    &files.path().join("daemon-enrollment.json"),
                    Vec::new(),
                ))
                .build(),
        )
        .await
        .unwrap();
        host.node().add_peer(daemon.node()).unwrap();
        root.publish(&daemon).await;
        eventually("the coord-host synchronizes from the daemon", async || {
            enrolled(&host)
                .check_peer(&public(CLIENT))
                .await
                .is_ok_and(|ready| ready.policy_sync.is_some())
        })
        .await;
        assert!(matches!(
            session_source(&host, DAEMON),
            Some(AdmissionEvidenceSource::RootAuthoredRecord { .. })
        ));

        client.close().await;
        host.shutdown().await;
        daemon.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// Test 7: replica trust is scoped to configured replicas. A node without
/// replicas starts as before, holding the fleet namespace for the root to
/// publish into, and still refuses to enroll with a credential-less peer it
/// holds no record for.
#[tokio::test]
async fn without_replicas_a_credential_less_peer_is_still_refused() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        root.register(public(DAEMON)).await;
        root.register(public(HOST)).await;
        let daemon = start_daemon(&root, files.path(), None).await;
        root.publish(&daemon).await;
        let host = start(
            base(HOST)
                .enrollment(enrollment(
                    &root,
                    &files.path().join("host-enrollment.json"),
                    Vec::new(),
                ))
                .build(),
        )
        .await
        .unwrap();
        assert_eq!(
            enrolled(&host).subject_standing(&public(DAEMON)),
            SubjectStanding::Unregistered
        );
        assert!(enrolled(&host).connect(daemon.node().addr()).await.is_err());
        assert!(session_source(&host, DAEMON).is_none());
        assert!(!rpc_reaches(daemon.node(), &host).await);
        // Nor on the ALPNs replica synchronization uses.
        let hash = host
            .node()
            .blobs()
            .add_bytes(b"host-only payload".to_vec())
            .await
            .unwrap();
        assert!(!blob_reaches(daemon.node(), &host, &hash).await);

        // As before, a root publication session delivers the daemon's
        // registration, and the daemon is then admitted.
        root.publish(&host).await;
        eventually("the root's publication reaches the node", async || {
            enrolled(&host).subject_standing(&public(DAEMON)) == SubjectStanding::Registered
        })
        .await;
        // Past the on-demand retry backoff of the refusals above.
        eventually("the daemon reaches the node", async || {
            rpc_reaches(daemon.node(), &host).await
        })
        .await;
        assert!(blob_reaches(daemon.node(), &host, &hash).await);

        host.shutdown().await;
        daemon.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}
