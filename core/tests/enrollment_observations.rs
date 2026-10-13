#![cfg(feature = "enrollment")]
//! Observations and promotion (enrollment B10): INV-E11, INV-E18, INV-E26,
//! and the sealed observer grant (D2).
use aster_transport_core::delegation::{
    issue_delegation, Chain, ChainError, DelegationGrant, LinkKind, Validity,
};
use aster_transport_core::grant_record::GrantRecord;
use aster_transport_core::grants::{grant_key, seal_namespace_grant, GrantContext};
use aster_transport_core::namespace::CoreNamespaceCapability;
use aster_transport_core::{enrollment::*, CoreDoc, CoreEndpointConfig, CoreNode};
use ed25519_dalek::SigningKey;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

const NOW: u64 = 1_800_000_000;
const FLEET: &str = "fleet";
const ROOT: u8 = 7;
const ISSUER: u8 = 6;
const NODE: u8 = 8;

fn public(seed: u8) -> [u8; 32] {
    SigningKey::from_bytes(&[seed; 32])
        .verifying_key()
        .to_bytes()
}

/// A private temporary directory, removed on drop.
struct Temp(std::path::PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "aster-enrollment-observations-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Root {
    node: CoreNode,
    fleet: CoreDoc,
    observations: CoreDoc,
    stores: PolicyStores,
    verifier: JwtEnrollmentVerifier,
    _temp: Temp,
}
const OBSERVATIONS_SECRET: [u8; 32] = [44; 32];

async fn setup() -> Root {
    let node = CoreNode::memory_with_alpns_and_gc(
        Vec::new(),
        Some(CoreEndpointConfig {
            relay_mode: Some("disabled".into()),
            secret_key: Some(vec![ROOT; 32]),
            ..Default::default()
        }),
        Some(Duration::from_secs(3600)),
    )
    .await
    .unwrap();
    let docs = node.docs_client();
    let fleet = docs.create().await.unwrap();
    let observations = docs
        .import_write_namespace(OBSERVATIONS_SECRET.to_vec())
        .await
        .unwrap();
    let temp = Temp::new();
    let stores = PolicyStores::new(Arc::new(
        EnrollmentPolicyStore::open(
            temp.0.join("fleet.json"),
            public(ROOT),
            *fleet.doc.id().as_bytes(),
            FLEET,
        )
        .unwrap(),
    ));
    Root {
        node,
        fleet,
        observations,
        stores,
        verifier: JwtEnrollmentVerifier::new(public(ROOT), FLEET).unwrap(),
        _temp: temp,
    }
}
async fn put_root(doc: &CoreDoc, key: Vec<u8>, value: Vec<u8>) {
    doc.set_bytes_durable(hex::encode(public(ROOT)), key, value)
        .await
        .unwrap();
}
/// Observers write with their own Docs authors; the root reads any author.
async fn put_observation(root: &Root, key: Vec<u8>, token: &str) {
    let author = root.node.docs_client().create_author().await.unwrap();
    root.observations
        .set_bytes(author, key, token.as_bytes().to_vec())
        .await
        .unwrap();
}
fn delegation(may_register: bool) -> Chain {
    let grant = DelegationGrant {
        may_issue: [LinkKind::Enrollment].into(),
        audiences: [FLEET.to_owned()].into(),
        scopes: ["build".to_owned()].into(),
        tags: Default::default(),
        attribute_keys: Default::default(),
        max_child_lifetime: 86_400,
        max_depth: 0,
        subjects: None,
        operations: Default::default(),
        mandatory: Default::default(),
        may_register,
    };
    issue_delegation(
        &[ROOT; 32],
        None,
        &public(ISSUER),
        &Audience::One(FLEET.into()),
        &grant,
        Validity {
            issued_at: NOW - 3600,
            not_before: None,
            expires_at: NOW + 7 * 86_400,
        },
    )
    .unwrap()
}
fn chain(subject: u8, registration: Registration) -> String {
    let mut request = EnrollmentIssueRequest::new(public(subject), FLEET, NOW + 3600);
    request.registration = registration;
    request.claims.scopes = Some(BTreeSet::from(["build".to_owned()]));
    JwtEnrollmentIssuer::from_seed(&[ISSUER; 32])
        .issue_delegated(
            &request,
            &delegation(registration == Registration::Persistent),
            NOW - 60,
        )
        .unwrap()
        .into_chain()
}
fn issued(observer: u8, subject: u8, chain: &str) -> String {
    issue_observation(
        &[observer; 32],
        &public(subject),
        chain,
        ObservationKind::Issued,
        NOW - 60,
        NOW - 30,
        NOW - 30,
    )
    .unwrap()
}
fn registration(subject: u8, scopes: &[&str]) -> NodeRegistrationV1 {
    NodeRegistrationV1 {
        version: 1,
        issuer: hex::encode(public(ROOT)),
        subject: hex::encode(public(subject)),
        audience: Audience::One(FLEET.into()),
        revision: 1,
        issued_at: NOW - 600,
        not_before: None,
        expires_at: None,
        status: NodeRegistrationStatus::Active,
        claims: AdmissionClaims {
            scopes: Some(scopes.iter().map(|scope| scope.to_string()).collect()),
            ..Default::default()
        },
        mandatory: vec![],
        reauthorizes_revision: None,
        reason: None,
    }
}
async fn review(root: &Root) -> ObservationReview {
    root.stores.fleet().refresh(&root.fleet).await.unwrap();
    review_observations(
        &root.observations,
        &root.fleet,
        &root.verifier,
        &root.stores,
        NOW,
    )
    .await
    .unwrap()
}
fn reasons(review: &ObservationReview) -> Vec<ObservationRejection> {
    review
        .rejected
        .iter()
        .map(|(_, _, reason)| reason.clone())
        .collect()
}

