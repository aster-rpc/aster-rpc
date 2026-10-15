#![cfg(all(feature = "enrollment", feature = "docs", feature = "rpc"))]
//! The issuer service over the network (enrollment B11). An issuer holding
//! a delegation from the root enrolls and renews nodes while the root is
//! offline, never for a subject the root denied or beyond its delegation,
//! and renewal stops once the root registers the node.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::delegation::{issue_delegation, Chain, DelegationGrant, LinkKind, Validity};
use aster::enrollment::*;
use aster::rpc::Server;
use aster::{AsterConfig, Doc, Node, RelayMode, SecretKey, RPC_ALPN};

const FLEET: &str = "issuer-test";

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
fn root_credential(seed: u8) -> String {
    let request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
    issue_credential(&key(1), &request, now())
        .unwrap()
        .into_chain()
}
/// The root (seed 1) delegates to the issuer (seed 2): scope `build` only,
/// and the locality keys.
fn delegation() -> Chain {
    issue_delegation(
        &key(1),
        None,
        &key(2).public_key(),
        &Audience::One(FLEET.into()),
        &DelegationGrant {
            may_issue: [LinkKind::Enrollment].into(),
            audiences: [FLEET.to_string()].into(),
            scopes: ["build".to_string()].into(),
            tags: BTreeSet::new(),
            attribute_keys: [
                "aster.locality.region".to_string(),
                "aster.locality.zone".to_string(),
            ]
            .into(),
            max_child_lifetime: 3600,
            max_depth: 0,
            subjects: None,
            operations: BTreeSet::new(),
            mandatory: BTreeSet::new(),
            may_register: true,
        },
        Validity {
            issued_at: now(),
            not_before: None,
            expires_at: now() + 3600,
        },
    )
    .unwrap()
}
fn build() -> AdmissionClaims {
    let zone = |value: &str| TypedAttribute::String(value.into());
    AdmissionClaims {
        scopes: Some(["build".to_string()].into()),
        tags: Some(BTreeSet::new()),
        attributes: Some(
            [
                ("aster.locality.region".to_string(), zone("eu-west-1")),
                ("aster.locality.zone".to_string(), zone("eu-west-1a")),
            ]
            .into(),
        ),
    }
}
async fn node(seed: u8, alpns: Vec<Vec<u8>>) -> Node {
    Node::start_with_alpns(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(key(seed))
            .hooks(true)
            .hook_failure_mode(aster::HookFailureMode::FailClosed)
            .hook_timeout_ms(1000)
            .build(),
        alpns,
    )
    .await
    .unwrap()
}
fn enrollment(
    node: &Node,
    fleet: [u8; 32],
    path: &std::path::Path,
    chain: Option<String>,
) -> Enrollment {
    Enrollment::install(
        node,
        Arc::new(EnrollmentPolicyStore::open(path, public(1), fleet, FLEET).unwrap()),
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        chain,
        Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )),
        EnrollmentOptions {
            reconciliation_interval: Duration::from_millis(200),
            ..Default::default()
        },
    )
    .unwrap()
}
fn serve_enrollment(node: Node, enrollment: Enrollment) {
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
fn registration(subject: [u8; 32], status: NodeRegistrationStatus) -> Vec<u8> {
    serde_json::to_vec(&NodeRegistrationV1 {
        version: 1,
        issuer: hex::encode(public(1)),
        subject: hex::encode(subject),
        audience: Audience::One(FLEET.into()),
        revision: 1,
        issued_at: now() - 60,
        not_before: None,
        expires_at: None,
        status,
        claims: build(),
        mandatory: vec![],
        reauthorizes_revision: None,
        reason: None,
    })
    .unwrap()
}

struct Fleet {
    _files: tempfile::TempDir,
    files: std::path::PathBuf,
    root: Node,
    fleet: Doc,
    issuer_node: Node,
    issuer_enrollment: Enrollment,
    issuer: Issuer,
    _server: aster::rpc::ServerHandle,
}

/// The root publishes `records` into the fleet namespace, the issuer
/// enrolls with it and syncs them, and serves enrollment and renewal.
async fn fleet(records: &[([u8; 32], NodeRegistrationStatus)]) -> Fleet {
    let files = tempfile::tempdir().unwrap();
    let path = files.path().to_path_buf();
    let root = node(1, vec![ENROLLMENT_ALPN.to_vec()]).await;
    let fleet = root.docs().create().await.unwrap();
    let author = root.docs().default_author().await.unwrap();
    for (subject, status) in records {
        fleet
            .set_bytes(
                &author,
                node_registration_key(subject),
                registration(*subject, *status),
            )
            .await
            .unwrap();
    }
    let root_enroll = enrollment(
        &root,
        namespace(&fleet),
        &path.join("root.json"),
        Some(root_credential(1)),
    );
    root_enroll.set_policy_document(&fleet).unwrap();
    serve_enrollment(root.clone(), root_enroll);

    let issuer_node = node(2, vec![RPC_ALPN.to_vec(), ENROLLMENT_ALPN.to_vec()]).await;
    let issuer_enrollment = enrollment(
        &issuer_node,
        namespace(&fleet),
        &path.join("issuer.json"),
        Some(root_credential(2)),
    );
    let (ready, ticket) = issuer_enrollment.connect(root.addr()).await.unwrap();
    issuer_enrollment
        .synchronize(ready.peer, ticket.unwrap(), Duration::from_secs(20))
        .await
        .unwrap();
    let issuer = Issuer::new(
        &issuer_node,
        issuer_enrollment.clone(),
        delegation(),
        Duration::from_secs(6),
    )
    .unwrap();
    let routed = issuer_enrollment.clone();
    let server = Server::new(&issuer_node)
        .register(issuer.clone())
        .route_alpn(ENROLLMENT_ALPN, move |connection| {
            let enrollment = routed.clone();
            tokio::spawn(async move {
                if enrollment.accept(&connection).await.is_ok() {
                    connection.closed().await;
                }
            });
        })
        .serve();
    Fleet {
        _files: files,
        files: path,
        root,
        fleet,
        issuer_node,
        issuer_enrollment,
        issuer,
        _server: server,
    }
}

/// A node enrolled by the issuer, connected to it.
async fn enrolled(fleet: &Fleet, seed: u8, registration: Registration) -> (Node, Enrollment) {
    let credential = fleet
        .issuer
        .issue_for(&public(seed), build(), registration)
        .unwrap();
    let node = node(seed, vec![ENROLLMENT_ALPN.to_vec()]).await;
    let enrollment = enrollment(
        &node,
        namespace(&fleet.fleet),
        &fleet.files.join(format!("node-{seed}.json")),
        Some(credential.into_chain()),
    );
    serve_enrollment(node.clone(), enrollment.clone());
    enrollment.connect(fleet.issuer_node.addr()).await.unwrap();
    (node, enrollment)
}

fn chain_digest(ready: &EnrollmentReadiness) -> Option<[u8; 32]> {
    match ready.source {
        AdmissionEvidenceSource::PresentedChain { digest, .. } => Some(digest),
        AdmissionEvidenceSource::RootAuthoredRecord { .. } => None,
    }
}

#[tokio::test]
async fn renewal_keeps_a_node_admitted_across_lifetimes_with_the_root_offline() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let fleet = fleet(&[]).await;
        // The root goes offline; the issuer holds its delegation.
        fleet.root.clone().shutdown().await;
        let (node, enrollment) = enrolled(&fleet, 7, Registration::Ephemeral).await;
        let renewal = enrollment.start_renewal(RenewalConfig::new(fleet.issuer_node.addr()));

        // Three 6-second lifetimes: the node stays admitted on renewed chains,
        // and its locality statement is renewed with each credential.
        let deadline = now() + 3 * 6 + 2;
        let mut digests = BTreeSet::new();
        let mut statements = BTreeSet::new();
        let own = enrollment.topology_locality();
        let reader = fleet.issuer_enrollment.topology_locality();
        while now() < deadline {
            let ready = fleet
                .issuer_enrollment
                .check_peer(&public(7))
                .await
                .expect("the renewed node stays admitted");
            digests.extend(chain_digest(&ready));
            let statement = (own.own_statement)().expect("statement renewed with the credential");
            let verified = (reader.verify)(&hex::encode(public(7)), &statement)
                .expect("the current statement verifies");
            assert_eq!(verified.declared.zone.as_deref(), Some("eu-west-1a"));
            assert!(verified.expires_at.unwrap() <= now() + 6);
            statements.insert(statement);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(digests.len() >= 4, "renewed {} times", digests.len() - 1);
        assert!(statements.len() >= 4, "{} statements", statements.len());
        drop(renewal);
        for node in [node, fleet.issuer_node] {
            node.shutdown().await;
        }
    })
    .await
    .unwrap();
}

