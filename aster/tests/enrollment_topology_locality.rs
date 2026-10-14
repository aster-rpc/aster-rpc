#![cfg(all(feature = "enrollment", feature = "docs", feature = "rpc"))]
//! `Enrollment::topology_locality`: the declared-locality source the shared
//! topology engine uses. Its region and zone come only from root-signed
//! claims: a chain that verifies for the node that published it, or the
//! root's registration record for a node that holds no chain.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::topology::{REGION_ATTRIBUTE, ZONE_ATTRIBUTE};
use aster::{AsterConfig, Doc, Node, RelayMode, SecretKey};

const FLEET: &str = "locality-test";

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

fn located(region: &str, zone: &str) -> AdmissionClaims {
    let attributes = BTreeMap::from([
        (
            REGION_ATTRIBUTE.to_string(),
            TypedAttribute::String(region.into()),
        ),
        (
            ZONE_ATTRIBUTE.to_string(),
            TypedAttribute::String(zone.into()),
        ),
    ]);
    AdmissionClaims {
        scopes: Some(BTreeSet::new()),
        tags: Some(BTreeSet::new()),
        attributes: Some(attributes),
    }
}

/// A root-signed credential for `seed` carrying `claims`.
fn credential(seed: u8, claims: AdmissionClaims) -> String {
    let mut request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
    request.claims = claims;
    issue_credential(&key(1), &request, now())
        .unwrap()
        .into_chain()
}

fn registration(subject: [u8; 32], claims: AdmissionClaims) -> Vec<u8> {
    serde_json::to_vec(&NodeRegistrationV1 {
        version: 1,
        issuer: hex::encode(public(1)),
        subject: hex::encode(subject),
        audience: Audience::One(FLEET.into()),
        revision: 1,
        issued_at: now() - 60,
        not_before: None,
        expires_at: None,
        status: NodeRegistrationStatus::Active,
        claims,
        mandatory: vec![],
        reauthorizes_revision: None,
        reason: None,
    })
    .unwrap()
}

fn namespace(doc: &Doc) -> [u8; 32] {
    hex::decode(doc.id().unwrap().to_string())
        .unwrap()
        .try_into()
        .unwrap()
}

#[tokio::test]
async fn locality_comes_only_from_root_signed_claims() {
    let files = tempfile::tempdir().unwrap();
    let root = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(key(1))
            .hooks(true)
            .hook_failure_mode(aster::HookFailureMode::FailClosed)
            .hook_timeout_ms(1000)
            .build(),
    )
    .await
    .unwrap();
    let fleet = root.docs().create().await.unwrap();
    let author = root.docs().default_author().await.unwrap();
    // Seed 4 holds no chain: the root registered it, zone included.
    fleet
        .set_bytes(
            &author,
            node_registration_key(&public(4)),
            registration(public(4), located("eu-west-1", "eu-west-1c")),
        )
        .await
        .unwrap();

    let own_chain = credential(1, located("eu-west-1", "eu-west-1a"));
    let enrollment = Enrollment::install(
        &root,
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
        Some(own_chain.clone()),
        Arc::new(PolicyExpressionV1::Predicate(
            PolicyPredicate::TolerateStalePolicy,
        )),
        EnrollmentOptions {
            reconciliation_interval: Duration::from_millis(200),
            ..Default::default()
        },
    )
    .unwrap();
    enrollment.set_policy_document(&fleet).unwrap();

    let source = enrollment.topology_locality();
    let hex_of = |seed: u8| hex::encode(public(seed));

    // This node's own chain, as the publisher reads and checks it.
    assert_eq!((source.own_chain)(), Some(own_chain.clone()));
    let mine = (source.verify)(&hex_of(1), &own_chain).unwrap();
    assert_eq!(mine.zone.as_deref(), Some("eu-west-1a"));

    // A peer's chain verifies for that peer only.
    let peer_chain = credential(2, located("eu-west-1", "eu-west-1b"));
    let peer = (source.verify)(&hex_of(2), &peer_chain).unwrap();
    assert_eq!(peer.zone.as_deref(), Some("eu-west-1b"));
    assert_eq!(
        (source.verify)(&hex_of(3), &peer_chain),
        None,
        "a chain republished by another node declares nothing"
    );

    // A chain not signed by this fleet's root declares nothing.
    let mut request = EnrollmentIssueRequest::new(public(2), FLEET, now() + 3600);
    request.claims = located("eu-west-1", "eu-west-1a");
    let foreign = issue_credential(&key(9), &request, now())
        .unwrap()
        .into_chain();
    assert_eq!((source.verify)(&hex_of(2), &foreign), None);

    // No chain: the root's registration record answers, once reconciled.
    let mut registered = None;
    for _ in 0..100 {
        registered = (source.verify)(&hex_of(4), "");
        if registered.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        registered.and_then(|l| l.zone).as_deref(),
        Some("eu-west-1c")
    );
    // ...and an unregistered node without a chain declares nothing.
    assert_eq!((source.verify)(&hex_of(5), ""), None);
}
