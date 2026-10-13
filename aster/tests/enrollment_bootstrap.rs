#![cfg(all(feature = "enrollment", feature = "docs"))]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::{AsterConfig, Doc, Node, RelayMode, SecretKey};
use tokio::sync::mpsc;

const NORMAL: &[u8] = b"enrollment.application.test/1";
const FLEET: &str = "offline-root-test";

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
fn token(seed: u8) -> String {
    issue_jwt(
        &key(1),
        &EnrollmentIssueRequest::new(public(seed), FLEET),
        now(),
    )
    .unwrap()
    .into_jwt()
}
async fn node(seed: u8) -> Node {
    Node::start_with_alpns(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(key(seed))
            .hooks(true)
            .hook_failure_mode(aster::HookFailureMode::FailClosed)
            .hook_timeout_ms(1000)
            .build(),
        vec![ENROLLMENT_ALPN.to_vec(), NORMAL.to_vec()],
    )
    .await
    .unwrap()
}
fn enrollment(
    node: &Node,
    namespace: [u8; 32],
    path: &std::path::Path,
    credential: Option<String>,
) -> Enrollment {
    Enrollment::install(
        node,
        Arc::new(EnrollmentPolicyStore::open(path, public(1), namespace, FLEET).unwrap()),
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        credential,
        Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )),
        EnrollmentOptions {
            reconciliation_interval: Duration::from_millis(50),
            ..Default::default()
        },
    )
    .unwrap()
}
fn serve(node: Node, enrollment: Enrollment) -> mpsc::Receiver<aster::Connection> {
    let (send, recv) = mpsc::channel(16);
    tokio::spawn(async move {
        while let Ok((alpn, connection)) = node.accept().await {
            if alpn == ENROLLMENT_ALPN {
                let enrollment = enrollment.clone();
                tokio::spawn(async move {
                    let _ = enrollment.accept(&connection).await;
                    connection.closed().await;
                });
            } else {
                enrollment.track_connection(&connection).await.unwrap();
                let _ = send.send(connection).await;
            }
        }
    });
    recv
}
fn registration(seed: u8, revision: u64, status: NodeRegistrationStatus) -> NodeRegistrationV1 {
    NodeRegistrationV1 {
        version: 1,
        issuer: hex::encode(public(1)),
        subject: hex::encode(public(seed)),
        audience: Audience::One(FLEET.into()),
        revision,
        issued_at: now(),
        not_before: None,
        expires_at: None,
        status,
        claims: AdmissionClaims::default(),
        mandatory: vec![],
        reauthorizes_revision: None,
    }
}
async fn write(root: &Node, doc: &Doc, record: &NodeRegistrationV1) {
    let subject: [u8; 32] = hex::decode(&record.subject).unwrap().try_into().unwrap();
    doc.set_bytes(
        &root.docs().default_author().await.unwrap(),
        node_registration_key(&subject),
        serde_json::to_vec(record).unwrap(),
    )
    .await
    .unwrap();
}
async fn sync(enrollment: &Enrollment, addr: aster::NodeAddr) -> EnrollmentReadiness {
    let (ready, ticket) = enrollment.connect(addr).await.unwrap();
    enrollment
        .synchronize(ready.peer, ticket.unwrap(), Duration::from_secs(15))
        .await
        .unwrap();
    ready
}

async fn ungated_node(seed: u8) -> Node {
    Node::start_with_alpns(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(key(seed))
            .build(),
        vec![ENROLLMENT_ALPN.to_vec()],
    )
    .await
    .unwrap()
}

async fn raw_presentation(node: &Node, replica: &Node, jwt: &str) -> bool {
    node.add_peer(replica).unwrap();
    let connection = node.connect(&replica.id(), ENROLLMENT_ALPN).await.unwrap();
    let (send, recv) = connection.open_bi().await.unwrap();
    let payload = serde_json::to_vec(&serde_json::json!({"version":1,"sent_at":now()})).unwrap();
    send.write_all((payload.len() as u32).to_be_bytes().to_vec())
        .await
        .unwrap();
    send.write_all(payload).await.unwrap();
    let length = u32::from_be_bytes(recv.read_exact(4).await.unwrap().try_into().unwrap());
    let proof: serde_json::Value =
        serde_json::from_slice(&recv.read_exact(length as usize).await.unwrap()).unwrap();
    assert!(proof.get("credential").is_some());
    assert!(proof.get("policy_ticket").is_none());
    let presentation =
        serde_json::to_vec(&serde_json::json!({"credential":jwt,"sent_at":now()})).unwrap();
    send.write_all((presentation.len() as u32).to_be_bytes().to_vec())
        .await
        .unwrap();
    send.write_all(presentation).await.unwrap();
    let denied = tokio::time::timeout(Duration::from_secs(5), recv.read_exact(4))
        .await
        .unwrap()
        .is_err();
    connection.close(0, b"done".to_vec()).unwrap();
    denied
}

