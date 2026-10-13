//! Enrollment on delegation chains and consumer-selected admission policies.
//!
//! A credential is a delegation chain whose leaf enrolls the node: one link
//! when the root issues it, more when an online issuer does. Core evidence is
//! opaque and is created only after the whole chain verifies for the peer. Use [`verify_connection`] to obtain the subject key directly
//! from an Aster authenticated connection, then call [`admit`] with an explicit
//! consumer policy and the consumer's known revocations. Admission is separate
//! from application/operation authorization.
//!
//! [`Enrollment`] installs opt-in restricted-ALPN admission and reciprocal
//! identity verification. Consumers retain their sole custom ALPN accept loop
//! and route enrollment connections into [`Enrollment::accept`].

pub use aster_transport_core::enrollment::{
    admit, check_root_revision, credential_status_key, local_clock_synchronization,
    node_registration_key, should_record_rejection, validate_root_record_json, AdmissionClaims,
    AdmissionContext, AdmissionDecision, AdmissionError, AdmissionEvidenceSource, AdmissionGuard,
    AdmissionPolicy, Audience, ClockSynchronization, Confirmation, CredentialStatusV1,
    DenialReason, EnrollmentError, EnrollmentExtensionV1, EnrollmentIssueRequest,
    EnrollmentOptions, EnrollmentPolicyStore, EnrollmentReadiness, EnrollmentRevoked,
    IssuedEnrollmentCredential, JwtEnrollmentIssuer, JwtEnrollmentVerifier, KnownRevocations,
    MatchState, NodeRegistrationStatus, NodeRegistrationV1, ObservedSourceAddress, PolicyError,
    PolicyExpressionV1, PolicyPredicate, PolicyRefreshIssue, PolicyRefreshReport,
    PolicySynchronization, PublicConfirmationKey, RecordAvailability, RecordError, Registration,
    RootRecordError, RootRecordRead, RootRevisionError, SourceAddressProvenance, SourceCidr,
    SynchronizationSource, TypedAttribute, VerifiedAdmissionEvidence,
    DEFAULT_ACTIVATION_LEEWAY_SECONDS, ENROLLMENT_ALPN, ENROLLMENT_JWT_TYPE,
    MAX_ACTIVATION_LEEWAY_SECONDS, MAX_CHAIN_BYTES, MAX_POLICY_DEPTH, MAX_POLICY_NODES,
    MAX_POLICY_RECORDS, MAX_POLICY_RECORD_BYTES, POLICY_RECORD_PREFIX, POLICY_RECORD_VERSION,
};

use aster_transport_core::enrollment::{EnrollmentService, RootAuthoredDocView};

use std::sync::Arc;
use std::time::Duration;

/// Issue a one-link credential as the root.
pub fn issue_credential(
    root: &crate::SecretKey,
    request: &EnrollmentIssueRequest,
    issued_at: u64,
) -> Result<IssuedEnrollmentCredential, EnrollmentError> {
    JwtEnrollmentIssuer::from_seed(&root.to_bytes()).issue(request, issued_at)
}

/// Issue a credential as a delegated issuer holding `delegation`, its own
/// chain from the root. Refuses anything the delegation does not allow.
pub fn issue_delegated_credential(
    issuer: &crate::SecretKey,
    delegation: &crate::delegation::Chain,
    request: &EnrollmentIssueRequest,
    issued_at: u64,
) -> Result<IssuedEnrollmentCredential, EnrollmentError> {
    JwtEnrollmentIssuer::from_seed(&issuer.to_bytes())
        .issue_delegated(request, delegation, issued_at)
}

/// Reads the records one root author wrote in a Docs namespace, within size
/// and count bounds. Other authors' entries under the same keys are ignored.
#[cfg(feature = "docs")]
#[derive(Clone)]
pub struct RootAuthoredReader(RootAuthoredDocView);

#[cfg(feature = "docs")]
impl RootAuthoredReader {
    pub fn new(
        namespace: [u8; 32],
        root: [u8; 32],
        max_record_bytes: u64,
        max_records: usize,
    ) -> Self {
        Self(RootAuthoredDocView::new(
            namespace,
            root,
            max_record_bytes,
            max_records,
        ))
    }

