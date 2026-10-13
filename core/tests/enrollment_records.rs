#![cfg(feature = "enrollment")]
use aster_transport_core::{enrollment::*, CoreDoc, CoreEndpointConfig, CoreNode};
use ed25519_dalek::SigningKey;
use iroh_docs::AuthorId;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
const NOW: u64 = 1_800_000_000;
fn root() -> [u8; 32] {
    SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes()
}
fn peer() -> [u8; 32] {
    SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes()
}
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
fn registration(revision: u64, status: NodeRegistrationStatus) -> NodeRegistrationV1 {
    NodeRegistrationV1 {
        version: 1,
        issuer: hex::encode(root()),
        subject: hex::encode(peer()),
        audience: Audience::One("fleet".into()),
        revision,
        issued_at: NOW - 60,
        not_before: None,
        expires_at: None,
        status,
        claims: AdmissionClaims {
            scopes: Some(BTreeSet::from(["registered".into()])),
            tags: Some(BTreeSet::new()),
            attributes: Some(BTreeMap::from([("workers".into(), TypedAttribute::Int(8))])),
        },
        mandatory: vec![],
        reauthorizes_revision: None,
        reason: None,
    }
}
async fn write(doc: &CoreDoc, record: &NodeRegistrationV1) {
    doc.set_bytes_durable(
        hex::encode(root()),
        node_registration_key(&peer()),
        serde_json::to_vec(record).unwrap(),
    )
    .await
    .unwrap();
}
/// A root-issued one-link chain with a fixed `jti`, signed raw so tests can
/// publish a credential status for it.
fn token() -> VerifiedAdmissionEvidence {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use ed25519_dalek::Signer;
    let c = serde_json::json!({
        "iss": hex::encode(root()),
        "sub": hex::encode(peer()),
        "aud": "fleet",
        "jti": "credential-0000001",
        "iat": NOW - 60,
        "exp": NOW + 20 * 86_400,
        "cnf": {"jwk": {"kty": "OKP", "crv": "Ed25519", "x": URL_SAFE_NO_PAD.encode(peer())}},
        "aster_enrollment_v1": {"scopes": ["credential-only"], "mandatory": []},
    });
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"aster-enrollment-v1+jwt"}"#),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&c).unwrap())
    );
    let chain = format!(
        "{}~{}.{}",
        aster_transport_core::delegation::CHAIN_PREFIX,
        input,
        URL_SAFE_NO_PAD.encode(
            SigningKey::from_bytes(&[7; 32])
                .sign(input.as_bytes())
                .to_bytes()
        )
    );
    JwtEnrollmentVerifier::new(root(), "fleet")
        .unwrap()
        .verify(&chain, &peer(), NOW)
        .unwrap()
}
fn store(path: &std::path::Path, doc: &CoreDoc) -> EnrollmentPolicyStore {
    EnrollmentPolicyStore::open(path, root(), *doc.doc.id().as_bytes(), "fleet").unwrap()
}
struct Temp(std::path::PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "aster-enrollment-records-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path.join("policy.json"))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}

#[tokio::test]
async fn exact_root_ignores_spoofed_writer_and_pending_never_manufactures_evidence() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let store = store(&path.0, &doc);
    let attacker = node.docs_client().create_author().await.unwrap();
    doc.set_bytes(
        attacker.clone(),
        node_registration_key(&peer()),
        serde_json::to_vec(&registration(99, NodeRegistrationStatus::Active)).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        store.refresh_node(&doc, &peer()).await.unwrap(),
        RecordAvailability::Absent
    );
    assert_eq!(
        store.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::MissingEvidence
    );
    assert_eq!(
        store
            .select_evidence(&peer(), Some(&token()), NOW)
            .unwrap()
            .source(),
        AdmissionEvidenceSource::PresentedChain {
            anchor: root(),
            issuers: vec![root()],
            digest: *blake3::Hasher::new()
                .update(b"aster.chain.v1\0")
                .update(token().presented_chain().unwrap().as_bytes())
                .finalize()
                .as_bytes(),
        }
    );
    let bytes = serde_json::to_vec(&registration(1, NodeRegistrationStatus::Active)).unwrap();
    doc.doc
        .set_hash(
            AuthorId::from(root()),
            node_registration_key(&peer()),
            iroh_blobs::Hash::new(&bytes),
            bytes.len() as u64,
        )
        .await
        .unwrap();
    assert_eq!(
        store.refresh_node(&doc, &peer()).await.unwrap(),
        RecordAvailability::Pending
    );
    assert_eq!(
        store.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::MissingEvidence
    );
    write(&doc, &registration(1, NodeRegistrationStatus::Active)).await;
    doc.set_bytes(
        attacker,
        node_registration_key(&peer()),
        b"invalid latest author".to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(
        store.refresh_node(&doc, &peer()).await.unwrap(),
        RecordAvailability::Available
    );
    let e = store.select_evidence(&peer(), Some(&token()), NOW).unwrap();
    assert_eq!(e.credential_id(), None);
    assert_eq!(e.presented_chain(), None);
    assert_eq!(e.registration(), None);
    assert_eq!(
        e.claims().attributes.as_ref().unwrap()["workers"],
        TypedAttribute::Int(8)
    );
    assert!(e.claims().scopes.as_ref().unwrap().contains("registered"));
    assert!(!e
        .claims()
        .scopes
        .as_ref()
        .unwrap()
        .contains("credential-only"));
    assert!(matches!(
        e.source(),
        AdmissionEvidenceSource::RootAuthoredRecord { revision: 1, .. }
    ));
    node.close().await;
}

/// A revocation may carry an operator note; it is bounded and confers
/// nothing, so an oversized or control-character note makes the record
/// unusable rather than silently accepted.
#[tokio::test]
async fn revocation_reason_is_bounded_and_informational() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let store = store(&path.0, &doc);
    let mut revoked = registration(1, NodeRegistrationStatus::Revoked);
    revoked.reason = Some("key exposed in CI logs".into());
    write(&doc, &revoked).await;
    store.refresh_node(&doc, &peer()).await.unwrap();
    assert_eq!(
        store.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::NodeRevoked)
    );
    for reason in ["x".repeat(MAX_REGISTRATION_REASON_BYTES + 1), "a\nb".into()] {
        let mut record = registration(2, NodeRegistrationStatus::Revoked);
        record.reason = Some(reason);
        write(&doc, &record).await;
        assert!(store.refresh_node(&doc, &peer()).await.is_err());
        // The unusable newer statement is a durable denial marker; the node
        // stays denied.
        assert_eq!(
            store.select_evidence(&peer(), None, NOW).unwrap_err(),
            RecordError::Denied(DenialReason::RegistrationRejected)
        );
    }
    node.close().await;
}