#[tokio::test]
async fn network_rejects_copied_wrong_root_and_wrong_audience_credentials() {
    let files = tempfile::tempdir().unwrap();
    let replica = node(2).await;
    let enroll = enrollment(
        &replica,
        [42; 32],
        &files.path().join("replica.json"),
        Some(token(2)),
    );
    let _connections = serve(replica.clone(), enroll.clone());
    let attacker = ungated_node(8).await;
    assert!(raw_presentation(&attacker, &replica, &token(4)).await);
    let wrong_root = issue_jwt(
        &key(9),
        &EnrollmentIssueRequest::new(public(8), FLEET),
        now(),
    )
    .unwrap();
    assert!(raw_presentation(&attacker, &replica, wrong_root.jwt()).await);
    let wrong_audience = issue_jwt(
        &key(1),
        &EnrollmentIssueRequest::new(public(8), "other-fleet"),
        now(),
    )
    .unwrap();
    assert!(raw_presentation(&attacker, &replica, wrong_audience.jwt()).await);
    assert!(enroll.check_peer(&public(8)).await.is_err());
    attacker.shutdown().await;
    replica.shutdown().await;
}

#[tokio::test]
async fn reciprocal_validation_rejects_rogue_replica_and_failopen_install() {
    let files = tempfile::tempdir().unwrap();
    let client = node(4).await;
    let enroll = enrollment(
        &client,
        [42; 32],
        &files.path().join("client.json"),
        Some(token(4)),
    );
    let rogue = ungated_node(9).await;
    let rogue_node = rogue.clone();
    let handler = tokio::spawn(async move {
        let (_, connection) = rogue_node.accept().await.unwrap();
        let (send, recv) = connection.accept_bi().await.unwrap();
        let count = u32::from_be_bytes(recv.read_exact(4).await.unwrap().try_into().unwrap());
        let probe: serde_json::Value =
            serde_json::from_slice(&recv.read_exact(count as usize).await.unwrap()).unwrap();
        assert_eq!(probe["version"], 1);
        assert!(probe["sent_at"].as_u64().unwrap().abs_diff(now()) <= 1);
        let rogue_token = issue_jwt(
            &key(9),
            &EnrollmentIssueRequest::new(public(9), FLEET),
            now(),
        )
        .unwrap();
        let payload = serde_json::to_vec(&serde_json::json!({
            "version":1,"credential":rogue_token.jwt(),"sent_at":now()
        }))
        .unwrap();
        send.write_all((payload.len() as u32).to_be_bytes().to_vec())
            .await
            .unwrap();
        send.write_all(payload).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), recv.read_exact(4))
                .await
                .unwrap()
                .is_err(),
            "client disclosed presentation to unverified replica"
        );
        tokio::time::timeout(Duration::from_secs(5), connection.closed())
            .await
            .unwrap();
    });
    assert!(enroll.connect(rogue.addr()).await.is_err());
    assert!(enroll.check_peer(&public(9)).await.is_err());
    handler.await.unwrap();
    client.shutdown().await;
    rogue.shutdown().await;

    let failopen = Node::start_with_alpns(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(key(10))
            .hooks(true)
            .build(),
        vec![ENROLLMENT_ALPN.to_vec()],
    )
    .await
    .unwrap();
    let store = Arc::new(
        EnrollmentPolicyStore::open(
            files.path().join("failopen.json"),
            public(1),
            [42; 32],
            FLEET,
        )
        .unwrap(),
    );
    assert!(Enrollment::install(
        &failopen,
        store,
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        Some(token(10)),
        Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy
        )),
        Default::default()
    )
    .is_err());
    failopen.shutdown().await;
}

