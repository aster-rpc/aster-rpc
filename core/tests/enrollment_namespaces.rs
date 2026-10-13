#![cfg(feature = "enrollment")]
//! Multiple policy namespaces (enrollment B9) at the store: INV-E3 for
//! bindings, INV-E9, binding retirement and restart.
use aster_transport_core::{enrollment::*, CoreDoc, CoreEndpointConfig, CoreNode};
use ed25519_dalek::SigningKey;
use std::collections::BTreeSet;
use std::sync::Arc;
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
fn peer() -> [u8; 32] {
    public(8)
}

/// A private temporary directory, removed on drop.
struct Temp(std::path::PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "aster-enrollment-namespaces-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn file(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fleet {
    node: CoreNode,
    fleet: CoreDoc,
    build: CoreDoc,
    deploy: CoreDoc,
}
async fn setup() -> Fleet {
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
    let docs = node.docs_client();
    let fleet = docs.create().await.unwrap();
    let build = docs.create().await.unwrap();
    let deploy = docs.create().await.unwrap();
    Fleet {
        node,
        fleet,
        build,
        deploy,
    }
}
fn id(doc: &CoreDoc) -> [u8; 32] {
    *doc.doc.id().as_bytes()
}
async fn put(doc: &CoreDoc, key: Vec<u8>, value: Vec<u8>) {
    doc.set_bytes_durable(hex::encode(root()), key, value)
        .await
        .unwrap();
}
fn binding(namespace: &CoreDoc, scope: &str, revision: u64) -> NamespaceBindingV1 {
    NamespaceBindingV1 {
        version: 1,
        issuer: hex::encode(root()),
        namespace: hex::encode(id(namespace)),
        purpose: NamespacePurpose::Membership,
        read_scopes: BTreeSet::from([scope.to_owned()]),
        write_scopes: BTreeSet::new(),
        revision,
        status: BindingStatus::Active,
    }
}
async fn bind(fleet: &CoreDoc, record: &NamespaceBindingV1) {
    let namespace: [u8; 32] = hex::decode(&record.namespace).unwrap().try_into().unwrap();
    put(
        fleet,
        namespace_binding_key(&namespace),
        serde_json::to_vec(record).unwrap(),
    )
    .await;
}
fn registration(
    revision: u64,
    status: NodeRegistrationStatus,
    reauthorizes_revision: Option<u64>,
) -> NodeRegistrationV1 {
    NodeRegistrationV1 {
        version: 1,
        issuer: hex::encode(root()),
        subject: hex::encode(peer()),
        audience: Audience::One(FLEET.into()),
        revision,
        issued_at: NOW - 60,
        not_before: None,
        expires_at: None,
        status,
        claims: AdmissionClaims::default(),
        mandatory: vec![],
        reauthorizes_revision,
    }
}
async fn register(doc: &CoreDoc, record: &NodeRegistrationV1) {
    put(
        doc,
        node_registration_key(&peer()),
        serde_json::to_vec(record).unwrap(),
    )
    .await;
}
/// Refresh the fleet, open bound stores, then refresh each bound store.
async fn refresh(stores: &PolicyStores, fleet: &Fleet) {
    stores.fleet().refresh(&fleet.fleet).await.unwrap();
    stores.sync_bindings().unwrap();
    for doc in [&fleet.build, &fleet.deploy] {
        if let Some(store) = stores.membership(&id(doc)) {
            let _ = store.refresh(doc).await;
        }
    }
}
fn open(temp: &Temp, fleet: &Fleet) -> PolicyStores {
    PolicyStores::new(Arc::new(
        EnrollmentPolicyStore::open(temp.file("fleet.json"), root(), id(&fleet.fleet), FLEET)
            .unwrap(),
    ))
}

#[tokio::test]
async fn inv_e9_highest_revision_wins_and_a_denial_wins_a_tie_across_namespaces() {
    let temp = Temp::new();
    let fleet = setup().await;
    let stores = open(&temp, &fleet);
    bind(&fleet.fleet, &binding(&fleet.build, "build", 1)).await;

    // Active in the membership namespace: admitted on that record.
    register(
        &fleet.build,
        &registration(1, NodeRegistrationStatus::Active, None),
    )
    .await;
    refresh(&stores, &fleet).await;
    let selected = stores.select_evidence(&peer(), None, NOW).unwrap();
    assert_eq!(
        selected.source(),
        AdmissionEvidenceSource::RootAuthoredRecord {
            document_id: id(&fleet.build),
            revision: 1,
        }
    );
    assert_eq!(stores.denial(&selected, NOW), None);

    // A denial mirrored into the fleet at the same revision wins the tie.
    register(
        &fleet.fleet,
        &registration(1, NodeRegistrationStatus::Revoked, None),
    )
    .await;
    refresh(&stores, &fleet).await;
    assert_eq!(
        stores.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::NodeRevoked)
    );
    assert_eq!(
        stores.denial(&selected, NOW),
        Some(DenialReason::NodeRevoked)
    );

    // A newer active record that does not name the denial cannot outrank it.
    register(
        &fleet.build,
        &registration(2, NodeRegistrationStatus::Active, None),
    )
    .await;
    refresh(&stores, &fleet).await;
    assert_eq!(
        stores.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::NodeRevoked)
    );

    // Naming the denied revision reauthorizes across namespaces.
    register(
        &fleet.build,
        &registration(3, NodeRegistrationStatus::Active, Some(1)),
    )
    .await;
    refresh(&stores, &fleet).await;
    let selected = stores.select_evidence(&peer(), None, NOW).unwrap();
    assert_eq!(
        selected.source(),
        AdmissionEvidenceSource::RootAuthoredRecord {
            document_id: id(&fleet.build),
            revision: 3,
        }
    );

    // A higher-revision denial anywhere beats a lower-revision active one.
    register(
        &fleet.fleet,
        &registration(4, NodeRegistrationStatus::Revoked, None),
    )
    .await;
    refresh(&stores, &fleet).await;
    assert_eq!(
        stores.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::NodeRevoked)
    );
    assert_eq!(
        stores.denial(&selected, NOW),
        Some(DenialReason::NodeRevoked)
    );
}

