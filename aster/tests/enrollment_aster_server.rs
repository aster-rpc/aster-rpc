#![cfg(all(feature = "enrollment", feature = "docs", feature = "rpc"))]
//! Enrollment through the high-level path only: `AsterServer` and
//! `AsterClient` configured by `AsterConfig`'s `[enrollment]` section. The
//! root is played with the low-level API (root operations belong to the CLI);
//! everything the application writes is configuration and `requires`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::delegation::{issue_delegation, Chain, DelegationGrant, LinkKind, Validity};
use aster::enrollment::*;
use aster::rpc::{
    require_any_of, require_role, AsterClient, AsterServer, Call, CapabilityRequirement,
    RoleChangeError, RpcConnection, RpcStatus, SerializationMode, ServiceDispatch, StatusCode,
    StreamHeader,
};
use aster::{
    AsterConfig, Credential, Doc, EnrollmentConfig, EnrollmentLimits, Node, PublicKey, RelayMode,
    SecretKey, Ticket,
};

const FLEET: &str = "mission-control";

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
fn role_claims(roles: Option<&str>) -> AdmissionClaims {
    let mut attributes = std::collections::BTreeMap::new();
    if let Some(roles) = roles {
        attributes.insert("aster.role".into(), TypedAttribute::String(roles.into()));
    }
    AdmissionClaims {
        scopes: Some(BTreeSet::new()),
        tags: Some(BTreeSet::new()),
        attributes: Some(attributes),
    }
}
/// A root-issued credential for `seed`, carrying `roles` in `aster.role`.
fn credential(seed: u8, roles: Option<&str>) -> String {
    let mut request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
    request.claims = role_claims(roles);
    issue_credential(&key(1), &request, now())
        .unwrap()
        .into_chain()
}

/// `status` requires `ops.status`; `logs` requires `ops.logs` or
/// `ops.admin`; `ping` is open. Each echoes the caller's `aster.role`.
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
        &["ping", "status", "logs"]
    }
    fn method_requires(&self, method: &str) -> Option<CapabilityRequirement> {
        match method {
            "status" => Some(require_role("ops.status")),
            "logs" => Some(require_any_of(["ops.logs", "ops.admin"])),
            _ => None,
        }
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let _ = call.recv_request().await;
        let role = call
            .attributes()
            .get("aster.role")
            .cloned()
            .unwrap_or_default();
        let _ = call.respond(role.into_bytes(), &RpcStatus::ok()).await;
    }
}

fn header(method: &str) -> StreamHeader {
    StreamHeader {
        service: "Ops".into(),
        method: method.into(),
        version: 1,
        serialization_mode: SerializationMode::Xlang.as_i8(),
        ..Default::default()
    }
}

async fn call(rpc: &RpcConnection, method: &str) -> aster::Result<String> {
    let reply = tokio::time::timeout(Duration::from_secs(10), rpc.unary(&header(method), vec![1]))
        .await
        .map_err(|_| aster::Error::Connection("call timed out".into()))??;
    Ok(String::from_utf8(reply).unwrap())
}

fn denied(result: &aster::Result<String>) -> bool {
    matches!(result, Err(e) if e.rpc_code() == Some(StatusCode::PermissionDenied))
}

fn base(seed: u8) -> aster::AsterConfigBuilder {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(key(seed))
        .root_pubkey(PublicKey::from_bytes(public(1)))
}

/// The server: seed 2, with a root-issued credential.
fn server_base() -> aster::AsterConfigBuilder {
    base(2).enrollment_credential_chain(credential(2, None))
}

fn namespace(doc: &Doc) -> [u8; 32] {
    hex::decode(doc.id().unwrap().to_string())
        .unwrap()
        .try_into()
        .unwrap()
}

/// Wait until `node` has a direct address to hand out: a condition wait
/// on the endpoint's address watcher, failing loudly after 30 s rather
/// than letting a later connect fail for an unrelated-looking reason.
async fn wait_for_addr(node: &Node) {
    use aster::native::iroh::Watcher;
    let mut addrs = node.native_endpoint().watch_addr();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while node.addr().direct_addresses.is_empty() {
            addrs.updated().await.expect("endpoint closed");
        }
    })
    .await
    .expect("the node had no direct address after 30 s");
}