#[tokio::test]
async fn strict_operation_freshness_and_new_connection_source_are_checked() {
    let files = tempfile::tempdir().unwrap();
    let replica = Node::start_with_alpns(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("0.0.0.0:0")
            .secret_key(key(2))
            .hooks(true)
            .hook_failure_mode(aster::HookFailureMode::FailClosed)
            .hook_timeout_ms(1000)
            .build(),
        vec![ENROLLMENT_ALPN.to_vec(), NORMAL.to_vec()],
    )
    .await
    .unwrap();
    let replica_addr = replica.addr();
    let non_loopback = replica_addr
        .direct_addresses
        .iter()
        .filter_map(|value| value.parse::<std::net::SocketAddr>().ok())
        .find(|addr| addr.is_ipv4() && !addr.ip().is_loopback())
        .expect("test requires a local IPv4 interface");
    let mut loopback_addr = replica_addr.clone();
    loopback_addr.direct_addresses = vec![format!("127.0.0.1:{}", non_loopback.port())];
    let doc = replica.docs().create().await.unwrap();
    let namespace: [u8; 32] = hex::decode(doc.id().unwrap().to_string())
        .unwrap()
        .try_into()
        .unwrap();
    let replica_enroll = Enrollment::install(
        &replica,
        Arc::new(
            EnrollmentPolicyStore::open(
                files.path().join("replica.json"),
                public(1),
                namespace,
                FLEET,
            )
            .unwrap(),
        ),
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        Some(token(2)),
        Arc::new(PolicyExpressionV1::Predicate(PolicyPredicate::SourceCidr(
            "127.0.0.1/32".parse().unwrap(),
        ))),
        Default::default(),
    )
    .unwrap();
    replica_enroll.set_policy_document(&doc).unwrap();
    let mut received = serve(replica.clone(), replica_enroll.clone());
    let client = node(4).await;
    let client_enroll = enrollment(
        &client,
        namespace,
        &files.path().join("client.json"),
        Some(token(4)),
    );
    let (ready, ticket) = client_enroll.connect(loopback_addr.clone()).await.unwrap();
    client.add_node_addr(loopback_addr).unwrap();
    let connection = client.connect(&replica.id(), NORMAL).await.unwrap();
    let accepted = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    let strict = PolicyExpressionV1::Predicate(PolicyPredicate::PolicySynchronizedWithin {
        max_age: Duration::from_secs(60),
        require_root: false,
    });
    assert!(client_enroll
        .check_connection_with_policy(&connection, &strict, Duration::from_secs(1))
        .await
        .is_err());
    // Operation denial does not withdraw otherwise valid fleet membership.
    assert!(client_enroll.check_peer(&public(2)).await.is_ok());
    client_enroll
        .synchronize(ready.peer, ticket.unwrap(), Duration::from_secs(10))
        .await
        .unwrap();
    assert!(client_enroll
        .check_connection_with_policy(&connection, &strict, Duration::from_secs(1))
        .await
        .is_ok());
    let root_only = PolicyExpressionV1::Predicate(PolicyPredicate::PolicySynchronizedWithin {
        max_age: Duration::from_secs(60),
        require_root: true,
    });
    assert!(client_enroll
        .check_connection_with_policy(&connection, &root_only, Duration::from_secs(1))
        .await
        .is_err());
    connection.close(0, b"restart".to_vec()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), accepted.closed())
        .await
        .unwrap();
    client.shutdown().await;

    // Same authenticated key, new disallowed source; membership from the old
    // source must not bypass the new connection's own source-CIDR check.
    let moved = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("0.0.0.0:0")
            .secret_key(key(4))
            .build(),
    )
    .await
    .unwrap();
    let mut moved_target = replica_addr;
    moved_target.direct_addresses = vec![non_loopback.to_string()];
    moved.add_node_addr(moved_target).unwrap();
    if let Ok(connection) = moved.connect(&replica.id(), NORMAL).await {
        tokio::time::timeout(Duration::from_secs(5), connection.closed())
            .await
            .unwrap();
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), received.recv())
            .await
            .is_err()
    );
    moved.shutdown().await;
    replica.shutdown().await;
}