/// INV-E19: an issuer never enrolls or renews a subject the root denied, or
/// beyond its own delegation.
#[tokio::test]
async fn issuer_refuses_denied_subjects_and_claims_beyond_its_delegation() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let fleet = fleet(&[(public(9), NodeRegistrationStatus::Revoked)]).await;
        assert!(matches!(
            fleet
                .issuer
                .issue_for(&public(9), build(), Registration::Ephemeral),
            Err(IssuanceError::SubjectDenied(DenialReason::NodeRevoked))
        ));

        let wider = AdmissionClaims {
            scopes: Some(["build".to_string(), "deploy".to_string()].into()),
            ..build()
        };
        assert!(matches!(
            fleet
                .issuer
                .issue_for(&public(7), wider, Registration::Ephemeral),
            Err(IssuanceError::Credential(EnrollmentError::Chain(
                aster::delegation::ChainError::Attenuation
            )))
        ));

        // The denied subject holds a chain the issuer signed outside the
        // service; it can neither enroll with the issuer nor renew.
        let mut request = EnrollmentIssueRequest::new(public(9), FLEET, now() + 4);
        request.claims = build();
        let chain = issue_delegated_credential(&key(2), &delegation(), &request, now())
            .unwrap()
            .into_chain();
        let denied = node(9, vec![ENROLLMENT_ALPN.to_vec()]).await;
        let denied_enrollment = enrollment(
            &denied,
            namespace(&fleet.fleet),
            &fleet.files.join("denied.json"),
            Some(chain.clone()),
        );
        assert!(denied_enrollment
            .connect(fleet.issuer_node.addr())
            .await
            .is_err());
        assert!(renew_from(
            &denied,
            &fleet.issuer_node.id(),
            &chain,
            Duration::from_secs(5)
        )
        .await
        .is_err());
        let outcome = denied_enrollment
            .start_renewal(RenewalConfig::new(fleet.issuer_node.addr()))
            .finished()
            .await;
        assert_eq!(outcome, RenewalOutcome::Expired);
        for node in [denied, fleet.issuer_node, fleet.root] {
            node.shutdown().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn renewal_stops_once_the_root_registers_the_node() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let fleet = fleet(&[]).await;
        let (node, enrollment) = enrolled(&fleet, 7, Registration::Persistent).await;
        let renewal = enrollment.start_renewal(RenewalConfig::new(fleet.issuer_node.addr()));

        // The root promotes the node and delivers the record to the issuer.
        fleet
            .fleet
            .set_bytes(
                &fleet.root.docs().default_author().await.unwrap(),
                node_registration_key(&public(7)),
                registration(public(7), NodeRegistrationStatus::Active),
            )
            .await
            .unwrap();
        fleet
            .fleet
            .sync_with_timeout(&[fleet.issuer_node.id()], Duration::from_secs(10))
            .await
            .unwrap();
        fleet.issuer_enrollment.reconcile().await;
        assert!(matches!(
            fleet
                .issuer
                .issue_for(&public(7), build(), Registration::Persistent),
            Err(IssuanceError::Registered)
        ));
        assert_eq!(renewal.finished().await, RenewalOutcome::Registered);
        for node in [node, fleet.issuer_node, fleet.root] {
            node.shutdown().await;
        }
    })
    .await
    .unwrap();
}
