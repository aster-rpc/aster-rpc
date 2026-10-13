#![cfg(all(feature = "enrollment", feature = "docs"))]
//! Issuer revocation over the network (enrollment B8): INV-E6 and INV-E7.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::delegation::{issue_delegation, Chain, DelegationGrant, LinkKind, Validity};
use aster::enrollment::*;
use aster::{AsterConfig, Doc, Node, RelayMode, SecretKey};

const NORMAL: &[u8] = b"enrollment.issuers.test/1";
const FLEET: &str = "issuer-revocation-test";
const ISSUER: u8 = 6;

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
    issue_credential(
        &key(1),
        &EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600),
        now(),
    )
    .unwrap()
    .into_chain()
}
fn delegation() -> Chain {
    let grant = DelegationGrant {
        may_issue: [LinkKind::Enrollment].into(),
        audiences: [FLEET.to_owned()].into(),
        scopes: Default::default(),
        tags: Default::default(),
        attribute_keys: Default::default(),
        max_child_lifetime: 86_400,
        max_depth: 0,
        subjects: None,
        operations: Default::default(),
        mandatory: Default::default(),
        may_register: false,
    };
    issue_delegation(
        &key(1),
        None,
        &aster::PublicKey::from_bytes(public(ISSUER)),
        &Audience::One(FLEET.into()),
        &grant,
        Validity {
            issued_at: now() - 10,
            not_before: None,
            expires_at: now() + 7 * 86_400,
        },
    )
    .unwrap()
}
fn delegated_token(seed: u8) -> String {
    issue_delegated_credential(
        &key(ISSUER),
        &delegation(),
        &EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600),
        now(),
    )
    .unwrap()
    .into_chain()
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
fn serve(node: Node, enrollment: Enrollment) -> tokio::sync::mpsc::Receiver<aster::Connection> {
    let (send, recv) = tokio::sync::mpsc::channel(16);
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
async fn put(root: &Node, doc: &Doc, key: Vec<u8>, value: Vec<u8>) {
    doc.set_bytes(&root.docs().default_author().await.unwrap(), key, value)
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

/// INV-E6: once a replica has synced the root's revocation of an issuer, it
/// revokes every session resting only on that issuer's chains within one
/// reconciliation interval and closes their connections (B2). INV-E7: a
/// node the root already registered stays admitted.
#[tokio::test]
async fn revoked_issuer_closes_chain_sessions_and_keeps_registered_nodes() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let files = tempfile::tempdir().unwrap();
        let root = node(1).await;
        let doc = root.docs().create().await.unwrap();
        let namespace: [u8; 32] = hex::decode(doc.id().unwrap().to_string())
            .unwrap()
            .try_into()
            .unwrap();
        // Node 8 also holds a root registration; node 7 has only its chain.
        put(
            &root,
            &doc,
            node_registration_key(&public(8)),
            serde_json::to_vec(&NodeRegistrationV1 {
                version: 1,
                issuer: hex::encode(public(1)),
                subject: hex::encode(public(8)),
                audience: Audience::One(FLEET.into()),
                revision: 1,
                issued_at: now(),
                not_before: None,
                expires_at: None,
                status: NodeRegistrationStatus::Active,
                claims: AdmissionClaims::default(),
                mandatory: vec![],
                reauthorizes_revision: None,
            })
            .unwrap(),
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

        let chained = node(7).await;
        let chained_enroll = enrollment(
            &chained,
            namespace,
            &files.path().join("chained.json"),
            Some(delegated_token(7)),
        );
        chained_enroll.connect(replica.addr()).await.unwrap();
        assert!(matches!(
            replica_enroll.check_peer(&public(7)).await.unwrap().source,
            AdmissionEvidenceSource::PresentedChain { issuers, .. }
                if issuers == vec![public(ISSUER), public(1)]
        ));
        let registered = node(8).await;
        let registered_enroll = enrollment(
            &registered,
            namespace,
            &files.path().join("registered.json"),
            Some(delegated_token(8)),
        );
        registered_enroll.connect(replica.addr()).await.unwrap();
        assert!(matches!(
            replica_enroll.check_peer(&public(8)).await.unwrap().source,
            AdmissionEvidenceSource::RootAuthoredRecord { .. }
        ));
        // An ordinary connection Gate 0 admitted for the chain-only node.
        chained.add_peer(&replica).unwrap();
        let outbound = chained.connect(&replica.id(), NORMAL).await.unwrap();
        let inbound = tokio::time::timeout(Duration::from_secs(5), replica_connections.recv())
            .await
            .unwrap()
            .unwrap();

        let mut revocations = replica_enroll.revocations();
        put(
            &root,
            &doc,
            issuer_status_key(&public(ISSUER)),
            serde_json::to_vec(&IssuerStatusV1 {
                version: 1,
                issuer: hex::encode(public(1)),
                subject: hex::encode(public(ISSUER)),
                audience: Audience::One(FLEET.into()),
                revision: 1,
                issued_at: now(),
                status: IssuerStatus::Revoked,
                reauthorizes_revision: None,
            })
            .unwrap(),
        )
        .await;
        let revoked = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let event = revocations.recv().await.unwrap();
                if event.peer == public(7) {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(revoked.reason, DenialReason::IssuerRevoked);
        tokio::time::timeout(Duration::from_secs(5), inbound.closed())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), outbound.closed())
            .await
            .unwrap();
        assert_eq!(
            replica_enroll.check_peer(&public(7)).await.unwrap_err(),
            AdmissionError::Denied(DenialReason::IssuerRevoked)
        );
        // The revoked issuer's chain no longer enrolls anyone.
        assert!(chained_enroll.connect(replica.addr()).await.is_err());
        // INV-E7: the registered node is still admitted, on its record.
        replica_enroll.reconcile().await;
        assert!(matches!(
            replica_enroll.check_peer(&public(8)).await.unwrap().source,
            AdmissionEvidenceSource::RootAuthoredRecord { .. }
        ));
        chained.shutdown().await;
        registered.shutdown().await;
        replica.shutdown().await;
        root.shutdown().await;
    })
    .await
    .unwrap();
}
