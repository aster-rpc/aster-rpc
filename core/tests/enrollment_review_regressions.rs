#![cfg(feature = "enrollment")]
//! Regressions from the 2026-10-12 enrollment review.
use aster_transport_core::{enrollment::*, CoreDoc, CoreEndpointConfig, CoreNode};
use ed25519_dalek::SigningKey;
use std::time::Duration;

const NOW: u64 = 1_800_000_000;

fn root() -> [u8; 32] {
    SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes()
}
fn subject(seed: u8) -> [u8; 32] {
    SigningKey::from_bytes(&[seed; 32])
        .verifying_key()
        .to_bytes()
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
fn registration(
    seed: u8,
    revision: u64,
    status: NodeRegistrationStatus,
    audience: &str,
) -> NodeRegistrationV1 {
    NodeRegistrationV1 {
        version: 1,
        issuer: hex::encode(root()),
        subject: hex::encode(subject(seed)),
        audience: Audience::One(audience.into()),
        revision,
        issued_at: NOW - 100,
        not_before: None,
        expires_at: None,
        status,
        claims: AdmissionClaims::default(),
        mandatory: vec![],
        reauthorizes_revision: None,
        reason: None,
    }
}
async fn write(doc: &CoreDoc, record: &NodeRegistrationV1) {
    let key: [u8; 32] = hex::decode(&record.subject).unwrap().try_into().unwrap();
    doc.set_bytes_durable(
        hex::encode(root()),
        node_registration_key(&key),
        serde_json::to_vec(record).unwrap(),
    )
    .await
    .unwrap();
}
struct Temp(std::path::PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "aster-enrollment-review-{}-{}",
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
fn store(dir: &Temp, doc: &CoreDoc) -> EnrollmentPolicyStore {
    EnrollmentPolicyStore::open(
        dir.0.join("policy.json"),
        root(),
        *doc.doc.id().as_bytes(),
        "fleet",
    )
    .unwrap()
}

/// A renewal the root published while revision 1 was still valid is an
/// ordinary update, not a reauthorization. Whether it is accepted must not
/// depend on when an offline observer happens to sync it.
#[tokio::test]
async fn renewal_issued_before_expiry_is_accepted_when_observed_after_expiry() {
    let (node, doc) = setup().await;
    let dir = Temp::new();
    let s = store(&dir, &doc);
    let mut first = registration(8, 1, NodeRegistrationStatus::Active, "fleet");
    first.expires_at = Some(NOW + 10);
    write(&doc, &first).await;
    s.refresh_node(&doc, &subject(8)).await.unwrap();

    // Root renews at NOW - 50, well before revision 1 expires.
    let mut renewal = registration(8, 2, NodeRegistrationStatus::Active, "fleet");
    renewal.issued_at = NOW - 50;
    renewal.expires_at = Some(NOW + 1000);
    write(&doc, &renewal).await;

    // The observer was offline and only sees the renewal after NOW + 10.
    let later = NOW + 20;
    s.refresh_node(&doc, &subject(8))
        .await
        .expect("renewal issued before expiry must not require reauthorization");
    let evidence = s.select_evidence(&subject(8), None, later).unwrap();
    assert!(matches!(
        evidence.source(),
        AdmissionEvidenceSource::RootAuthoredRecord { revision: 2, .. }
    ));
    node.close().await;
}

/// A root-authored record this store cannot use (here: one scoped to another
/// audience sharing the policy namespace) must not stop the bulk refresh from
/// applying other records, notably revocations.
#[tokio::test]
async fn unusable_record_does_not_block_bulk_refresh_of_revocations() {
    let (node, doc) = setup().await;
    let dir = Temp::new();
    let s = store(&dir, &doc);
    let target = 8;
    write(
        &doc,
        &registration(target, 1, NodeRegistrationStatus::Active, "fleet"),
    )
    .await;
    s.refresh(&doc).await.unwrap();
    assert!(s.select_evidence(&subject(target), None, NOW).is_ok());

    // An entry for another service, keyed so it sorts before the target.
    let other = (9..=255u8)
        .find(|seed| hex::encode(subject(*seed)) < hex::encode(subject(target)))
        .unwrap();
    write(
        &doc,
        &registration(other, 1, NodeRegistrationStatus::Active, "other-service"),
    )
    .await;
    write(
        &doc,
        &registration(target, 2, NodeRegistrationStatus::Revoked, "fleet"),
    )
    .await;

    let refreshed = s.refresh(&doc).await;
    assert_eq!(
        s.select_evidence(&subject(target), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::NodeRevoked),
        "revocation not applied; refresh returned {refreshed:?}"
    );
    assert!(refreshed.is_ok(), "bulk refresh failed: {refreshed:?}");
    let report = refreshed.unwrap();
    assert_eq!(report.ignored, 1);
    assert!(!report.has_uncertainty());
    node.close().await;
}

/// Stale and rejected reauthorization leaves do not suppress an independent
/// node revocation later in the same root-authored batch.
#[tokio::test]
async fn stale_and_reauthorization_errors_do_not_hide_later_revocations() {
    let (node, doc) = setup().await;
    let dir = Temp::new();
    let s = store(&dir, &doc);
    let mut seeds = [8, 9, 10];
    seeds.sort_by_key(|seed| hex::encode(subject(*seed)));
    let stale = seeds[0];
    let denied = seeds[1];
    let target = seeds[2];
    for (seed, status) in [
        (stale, NodeRegistrationStatus::Active),
        (denied, NodeRegistrationStatus::Revoked),
        (target, NodeRegistrationStatus::Active),
    ] {
        write(&doc, &registration(seed, 2, status, "fleet")).await;
    }
    s.refresh(&doc).await.unwrap();
    write(
        &doc,
        &registration(stale, 1, NodeRegistrationStatus::Active, "fleet"),
    )
    .await;
    write(
        &doc,
        &registration(denied, 3, NodeRegistrationStatus::Active, "fleet"),
    )
    .await;
    write(
        &doc,
        &registration(target, 3, NodeRegistrationStatus::Revoked, "fleet"),
    )
    .await;
    let report = s.refresh(&doc).await.unwrap();
    assert_eq!(report.rejected.len(), 2);
    assert!(report.has_uncertainty());
    assert!(report
        .rejected
        .iter()
        .any(|issue| issue.error == RecordError::StaleRevision));
    assert!(report
        .rejected
        .iter()
        .any(|issue| issue.error == RecordError::ReauthorizationRequired));
    assert_eq!(
        s.select_evidence(&subject(target), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::NodeRevoked)
    );
    assert_eq!(
        s.select_evidence(&subject(denied), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::RegistrationRejected)
    );
    node.close().await;
}

#[tokio::test]
async fn bulk_commit_handles_many_records_and_unchanged_refresh_does_not_write() {
    let (node, doc) = setup().await;
    let dir = Temp::new();
    let s = store(&dir, &doc);
    for seed in 8..=135 {
        write(
            &doc,
            &registration(seed, 1, NodeRegistrationStatus::Active, "fleet"),
        )
        .await;
    }
    let report = s.refresh(&doc).await.unwrap();
    assert_eq!(report.available, 128);
    assert!(!report.has_uncertainty());
    for seed in 8..=135 {
        assert!(s.select_evidence(&subject(seed), None, NOW).is_ok());
    }
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.0.join("policy.json")).unwrap()).unwrap();
    assert_eq!(snapshot["nodes"].as_object().unwrap().len(), 128);
    let before = std::fs::metadata(dir.0.join("policy.json")).unwrap();
    s.refresh_node(&doc, &subject(8)).await.unwrap();
    s.refresh(&doc).await.unwrap();
    let after = std::fs::metadata(dir.0.join("policy.json")).unwrap();
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            before.ino(),
            after.ino(),
            "unchanged refresh replaced the snapshot"
        );
    }
    node.close().await;
}