#[tokio::test]
async fn inv_e3_only_namespaces_the_root_binds_confer_authority() {
    let temp = Temp::new();
    let fleet = setup().await;
    let stores = open(&temp, &fleet);
    register(
        &fleet.deploy,
        &registration(1, NodeRegistrationStatus::Active, None),
    )
    .await;
    // A binding written by another author is invisible.
    let author = fleet.node.docs_client().create_author().await.unwrap();
    fleet
        .fleet
        .set_bytes_durable(
            author,
            namespace_binding_key(&id(&fleet.deploy)),
            serde_json::to_vec(&binding(&fleet.deploy, "deploy", 1)).unwrap(),
        )
        .await
        .unwrap();
    // A binding inside a membership namespace binds nothing.
    bind(&fleet.build, &binding(&fleet.deploy, "deploy", 1)).await;
    refresh(&stores, &fleet).await;
    assert!(stores.fleet().bindings().is_empty());
    assert!(stores.membership(&id(&fleet.deploy)).is_none());
    assert_eq!(
        stores.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::MissingEvidence
    );
    // Once the root binds it, its registrations count.
    bind(&fleet.fleet, &binding(&fleet.deploy, "deploy", 1)).await;
    refresh(&stores, &fleet).await;
    assert!(stores.select_evidence(&peer(), None, NOW).is_ok());
    // A fleet-only record inside a membership namespace is ignored there.
    let report = stores
        .membership(&id(&fleet.deploy))
        .unwrap()
        .refresh(&fleet.deploy)
        .await
        .unwrap();
    assert!(!report.has_uncertainty());
}

#[tokio::test]
async fn retired_binding_withdraws_authority_and_cannot_be_reactivated() {
    let temp = Temp::new();
    let fleet = setup().await;
    let stores = open(&temp, &fleet);
    bind(&fleet.fleet, &binding(&fleet.build, "build", 1)).await;
    register(
        &fleet.build,
        &registration(1, NodeRegistrationStatus::Active, None),
    )
    .await;
    refresh(&stores, &fleet).await;
    assert!(stores.select_evidence(&peer(), None, NOW).is_ok());
    let mut retired = binding(&fleet.build, "build", 2);
    retired.status = BindingStatus::Retired;
    bind(&fleet.fleet, &retired).await;
    refresh(&stores, &fleet).await;
    assert!(stores.membership(&id(&fleet.build)).is_none());
    assert_eq!(
        stores.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::MissingEvidence
    );
    // Retirement is final for the namespace id.
    bind(&fleet.fleet, &binding(&fleet.build, "build", 3)).await;
    let report = stores.fleet().refresh(&fleet.fleet).await.unwrap();
    assert_eq!(report.rejected.len(), 1);
    assert_eq!(
        report.rejected[0].error,
        RecordError::ReauthorizationRequired
    );
    assert!(stores.fleet().bindings().is_empty());
}

#[tokio::test]
async fn bound_denials_survive_restart_before_any_resync() {
    let temp = Temp::new();
    let fleet = setup().await;
    {
        let stores = open(&temp, &fleet);
        bind(&fleet.fleet, &binding(&fleet.build, "build", 1)).await;
        register(
            &fleet.build,
            &registration(2, NodeRegistrationStatus::Revoked, None),
        )
        .await;
        refresh(&stores, &fleet).await;
        assert_eq!(
            stores.select_evidence(&peer(), None, NOW).unwrap_err(),
            RecordError::Denied(DenialReason::NodeRevoked)
        );
    }
    // Reopen with no Docs refresh at all: the persisted binding reopens the
    // bound store, whose durable denial applies at once.
    let stores = open(&temp, &fleet);
    stores.sync_bindings().unwrap();
    assert_eq!(
        stores.select_evidence(&peer(), None, NOW).unwrap_err(),
        RecordError::Denied(DenialReason::NodeRevoked)
    );
}

#[tokio::test]
async fn binding_records_are_validated() {
    let temp = Temp::new();
    let fleet = setup().await;
    let stores = open(&temp, &fleet);
    // write_scopes are only for the observations namespace.
    let mut membership_writers = binding(&fleet.build, "build", 1);
    membership_writers.write_scopes = BTreeSet::from(["observer".into()]);
    bind(&fleet.fleet, &membership_writers).await;
    // The fleet cannot bind itself.
    let mut itself = binding(&fleet.fleet, "build", 1);
    itself.namespace = hex::encode(id(&fleet.fleet));
    bind(&fleet.fleet, &itself).await;
    let report = stores.fleet().refresh(&fleet.fleet).await.unwrap();
    assert_eq!(report.rejected.len(), 2);
    assert!(stores.fleet().bindings().is_empty());
    let mut observations = binding(&fleet.deploy, "root", 1);
    observations.purpose = NamespacePurpose::Observations;
    observations.write_scopes = BTreeSet::from([OBSERVER_SCOPE.into()]);
    bind(&fleet.fleet, &observations).await;
    stores.fleet().refresh(&fleet.fleet).await.unwrap();
    assert_eq!(stores.fleet().bindings(), vec![observations]);
    // Observations bindings never open a membership store.
    stores.sync_bindings().unwrap();
    assert!(stores.membership(&id(&fleet.deploy)).is_none());
}