#[tokio::test]
async fn durable_revocation_survives_restart_stale_docs_and_requires_explicit_reauthorization() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let s = store(&path.0, &doc);
    write(&doc, &registration(1, NodeRegistrationStatus::Active)).await;
    s.refresh_node(&doc, &peer()).await.unwrap();
    let cached = s.select_evidence(&peer(), None, NOW).unwrap();
    write(&doc, &registration(2, NodeRegistrationStatus::Revoked)).await;
    s.refresh_node(&doc, &peer()).await.unwrap();
    assert_eq!(s.denial(&cached, NOW), Some(DenialReason::NodeRevoked));
    drop(s);
    let s = store(&path.0, &doc);
    let pending = serde_json::to_vec(&registration(3, NodeRegistrationStatus::Active)).unwrap();
    doc.doc
        .set_hash(
            AuthorId::from(root()),
            node_registration_key(&peer()),
            iroh_blobs::Hash::new(&pending),
            pending.len() as u64,
        )
        .await
        .unwrap();
    assert_eq!(
        s.refresh_node(&doc, &peer()).await.unwrap(),
        RecordAvailability::Pending
    );
    assert_eq!(
        s.select_evidence(&peer(), Some(&token()), NOW).unwrap_err(),
        RecordError::Denied(DenialReason::NodeRevoked)
    );
    write(&doc, &registration(1, NodeRegistrationStatus::Active)).await;
    assert_eq!(
        s.refresh_node(&doc, &peer()).await.unwrap_err(),
        RecordError::StaleRevision
    );
    let mut restored = registration(3, NodeRegistrationStatus::Active);
    write(&doc, &restored).await;
    assert_eq!(
        s.refresh_node(&doc, &peer()).await.unwrap_err(),
        RecordError::ReauthorizationRequired
    );
    restored.revision = 4;
    restored.reauthorizes_revision = Some(2);
    write(&doc, &restored).await;
    s.refresh_node(&doc, &peer()).await.unwrap();
    assert!(s.select_evidence(&peer(), Some(&token()), NOW).is_ok());
    assert_eq!(
        s.denial(&cached, NOW),
        Some(DenialReason::RegistrationSuperseded)
    );
    assert_eq!(
        s.denial(&token(), NOW),
        Some(DenialReason::RegistrationSuperseded)
    );
    node.close().await;
}