    /// The root author's record at `key`: available, pending download, or
    /// absent.
    pub async fn read_exact(
        &self,
        doc: &crate::docs::Doc,
        key: Vec<u8>,
    ) -> Result<RootRecordRead, RootRecordError> {
        self.0.read_exact(&doc.enrollment_core(), key).await
    }
}

/// Cloneable facade over shared opt-in enrollment state.
#[derive(Clone)]
pub struct Enrollment {
    inner: Arc<EnrollmentService>,
    #[cfg(feature = "docs")]
    node: crate::Node,
}

impl Enrollment {
    /// Start with hooks enabled and [`ENROLLMENT_ALPN`] registered. An explicit
    /// consumer policy is required; admission never grants operation access.
    pub fn install(
        node: &crate::Node,
        store: Arc<EnrollmentPolicyStore>,
        verifier: JwtEnrollmentVerifier,
        credential: Option<String>,
        policy: Arc<dyn AdmissionPolicy>,
        options: EnrollmentOptions,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner: EnrollmentService::install(
                node.enrollment_core(),
                store,
                verifier,
                credential,
                policy,
                options,
            )?,
            #[cfg(feature = "docs")]
            node: node.clone(),
        })
    }

    pub async fn accept(
        &self,
        connection: &crate::Connection,
    ) -> anyhow::Result<EnrollmentReadiness> {
        self.inner.accept(&connection.enrollment_core()).await
    }

    pub async fn connect(
        &self,
        replica: crate::NodeAddr,
    ) -> anyhow::Result<(EnrollmentReadiness, Option<String>)> {
        self.inner.connect(replica.to_core()).await
    }

    pub async fn connect_replicas(
        &self,
        replicas: Vec<crate::NodeAddr>,
    ) -> anyhow::Result<(EnrollmentReadiness, Option<String>)> {
        self.inner
            .connect_replicas(replicas.into_iter().map(|addr| addr.to_core()).collect())
            .await
    }

    #[cfg(feature = "docs")]
    pub fn set_policy_document(&self, document: &crate::docs::Doc) -> anyhow::Result<()> {
        self.inner.set_policy_document(document.enrollment_core())
    }

    #[cfg(feature = "docs")]
    pub async fn synchronize(
        &self,
        peer: [u8; 32],
        ticket: String,
        timeout: Duration,
    ) -> anyhow::Result<crate::docs::Doc> {
        Ok(crate::docs::Doc::from_enrollment_core(
            self.inner.synchronize(peer, ticket, timeout).await?,
            self.node.enrollment_core().docs_client(),
        ))
    }

    /// Recheck an admitted peer. Only [`AdmissionError::Denied`] revokes.
    pub async fn check_peer(&self, peer: &[u8; 32]) -> Result<EnrollmentReadiness, AdmissionError> {
        self.inner.check_peer(peer).await
    }

    pub async fn check_connection(
        &self,
        connection: &crate::Connection,
    ) -> Result<EnrollmentReadiness, AdmissionError> {
        self.inner
            .check_connection(&connection.enrollment_core())
            .await
    }

    pub async fn check_connection_with_policy(
        &self,
        connection: &crate::Connection,
        policy: &dyn AdmissionPolicy,
        timeout: Duration,
    ) -> Result<EnrollmentReadiness, AdmissionError> {
        self.inner
            .check_connection_with_policy(&connection.enrollment_core(), policy, timeout)
            .await
    }

    pub async fn track_connection(&self, connection: &crate::Connection) -> anyhow::Result<()> {
        self.inner
            .track_connection(connection.enrollment_core())
            .await
    }

    pub fn revocations(&self) -> tokio::sync::broadcast::Receiver<EnrollmentRevoked> {
        self.inner.revocations()
    }

    pub async fn reconcile(&self) {
        self.inner.reconcile().await;
    }
}

/// Verify a presented chain against the actual authenticated connecting peer.
/// The peer identity is read from the connection rather than request metadata.
/// `now` is trusted UTC time in integral seconds.
pub fn verify_connection(
    verifier: &JwtEnrollmentVerifier,
    token: &str,
    connection: &crate::Connection,
    now: u64,
) -> Result<VerifiedAdmissionEvidence, EnrollmentError> {
    let peer: [u8; 32] = hex::decode(connection.peer().as_str())
        .map_err(|_| EnrollmentError::InvalidIdentity)?
        .try_into()
        .map_err(|_| EnrollmentError::InvalidIdentity)?;
    verifier.verify(token, &peer, now)
}
