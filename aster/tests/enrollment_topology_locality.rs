#![cfg(all(feature = "enrollment", feature = "docs", feature = "rpc"))]
//! `Enrollment::topology_locality`: the declared-locality source the shared
//! topology engine uses. A node publishes a locality statement, never its
//! credential: a separate leaf carrying only region and zone, issued with the
//! credential by the same signer. A reader accepts it only when it verifies
//! for the endpoint that published it, as a credential would (root or
//! delegated issuer, expiry), and otherwise falls back to the root's
//! registration record.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::delegation::{
    issue_delegation, locality_statement, Chain, DelegationGrant, LinkKind, Validity,
};
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

/// Locality claims plus an unrelated attribute that must never be published.
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
        (
            "tier".to_string(),
            TypedAttribute::String("private-tier-value".into()),
        ),
    ]);
    AdmissionClaims {
        scopes: Some(BTreeSet::from(["private-scope".to_string()])),
        tags: Some(BTreeSet::new()),
        attributes: Some(attributes),
    }
}

/// A root-signed credential for `seed` carrying `claims`.
fn credential(seed: u8, claims: AdmissionClaims) -> String {
    credential_at(seed, claims, now(), now() + 3600)
}

fn credential_at(seed: u8, claims: AdmissionClaims, issued_at: u64, expires_at: u64) -> String {
    let mut request = EnrollmentIssueRequest::new(public(seed), FLEET, expires_at);
    request.claims = claims;
    issue_credential(&key(1), &request, issued_at)
        .unwrap()
        .into_chain()
}

fn statement_of(credential: &str) -> String {
    locality_statement(&Chain::from_text(credential).unwrap())
        .expect("credential with locality carries a statement")
        .into_text()
}