/// Bulk refresh stages all independent changes before its single durable
/// commit. A commit failure publishes none of the staged positive state and
/// poisons the store so a learned denial cannot be bypassed in this process.
#[tokio::test]
async fn bulk_persistence_failure_publishes_no_partial_state_and_fails_closed() {
    let (node, doc) = setup().await;
    let mut dir = Temp::new();
    let s = store(&dir, &doc);
    write(
        &doc,
        &registration(8, 1, NodeRegistrationStatus::Active, "fleet"),
    )
    .await;
    s.refresh(&doc).await.unwrap();
    let cached = s.select_evidence(&subject(8), None, NOW).unwrap();
    let original = std::fs::read(dir.0.join("policy.json")).unwrap();
    write(
        &doc,
        &registration(8, 2, NodeRegistrationStatus::Revoked, "fleet"),
    )
    .await;
    for seed in 9..=136 {
        write(
            &doc,
            &registration(seed, 1, NodeRegistrationStatus::Active, "fleet"),
        )
        .await;
    }
    // The Docs memory store remains available, but the final snapshot path no
    // longer exists. This makes commit fail after the entire batch was read.
    let moved = dir.0.with_extension("moved");
    std::fs::rename(&dir.0, &moved).unwrap();
    dir.0 = moved;
    assert!(matches!(
        s.refresh(&doc).await,
        Err(RecordError::Storage(_))
    ));
    assert_eq!(s.observed_node_revision(&subject(8)), Some(1));
    for seed in 9..=136 {
        assert_eq!(s.observed_node_revision(&subject(seed)), None);
    }
    assert_eq!(
        s.denial(&cached, NOW),
        Some(DenialReason::PolicyStateUnavailable)
    );
    assert_eq!(
        s.select_evidence(&subject(9), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::PolicyStateUnavailable)
    );
    assert_eq!(std::fs::read(dir.0.join("policy.json")).unwrap(), original);

    // Once storage is writable again, the next full refresh re-derives every
    // record, commits it, and lifts the fail-closed state.
    let restored = dir.0.with_extension("");
    std::fs::rename(&dir.0, &restored).unwrap();
    dir.0 = restored;
    s.refresh(&doc).await.unwrap();
    assert_eq!(
        s.select_evidence(&subject(8), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::NodeRevoked)
    );
    assert!(s.select_evidence(&subject(9), None, NOW).is_ok());
    assert_ne!(std::fs::read(dir.0.join("policy.json")).unwrap(), original);
    node.close().await;
}