#[tokio::test]
async fn issuer_observation_is_promoted_to_a_root_record_that_outlives_the_chain() {
    let root = setup().await;
    let chain = chain(NODE, Registration::Persistent);
    put_observation(
        &root,
        observation_key(&public(ISSUER), &public(NODE)),
        &issued(ISSUER, NODE, &chain),
    )
    .await;
    // INV-E11: an observation alone admits nobody.
    root.stores.fleet().refresh(&root.fleet).await.unwrap();
    assert_eq!(
        root.stores
            .select_evidence(&public(NODE), None, NOW)
            .unwrap_err(),
        RecordError::MissingEvidence
    );
    let review = review(&root).await;
    assert!(review.rejected.is_empty(), "{:?}", review.rejected);
    let [reviewed] = review.accepted.as_slice() else {
        panic!("one accepted observation expected");
    };
    assert_eq!(reviewed.chain_issuer(), Some(public(ISSUER)));
    // INV-E18: root-authored, with the verified leaf's claims.
    let record = promotion_record(&public(ROOT), reviewed, FLEET, 1, None, NOW);
    assert_eq!(record.issuer, hex::encode(public(ROOT)));
    assert_eq!(record.status, NodeRegistrationStatus::Active);
    assert_eq!(record.expires_at, None);
    assert_eq!(&record.claims, reviewed.evidence.claims());
    assert_eq!(
        record.claims.scopes,
        Some(BTreeSet::from(["build".to_owned()]))
    );
    put_root(
        &root.fleet,
        node_registration_key(&public(NODE)),
        serde_json::to_vec(&record).unwrap(),
    )
    .await;
    root.stores.fleet().refresh(&root.fleet).await.unwrap();
    // Long after the chain expired, the record still admits the node.
    let later = NOW + 30 * 86_400;
    assert!(root.verifier.verify(&chain, &public(NODE), later).is_err());
    let selected = root
        .stores
        .select_evidence(&public(NODE), None, later)
        .unwrap();
    assert!(matches!(
        selected.source(),
        AdmissionEvidenceSource::RootAuthoredRecord { revision: 1, .. }
    ));
    // Once registered, the subject is no longer a promotion candidate.
    let again = review_observations(
        &root.observations,
        &root.fleet,
        &root.verifier,
        &root.stores,
        NOW,
    )
    .await
    .unwrap();
    assert!(again.accepted.is_empty());
    assert_eq!(
        reasons(&again),
        vec![ObservationRejection::AlreadyRegistered]
    );
}

