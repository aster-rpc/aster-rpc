#![cfg(all(feature = "enrollment", feature = "docs"))]
//! Multiple policy namespaces over the network (enrollment B9): INV-E8 and
//! INV-E10, and the §10 acceptance criteria.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::{AsterConfig, Doc, Node, RelayMode, SecretKey};

const FLEET: &str = "namespace-binding-test";

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
fn token(seed: u8, scopes: &[&str]) -> String {
    let mut request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
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
fn enrollment(
    node: &Node,
    namespace: [u8; 32],
    path: &std::path::Path,
    credential: String,
) -> Enrollment {
    Enrollment::install(
        node,
        Arc::new(EnrollmentPolicyStore::open(path, public(1), namespace, FLEET).unwrap()),
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        Some(credential),
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
fn binding(doc: &Doc, purpose: NamespacePurpose, scope: &str) -> Vec<u8> {
    serde_json::to_vec(&NamespaceBindingV1 {
        version: 1,
        issuer: hex::encode(public(1)),
        namespace: hex::encode(namespace(doc)),
        purpose,
        read_scopes: BTreeSet::from([scope.to_owned()]),
        write_scopes: BTreeSet::new(),
        revision: 1,
        status: BindingStatus::Active,
    })
    .unwrap()
}
fn registration(seed: u8, revision: u64, status: NodeRegistrationStatus) -> Vec<u8> {
    serde_json::to_vec(&NodeRegistrationV1 {
        version: 1,
        issuer: hex::encode(public(1)),
        subject: hex::encode(public(seed)),
        audience: Audience::One(FLEET.into()),
        revision,
        issued_at: now(),
        not_before: None,
        expires_at: None,
        status,
        claims: AdmissionClaims {
            scopes: Some(BTreeSet::from(["build".to_owned()])),
            ..Default::default()
        },
        mandatory: vec![],
        reauthorizes_revision: None,
        reason: None,
    })
    .unwrap()
}
fn ids(documents: Vec<(NamespaceBindingV1, Doc)>) -> BTreeSet<[u8; 32]> {
    documents
        .into_iter()
        .map(|(_, doc)| namespace(&doc))
        .collect()
}
async fn join(enrollment: &Enrollment, root: &Node) {
    let (ready, ticket) = enrollment.connect(root.addr()).await.unwrap();
    enrollment
        .synchronize(ready.peer, ticket.unwrap(), Duration::from_secs(20))
        .await
        .unwrap();
}

/// INV-E8: a joining node receives read tickets only for the fleet and the
/// bound namespaces whose `read_scopes` intersect its verified scopes.
/// INV-E10: a denial mirrored into the fleet namespace is enforced by a node
/// that never received the subject's membership namespace.
#[tokio::test]
async fn scoped_tickets_and_fleet_denials_reach_every_role() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let files = tempfile::tempdir().unwrap();
        let root = node(1).await;
        let docs = root.docs();
        let fleet = docs.create().await.unwrap();
        let build = docs.create().await.unwrap();
        let deploy = docs.create().await.unwrap();
        let grants = docs.create().await.unwrap();
        for (doc, purpose, scope) in [
            (&build, NamespacePurpose::Membership, "build"),
            (&deploy, NamespacePurpose::Membership, "deploy"),
            (&grants, NamespacePurpose::ExecGrants, "build"),
        ] {
            put(
                &root,
                &fleet,
                namespace_binding_key(&namespace(doc)),
                binding(doc, purpose, scope),
            )
            .await;
        }
        // A build worker with a membership registration only in `build`.
        put(
            &root,
            &build,
            node_registration_key(&public(11)),
            registration(11, 1, NodeRegistrationStatus::Active),
        )
        .await;
        let root_enroll = enrollment(
            &root,
            namespace(&fleet),
            &files.path().join("root.json"),
            token(1, &[]),
        );
        root_enroll.set_policy_document(&fleet).unwrap();
        root_enroll.reconcile().await;
        for doc in [&build, &deploy, &grants] {
            root_enroll.add_policy_document(doc).unwrap();
        }
        serve(root.clone(), root_enroll.clone());

        // INV-E8: each role receives exactly its own namespaces.
        let builder = node(7).await;
        let builder_enroll = enrollment(
            &builder,
            namespace(&fleet),
            &files.path().join("builder.json"),
            token(7, &["build"]),
        );
        join(&builder_enroll, &root).await;
        assert_eq!(
            ids(builder_enroll.policy_documents(NamespacePurpose::Membership)),
            BTreeSet::from([namespace(&build)])
        );
        assert_eq!(
            ids(builder_enroll.policy_documents(NamespacePurpose::ExecGrants)),
            BTreeSet::from([namespace(&grants)])
        );
        let deployer = node(9).await;
        let deployer_enroll = enrollment(
            &deployer,
            namespace(&fleet),
            &files.path().join("deployer.json"),
            token(9, &["deploy"]),
        );
        serve(deployer.clone(), deployer_enroll.clone());
        join(&deployer_enroll, &root).await;
        assert_eq!(
            ids(deployer_enroll.policy_documents(NamespacePurpose::Membership)),
            BTreeSet::from([namespace(&deploy)])
        );
        assert!(deployer_enroll
            .policy_documents(NamespacePurpose::ExecGrants)
            .is_empty());

        // The build worker is admitted by the build-namespace record at the
        // builder, and on its chain at the deployer, which lacks `build`.
        let worker = node(11).await;
        let worker_enroll = enrollment(
            &worker,
            namespace(&fleet),
            &files.path().join("worker.json"),
            token(11, &["build"]),
        );
        serve(worker.clone(), worker_enroll.clone());
        worker_enroll.connect(deployer.addr()).await.unwrap();
        assert!(matches!(
            deployer_enroll
                .check_peer(&public(11))
                .await
                .unwrap()
                .source,
            AdmissionEvidenceSource::PresentedChain { .. }
        ));

        // INV-E10: the root revokes the worker in `build` and mirrors the
        // denial into the fleet. The deployer enforces it from the fleet.
        let mut revocations = deployer_enroll.revocations();
        put(
            &root,
            &build,
            node_registration_key(&public(11)),
            registration(11, 2, NodeRegistrationStatus::Revoked),
        )
        .await;
        put(
            &root,
            &fleet,
            node_registration_key(&public(11)),
            registration(11, 2, NodeRegistrationStatus::Revoked),
        )
        .await;
        let revoked = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let event = revocations.recv().await.unwrap();
                if event.peer == public(11) {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(revoked.reason, DenialReason::NodeRevoked);
        assert!(deployer_enroll
            .policy_documents(NamespacePurpose::Membership)
            .iter()
            .all(|(_, doc)| namespace(doc) != namespace(&build)));
        assert!(worker_enroll.connect(deployer.addr()).await.is_err());
        for node in [worker, deployer, builder, root] {
            node.shutdown().await;
        }
    })
    .await
    .unwrap();
}