#[tokio::test]
async fn renewal_issued_after_expiry_requires_explicit_reauthorization_even_if_seen_early() {
    let (node, doc) = setup().await;
    let dir = Temp::new();
    let s = store(&dir, &doc);
    let mut first = registration(8, 1, NodeRegistrationStatus::Active, "fleet");
    first.expires_at = Some(NOW + 10);
    write(&doc, &first).await;
    s.refresh_node(&doc, &subject(8)).await.unwrap();
    let mut renewal = registration(8, 2, NodeRegistrationStatus::Active, "fleet");
    renewal.issued_at = NOW + 10;
    write(&doc, &renewal).await;
    assert_eq!(
        s.refresh_node(&doc, &subject(8)).await.unwrap_err(),
        RecordError::ReauthorizationRequired
    );
    renewal.revision = 3;
    renewal.reauthorizes_revision = Some(1);
    write(&doc, &renewal).await;
    s.refresh_node(&doc, &subject(8)).await.unwrap();
    assert!(s.select_evidence(&subject(8), None, NOW + 10).is_ok());
    node.close().await;
}

#[tokio::test]
async fn pending_content_reports_uncertainty_without_hiding_later_revocation() {
    let (node, doc) = setup().await;
    let dir = Temp::new();
    let s = store(&dir, &doc);
    let mut seeds = [8, 9];
    seeds.sort_by_key(|seed| hex::encode(subject(*seed)));
    let pending = seeds[0];
    let target = seeds[1];
    for seed in seeds {
        write(
            &doc,
            &registration(seed, 1, NodeRegistrationStatus::Active, "fleet"),
        )
        .await;
    }
    s.refresh(&doc).await.unwrap();
    let bytes = serde_json::to_vec(&registration(
        pending,
        2,
        NodeRegistrationStatus::Active,
        "fleet",
    ))
    .unwrap();
    doc.doc
        .set_hash(
            iroh_docs::AuthorId::from(root()),
            node_registration_key(&subject(pending)),
            iroh_blobs::Hash::new(&bytes),
            bytes.len() as u64,
        )
        .await
        .unwrap();
    write(
        &doc,
        &registration(target, 2, NodeRegistrationStatus::Revoked, "fleet"),
    )
    .await;
    let report = s.refresh(&doc).await.unwrap();
    assert_eq!(report.pending, 1);
    assert!(report.has_uncertainty());
    assert!(
        s.select_evidence(&subject(pending), None, NOW).is_ok(),
        "pending content must preserve verified cached evidence"
    );
    assert_eq!(
        s.select_evidence(&subject(target), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::NodeRevoked)
    );
    node.close().await;
}

