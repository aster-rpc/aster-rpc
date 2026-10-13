#![cfg(feature = "enrollment")]
//! Issuer status records (enrollment B8): INV-E6 and INV-E7 at the store.
use aster_transport_core::delegation::{
    issue_delegation, Chain, ChainError, DelegationGrant, LinkKind, Validity,
};
use aster_transport_core::{enrollment::*, CoreDoc, CoreEndpointConfig, CoreNode};
use ed25519_dalek::SigningKey;
use std::time::Duration;

const NOW: u64 = 1_800_000_000;
const FLEET: &str = "fleet";

fn public(seed: u8) -> [u8; 32] {
    SigningKey::from_bytes(&[seed; 32])
        .verifying_key()
        .to_bytes()
}
fn root() -> [u8; 32] {
    public(7)
}
const ISSUER: u8 = 6;
const NODE: u8 = 8;

async fn setup() -> (CoreNode, CoreDoc) {
    let node = CoreNode::memory_with_alpns_and_gc(
        Vec::new(),
        Some(CoreEndpointConfig {
            relay_mode: Some("disabled".into()),
            secret_key: Some(vec![7; 32]),
            ..Default::default()
        }),
        Some(Duration::from_secs(3600)),
    )
    .await
    .unwrap();
    let doc = node.docs_client().create().await.unwrap();
    (node, doc)
}
/// A private temporary directory, removed on drop.
struct Temp(std::path::PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "aster-enrollment-issuers-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn namespace(doc: &CoreDoc) -> [u8; 32] {
    hex::decode(doc.doc_id()).unwrap().try_into().unwrap()
}
fn store(path: &std::path::Path, doc: &CoreDoc) -> EnrollmentPolicyStore {
    EnrollmentPolicyStore::open(path, root(), namespace(doc), FLEET).unwrap()
}
async fn put(doc: &CoreDoc, key: Vec<u8>, value: &impl serde::Serialize) {
    doc.set_bytes_durable(hex::encode(root()), key, serde_json::to_vec(value).unwrap())
        .await
        .unwrap();
}
fn issuer_status(revision: u64, status: IssuerStatus) -> IssuerStatusV1 {
    IssuerStatusV1 {
        version: 1,
        issuer: hex::encode(root()),
        subject: hex::encode(public(ISSUER)),
        audience: Audience::One(FLEET.into()),
        revision,
        issued_at: NOW - 60,
        status,
        reauthorizes_revision: None,
    }
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
        may_register: true,
    };
    issue_delegation(
        &[7; 32],
        None,
        &public(ISSUER),
        &Audience::One(FLEET.into()),
        &grant,
        Validity {
            issued_at: NOW - 600,
            not_before: None,
            expires_at: NOW + 7 * 86_400,
        },
    )
    .unwrap()
}
fn delegated_chain() -> String {
    JwtEnrollmentIssuer::from_seed(&[ISSUER; 32])
        .issue_delegated(
            &EnrollmentIssueRequest::new(public(NODE), FLEET, NOW + 3600),
            &delegation(),
            NOW - 60,
        )
        .unwrap()
        .into_chain()
}
fn registration(revision: u64) -> NodeRegistrationV1 {
    NodeRegistrationV1 {
        version: 1,
        issuer: hex::encode(root()),
        subject: hex::encode(public(NODE)),
        audience: Audience::One(FLEET.into()),
        revision,
        issued_at: NOW - 60,
        not_before: None,
        expires_at: None,
        status: NodeRegistrationStatus::Active,
        claims: AdmissionClaims::default(),
        mandatory: vec![],
        reauthorizes_revision: None,
        reason: None,
    }
}

#[tokio::test]
async fn inv_e6_revoked_issuer_rejects_its_chains_at_verification_and_as_cached_evidence() {
    let (_node, doc) = setup().await;
    let files = Temp::new();
    let store = store(&files.path().join("fleet.json"), &doc);
    let verifier = JwtEnrollmentVerifier::new(root(), FLEET).unwrap();
    let chain = delegated_chain();
    // Admitted before the root revokes the issuer.
    let admitted = verifier
        .verify_with(&chain, &public(NODE), NOW, &store)
        .unwrap();
    assert!(store
        .select_evidence(&public(NODE), Some(&admitted), NOW)
        .is_ok());

    put(
        &doc,
        issuer_status_key(&public(ISSUER)),
        &issuer_status(1, IssuerStatus::Revoked),
    )
    .await;
    store.refresh(&doc).await.unwrap();
    assert!(store.issuer_revoked(&public(ISSUER)));
    // A fresh presentation fails gate G6, before any signature work.
    assert_eq!(
        verifier
            .verify_with(&chain, &public(NODE), NOW, &store)
            .unwrap_err(),
        EnrollmentError::Chain(ChainError::Revoked)
    );
    // Evidence admitted earlier is now a definite denial, so a session
    // resting on it is revoked at the next reconciliation.
    let denial = store
        .select_evidence(&public(NODE), Some(&admitted), NOW)
        .unwrap_err();
    assert_eq!(denial, RecordError::Denied(DenialReason::IssuerRevoked));
    assert!(DenialReason::IssuerRevoked.is_definite());
    assert_eq!(
        store.denial(&admitted, NOW),
        Some(DenialReason::IssuerRevoked)
    );
    // A root-issued credential is unaffected: only the delegate is revoked.
    let root_chain = JwtEnrollmentIssuer::from_seed(&[7; 32])
        .issue(
            &EnrollmentIssueRequest::new(public(NODE), FLEET, NOW + 3600),
            NOW - 60,
        )
        .unwrap()
        .into_chain();
    let direct = verifier
        .verify_with(&root_chain, &public(NODE), NOW, &store)
        .unwrap();
    assert!(store
        .select_evidence(&public(NODE), Some(&direct), NOW)
        .is_ok());
}

