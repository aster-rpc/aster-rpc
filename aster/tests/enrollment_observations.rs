#![cfg(all(feature = "enrollment", feature = "docs"))]
//! Observation and promotion over the network (enrollment B10): a replica
//! holding the observer scope opens its sealed grant, observes a node
//! admitted only by its persistent chain, and the root promotes that node in
//! one session. After the chain expires the node is still admitted.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::grants::{grant_key, seal_namespace_grant, GrantContext, GrantRecord};
use aster::{AsterConfig, Doc, Node, RelayMode, SecretKey};

const FLEET: &str = "observation-test";

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
fn credential(seed: u8, scopes: &[&str], registration: Registration, lifetime: u64) -> String {
    let mut request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + lifetime);
    request.registration = registration;
    request.claims.scopes = Some(scopes.iter().map(|scope| scope.to_string()).collect());
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
            reconciliation_interval: Duration::from_millis(50),
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
async fn put(root: &Node, doc: &Doc, key: Vec<u8>, value: Vec<u8>) {
    doc.set_bytes(&root.docs().default_author().await.unwrap(), key, value)
        .await
        .unwrap();
}

#[tokio::test]
async fn observed_persistent_node_is_promoted_and_outlives_its_chain() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let files = tempfile::tempdir().unwrap();
        let root = node(1).await;
        let docs = root.docs();
        let fleet = docs.create().await.unwrap();
        let secret = aster::NamespaceSecret::from_bytes([77; 32]);
        let observations = docs
            .open_or_import_write_namespace(secret.clone())
            .await
            .unwrap();
        // The root binds the observations namespace and seals its write
        // capability to the replica (seed 2), which holds the observer scope.
        put(
            &root,
            &fleet,
            namespace_binding_key(&namespace(&observations)),
            serde_json::to_vec(&NamespaceBindingV1 {
                version: 1,
                issuer: hex::encode(public(1)),
                namespace: hex::encode(namespace(&observations)),
                purpose: NamespacePurpose::Observations,
                read_scopes: BTreeSet::new(),
                write_scopes: BTreeSet::from([OBSERVER_SCOPE.to_owned()]),
                revision: 1,
                status: BindingStatus::Active,
            })
            .unwrap(),
        )
        .await;
        let granter = aster::NodeId::from_hex(hex::encode(public(1)));
        let recipient = aster::NodeId::from_hex(hex::encode(public(2)));
        let path = grant_key(OBSERVER_GRANT_PREFIX, &recipient);
        let resource = secret.id().to_bytes();
        let envelope = seal_namespace_grant(
            &GrantContext {
                app: OBSERVER_GRANT_APP,
                granter: &granter,
                recipient: &recipient,
                resource: &resource,
                path: &path,
                role: "write",
            },
            &aster::NamespaceCapability::write(secret.clone()),
        )
        .unwrap();
        put(
            &root,
            &fleet,
            path.into_bytes(),
            GrantRecord::active("write", 1, envelope).encode_fory(),
        )
        .await;
        let root_enroll = enrollment(
            &root,
            namespace(&fleet),
            &files.path().join("root.json"),
            credential(1, &[], Registration::Ephemeral, 3600),
        );
        root_enroll.set_policy_document(&fleet).unwrap();
        serve(root.clone(), root_enroll.clone());

        let replica = node(2).await;
        let replica_enroll = enrollment(
            &replica,
            namespace(&fleet),
            &files.path().join("replica.json"),
            credential(2, &[OBSERVER_SCOPE], Registration::Ephemeral, 3600),
        );
        serve(replica.clone(), replica_enroll.clone());
        let (ready, ticket) = replica_enroll.connect(root.addr()).await.unwrap();
        replica_enroll
            .synchronize(ready.peer, ticket.unwrap(), Duration::from_secs(20))
            .await
            .unwrap();

        // A persistent node admitted only by its chain, which expires soon.
        let lifetime = 20;
        let joiner = node(7).await;
        let joiner_chain = credential(7, &["build"], Registration::Persistent, lifetime);
        let expires_at = now() + lifetime;
        let joiner_enroll = enrollment(
            &joiner,
            namespace(&fleet),
            &files.path().join("joiner.json"),
            joiner_chain,
        );
        // The replica becomes an observer at its next reconciliation; once it
        // is, admitting the joiner writes an observation the root can sync.
        let stores = PolicyStores::new(Arc::new(
            EnrollmentPolicyStore::open(
                files.path().join("root-review.json"),
                public(1),
                namespace(&fleet),
                FLEET,
            )
            .unwrap(),
        ));
        let verifier = JwtEnrollmentVerifier::new(public(1), FLEET).unwrap();
        let reviewed = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let _ = joiner_enroll.connect(replica.addr()).await; // swallow-ok: retried until the review lands; the outcome is asserted after the loop
                                                                     // short-wait-ok: one sync round per try; the loop retries within its 30 s bound
                let _ = observations
                    .sync_with_timeout(&[replica.id()], Duration::from_secs(2))
                    .await;
                refresh_policy(&stores, &fleet, &[]).await.unwrap();
                let review = review_observations(&observations, &fleet, &verifier, &stores, now())
                    .await
                    .unwrap();
                if let Some(reviewed) = review.accepted.into_iter().next() {
                    break reviewed;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(reviewed.observation.observer, public(2));
        assert_eq!(reviewed.observation.subject, public(7));
        assert_eq!(reviewed.observation.kind, ObservationKind::Admitted);
        assert!(matches!(
            replica_enroll.check_peer(&public(7)).await.unwrap().source,
            AdmissionEvidenceSource::PresentedChain { .. }
        ));

        // One root session: publish the promotion.
        let record = promotion_record(&public(1), &reviewed, FLEET, 1, None, now());
        put(
            &root,
            &fleet,
            node_registration_key(&public(7)),
            serde_json::to_vec(&record).unwrap(),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                // Reconciliation replaces the chain session with the record.
                replica_enroll.reconcile().await;
                if replica_enroll
                    .check_peer(&public(7))
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
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();

        // After the chain expires, the node still enrolls, on the record.
        while now() <= expires_at {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let (_, _) = joiner_enroll.connect(replica.addr()).await.unwrap();
        assert!(matches!(
            replica_enroll.check_peer(&public(7)).await.unwrap().source,
            AdmissionEvidenceSource::RootAuthoredRecord { .. }
        ));
        for node in [joiner, replica, root] {
            node.shutdown().await;
        }
    })
    .await
    .unwrap();
}