#[tokio::test]
async fn oversized_partial_handshakes_and_dropped_hook_owner_fail_closed() {
    let files = tempfile::tempdir().unwrap();
    let replica = node(2).await;
    let enroll = Enrollment::install(
        &replica,
        Arc::new(
            EnrollmentPolicyStore::open(
                files.path().join("replica.json"),
                public(1),
                [42; 32],
                FLEET,
            )
            .unwrap(),
        ),
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        Some(token(2)),
        Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )),
        EnrollmentOptions {
            handshake_timeout: Duration::from_millis(100),
            ..Default::default()
        },
    )
    .unwrap();
    let attacker = ungated_node(8).await;
    attacker.add_peer(&replica).unwrap();
    for oversized in [true, false] {
        let outbound = attacker
            .connect(&replica.id(), ENROLLMENT_ALPN)
            .await
            .unwrap();
        let (_, inbound) = replica.accept().await.unwrap();
        let handler = tokio::spawn({
            let enroll = enroll.clone();
            async move { enroll.accept(&inbound).await }
        });
        let (send, _recv) = outbound.open_bi().await.unwrap();
        send.write_all(
            if oversized { u32::MAX } else { 1u32 }
                .to_be_bytes()
                .to_vec(),
        )
        .await
        .unwrap();
        assert!(handler.await.unwrap().is_err());
        tokio::time::timeout(Duration::from_secs(2), outbound.closed())
            .await
            .unwrap();
    }
    drop(enroll);
    // Let the weak reconciler release its last temporary owner, then an
    // ordinary handshake must reject even when no consumer answers hooks.
    tokio::time::sleep(Duration::from_millis(150)).await;
    if let Ok(connection) = attacker.connect(&replica.id(), NORMAL).await {
        tokio::time::timeout(Duration::from_secs(3), connection.closed())
            .await
            .unwrap();
    }
    attacker.shutdown().await;
    replica.shutdown().await;
}