/// The root: holds the writable fleet namespace and serves enrollment, as a
/// CLI publication session or a long-running policy replica would.
struct Root {
    node: Node,
    fleet: Doc,
    enrollment: Enrollment,
    _files: tempfile::TempDir,
}

impl Root {
    async fn start() -> Self {
        let files = tempfile::tempdir().unwrap();
        let node = Node::start_with_alpns(
            base(1)
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
                    public(1),
                    namespace(&fleet),
                    FLEET,
                )
                .unwrap(),
            ),
            JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
            Some(credential(1, None)),
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy,
            )),
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

    fn ticket(&self) -> String {
        Ticket::from_node_addr(&self.node.addr(), Credential::Open)
            .unwrap()
            .to_base58()
            .unwrap()
    }

    fn namespace(&self) -> String {
        hex::encode(namespace(&self.fleet))
    }

    /// Publish a revoked registration for `subject` in the fleet namespace.
    async fn revoke(&self, subject: [u8; 32]) {
        self.publish(subject, NodeRegistrationStatus::Revoked).await;
    }

    /// Promote `subject`: publish an active registration in the fleet
    /// namespace, as `aster enroll promote` does.
    async fn register(&self, subject: [u8; 32]) {
        self.publish(subject, NodeRegistrationStatus::Active).await;
    }

    async fn publish(&self, subject: [u8; 32], status: NodeRegistrationStatus) {
        let record = NodeRegistrationV1 {
            version: 1,
            issuer: hex::encode(public(1)),
            subject: hex::encode(subject),
            audience: Audience::One(FLEET.into()),
            revision: 1,
            issued_at: now() - 60,
            not_before: None,
            expires_at: None,
            status,
            claims: role_claims(None),
            mandatory: vec![],
            reauthorizes_revision: None,
            reason: Some("test".into()),
        };
        let author = self.node.docs().default_author().await.unwrap();
        self.fleet
            .set_bytes(
                &author,
                node_registration_key(&subject),
                serde_json::to_vec(&record).unwrap(),
            )
            .await
            .unwrap();
    }
}