#[tokio::test]
async fn expired_registration_and_tombstones_cannot_fall_back_to_old_jwt() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let s = store(&path.0, &doc);
    let mut r = registration(1, NodeRegistrationStatus::Active);
    r.expires_at = Some(NOW + 1);
    write(&doc, &r).await;
    s.refresh_node(&doc, &peer()).await.unwrap();
    assert_eq!(
        s.select_evidence(&peer(), Some(&token()), NOW + 1)
            .unwrap_err(),
        RecordError::Denied(DenialReason::InvalidValidity)
    );
    doc.del(hex::encode(root()), node_registration_key(&peer()))
        .await
        .unwrap();
    assert_eq!(
        s.refresh_node(&doc, &peer()).await.unwrap(),
        RecordAvailability::Absent
    );
    assert_eq!(
        s.select_evidence(&peer(), Some(&token()), NOW + 1)
            .unwrap_err(),
        RecordError::Denied(DenialReason::InvalidValidity)
    );
    r.revision = 2;
    r.issued_at = NOW + 1;
    r.expires_at = None;
    write(&doc, &r).await;
    assert_eq!(
        s.refresh_node(&doc, &peer()).await.unwrap_err(),
        RecordError::ReauthorizationRequired
    );
    r.revision = 3;
    r.reauthorizes_revision = Some(1);
    write(&doc, &r).await;
    s.refresh_node(&doc, &peer()).await.unwrap();
    assert!(s.select_evidence(&peer(), None, NOW + 1).is_ok());
    node.close().await;
}

#[tokio::test]
async fn credential_revoke_does_not_revoke_independent_registration_and_handler_cannot_override() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let s = store(&path.0, &doc);
    let status = CredentialStatusV1 {
        version: 1,
        issuer: hex::encode(root()),
        credential_id: "credential-0000001".into(),
        revision: 1,
        issued_at: NOW - 1,
        revoked: true,
        reauthorizes_revision: None,
    };
    doc.set_bytes_durable(
        hex::encode(root()),
        credential_status_key(&status.credential_id),
        serde_json::to_vec(&status).unwrap(),
    )
    .await
    .unwrap();
    s.refresh_credential(&doc, "credential-0000001")
        .await
        .unwrap();
    let decision = admit(
        &token(),
        &s,
        &AdmissionContext::default(),
        &PolicyExpressionV1::Predicate(PolicyPredicate::TolerateStalePolicy),
        Duration::from_secs(1),
        || NOW,
    )
    .await;
    assert_eq!(
        decision,
        AdmissionDecision::Deny(DenialReason::CredentialRevoked)
    );
    write(&doc, &registration(1, NodeRegistrationStatus::Active)).await;
    s.refresh(&doc).await.unwrap();
    let e = s.select_evidence(&peer(), Some(&token()), NOW).unwrap();
    assert_eq!(s.denial(&e, NOW), None);
    drop(s);
    let s = store(&path.0, &doc);
    assert_eq!(s.denial(&e, NOW), Some(DenialReason::MissingEvidence));
    s.refresh_node(&doc, &peer()).await.unwrap();
    assert_eq!(s.denial(&e, NOW), None);
    node.close().await;
}