#[tokio::test]
async fn offline_root_reciprocal_bootstrap_registration_and_live_revocation() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let files = tempfile::tempdir().unwrap();
        let root = node(1).await;
        let doc = root.docs().create().await.unwrap();
        let namespace: [u8; 32] = hex::decode(doc.id().unwrap().to_string())
            .unwrap()
            .try_into()
            .unwrap();
        write(
            &root,
            &doc,
            &registration(6, 1, NodeRegistrationStatus::Active),
        )
        .await;
        write(
            &root,
            &doc,
            &registration(7, 1, NodeRegistrationStatus::Revoked),
        )
        .await;
        let root_enroll = enrollment(
            &root,
            namespace,
            &files.path().join("root.json"),
            Some(token(1)),
        );
        root_enroll.set_policy_document(&doc).unwrap();
        let _root_connections = serve(root.clone(), root_enroll);

        let replica = node(2).await;
        let replica_enroll = enrollment(
            &replica,
            namespace,
            &files.path().join("replica.json"),
            Some(token(2)),
        );
        let mut replica_connections = serve(replica.clone(), replica_enroll.clone());
        sync(&replica_enroll, root.addr()).await;

        let second = node(3).await;
        let second_enroll = enrollment(
            &second,
            namespace,
            &files.path().join("second.json"),
            Some(token(3)),
        );
        let _second_connections = serve(second.clone(), second_enroll.clone());
        sync(&second_enroll, root.addr()).await;

        let transition = node(9).await;
        let transition_token = token(9);
        let transition_enroll = enrollment(
            &transition,
            namespace,
            &files.path().join("transition.json"),
            Some(transition_token.clone()),
        );
        sync(&transition_enroll, replica.addr()).await;
        assert!(matches!(
            replica_enroll.check_peer(&public(9)).await.unwrap().source,
            AdmissionEvidenceSource::PresentedJwt
        ));
        write(
            &root,
            &doc,
            &registration(9, 1, NodeRegistrationStatus::Active),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                replica_enroll.reconcile().await;
                if replica_enroll
                    .check_peer(&public(9))
                    .await
                    .is_ok_and(|ready| {
                        matches!(
                            ready.source,
                            AdmissionEvidenceSource::RootAuthoredRecord { .. }
                        )
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        transition.shutdown().await;

        let live = node(5).await;
        let live_enroll = enrollment(
            &live,
            namespace,
            &files.path().join("live.json"),
            Some(token(5)),
        );
        let mut revoked = replica_enroll.revocations();
        assert!(matches!(
            sync(&live_enroll, replica.addr()).await.source,
            AdmissionEvidenceSource::PresentedJwt
        ));
        live.add_peer(&replica).unwrap();
        let outbound = live.connect(&replica.id(), NORMAL).await.unwrap();
        let tracked = tokio::time::timeout(Duration::from_secs(5), replica_connections.recv())
            .await
            .unwrap()
            .unwrap();
        let source = replica_enroll
            .check_connection(&tracked)
            .await
            .unwrap()
            .observed_source
            .unwrap();
        assert!(source.address.is_loopback());
        assert_eq!(source.provenance, SourceAddressProvenance::DirectPeer);
        write(
            &root,
            &doc,
            &registration(5, 1, NodeRegistrationStatus::Revoked),
        )
        .await;
        let revoke_started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if revoked.recv().await.unwrap().peer == public(5) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(revoke_started.elapsed() < Duration::from_secs(15));
        tokio::time::timeout(Duration::from_secs(5), tracked.closed())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), outbound.closed())
            .await
            .unwrap();
        assert!(replica_enroll.check_peer(&public(5)).await.is_err());
        assert!(live_enroll.connect(replica.addr()).await.is_err());
        live.shutdown().await;

        // Withdrawing one JWT does not revoke a node or an independent root
        // registration. A different credential for the same key can replace it.
        let mut request = EnrollmentIssueRequest::new(public(10), FLEET);
        request.credential_id = Some("withdraw-10".into());
        let withdrawn = issue_jwt(&key(1), &request, now()).unwrap().into_jwt();
        let credential_node = node(10).await;
        let credential_enroll = enrollment(
            &credential_node,
            namespace,
            &files.path().join("credential.json"),
            Some(withdrawn.clone()),
        );
        sync(&credential_enroll, replica.addr()).await;
        let transition_id = JwtEnrollmentVerifier::new(public(1), FLEET)
            .unwrap()
            .verify(&transition_token, &public(9), now())
            .unwrap()
            .credential_id()
            .unwrap()
            .to_owned();
        for id in ["withdraw-10".to_owned(), transition_id] {
            let record = CredentialStatusV1 {
                version: 1,
                issuer: hex::encode(public(1)),
                credential_id: id.clone(),
                revision: 1,
                issued_at: now(),
                revoked: true,
                reauthorizes_revision: None,
            };
            doc.set_bytes(
                &root.docs().default_author().await.unwrap(),
                credential_status_key(&id),
                serde_json::to_vec(&record).unwrap(),
            )
            .await
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if revoked.recv().await.unwrap().peer == public(10) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(replica_enroll.check_peer(&public(10)).await.is_err());
        assert!(credential_enroll.connect(replica.addr()).await.is_err());
        assert!(matches!(
            replica_enroll.check_peer(&public(9)).await.unwrap().source,
            AdmissionEvidenceSource::RootAuthoredRecord { .. }
        ));
        credential_node.shutdown().await;
        let renewed = node(10).await;
        let renewed_enroll = enrollment(
            &renewed,
            namespace,
            &files.path().join("renewed.json"),
            Some(token(10)),
        );
        sync(&renewed_enroll, replica.addr()).await;
        renewed.shutdown().await;

        // A rejected unrelated root leaf is uncertainty, not a transport-sync
        // failure. Explicit stale-tolerant enrollment can still bootstrap;
        // freshness-sensitive operations must observe and reject uncertainty.
        doc.set_bytes(
            &root.docs().default_author().await.unwrap(),
            node_registration_key(&public(11)),
            b"{".to_vec(),
        )
        .await
        .unwrap();
        sync(&replica_enroll, root.addr()).await;

        // No root endpoint remains reachable from this point onward.
        root.shutdown().await;

        let registered = node(6).await;
        let registered_enroll = enrollment(
            &registered,
            namespace,
            &files.path().join("registered.json"),
            None,
        );
        let ready = sync(&registered_enroll, replica.addr()).await;
        assert_eq!(ready.peer, public(2));
        assert!(matches!(
            replica_enroll.check_peer(&public(6)).await.unwrap().source,
            AdmissionEvidenceSource::RootAuthoredRecord { .. }
        ));
        registered.shutdown().await;

        let reusable = token(4);
        for attempt in 0..2 {
            let fresh = node(4).await;
            let fresh_enroll = enrollment(
                &fresh,
                namespace,
                &files.path().join(format!("fresh-{attempt}.json")),
                Some(reusable.clone()),
            );
            fresh.add_peer(&replica).unwrap();
            // Gate-0 rejects ordinary traffic from the not-yet-enrolled identity.
            if attempt == 0 {
                let connection = fresh.connect(&replica.id(), NORMAL).await;
                if let Ok(connection) = connection {
                    assert!(
                        tokio::time::timeout(Duration::from_secs(3), connection.open_bi())
                            .await
                            .unwrap()
                            .is_err()
                    );
                }
            }
            assert_eq!(sync(&fresh_enroll, replica.addr()).await.peer, public(2));
            if attempt == 0 {
                let connection = fresh.connect(&replica.id(), NORMAL).await.unwrap();
                assert!(fresh_enroll
                    .check_peer(&public(2))
                    .await
                    .unwrap()
                    .policy_sync
                    .is_some());
                let strict =
                    PolicyExpressionV1::Predicate(PolicyPredicate::PolicySynchronizedWithin {
                        max_age: Duration::from_secs(60),
                        require_root: false,
                    });
                assert!(fresh_enroll
                    .check_connection_with_policy(&connection, &strict, Duration::from_secs(1))
                    .await
                    .is_err());
            }
            assert_eq!(sync(&fresh_enroll, second.addr()).await.peer, public(3));
            assert!(matches!(
                replica_enroll.check_peer(&public(4)).await.unwrap().source,
                AdmissionEvidenceSource::PresentedJwt
            ));
            fresh.shutdown().await;
        }

        let copied = node(8).await;
        // Local credential validation also rejects a credential copied to a
        // different persisted key, before it could be put on the network.
        let copied_store = Arc::new(
            EnrollmentPolicyStore::open(
                files.path().join("copied.json"),
                public(1),
                namespace,
                FLEET,
            )
            .unwrap(),
        );
        assert!(Enrollment::install(
            &copied,
            copied_store,
            JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
            Some(reusable),
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy
            )),
            Default::default()
        )
        .is_err());
        copied.shutdown().await;

        let replay = node(7).await;
        let replay_enroll = enrollment(
            &replay,
            namespace,
            &files.path().join("replay.json"),
            Some(token(7)),
        );
        assert!(replay_enroll.connect(replica.addr()).await.is_err());
        replay.shutdown().await;
        replica.shutdown().await;
        second.shutdown().await;
    })
    .await
    .expect("bootstrap integration timed out");
}