/// The server's `[enrollment]` section.
fn server_enrollment(root: &Root, state: &std::path::Path) -> EnrollmentConfig {
    EnrollmentConfig {
        audience: Some(FLEET.into()),
        fleet_namespace: Some(root.namespace()),
        state_path: Some(state.join("server-enrollment.json")),
        limits: EnrollmentLimits {
            reconciliation_interval_ms: Some(200),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn start_server(config: AsterConfig) -> aster::Result<AsterServer> {
    let server = AsterServer::builder()
        .service(Ops)
        .config(config)
        .start()
        .await?;
    wait_for_addr(server.node()).await;
    Ok(server)
}

/// [`start_server`] for a restart in the same process: the previous
/// instance's tasks release the policy store's lock shortly after shutdown.
async fn restart_server(config: AsterConfig) -> aster::Result<AsterServer> {
    for _ in 0..100 {
        match start_server(config.clone()).await {
            Err(error) if error.to_string().contains("lock acquisition failed") => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            result => return result,
        }
    }
    start_server(config).await
}

fn client_config(root: &Root, seed: u8, chain: String) -> AsterConfig {
    base(seed)
        .enrollment_credential_chain(chain)
        .enrollment(EnrollmentConfig {
            audience: Some(FLEET.into()),
            fleet_namespace: Some(root.namespace()),
            ..Default::default()
        })
        .build()
}

async fn connect(server: &AsterServer, config: AsterConfig) -> aster::Result<AsterClient> {
    AsterClient::builder()
        .config(config)
        .address(server.address()?)
        .connect()
        .await
}

/// The roles in a client's credential are what `requires` checks, with no
/// application code; a credential without the role is refused, and a node
/// with no credential cannot even reach the RPC service. A revocation the
/// root publishes refuses the next call and closes the connection.
#[tokio::test]
async fn credential_roles_gate_requires_and_root_revocation_closes_the_connection() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        // No replicas: the server holds the fleet namespace and the root
        // publishes into it, as `aster enroll revoke --peer <server>` does.
        let server = start_server(
            server_base()
                .enrollment(server_enrollment(&root, files.path()))
                .build(),
        )
        .await
        .unwrap();
        assert!(server.enrollment().is_some());
        assert!(server.take_admission().is_none(), "Enrollment owns Gate 0");

        let operator = connect(
            &server,
            client_config(&root, 5, credential(5, Some("ops.status, ops.logs"))),
        )
        .await
        .unwrap();
        assert_eq!(
            call(&operator.rpc(), "ping").await.unwrap(),
            "ops.logs,ops.status"
        );
        assert_eq!(
            call(&operator.rpc(), "status").await.unwrap(),
            "ops.logs,ops.status"
        );
        assert_eq!(
            call(&operator.rpc(), "logs").await.unwrap(),
            "ops.logs,ops.status"
        );

        let ingest = connect(
            &server,
            client_config(&root, 6, credential(6, Some("ops.ingest"))),
        )
        .await
        .unwrap();
        assert_eq!(call(&ingest.rpc(), "ping").await.unwrap(), "ops.ingest");
        assert!(denied(&call(&ingest.rpc(), "status").await));
        assert!(denied(&call(&ingest.rpc(), "logs").await));

        // No enrollment at all: Gate 0 refuses the connection itself.
        let stranger = AsterClient::builder()
            .config(base(8).build())
            .address(server.address().unwrap())
            .connect()
            .await;
        match stranger {
            Err(_) => {}
            Ok(stranger) => {
                assert!(call(&stranger.rpc(), "ping").await.is_err());
                stranger.close().await;
            }
        }

        // The root enrolls with the server and delivers a revocation of the
        // operator into the fleet namespace the server holds.
        let mut revoked = server.enrollment().unwrap().revocations();
        root.enrollment.connect(server.node().addr()).await.unwrap();
        root.revoke(public(5)).await;
        root.fleet
            .sync_with_timeout(&[server.id()], Duration::from_secs(15))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if revoked.recv().await.unwrap().peer == public(5) {
                    break;
                }
            }
        })
        .await
        .expect("the server revokes the operator");
        let rpc = operator.rpc();
        assert!(call(&rpc, "status").await.is_err(), "revoked: refused");
        assert!(call(&rpc, "ping").await.is_err(), "connection closed");
        // Unaffected peers keep working.
        assert_eq!(call(&ingest.rpc(), "ping").await.unwrap(), "ops.ingest");
        // Enrolling again as the revoked operator: the server's refusal keeps
        // its admission kind (denied), not a generic failure.
        match connect(
            &server,
            client_config(&root, 5, credential(5, Some("ops.status"))),
        )
        .await
        {
            Err(aster::Error::Transport(error)) => assert!(
                matches!(
                    error.downcast_ref::<AdmissionError>(),
                    Some(AdmissionError::Denied(_))
                ),
                "{error:#}"
            ),
            Err(other) => panic!("expected a denied admission, got {other:?}"),
            Ok(_) => panic!("a revoked operator enrolled again"),
        }

        for client in [operator, ingest] {
            client.close().await;
        }
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// Peer events on an enrolled server: a peer the gate refuses produces none,
/// an admitted one is `Connected` with the roles of its credential, and the
/// root's revocation ends it as `Revoked`.
#[tokio::test]
async fn peer_events_follow_admission_and_report_revocation() {
    use aster::rpc::{DisconnectReason, PeerEvent};
    use aster_transport_core::test_support::wait_budget;

    async fn event(events: &mut aster::rpc::PeerEventReceiver) -> PeerEvent {
        tokio::time::timeout(wait_budget(), events.recv())
            .await
            .expect("a peer event")
            .unwrap()
    }

    tokio::time::timeout(wait_budget() * 8, async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = start_server(
            server_base()
                .enrollment(server_enrollment(&root, files.path()))
                .build(),
        )
        .await
        .unwrap();
        let (snapshot, mut events) = server.peer_events();
        assert!(snapshot.is_empty());

        // No enrollment: Gate 0 refuses it, so it never connects. The next
        // event is the operator's, not the stranger's.
        let stranger = AsterClient::builder()
            .config(base(8).build())
            .address(server.address().unwrap())
            .connect()
            .await;
        if let Ok(stranger) = stranger {
            let _ = call(&stranger.rpc(), "ping").await;
            stranger.close().await;
        }

        let operator = connect(
            &server,
            client_config(&root, 5, credential(5, Some("ops.status, ops.logs"))),
        )
        .await
        .unwrap();
        let operator_id = hex::encode(public(5));
        match event(&mut events).await {
            PeerEvent::Connected {
                peer,
                alpn,
                identity,
            } => {
                assert_eq!(peer, operator_id);
                assert_eq!(alpn, b"aster/1");
                assert_eq!(identity.endpoint_id, operator_id);
                assert_eq!(identity.principal.as_deref(), Some(operator_id.as_str()));
                assert_eq!(identity.roles, vec!["ops.logs", "ops.status"]);
            }
            other => panic!("expected the operator to connect, got {other:?}"),
        }
        assert_eq!(server.peer_events().0.len(), 1);

        let mut revoked = server.enrollment().unwrap().revocations();
        root.enrollment.connect(server.node().addr()).await.unwrap();
        root.revoke(public(5)).await;
        root.fleet
            .sync_with_timeout(&[server.id()], wait_budget())
            .await
            .unwrap();
        tokio::time::timeout(wait_budget(), async {
            loop {
                if revoked.recv().await.unwrap().peer == public(5) {
                    break;
                }
            }
        })
        .await
        .expect("the server revokes the operator");
        assert_eq!(
            event(&mut events).await,
            PeerEvent::Disconnected {
                peer: operator_id,
                reason: DisconnectReason::Revoked
            }
        );

        operator.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// A credential admits a persistent node only until the root registers it
/// (design §4), so a registered server restarts with its credential expired,
/// or with none, and its replica admits it by the registration. While it ran
/// past expiry it stopped publishing the expired credential's locality
/// statement (declared locality falls back to the record). An unregistered node with an expired
/// credential is still refused.
#[tokio::test]
async fn registered_server_restarts_without_a_valid_credential() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let enrollment = EnrollmentConfig {
            replicas: vec![root.ticket()],
            ..server_enrollment(&root, files.path())
        };
        let expires = now() + 4;
        let short = {
            let mut request = EnrollmentIssueRequest::new(public(2), FLEET, expires);
            // Locality claims, so the node has a statement to publish (and to
            // stop publishing once the credential expires).
            request.claims.attributes = Some(std::collections::BTreeMap::from([
                (
                    aster::topology::REGION_ATTRIBUTE.to_string(),
                    TypedAttribute::String("eu-west".into()),
                ),
                (
                    aster::topology::ZONE_ATTRIBUTE.to_string(),
                    TypedAttribute::String("eu-west-1a".into()),
                ),
            ]));
            issue_credential(&key(1), &request, now())
                .unwrap()
                .into_chain()
        };
        let config = |chain: Option<String>| {
            let mut builder = base(2).enrollment(enrollment.clone());
            if let Some(chain) = chain {
                builder = builder.enrollment_credential_chain(chain);
            }
            builder.build()
        };
        let source = |peer: [u8; 32]| {
            root.enrollment
                .peer_session(&peer)
                .map(|(_, evidence)| evidence.source())
        };

        // Admitted on its credential; the root then promotes it.
        let server = start_server(config(Some(short.clone()))).await.unwrap();
        assert!(matches!(
            source(public(2)),
            Some(AdmissionEvidenceSource::PresentedChain { .. })
        ));
        root.register(public(2)).await;
        let locality = server.enrollment().unwrap().topology_locality();
        assert!((locality.own_statement)().is_some());
        while now() <= expires {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            (locality.own_statement)().is_none(),
            "an expired credential's locality statement is no longer published"
        );
        // The locality source holds the enrollment; release it so the
        // restart below can take the policy store's lock.
        drop(locality);
        server.shutdown().await;

        // Restarts with the expired credential, then with none at all.
        for chain in [Some(short.clone()), None] {
            let server = restart_server(config(chain)).await.unwrap();
            assert!(server.enrollment().unwrap().credential().is_none());
            assert!(matches!(
                source(public(2)),
                Some(AdmissionEvidenceSource::RootAuthoredRecord { .. })
            ));
            server.shutdown().await;
        }

        // Not registered: the expired credential starts nothing the replica
        // admits, so the server cannot attach policy and does not start.
        let expired = {
            let request = EnrollmentIssueRequest::new(public(4), FLEET, now() - 60);
            issue_credential(&key(1), &request, now() - 3600)
                .unwrap()
                .into_chain()
        };
        let refused = start_server(
            base(4)
                .enrollment_credential_chain(expired)
                .enrollment(EnrollmentConfig {
                    state_path: Some(files.path().join("unregistered.json")),
                    ..enrollment.clone()
                })
                .build(),
        )
        .await;
        let error = refused
            .err()
            .expect("an unregistered expired node is refused");
        assert!(
            error
                .to_string()
                .contains("enrolling with the policy replicas"),
            "{error}"
        );
        assert!(source(public(4)).is_none());
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// The credential sets the maximum; runtime settings can only restrict
/// (design §15.6). A credential granting `ops.status,ops.logs` restricted to
/// `ops.status` refuses `logs` on the next call; an empty restriction refuses
/// every gated method; neither `set_role` nor a restriction can grant a role
/// the credential lacks; clearing restores the credential roles; and a
/// revocation ends the session and the restriction with it.
#[tokio::test]
async fn runtime_restriction_narrows_credential_roles() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = start_server(
            server_base()
                .enrollment(server_enrollment(&root, files.path()))
                .build(),
        )
        .await
        .unwrap();
        let client = connect(
            &server,
            client_config(&root, 5, credential(5, Some("ops.status,ops.logs"))),
        )
        .await
        .unwrap();
        let rpc = client.rpc();
        let peer = hex::encode(public(5));
        let attributes = server.attributes();
        assert_eq!(call(&rpc, "logs").await.unwrap(), "ops.logs,ops.status");

        // Restrict to `ops.status`: `logs` is refused from the next call.
        attributes.restrict_roles(&peer, ["ops.status"]).unwrap();
        assert_eq!(call(&rpc, "status").await.unwrap(), "ops.status");
        assert!(denied(&call(&rpc, "logs").await));

        // Restrict to nothing: every gated method is refused.
        attributes
            .restrict_roles(&peer, Vec::<String>::new())
            .unwrap();
        assert_eq!(call(&rpc, "ping").await.unwrap(), "");
        assert!(denied(&call(&rpc, "status").await));
        assert!(denied(&call(&rpc, "logs").await));

        // A runtime grant of a role the credential lacks is refused outright,
        // and a restriction naming it grants nothing.
        assert_eq!(
            attributes.set_role(&peer, "ops.admin"),
            Err(RoleChangeError::CredentialOwnsRoles(peer.clone()))
        );
        attributes.restrict_roles(&peer, ["ops.admin"]).unwrap();
        assert!(denied(&call(&rpc, "logs").await));

        // Clearing restores the credential roles.
        assert!(attributes.clear_restriction(&peer));
        assert_eq!(call(&rpc, "logs").await.unwrap(), "ops.logs,ops.status");

        // A revocation ends the session, and the restriction with it.
        attributes.restrict_roles(&peer, ["ops.status"]).unwrap();
        let mut revoked = server.enrollment().unwrap().revocations();
        root.enrollment.connect(server.node().addr()).await.unwrap();
        root.revoke(public(5)).await;
        root.fleet
            .sync_with_timeout(&[server.id()], Duration::from_secs(15))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while revoked.recv().await.unwrap().peer != public(5) {}
        })
        .await
        .expect("the server revokes the client");
        assert_eq!(attributes.restricted_roles(&peer), None);
        assert_eq!(
            attributes.restrict_roles(&peer, ["ops.status"]),
            Err(RoleChangeError::NotEnrolled(peer.clone()))
        );

        client.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// Without enrollment there is no credential to bound the roles: the