#[tokio::test]
async fn inv_e11_forged_or_unqualified_observations_are_ignored_and_reported() {
    let root = setup().await;
    let persistent = chain(NODE, Registration::Persistent);
    // Bad signature: a valid token with its signature segment replaced.
    let valid = issued(ISSUER, NODE, &persistent);
    let mut parts: Vec<&str> = valid.split('.').collect();
    let other = issued(ISSUER, 9, &chain(9, Registration::Persistent));
    let other_signature = other.split('.').nth(2).unwrap().to_owned();
    parts[2] = &other_signature;
    put_observation(
        &root,
        observation_key(&public(ISSUER), &public(NODE)),
        &parts.join("."),
    )
    .await;
    // Stored under another subject's key.
    put_observation(
        &root,
        observation_key(&public(ISSUER), &public(11)),
        &issued(ISSUER, NODE, &persistent),
    )
    .await;
    // `issued` by a key that did not sign the chain.
    put_observation(
        &root,
        observation_key(&public(12), &public(NODE)),
        &issued(12, NODE, &persistent),
    )
    .await;
    // `admitted` by a node the root does not admit as an observer.
    let admitted = issue_observation(
        &[13; 32],
        &public(NODE),
        &persistent,
        ObservationKind::Admitted,
        NOW - 60,
        NOW - 30,
        NOW - 30,
    )
    .unwrap();
    put_observation(
        &root,
        observation_key(&public(13), &public(NODE)),
        &admitted,
    )
    .await;
    let review = review(&root).await;
    assert!(review.accepted.is_empty());
    let mut found = reasons(&review);
    found.sort_by_key(|reason| format!("{reason:?}"));
    assert_eq!(
        found,
        vec![
            ObservationRejection::Invalid(ObservationError::KeyMismatch),
            ObservationRejection::Invalid(ObservationError::Signature),
            ObservationRejection::ObserverNotQualified,
            ObservationRejection::ObserverNotQualified,
        ]
    );

    // The root's active grant record for a key makes it an observer, and a
    // denial withdraws that again.
    put_root(
        &root.fleet,
        grant_key(OBSERVER_GRANT_PREFIX, &hex::encode(public(13))).into_bytes(),
        GrantRecord::active("write", 1, vec![1, 2, 3]).encode_fory(),
    )
    .await;
    let review = crate::review(&root).await;
    assert_eq!(review.accepted.len(), 1);
    assert_eq!(review.accepted[0].observation.observer, public(13));
    let mut denied = registration(13, &[OBSERVER_SCOPE]);
    denied.status = NodeRegistrationStatus::Revoked;
    put_root(
        &root.fleet,
        node_registration_key(&public(13)),
        serde_json::to_vec(&denied).unwrap(),
    )
    .await;
    let review = crate::review(&root).await;
    assert!(review.accepted.is_empty());
}

#[tokio::test]
async fn inv_e26_ephemeral_chains_are_never_promoted() {
    let root = setup().await;
    let ephemeral = chain(NODE, Registration::Ephemeral);
    put_observation(
        &root,
        observation_key(&public(ISSUER), &public(NODE)),
        &issued(ISSUER, NODE, &ephemeral),
    )
    .await;
    let review = review(&root).await;
    assert!(review.accepted.is_empty());
    assert_eq!(reasons(&review), vec![ObservationRejection::Ephemeral]);
}