#[tokio::test]
async fn install_rejects_mismatched_activation_leeway() {
    let files = tempfile::tempdir().unwrap();
    let node = node(11).await;
    let store = |name: &str| {
        Arc::new(
            EnrollmentPolicyStore::open(files.path().join(name), public(1), [42; 32], FLEET)
                .unwrap(),
        )
    };
    let install = |store, verifier| {
        Enrollment::install(
            &node,
            store,
            verifier,
            Some(token(11)),
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy,
            )),
            EnrollmentOptions::default(),
        )
    };
    let exact = JwtEnrollmentVerifier::new(public(1), FLEET)
        .unwrap()
        .with_activation_leeway(0)
        .unwrap();
    let error = install(store("default.json"), exact).err().unwrap();
    assert!(
        error.to_string().contains("activation leeways differ"),
        "{error}"
    );
    assert!(
        EnrollmentPolicyStore::open(files.path().join("bad.json"), public(1), [42; 32], FLEET)
            .unwrap()
            .with_activation_leeway(MAX_ACTIVATION_LEEWAY_SECONDS + 1)
            .is_err()
    );
    let matched = Arc::new(
        EnrollmentPolicyStore::open(files.path().join("exact.json"), public(1), [42; 32], FLEET)
            .unwrap()
            .with_activation_leeway(0)
            .unwrap(),
    );
    let exact = JwtEnrollmentVerifier::new(public(1), FLEET)
        .unwrap()
        .with_activation_leeway(0)
        .unwrap();
    assert!(install(matched, exact).is_ok());
    node.shutdown().await;
}

/// Allows everyone until `failing` is set, then errors (policy unavailable,
/// never a definite denial).
struct Switchable {
    failing: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl AdmissionPolicy for Switchable {
    async fn evaluate(
        &self,
        _: &VerifiedAdmissionEvidence,
        _: &AdmissionContext,
        _: u64,
    ) -> Result<AdmissionDecision, PolicyError> {
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            Err(PolicyError::Handler("unavailable".into()))
        } else {
            Ok(AdmissionDecision::Allow)
        }
    }
}