/// application's runtime attributes grant them exactly as before, and a
/// restriction is refused because there is no session to restrict.
#[tokio::test]
async fn runtime_roles_still_grant_without_enrollment() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let server = start_server(base(2).build()).await.unwrap();
        let client = AsterClient::builder()
            .config(base(9).build())
            .address(server.address().unwrap())
            .connect()
            .await
            .unwrap();
        let rpc = client.rpc();
        let peer = client.node().id().to_string();
        assert!(denied(&call(&rpc, "status").await));
        server.attributes().set_role(&peer, "ops.status").unwrap();
        assert_eq!(call(&rpc, "status").await.unwrap(), "ops.status");
        assert_eq!(
            server.attributes().restrict_roles(&peer, ["ops.status"]),
            Err(RoleChangeError::NotEnrolled(peer.clone()))
        );
        client.close().await;
        server.shutdown().await;
    })
    .await
    .unwrap();
}

/// A server configured as a delegated issuer serves renewals; a client whose
/// credential that server signed renews automatically and stays admitted, and
/// its roles keep working, across several short lifetimes. The server syncs
/// policy from a configured replica (the root).
#[tokio::test]
async fn config_driven_issuer_renews_client_credentials() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let delegation: Chain = issue_delegation(
            &key(1),
            None,
            &key(2).public_key(),
            &Audience::One(FLEET.into()),
            &DelegationGrant {
                may_issue: [LinkKind::Enrollment].into(),
                audiences: [FLEET.to_string()].into(),
                scopes: BTreeSet::new(),
                tags: BTreeSet::new(),
                attribute_keys: ["aster.role".to_string()].into(),
                max_child_lifetime: 3600,
                max_depth: 0,
                subjects: None,
                operations: BTreeSet::new(),
                mandatory: BTreeSet::new(),
                may_register: false,
            },
            Validity {
                issued_at: now(),
                not_before: None,
                expires_at: now() + 3600,
            },
        )
        .unwrap();
        let delegation_file = files.path().join("issuer.chain");
        std::fs::write(&delegation_file, format!("{}\n", delegation.to_text())).unwrap();
        let server = start_server(
            server_base()
                .enrollment(EnrollmentConfig {
                    replicas: vec![root.ticket()],
                    issuer_delegation_file: Some(delegation_file),
                    issuer_lifetime_secs: Some(15),
                    ..server_enrollment(&root, files.path())
                })
                .build(),
        )
        .await
        .unwrap();
        let issuer = server.issuer().expect("configured issuer");
        let issued = issuer
            .issue_for(
                &public(7),
                role_claims(Some("ops.status,ops.logs")),
                Registration::Ephemeral,
            )
            .unwrap()
            .into_chain();

        let client = connect(&server, client_config(&root, 7, issued.clone()))
            .await
            .unwrap();
        assert!(client.is_renewing(), "the server signed it, so it renews");
        // A restriction belongs to the session, so it survives renewals of
        // the same subject.
        server
            .attributes()
            .restrict_roles(&hex::encode(public(7)), ["ops.status"])
            .unwrap();
        // Past the first credential's expiry: renewal starts with a third of the
        // lifetime left (5 s), enough to re-present on a loaded host.
        let deadline = now() + 15 + 3;
        while now() < deadline {
            assert_eq!(call(&client.rpc(), "status").await.unwrap(), "ops.status");
            assert!(denied(&call(&client.rpc(), "logs").await));
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let (current, _) = client.enrollment().unwrap().credential().unwrap();
        assert_ne!(current, issued, "the credential was renewed");

        client.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// Misconfiguration stops the start with an error that names the problem;
/// nothing runs open.
#[tokio::test]
async fn misconfiguration_fails_closed_with_a_clear_error() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let good = server_enrollment(&root, files.path());

        async fn refused(config: AsterConfig, expect: &str) {
            let error = match start_server(config).await {
                Ok(_) => panic!("started despite: {expect}"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains(expect), "{expect:?} not in {error:?}");
        }

        refused(
            server_base()
                .enrollment(EnrollmentConfig {
                    audience: None,
                    ..good.clone()
                })
                .build(),
            "audience is required",
        )
        .await;
        refused(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .secret_key(key(2))
                .enrollment_credential_chain(credential(2, None))
                .enrollment(good.clone())
                .build(),
            "no trusted root",
        )
        .await;
        refused(
            server_base()
                .enrollment(EnrollmentConfig {
                    state_path: None,
                    ..good.clone()
                })
                .build(),
            "keeps denials durably",
        )
        .await;
        // A server without a credential starts: only a root registration
        // can admit it (`registered_server_restarts_without_a_valid_credential`).
        let uncredentialed = restart_server(base(2).enrollment(good.clone()).build())
            .await
            .unwrap();
        assert!(uncredentialed.enrollment().unwrap().credential().is_none());
        uncredentialed.shutdown().await;
        // A file path where the inline chain belongs.
        refused(
            base(2)
                .enrollment_credential_chain("/etc/aster/server.cred")
                .enrollment(good.clone())
                .build(),
            "ASTER_ENROLLMENT_CREDENTIAL)",
        )
        .await;
        // A credential issued for another key does not verify for this node:
        // it is dropped and never presented, like an expired one.
        let foreign = restart_server(
            base(2)
                .enrollment_credential_chain(credential(3, None))
                .enrollment(good.clone())
                .build(),
        )
        .await
        .unwrap();
        assert!(foreign.enrollment().unwrap().credential().is_none());
        foreign.shutdown().await;
        refused(
            server_base()
                .enrollment(EnrollmentConfig {
                    policy: Some(serde_json::json!("allow_everyone")),
                    ..good.clone()
                })
                .build(),
            "policy",
        )
        .await;
        refused(
            server_base()
                .enrollment(EnrollmentConfig {
                    fleet_namespace: Some("not-hex".into()),
                    ..good.clone()
                })
                .build(),
            "fleet_namespace",
        )
        .await;
        // Enabled with nothing else set: still refused, never open.
        refused(
            server_base()
                .enrollment(EnrollmentConfig {
                    enabled: Some(true),
                    ..Default::default()
                })
                .build(),
            "audience is required",
        )
        .await;

        // A client: no credential, and server-only issuer settings.
        let server = restart_server(server_base().enrollment(good.clone()).build())
            .await
            .unwrap();
        let mut no_credential = client_config(&root, 5, String::new());
        no_credential.enrollment_credential_chain = None;
        let error = connect(&server, no_credential).await.err().unwrap();
        assert!(error.to_string().contains("no credential"), "{error}");
        let mut issuer = client_config(&root, 5, credential(5, None));
        issuer.enrollment.issuer_delegation_file = Some(files.path().join("x"));
        let error = connect(&server, issuer).await.err().unwrap();
        assert!(error.to_string().contains("server-only"), "{error}");
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}

/// With Enrollment installed, the compressed blobs ALPN needs an admitted
/// session exactly like the plain one: a node without a credential is refused
/// on both, and an enrolled client fetches over zstd.
#[tokio::test]
async fn compressed_blob_transport_requires_a_session_like_plain_blobs() {
    use aster::{BlobFormat, BlobTransportCompression, FetchStrategy};
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = Root::start().await;
        let files = tempfile::tempdir().unwrap();
        let server = start_server(
            server_base()
                .enrollment(server_enrollment(&root, files.path()))
                .build(),
        )
        .await
        .unwrap();
        let payload = b"enrolled compressed payload ".repeat(512);
        let hash = server.blobs().add_bytes(payload.clone()).await.unwrap();
        let fetch = |blobs: aster::Blobs, mode: BlobTransportCompression| {
            let hash = hash.clone();
            let from = [server.node().id()];
            async move {
                tokio::time::timeout(
                    Duration::from_secs(20),
                    blobs
                        .with_transport_compression(mode)
                        .download_hash_to_store_from(
                            &hash,
                            &from,
                            BlobFormat::Raw,
                            FetchStrategy::Whole,
                        ),
                )
                .await
                .expect("fetch timed out")
            }
        };

        // No credential, so no session: refused on both blobs ALPNs.
        let stranger = Node::start(base(9).build()).await.unwrap();
        wait_for_addr(&stranger).await;
        stranger.add_peer(server.node()).unwrap();
        for mode in [
            BlobTransportCompression::RequireZstd,
            BlobTransportCompression::Off,
        ] {
            let res = fetch(stranger.blobs(), mode).await;
            assert!(res.is_err(), "{mode:?}: sessionless node fetched: {res:?}");
        }
        assert!(!stranger.blobs().has(&hash).await.unwrap());

        // An enrolled client has a session: both ALPNs serve it.
        let client = connect(&server, client_config(&root, 5, credential(5, None)))
            .await
            .unwrap();
        client.node().add_peer(server.node()).unwrap();
        let report = fetch(client.node().blobs(), BlobTransportCompression::RequireZstd)
            .await
            .unwrap();
        assert_eq!(report.served().count(), 1);
        assert_eq!(
            client.node().blobs().read_to_bytes(&hash).await.unwrap(),
            payload
        );
        assert!(client.node().blobs().transfer_stats().fetched_wire_bytes > 0);
        assert!(fetch(client.node().blobs(), BlobTransportCompression::Off)
            .await
            .is_ok());

        stranger.shutdown().await;
        client.close().await;
        server.shutdown().await;
        root.node.shutdown().await;
    })
    .await
    .unwrap();
}