#[tokio::test]
async fn inv_e7_issuer_revocation_leaves_published_registrations_in_force() {
    let (_node, doc) = setup().await;
    let files = Temp::new();
    let store = store(&files.path().join("fleet.json"), &doc);
    put(&doc, node_registration_key(&public(NODE)), &registration(1)).await;
    put(
        &doc,
        issuer_status_key(&public(ISSUER)),
        &issuer_status(1, IssuerStatus::Revoked),
    )
    .await;
    store.refresh(&doc).await.unwrap();
    let selected = store.select_evidence(&public(NODE), None, NOW).unwrap();
    assert_eq!(
        selected.source(),
        AdmissionEvidenceSource::RootAuthoredRecord {
            document_id: namespace(&doc),
            revision: 1,
        }
    );
    assert_eq!(store.denial(&selected, NOW), None);
}

#[tokio::test]
async fn issuer_status_is_monotonic_scoped_and_reactivation_names_the_revocation() {
    let (_node, doc) = setup().await;
    let files = Temp::new();
    let path = files.path().join("fleet.json");
    let store = store(&path, &doc);
    let key = issuer_status_key(&public(ISSUER));
    put(&doc, key.clone(), &issuer_status(2, IssuerStatus::Revoked)).await;
    assert_eq!(
        store.refresh_issuer(&doc, &public(ISSUER)).await.unwrap(),
        RecordAvailability::Available
    );
    assert!(store.issuer_revoked(&public(ISSUER)));
    // Reactivation without naming the revoked revision is refused.
    put(&doc, key.clone(), &issuer_status(3, IssuerStatus::Active)).await;
    assert_eq!(
        store
            .refresh_issuer(&doc, &public(ISSUER))
            .await
            .unwrap_err(),
        RecordError::ReauthorizationRequired
    );
    assert!(store.issuer_revoked(&public(ISSUER)));
    // A record for another audience is unrelated and changes nothing.
    let mut other = issuer_status(4, IssuerStatus::Active);
    other.audience = Audience::One("other-fleet".into());
    other.reauthorizes_revision = Some(2);
    put(&doc, key.clone(), &other).await;
    assert_eq!(
        store
            .refresh_issuer(&doc, &public(ISSUER))
            .await
            .unwrap_err(),
        RecordError::WrongAudience
    );
    assert!(store.issuer_revoked(&public(ISSUER)));
    // Naming it reactivates, and the state survives restart.
    let mut reactivated = issuer_status(5, IssuerStatus::Active);
    reactivated.reauthorizes_revision = Some(2);
    put(&doc, key.clone(), &reactivated).await;
    store.refresh_issuer(&doc, &public(ISSUER)).await.unwrap();
    assert!(!store.issuer_revoked(&public(ISSUER)));
    assert_eq!(store.observed_issuer_revision(&public(ISSUER)), Some(5));
    drop(store);
    let reopened = EnrollmentPolicyStore::open(&path, root(), namespace(&doc), FLEET).unwrap();
    assert_eq!(reopened.observed_issuer_revision(&public(ISSUER)), Some(5));
    // An older revision replayed later is stale and cannot revoke again.
    put(&doc, key, &issuer_status(4, IssuerStatus::Revoked)).await;
    assert_eq!(
        reopened
            .refresh_issuer(&doc, &public(ISSUER))
            .await
            .unwrap_err(),
        RecordError::StaleRevision
    );
    assert!(!reopened.issuer_revoked(&public(ISSUER)));
}

#[tokio::test]
async fn issuer_record_for_the_root_or_from_another_author_is_rejected() {
    let (node, doc) = setup().await;
    let files = Temp::new();
    let store = store(&files.path().join("fleet.json"), &doc);
    // The root cannot be revoked as a delegate.
    let mut root_record = issuer_status(1, IssuerStatus::Revoked);
    root_record.subject = hex::encode(root());
    put(&doc, issuer_status_key(&root()), &root_record).await;
    assert_eq!(
        store.refresh_issuer(&doc, &root()).await.unwrap_err(),
        RecordError::InvalidRecord
    );
    // Another author writing at the issuer key is invisible.
    let author = node.docs_client().create_author().await.unwrap();
    doc.set_bytes_durable(
        author,
        issuer_status_key(&public(ISSUER)),
        serde_json::to_vec(&issuer_status(1, IssuerStatus::Revoked)).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        store.refresh_issuer(&doc, &public(ISSUER)).await.unwrap(),
        RecordAvailability::Absent
    );
    assert!(!store.issuer_revoked(&public(ISSUER)));
}