/// Route enrollment only; ordinary connections are held open but never
/// registered with `track_connection`, so only Gate 0 can close them.
fn serve_untracked(node: Node, enrollment: Enrollment) -> mpsc::Receiver<aster::Connection> {
    let (send, recv) = mpsc::channel(16);
    tokio::spawn(async move {
        while let Ok((alpn, connection)) = node.accept().await {
            if alpn == ENROLLMENT_ALPN {
                let enrollment = enrollment.clone();
                tokio::spawn(async move {
                    let _ = enrollment.accept(&connection).await;
                    connection.closed().await;
                });
            } else {
                let _ = send.send(connection).await;
            }
        }
    });
    recv
}

#[tokio::test]
async fn definite_denial_closes_every_gate_admitted_connection() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let files = tempfile::tempdir().unwrap();
        let root = node(1).await;
        let doc = root.docs().create().await.unwrap();
        let namespace: [u8; 32] = hex::decode(doc.id().unwrap().to_string())
            .unwrap()
            .try_into()
            .unwrap();
        let root_enroll = enrollment(
            &root,
            namespace,
            &files.path().join("root.json"),
            Some(token(1)),
        );
        root_enroll.set_policy_document(&doc).unwrap();
        let _root_connections = serve(root.clone(), root_enroll);

        let interval = Duration::from_millis(50);
        let replica = node(2).await;
        let policy = Arc::new(Switchable {
            failing: false.into(),
        });
        let replica_enroll = Enrollment::install(
            &replica,
            Arc::new(
                EnrollmentPolicyStore::open(
                    files.path().join("replica.json"),
                    public(1),
                    namespace,
                    FLEET,
                )
                .unwrap(),
            ),
            JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
            Some(token(2)),
            policy.clone(),
            EnrollmentOptions {
                reconciliation_interval: interval,
                ..Default::default()
            },
        )
        .unwrap();
        let mut inbound = serve_untracked(replica.clone(), replica_enroll.clone());
        sync(&replica_enroll, root.addr()).await;

        let peer = node(5).await;
        let peer_enroll = enrollment(
            &peer,
            namespace,
            &files.path().join("peer.json"),
            Some(token(5)),
        );
        let mut peer_inbound = serve_untracked(peer.clone(), peer_enroll.clone());
        sync(&peer_enroll, replica.addr()).await;

        // Two connections from the peer and one the replica opened to it.
        peer.add_peer(&replica).unwrap();
        replica.add_peer(&peer).unwrap();
        let mut held = Vec::new();
        for _ in 0..2 {
            held.push(peer.connect(&replica.id(), NORMAL).await.unwrap());
            held.push(inbound.recv().await.unwrap());
        }
        held.push(replica.connect(&peer.id(), NORMAL).await.unwrap());
        held.push(peer_inbound.recv().await.unwrap());

        // Policy unavailable is not a denial: nothing closes.
        policy
            .failing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        replica_enroll.reconcile().await;
        assert!(matches!(
            replica_enroll.check_peer(&public(5)).await,
            Err(AdmissionError::Unavailable(_))
        ));
        tokio::time::sleep(interval * 4).await;
        for connection in &held {
            assert!(
                tokio::time::timeout(Duration::from_millis(1), connection.closed())
                    .await
                    .is_err(),
                "an unavailable policy closed a connection"
            );
        }
        policy
            .failing
            .store(false, std::sync::atomic::Ordering::SeqCst);

        let mut revoked = replica_enroll.revocations();
        write(
            &root,
            &doc,
            &registration(5, 1, NodeRegistrationStatus::Revoked),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(15), async {
            while revoked.recv().await.unwrap().peer != public(5) {}
        })
        .await
        .unwrap();
        let denied = std::time::Instant::now();
        for connection in &held {
            tokio::time::timeout(interval + Duration::from_secs(1), connection.closed())
                .await
                .expect("Gate 0 left an admitted connection open after denial");
        }
        assert!(denied.elapsed() <= interval + Duration::from_secs(1));
        assert!(
            peer.connect(&replica.id(), NORMAL).await.is_err()
                || tokio::time::timeout(Duration::from_secs(3), async {
                    peer.connect(&replica.id(), NORMAL)
                        .await
                        .unwrap()
                        .closed()
                        .await
                })
                .await
                .is_ok()
        );
        peer.shutdown().await;
        replica.shutdown().await;
        root.shutdown().await;
    })
    .await
    .expect("close-on-deny test timed out");
}