/// A newer authenticated statement with unsupported mandatory restrictions
/// cannot be bypassed using the previously valid cached registration.
#[tokio::test]
async fn unsupported_newer_policy_blocks_cached_grants_across_restart_and_old_replay() {
    let (node, doc) = setup().await;
    let dir = Temp::new();
    let s = store(&dir, &doc);
    write(
        &doc,
        &registration(8, 1, NodeRegistrationStatus::Active, "fleet"),
    )
    .await;
    s.refresh(&doc).await.unwrap();
    let cached = s.select_evidence(&subject(8), None, NOW).unwrap();
    let mut restricted = registration(8, 2, NodeRegistrationStatus::Active, "fleet");
    restricted
        .mandatory
        .push("unimplemented-region-check".into());
    write(&doc, &restricted).await;
    let report = s.refresh(&doc).await.unwrap();
    assert!(report.has_uncertainty());
    assert_eq!(
        report.rejected[0].error,
        RecordError::UnsupportedMandatoryRestriction
    );
    assert_eq!(
        s.select_evidence(&subject(8), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::RegistrationRejected)
    );
    let policy = PolicyExpressionV1::Predicate(PolicyPredicate::TolerateStalePolicy);
    assert_eq!(
        admit(
            &cached,
            &s,
            &AdmissionContext::default(),
            &policy,
            Duration::from_secs(1),
            || NOW
        )
        .await,
        AdmissionDecision::Deny(DenialReason::RegistrationRejected)
    );
    drop(s);
    let s = store(&dir, &doc);
    assert_eq!(
        s.denial(&cached, NOW),
        Some(DenialReason::RegistrationRejected)
    );
    for revision in [1, 2] {
        write(
            &doc,
            &registration(8, revision, NodeRegistrationStatus::Active, "fleet"),
        )
        .await;
        assert_eq!(
            s.refresh_node(&doc, &subject(8)).await.unwrap_err(),
            RecordError::StaleRevision
        );
        assert_eq!(
            s.denial(&cached, NOW),
            Some(DenialReason::RegistrationRejected)
        );
    }
    write(
        &doc,
        &registration(8, 3, NodeRegistrationStatus::Active, "fleet"),
    )
    .await;
    s.refresh_node(&doc, &subject(8)).await.unwrap();
    assert!(s.select_evidence(&subject(8), None, NOW).is_ok());
    node.close().await;
}

#[tokio::test]
async fn individual_conflicting_revision_blocks_old_grant_until_higher_root_statement() {
    let (node, doc) = setup().await;
    let dir = Temp::new();
    let s = store(&dir, &doc);
    write(
        &doc,
        &registration(8, 1, NodeRegistrationStatus::Active, "fleet"),
    )
    .await;
    s.refresh_node(&doc, &subject(8)).await.unwrap();
    let cached = s.select_evidence(&subject(8), None, NOW).unwrap();
    let mut conflicting = registration(8, 1, NodeRegistrationStatus::Active, "fleet");
    conflicting.claims.tags = Some(std::collections::BTreeSet::from(["quarantine".into()]));
    write(&doc, &conflicting).await;
    assert_eq!(
        s.refresh_node(&doc, &subject(8)).await.unwrap_err(),
        RecordError::ConflictingRevision
    );
    assert_eq!(
        s.denial(&cached, NOW),
        Some(DenialReason::RegistrationRejected)
    );
    write(
        &doc,
        &registration(9, 1, NodeRegistrationStatus::Active, "fleet"),
    )
    .await;
    s.refresh_node(&doc, &subject(9)).await.unwrap();
    assert!(
        s.select_evidence(&subject(9), None, NOW).is_ok(),
        "one rejected subject must not poison the fleet"
    );
    write(
        &doc,
        &registration(8, 2, NodeRegistrationStatus::Active, "fleet"),
    )
    .await;
    s.refresh_node(&doc, &subject(8)).await.unwrap();
    assert!(s.select_evidence(&subject(8), None, NOW).is_ok());
    node.close().await;
}

#[tokio::test]
async fn unrelated_future_schema_is_ignored_without_poisoning_fleet_freshness() {
    let (node, doc) = setup().await;
    let dir = Temp::new();
    let s = store(&dir, &doc);
    let mut unrelated = serde_json::to_value(registration(
        9,
        1,
        NodeRegistrationStatus::Active,
        "other-service",
    ))
    .unwrap();
    unrelated["version"] = serde_json::json!(2);
    unrelated.as_object_mut().unwrap().remove("claims");
    unrelated["future_policy_schema"] = serde_json::json!({"new_shape":true});
    doc.set_bytes_durable(
        hex::encode(root()),
        node_registration_key(&subject(9)),
        serde_json::to_vec(&unrelated).unwrap(),
    )
    .await
    .unwrap();
    write(
        &doc,
        &registration(8, 1, NodeRegistrationStatus::Active, "fleet"),
    )
    .await;
    let report = s.refresh(&doc).await.unwrap();
    assert_eq!(report.ignored, 1);
    assert!(!report.has_uncertainty());
    assert!(s.select_evidence(&subject(8), None, NOW).is_ok());
    assert_eq!(s.rejected_node_revision(&subject(9)), None);
    node.close().await;
}