/// The root (seed 1) delegates to an issuer (seed 2) that may sign locality.
fn delegation() -> Chain {
    issue_delegation(
        &key(1),
        None,
        &key(2).public_key(),
        &Audience::One(FLEET.into()),
        &DelegationGrant {
            may_issue: [LinkKind::Enrollment].into(),
            audiences: [FLEET.to_string()].into(),
            scopes: ["private-scope".to_string()].into(),
            tags: BTreeSet::new(),
            attribute_keys: [REGION_ATTRIBUTE, ZONE_ATTRIBUTE, "tier"]
                .map(str::to_string)
                .into(),
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
    .unwrap()
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

/// Each link's decoded JSON payload.
fn payloads(chain: &str) -> Vec<String> {
    use base64::Engine;
    chain
        .split('~')
        .skip(1)
        .map(|link| {
            let payload = link.split('.').nth(1).unwrap();
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload)
                .unwrap();
            String::from_utf8(bytes).unwrap()
        })
        .collect()
}

/// Seed 1, the root, with `own_chain` installed against a fresh fleet doc.
async fn root_with(own_chain: &str, files: &std::path::Path) -> (Node, Doc, Enrollment) {
    let root = Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(key(1))
            .monitoring(true)
            .hooks(true)
            .hook_failure_mode(aster::HookFailureMode::FailClosed)
            .hook_timeout_ms(1000)
            .build(),
    )
    .await
    .unwrap();
    let fleet = root.docs().create().await.unwrap();
    let enrollment = Enrollment::install(
        &root,
        Arc::new(
            EnrollmentPolicyStore::open(
                files.join("root.json"),
                public(1),
                namespace(&fleet),
                FLEET,
            )
            .unwrap(),
        ),
        JwtEnrollmentVerifier::new(public(1), FLEET).unwrap(),
        Some(own_chain.to_string()),
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
    (root, fleet, enrollment)
}

#[tokio::test]
async fn locality_comes_only_from_endpoint_bound_root_verifiable_statements() {
    let files = tempfile::tempdir().unwrap();
    let own_chain = credential(1, located("eu-west-1", "eu-west-1a"));
    let (root, fleet, enrollment) = root_with(&own_chain, files.path()).await;
    let author = root.docs().default_author().await.unwrap();
    // Seed 4 holds no credential: the root registered it, zone included.
    fleet
        .set_bytes(
            &author,
            node_registration_key(&public(4)),
            registration(public(4), located("eu-west-1", "eu-west-1c")),
        )
        .await
        .unwrap();

    let source = enrollment.topology_locality();
    let hex_of = |seed: u8| hex::encode(public(seed));
    let zone_of = |seed: u8, statement: &str| {
        (source.verify)(&hex_of(seed), statement).and_then(|v| v.declared.zone)
    };

    // This node publishes its statement, not its credential, and the
    // statement carries nothing else the credential does.
    let own = (source.own_statement)().expect("own statement");
    assert_ne!(own, own_chain);
    let own_leaf = own_chain.split('~').nth(1).unwrap();
    assert!(
        !own.contains(own_leaf),
        "the credential leaf is not published"
    );
    for payload in payloads(&own) {
        assert!(!payload.contains("private-tier-value"), "{payload}");
        assert!(!payload.contains("private-scope"), "{payload}");
    }
    assert_eq!(zone_of(1, &own).as_deref(), Some("eu-west-1a"));
    let verified = (source.verify)(&hex_of(1), &own).unwrap();
    assert!(verified.expires_at.unwrap() <= now() + 3600);
    // A full credential is not a statement.
    assert_eq!(zone_of(1, &own_chain), None);

    // A peer's statement verifies for that peer only.
    let peer = statement_of(&credential(2, located("eu-west-1", "eu-west-1b")));
    assert_eq!(zone_of(2, &peer).as_deref(), Some("eu-west-1b"));
    assert_eq!(
        zone_of(3, &peer),
        None,
        "a statement republished by another endpoint declares nothing"
    );

    // An expired statement declares nothing.
    let expired = statement_of(&credential_at(
        3,
        located("eu-west-1", "eu-west-1a"),
        now() - 120,
        now() - 60,
    ));
    assert_eq!(zone_of(3, &expired), None);

    // A statement not signed by this fleet's root declares nothing.
    let mut request = EnrollmentIssueRequest::new(public(2), FLEET, now() + 3600);
    request.claims = located("eu-west-1", "eu-west-1a");
    let foreign = issue_credential(&key(9), &request, now())
        .unwrap()
        .into_chain();
    assert_eq!(zone_of(2, &statement_of(&foreign)), None);

    // A delegated issuer's statement verifies through its delegation.
    let mut request = EnrollmentIssueRequest::new(public(5), FLEET, now() + 600);
    request.claims = located("eu-west-1", "eu-west-1d");
    let delegated = issue_delegated_credential(&key(2), &delegation(), &request, now())
        .unwrap()
        .into_chain();
    let delegated = statement_of(&delegated);
    assert_eq!(
        delegated.split('~').count(),
        3,
        "prefix, locality, delegation"
    );
    assert_eq!(zone_of(5, &delegated).as_deref(), Some("eu-west-1d"));

    // No statement: the root's registration record answers, once reconciled,
    // and it takes precedence over any statement, as in admission.
    let mut registered = None;
    for _ in 0..100 {
        registered = zone_of(4, "");
        if registered.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(registered.as_deref(), Some("eu-west-1c"));
    let stale = statement_of(&credential(4, located("eu-west-1", "eu-west-1a")));
    assert_eq!(zone_of(4, &stale).as_deref(), Some("eu-west-1c"));
    // ...and an unregistered node without a statement declares nothing.
    assert_eq!(zone_of(6, ""), None);
}

/// What actually lands in the shared topology doc: the locality statement
/// and nothing else from the credential. Its leaf names this endpoint and
/// carries only region and zone.
#[tokio::test]
async fn the_published_record_carries_only_locality() {
    use aster::topology::TopologySwarmOptions;
    use aster::NamespaceSecret;
    use aster_transport_core::topology::records::{decode_locality, locality_key};

    let files = tempfile::tempdir().unwrap();
    let own_chain = credential(1, located("eu-west-1", "eu-west-1a"));
    let (root, _fleet, enrollment) = root_with(&own_chain, files.path()).await;
    let secret = NamespaceSecret::from_bytes(SecretKey::generate().to_bytes());
    root.topology()
        .enable_shared(
            &secret,
            TopologySwarmOptions {
                locality: Some(enrollment.topology_locality()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let doc = root.docs().open(secret.id()).await.unwrap().unwrap();
    let own_hex = hex::encode(public(1));
    let entry = doc
        .query_latest_exact(locality_key(&own_hex))
        .await
        .unwrap()
        .expect("locality record published on join");
    let bytes = doc.read_entry_content(&entry.content_hash).await.unwrap();
    let record = decode_locality(&bytes).unwrap();

    let own_statement = (enrollment.topology_locality().own_statement)();
    assert_eq!(Some(&record.statement), own_statement.as_ref());
    assert_ne!(record.statement, own_chain);
    let decoded = payloads(&record.statement);
    assert_eq!(decoded.len(), 1, "root-issued: one locality link");
    let leaf: serde_json::Value = serde_json::from_str(&decoded[0]).unwrap();
    let mut members: Vec<_> = leaf.as_object().unwrap().keys().cloned().collect();
    members.sort();
    assert_eq!(
        members,
        [
            "aster_locality_v1",
            "aud",
            "cnf",
            "exp",
            "iat",
            "iss",
            "jti",
            "sub"
        ]
    );
    assert_eq!(
        leaf["sub"],
        own_hex.as_str(),
        "bound to the publishing endpoint"
    );
    assert_eq!(
        leaf["aster_locality_v1"],
        serde_json::json!({"region": "eu-west-1", "zone": "eu-west-1a"})
    );
    let raw = String::from_utf8_lossy(&bytes);
    for private in ["private-tier-value", "private-scope", "aster_enrollment_v1"] {
        assert!(!decoded[0].contains(private), "{private} in the statement");
        assert!(!raw.contains(private), "{private} in the record");
    }
    root.shutdown().await;
}