#[tokio::test]
async fn rejects_conflicting_revisions_unknown_mandatory_and_other_namespace() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let s = store(&path.0, &doc);
    write(&doc, &registration(1, NodeRegistrationStatus::Active)).await;
    s.refresh_node(&doc, &peer()).await.unwrap();
    let mut conflict = registration(1, NodeRegistrationStatus::Revoked);
    write(&doc, &conflict).await;
    assert_eq!(
        s.refresh_node(&doc, &peer()).await.unwrap_err(),
        RecordError::ConflictingRevision
    );
    conflict.revision = 2;
    conflict.mandatory.push("region".into());
    write(&doc, &conflict).await;
    assert_eq!(
        s.refresh_node(&doc, &peer()).await.unwrap_err(),
        RecordError::UnsupportedMandatoryRestriction
    );
    let other = node.docs_client().create().await.unwrap();
    assert_eq!(
        s.refresh_node(&other, &peer()).await.unwrap_err(),
        RecordError::WrongNamespace
    );
    assert!(
        EnrollmentPolicyStore::open(&path.0, root(), *doc.doc.id().as_bytes(), "fleet").is_err()
    );
    node.close().await;
}

#[test]
fn cidr_only_uses_trusted_direct_observation_and_missing_cannot_be_negated() {
    let e = token();
    let expr = PolicyExpressionV1::NoneOf(vec![PolicyExpressionV1::Predicate(
        PolicyPredicate::SourceCidr("10.0.0.0/8".parse().unwrap()),
    )]);
    let mut c = AdmissionContext::default();
    assert_eq!(expr.evaluate(&e, &c, NOW), Ok(MatchState::Unknown));
    for provenance in [
        SourceAddressProvenance::Relay,
        SourceAddressProvenance::PeerReported,
    ] {
        c.observed_source = Some(ObservedSourceAddress {
            address: "10.1.2.3".parse().unwrap(),
            provenance,
        });
        assert_eq!(expr.evaluate(&e, &c, NOW), Ok(MatchState::Unknown));
    }
    c.observed_source = Some(ObservedSourceAddress {
        address: "10.1.2.3".parse().unwrap(),
        provenance: SourceAddressProvenance::DirectPeer,
    });
    assert_eq!(expr.evaluate(&e, &c, NOW), Ok(MatchState::NoMatch));
    assert!("10.0.0.0/33".parse::<SourceCidr>().is_err());
    assert!("::/0"
        .parse::<SourceCidr>()
        .unwrap()
        .contains("fe80::1".parse().unwrap()));
}

#[tokio::test]
async fn future_registration_is_cached_but_cannot_allow_or_fall_back_before_issuance() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let s = store(&path.0, &doc);
    let mut r = registration(1, NodeRegistrationStatus::Active);
    // Beyond the default 60 s activation leeway; records stay definite.
    r.issued_at = NOW + 61;
    write(&doc, &r).await;
    s.refresh_node(&doc, &peer()).await.unwrap();
    assert_eq!(
        s.select_evidence(&peer(), Some(&token()), NOW).unwrap_err(),
        RecordError::Denied(DenialReason::InvalidValidity)
    );
    assert!(s.select_evidence(&peer(), None, NOW + 1).is_ok());
    node.close().await;
}
#[tokio::test]
async fn credential_denial_restart_and_explicit_reauthorization_are_monotonic() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let s = store(&path.0, &doc);
    let mut status = CredentialStatusV1 {
        version: 1,
        issuer: hex::encode(root()),
        credential_id: "credential-0000001".into(),
        revision: 2,
        issued_at: NOW - 1,
        revoked: true,
        reauthorizes_revision: None,
    };
    doc.set_bytes_durable(
        hex::encode(root()),
        credential_status_key(&status.credential_id),
        serde_json::to_vec(&status).unwrap(),
    )
    .await
    .unwrap();
    s.refresh_credential(&doc, "credential-0000001")
        .await
        .unwrap();
    drop(s);
    let s = store(&path.0, &doc);
    assert_eq!(
        s.select_evidence(&peer(), Some(&token()), NOW).unwrap_err(),
        RecordError::Denied(DenialReason::CredentialRevoked)
    );
    status.revision = 1;
    status.revoked = false;
    doc.set_bytes_durable(
        hex::encode(root()),
        credential_status_key(&status.credential_id),
        serde_json::to_vec(&status).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        s.refresh_credential(&doc, "credential-0000001")
            .await
            .unwrap_err(),
        RecordError::StaleRevision
    );
    status.revision = 3;
    doc.set_bytes_durable(
        hex::encode(root()),
        credential_status_key(&status.credential_id),
        serde_json::to_vec(&status).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        s.refresh_credential(&doc, "credential-0000001")
            .await
            .unwrap_err(),
        RecordError::ReauthorizationRequired
    );
    status.reauthorizes_revision = Some(2);
    doc.set_bytes_durable(
        hex::encode(root()),
        credential_status_key(&status.credential_id),
        serde_json::to_vec(&status).unwrap(),
    )
    .await
    .unwrap();
    s.refresh_credential(&doc, "credential-0000001")
        .await
        .unwrap();
    assert!(s.select_evidence(&peer(), Some(&token()), NOW).is_ok());
    node.close().await;
}

