#![cfg(all(feature = "enrollment", feature = "docs"))]
//! Promotion race over the network: a replica admits a node on its chain,
//! the root publishes a registration for it, and the replica learns it by
//! live policy sync. A check that runs before any reconciliation must move
//! the session onto the registration, never revoke the node as superseded.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::{AsterConfig, Doc, Node, RelayMode, SecretKey};

const FLEET: &str = "promotion-test";

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
fn credential(seed: u8, registration: Registration) -> String {
    let mut request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
    request.registration = registration;
    issue_credential(&key(1), &request, now())
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
        vec![ENROLLMENT_ALPN.to_vec()],
    )
    .await
    .unwrap()
}
fn enrollment(node: &Node, fleet: [u8; 32], path: &std::path::Path, chain: String) -> Enrollment {
    Enrollment::install(
        node,
        Arc::new(EnrollmentPolicyStore::open(path, public(1), fleet, FLEET).unwrap()),
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        Some(chain),
        Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )),
        EnrollmentOptions {
            // No periodic reconciliation during the test: only live policy
            // sync updates the replica's store.
            reconciliation_interval: Duration::from_secs(3600),
            ..Default::default()
        },
    )
    .unwrap()
}
fn serve(node: Node, enrollment: Enrollment) {
    tokio::spawn(async move {
        while let Ok((alpn, connection)) = node.accept().await {
            if alpn == ENROLLMENT_ALPN {
                let enrollment = enrollment.clone();
                tokio::spawn(async move {
                    let _ = enrollment.accept(&connection).await;
                    connection.closed().await;
                });
            }
        }
    });
}
fn namespace(doc: &Doc) -> [u8; 32] {
    hex::decode(doc.id().unwrap().to_string())
        .unwrap()
        .try_into()
        .unwrap()
}

#[tokio::test]
async fn check_before_reconciliation_moves_a_promoted_node_onto_its_record() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let files = tempfile::tempdir().unwrap();
        let root = node(1).await;
        let fleet = root.docs().create().await.unwrap();
        let root_enroll = enrollment(
            &root,
            namespace(&fleet),
            &files.path().join("root.json"),
            credential(1, Registration::Ephemeral),
        );
        root_enroll.set_policy_document(&fleet).unwrap();
        serve(root.clone(), root_enroll.clone());

        let replica = node(2).await;
        let replica_enroll = enrollment(
            &replica,
            namespace(&fleet),
            &files.path().join("replica.json"),
            credential(2, Registration::Ephemeral),
        );
        serve(replica.clone(), replica_enroll.clone());
        let (ready, ticket) = replica_enroll.connect(root.addr()).await.unwrap();
        replica_enroll
            .synchronize(ready.peer, ticket.unwrap(), Duration::from_secs(20))
            .await
            .unwrap();

        let joiner = node(7).await;
        let joiner_enroll = enrollment(
            &joiner,
            namespace(&fleet),
            &files.path().join("joiner.json"),
            credential(7, Registration::Persistent),
        );
        joiner_enroll.connect(replica.addr()).await.unwrap();
        assert!(matches!(
            replica_enroll.check_peer(&public(7)).await.unwrap().source,
            AdmissionEvidenceSource::PresentedChain { .. }
        ));

        // The root promotes the node.
        let record = NodeRegistrationV1 {
            version: 1,
            issuer: hex::encode(public(1)),
            subject: hex::encode(public(7)),
            audience: Audience::One(FLEET.into()),
            revision: 1,
            issued_at: now() - 60,
            not_before: None,
            expires_at: None,
            status: NodeRegistrationStatus::Active,
            claims: AdmissionClaims::default(),
            mandatory: vec![],
            reauthorizes_revision: None,
            reason: None,
        };
        fleet
            .set_bytes(
                &root.docs().default_author().await.unwrap(),
                node_registration_key(&public(7)),
                serde_json::to_vec(&record).unwrap(),
            )
            .await
            .unwrap();

        // Every check before reconciliation must keep the node admitted, and
        // one of them moves it onto the record once a completed sync round
        // with the root has refreshed the replica's store.
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                // short-wait-ok: one sync round per try; the loop retries within its 30 s bound
                let _ = fleet
                    .sync_with_timeout(&[replica.id()], Duration::from_secs(2))
                    .await;
                let ready = replica_enroll
                    .check_peer(&public(7))
                    .await
                    .expect("a promoted node is never revoked as superseded");
                if matches!(
                    ready.source,
                    AdmissionEvidenceSource::RootAuthoredRecord { revision: 1, .. }
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        for node in [joiner, replica, root] {
            node.shutdown().await;
        }
    })
    .await
    .unwrap();
}