#[tokio::test]
async fn chains_through_a_revoked_issuer_are_not_promoted() {
    let root = setup().await;
    let persistent = chain(NODE, Registration::Persistent);
    put_observation(
        &root,
        observation_key(&public(ISSUER), &public(NODE)),
        &issued(ISSUER, NODE, &persistent),
    )
    .await;
    put_root(
        &root.fleet,
        issuer_status_key(&public(ISSUER)),
        serde_json::to_vec(&IssuerStatusV1 {
            version: 1,
            issuer: hex::encode(public(ROOT)),
            subject: hex::encode(public(ISSUER)),
            audience: Audience::One(FLEET.into()),
            revision: 1,
            issued_at: NOW - 10,
            status: IssuerStatus::Revoked,
            reauthorizes_revision: None,
        })
        .unwrap(),
    )
    .await;
    let review = review(&root).await;
    assert!(review.accepted.is_empty());
    assert_eq!(
        reasons(&review),
        vec![ObservationRejection::ChainInvalid(EnrollmentError::Chain(
            ChainError::Revoked
        ))]
    );
}

#[test]
fn observation_tokens_are_bounded_and_self_consistent() {
    let persistent = chain(NODE, Registration::Persistent);
    let token = issued(ISSUER, NODE, &persistent);
    let observation = verify_observation(&token, NOW, 60).unwrap();
    assert_eq!(observation.observer, public(ISSUER));
    assert_eq!(observation.subject, public(NODE));
    assert_eq!(observation.kind, ObservationKind::Issued);
    assert_eq!(
        observation.expires_at - observation.issued_at,
        MAX_OBSERVATION_LIFETIME
    );
    // Expired, or issued too far in the future.
    assert_eq!(
        verify_observation(&token, NOW + MAX_OBSERVATION_LIFETIME, 60).unwrap_err(),
        ObservationError::Validity
    );
    assert_eq!(
        verify_observation(&token, NOW - 1000, 60).unwrap_err(),
        ObservationError::Validity
    );
    // last_seen cannot follow issuance.
    assert!(issue_observation(
        &[ISSUER; 32],
        &public(NODE),
        &persistent,
        ObservationKind::Issued,
        NOW,
        NOW + 1,
        NOW,
    )
    .is_err());
}

#[tokio::test]
async fn sealed_observer_grant_opens_only_for_its_observer_and_honours_withdrawal() {
    let root = setup().await;
    let observations: [u8; 32] = *root.observations.doc.id().as_bytes();
    let observer = public(ISSUER);
    let recipient = hex::encode(observer);
    let key = grant_key(OBSERVER_GRANT_PREFIX, &recipient);
    let granter = hex::encode(public(ROOT));
    let context = GrantContext {
        app: OBSERVER_GRANT_APP,
        granter: &granter,
        recipient: &recipient,
        resource: &observations,
        path: &key,
        role: "write",
    };
    assert!(matches!(
        open_observer_grant(&root.fleet, &public(ROOT), &[ISSUER; 32], &observations)
            .await
            .unwrap(),
        ObserverGrant::Absent
    ));
    let envelope = seal_namespace_grant(
        &context,
        &CoreNamespaceCapability::Write(OBSERVATIONS_SECRET),
    )
    .unwrap();
    put_root(
        &root.fleet,
        key.clone().into_bytes(),
        GrantRecord::active("write", 1, envelope).encode_fory(),
    )
    .await;
    match open_observer_grant(&root.fleet, &public(ROOT), &[ISSUER; 32], &observations)
        .await
        .unwrap()
    {
        ObserverGrant::Active(secret) => assert_eq!(secret, OBSERVATIONS_SECRET),
        other => panic!("{other:?}"),
    }
    // Another key cannot open it, and a grant from another author is invisible.
    // Another key reads the absent grant at its own key.
    assert!(matches!(
        open_observer_grant(&root.fleet, &public(ROOT), &[12; 32], &observations)
            .await
            .unwrap(),
        ObserverGrant::Absent
    ));
    assert!(matches!(
        open_observer_grant(&root.fleet, &public(12), &[ISSUER; 32], &observations)
            .await
            .unwrap(),
        ObserverGrant::Absent
    ));
    put_root(
        &root.fleet,
        key.into_bytes(),
        GrantRecord::revoked("write", "observer withdrawn").encode_fory(),
    )
    .await;
    assert!(matches!(
        open_observer_grant(&root.fleet, &public(ROOT), &[ISSUER; 32], &observations)
            .await
            .unwrap(),
        ObserverGrant::Revoked
    ));
}