#[tokio::test]
async fn durable_node_revocation_learned_during_handler_overrides_allow() {
    struct Revoke<'a> {
        store: &'a EnrollmentPolicyStore,
        doc: &'a CoreDoc,
    }
    #[async_trait::async_trait]
    impl AdmissionPolicy for Revoke<'_> {
        async fn evaluate(
            &self,
            _: &VerifiedAdmissionEvidence,
            _: &AdmissionContext,
            _: u64,
        ) -> Result<AdmissionDecision, PolicyError> {
            write(self.doc, &registration(1, NodeRegistrationStatus::Revoked)).await;
            self.store
                .refresh_node(self.doc, &peer())
                .await
                .map_err(|e| PolicyError::Handler(e.to_string()))?;
            Ok(AdmissionDecision::Allow)
        }
    }
    let (node, doc) = setup().await;
    let path = Temp::new();
    let s = store(&path.0, &doc);
    assert_eq!(
        admit(
            &token(),
            &s,
            &AdmissionContext::default(),
            &Revoke {
                store: &s,
                doc: &doc
            },
            Duration::from_secs(1),
            || NOW
        )
        .await,
        AdmissionDecision::Deny(DenialReason::NodeRevoked)
    );
    node.close().await;
}
#[tokio::test]
async fn persistence_failure_blocks_every_admission_in_current_process() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let s = store(&path.0, &doc);
    std::fs::remove_file(&path.0).unwrap();
    std::fs::create_dir(&path.0).unwrap();
    write(&doc, &registration(1, NodeRegistrationStatus::Revoked)).await;
    assert!(matches!(
        s.refresh_node(&doc, &peer()).await,
        Err(RecordError::Storage(_))
    ));
    assert_eq!(
        s.denial(&token(), NOW),
        Some(DenialReason::PolicyStateUnavailable)
    );
    assert_eq!(
        s.select_evidence(&peer(), Some(&token()), NOW).unwrap_err(),
        RecordError::Denied(DenialReason::PolicyStateUnavailable)
    );
    node.close().await;
}

#[tokio::test]
async fn fabricated_positive_cache_cannot_create_verified_evidence() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let s = store(&path.0, &doc);
    drop(s);
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path.0).unwrap()).unwrap();
    snapshot["nodes"][hex::encode(peer())] =
        serde_json::to_value(registration(99, NodeRegistrationStatus::Active)).unwrap();
    std::fs::write(&path.0, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    let s = store(&path.0, &doc);
    assert_eq!(
        s.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::MissingEvidence
    );
    assert_eq!(
        s.refresh_node(&doc, &peer()).await.unwrap(),
        RecordAvailability::Absent
    );
    assert_eq!(
        s.select_evidence(&peer(), Some(&token()), NOW).unwrap_err(),
        RecordError::MissingEvidence
    );
    write(&doc, &registration(1, NodeRegistrationStatus::Active)).await;
    assert_eq!(
        s.refresh_node(&doc, &peer()).await.unwrap_err(),
        RecordError::StaleRevision
    );
    assert_eq!(
        s.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::MissingEvidence
    );
    node.close().await;
}
#[tokio::test]
async fn offline_restart_reauthenticates_positive_registration_from_persistent_local_docs() {
    let path = Temp::new();
    let node_path = path.0.parent().unwrap().join("node");
    let config = || CoreEndpointConfig {
        relay_mode: Some("disabled".into()),
        secret_key: Some(vec![7; 32]),
        ..Default::default()
    };
    let node = CoreNode::persistent_with_alpns(
        node_path.to_string_lossy().into_owned(),
        Vec::new(),
        Some(config()),
    )
    .await
    .unwrap();
    let doc = node.docs_client().create().await.unwrap();
    let namespace = doc.doc_id();
    let s = store(&path.0, &doc);
    write(&doc, &registration(1, NodeRegistrationStatus::Active)).await;
    s.refresh_node(&doc, &peer()).await.unwrap();
    assert!(s.select_evidence(&peer(), None, NOW).is_ok());
    drop(s);
    node.close().await;
    drop(doc);
    drop(node);
    let node = CoreNode::persistent_with_alpns(
        node_path.to_string_lossy().into_owned(),
        Vec::new(),
        Some(config()),
    )
    .await
    .unwrap();
    let doc = node.docs_client().open(namespace).await.unwrap().unwrap();
    let s = store(&path.0, &doc);
    assert_eq!(
        s.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::MissingEvidence
    );
    s.refresh_node(&doc, &peer()).await.unwrap();
    assert!(matches!(
        s.select_evidence(&peer(), None, NOW).unwrap().source(),
        AdmissionEvidenceSource::RootAuthoredRecord { revision: 1, .. }
    ));
    node.close().await;
}
#[tokio::test]
async fn future_issued_credential_denial_is_not_dropped() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let s = store(&path.0, &doc);
    let status = CredentialStatusV1 {
        version: 1,
        issuer: hex::encode(root()),
        credential_id: "credential-0000001".into(),
        revision: 1,
        issued_at: NOW + 60,
        revoked: true,
        reauthorizes_revision: None,
    };
    doc.set_bytes_durable(
        hex::encode(root()),
        credential_status_key(&status.credential_id),
        serde_json::to_vec(&status).unwrap(),
    )
    .await
    .unwrap();
    s.refresh_credential(&doc, "credential-0000001")
        .await
        .unwrap();
    assert_eq!(
        s.denial(&token(), NOW),
        Some(DenialReason::CredentialRevoked)
    );
    node.close().await;
}

/// Consumer predicates over the evidence source. `RequireRegistration` is
/// Portal's strict mode: a valid chain alone does not match. `IssuedBy`
/// names a key on a chain's issuer path and never matches a registration.
/// `MaxCredentialAge` bounds `now - issued_at` for either kind.
#[tokio::test]
async fn source_predicates_distinguish_registration_and_chain_evidence() {
    let (node, doc) = setup().await;
    let path = Temp::new();
    let store = store(&path.0, &doc);
    let chain = store.select_evidence(&peer(), Some(&token()), NOW).unwrap();
    write(&doc, &registration(1, NodeRegistrationStatus::Active)).await;
    store.refresh_node(&doc, &peer()).await.unwrap();
    let record = store.select_evidence(&peer(), Some(&token()), NOW).unwrap();
    assert!(matches!(
        record.source(),
        AdmissionEvidenceSource::RootAuthoredRecord { .. }
    ));
    let context = AdmissionContext::default();
    let eval = |predicate: PolicyPredicate, evidence: &VerifiedAdmissionEvidence| {
        PolicyExpressionV1::Predicate(predicate)
            .evaluate(evidence, &context, NOW)
            .unwrap()
    };
    assert_eq!(
        eval(PolicyPredicate::RequireRegistration, &record),
        MatchState::Match
    );
    assert_eq!(
        eval(PolicyPredicate::RequireRegistration, &chain),
        MatchState::NoMatch
    );
    assert_eq!(
        eval(PolicyPredicate::IssuedBy(root()), &chain),
        MatchState::Match
    );
    assert_eq!(
        eval(PolicyPredicate::IssuedBy(peer()), &chain),
        MatchState::NoMatch
    );
    assert_eq!(
        eval(PolicyPredicate::IssuedBy(root()), &record),
        MatchState::NoMatch
    );
    // Both were issued 60 s before NOW.
    for evidence in [&chain, &record] {
        assert_eq!(
            eval(
                PolicyPredicate::MaxCredentialAge(Duration::from_secs(60)),
                evidence
            ),
            MatchState::Match
        );
        assert_eq!(
            eval(
                PolicyPredicate::MaxCredentialAge(Duration::from_secs(59)),
                evidence
            ),
            MatchState::NoMatch
        );
    }
    node.close().await;
}
