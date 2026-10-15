//! Enrollment on delegation chains and consumer-selected admission policies.
//!
//! A credential is a delegation chain whose leaf enrolls the node: one link
//! when the root issues it, more when an online issuer does. Long-lived
//! membership is a root-authored [`NodeRegistrationV1`] in the replicated
//! policy namespaces, which replaces a `persistent` credential once the root
//! promotes the node. Evidence is opaque and is created only from a chain that
//! verifies for the authenticated peer, or from an exact-root record. Use
//! [`verify_connection`] to obtain the subject key directly from an Aster
//! authenticated connection, then call [`admit`] with an explicit consumer
//! policy and the consumer's known revocations. Admission is separate from
//! application/operation authorization.
//!
//! [`Enrollment`] installs opt-in restricted-ALPN admission and reciprocal
//! identity verification, keeps policy current, closes every connection of a
//! denied peer, and follows root rotations. Consumers retain their sole custom
//! ALPN accept loop and route enrollment connections into
//! [`Enrollment::accept`]. An online issuer runs `Issuer` (features `rpc`
//! and `docs`), and nodes renew with `Enrollment::start_renewal`.
//!
//! Most applications never call any of this directly: `AsterServer` and
//! `AsterClient` install Enrollment from the `[enrollment]` section of
//! [`AsterConfig`](crate::AsterConfig), serve the handshake, map verified
//! claims to the roles `requires` checks (`RoleMapping`), and renew
//! credentials. The low-level API stays available as an escape hatch.
//!
//! Design and tested invariants: `docs/_internal/working_ideas/aster-enrollment.md`.

pub use aster_transport_core::enrollment::{
    admit, check_root_revision, credential_status_key, issuer_status_key,
    local_clock_synchronization, namespace_binding_key, node_registration_key, observation_key,
    should_record_rejection, validate_root_record_json, AdmissionClaims, AdmissionContext,
    AdmissionDecision, AdmissionError, AdmissionEvidenceSource, AdmissionGuard, AdmissionPolicy,
    Audience, BindingStatus, ClockSynchronization, Confirmation, CredentialStatusV1, DenialReason,
    EnrollmentError, EnrollmentExtensionV1, EnrollmentIssueRequest, EnrollmentOptions,
    EnrollmentPolicyStore, EnrollmentReadiness, EnrollmentRevoked, IssuedEnrollmentCredential,
    IssuerStatus, IssuerStatusV1, JwtEnrollmentIssuer, JwtEnrollmentVerifier, KnownRevocations,
    MatchState, NamespaceBindingV1, NamespacePurpose, NodeRegistrationStatus, NodeRegistrationV1,
    ObservedSourceAddress, PolicyError, PolicyExpressionV1, PolicyPredicate, PolicyRefreshIssue,
    PolicyRefreshReport, PolicySynchronization, PublicConfirmationKey, RecordAvailability,
    RecordError, Registration, RootRecordError, RootRecordRead, RootRevisionError,
    SourceAddressProvenance, SourceCidr, SynchronizationSource, TypedAttribute,
    VerifiedAdmissionEvidence, DEFAULT_ACTIVATION_LEEWAY_SECONDS, ENROLLMENT_ALPN,
    ENROLLMENT_JWT_TYPE, MAX_ACTIVATION_LEEWAY_SECONDS, MAX_CHAIN_BYTES, MAX_POLICY_DEPTH,
    MAX_POLICY_NODES, MAX_POLICY_RECORDS, MAX_POLICY_RECORD_BYTES, MAX_REGISTRATION_REASON_BYTES,
    OBSERVER_GRANT_APP, OBSERVER_GRANT_PREFIX, OBSERVER_SCOPE, POLICY_RECORD_PREFIX,
    POLICY_RECORD_VERSION,
};
pub use aster_transport_core::enrollment::{
    issue_observation, promotion_record, verify_observation, ObservationError, ObservationKind,
    ObservationRejection, ObservationReview, ObservationV1, ObserverGrant, PolicyStores,
    ReviewedObservation, MAX_OBSERVATION_LIFETIME, OBSERVATION_JWT_TYPE,
};
pub use aster_transport_core::enrollment::{AcceptedRecords, RotationPin, MAX_FORWARDED_ROTATIONS};
pub use aster_transport_core::enrollment::{
    DelegatedIssuer, IssuanceError, ReplicaAttachment, SubjectStanding,
};

#[cfg(all(feature = "rpc", feature = "docs"))]
pub mod issuer;
#[cfg(all(feature = "rpc", feature = "docs"))]
pub use issuer::{
    renew_from, Issuer, IssuerErrorCodeV1, IssuerErrorV1, RenewRequestV1, RenewalConfig,
    RenewalHandle, RenewalOutcome, RenewedV1, DEFAULT_ISSUED_LIFETIME, ISSUER_SERVICE,
    ISSUER_SERVICE_VERSION,
};

#[cfg(all(feature = "rpc", feature = "docs"))]
pub(crate) mod runtime;
#[cfg(all(feature = "rpc", feature = "docs"))]
pub use runtime::{PolicyConfig, RoleMapping};

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

/// Refresh a policy view from local documents: the fleet store from `fleet`,
/// then the stores for every membership namespace it binds from the matching
/// document in `membership` (others are ignored). For root-side tools such as
/// review; an installed [`Enrollment`] refreshes itself. Returns whether any
/// namespace left content pending or records rejected, so the view may be
/// incomplete.
#[cfg(feature = "docs")]
pub async fn refresh_policy(
    stores: &PolicyStores,
    fleet: &crate::docs::Doc,
    membership: &[crate::docs::Doc],
) -> anyhow::Result<bool> {
    let mut uncertain = stores
        .fleet()
        .refresh(&fleet.enrollment_core())
        .await?
        .has_uncertainty();
    stores.sync_bindings()?;
    for doc in membership {
        let core = doc.enrollment_core();
        let namespace: [u8; 32] = *core.doc.id().as_bytes();
        if let Some(store) = stores.membership(&namespace) {
            uncertain |= store.refresh(&core).await?.has_uncertainty();
        }
    }
    Ok(uncertain)
}

/// Review the observations namespace as the root: see
/// [`aster_transport_core::enrollment::review_observations`]. Observations
/// confer no authority; only accepted ones are candidates for promotion.
#[cfg(feature = "docs")]
pub async fn review_observations(
    observations: &crate::docs::Doc,
    fleet: &crate::docs::Doc,
    verifier: &JwtEnrollmentVerifier,
    stores: &PolicyStores,
    now: u64,
) -> anyhow::Result<ObservationReview> {
    aster_transport_core::enrollment::review_observations(
        &observations.enrollment_core(),
        &fleet.enrollment_core(),
        verifier,
        stores,
        now,
    )
    .await
}

/// Open the root's sealed grant of the observations namespace's write
/// capability to `observer`, from the fleet document. An issuer process uses
/// this to start writing `issued` observations.
#[cfg(feature = "docs")]
pub async fn open_observer_grant(
    fleet: &crate::docs::Doc,
    root: &[u8; 32],
    observer: &crate::SecretKey,
    observations: &[u8; 32],
) -> anyhow::Result<ObserverGrant> {
    aster_transport_core::enrollment::open_observer_grant(
        &fleet.enrollment_core(),
        root,
        &observer.to_bytes(),
        observations,
    )
    .await
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

    /// [`Self::install`] on a node whose hook receiver a language binding
    /// already drains: the binding forwards both hook directions into
    /// `hooks`. For `aster::binding_support` hosts only.
    #[cfg(feature = "binding-support")]
    #[doc(hidden)]
    pub fn install_with_hooks(
        node: &crate::Node,
        hooks: aster_transport_core::CoreHookReceiver,
        store: Arc<EnrollmentPolicyStore>,
        verifier: JwtEnrollmentVerifier,
        credential: Option<String>,
        policy: Arc<dyn AdmissionPolicy>,
        options: EnrollmentOptions,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner: EnrollmentService::install_with_hooks(
                node.enrollment_core(),
                hooks,
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

    /// Hold the fleet namespace from the configured policy `replicas` and keep
    /// it synchronized with them. A configured replica this node has no record
    /// for yet is trusted for synchronizing the fleet namespace only; see
    /// `EnrollmentService::attach_replicas` in the transport core. This is
    /// what `AsterServer` runs for `[enrollment] replicas`.
    #[cfg(feature = "docs")]
    pub async fn attach_replicas(
        &self,
        replicas: Vec<crate::NodeAddr>,
        timeout: Duration,
    ) -> anyhow::Result<ReplicaAttachment> {
        self.inner
            .attach_replicas(
                replicas.into_iter().map(|addr| addr.to_core()).collect(),
                timeout,
            )
            .await
    }

    /// What the root's records, as this node last reconciled them, say about
    /// `subject`.
    pub fn subject_standing(&self, subject: &[u8; 32]) -> SubjectStanding {
        self.inner.subject_standing(subject)
    }

    #[cfg(feature = "docs")]
    pub fn set_policy_document(&self, document: &crate::docs::Doc) -> anyhow::Result<()> {
        self.inner.set_policy_document(document.enrollment_core())
    }

    /// Attach a document for a namespace the fleet binds (for example one the
    /// root or a replica holds). Membership registrations in it take part in
    /// admission, and joining nodes whose verified scopes intersect the
    /// binding's `read_scopes` receive a read ticket for it.
    #[cfg(feature = "docs")]
    pub fn add_policy_document(
        &self,
        document: &crate::docs::Doc,
    ) -> anyhow::Result<NamespaceBindingV1> {
        self.inner.add_policy_document(document.enrollment_core())
    }

    /// Documents this node holds for currently bound namespaces of `purpose`,
    /// such as the Exec-grant namespaces it received tickets for.
    #[cfg(feature = "docs")]
    pub fn policy_documents(
        &self,
        purpose: NamespacePurpose,
    ) -> Vec<(NamespaceBindingV1, crate::docs::Doc)> {
        self.inner
            .policy_documents(purpose)
            .into_iter()
            .map(|(binding, doc)| {
                (
                    binding,
                    crate::docs::Doc::from_enrollment_core(
                        doc,
                        self.node.enrollment_core().docs_client(),
                    ),
                )
            })
            .collect()
    }

    /// Sync the fleet namespace from `peer`, then every bound namespace it
    /// offered a read ticket for at `connect`. Returns the fleet document;
    /// bound ones are available from [`Self::policy_documents`].
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

    /// The verified evidence (subject, issuer path, claims, validity) that
    /// `peer`'s current admission session rests on, for mapping claims to
    /// application roles. `None` without a session. It reflects the last
    /// admission or check; call [`Self::check_peer`] first to re-evaluate.
    pub fn peer_evidence(&self, peer: &[u8; 32]) -> Option<VerifiedAdmissionEvidence> {
        self.inner.peer_evidence(peer)
    }

    /// [`Self::peer_evidence`] with the session's lineage: the same while the
    /// session is renewed in place, new once it ended (revocation, eviction,
    /// expiry) and the peer was admitted again.
    pub fn peer_session(&self, peer: &[u8; 32]) -> Option<(u64, VerifiedAdmissionEvidence)> {
        self.inner.peer_session(peer)
    }

    /// This node's current credential chain and its verified evidence.
    pub fn credential(&self) -> Option<(String, VerifiedAdmissionEvidence)> {
        self.inner.credential()
    }

    /// The fleet audience this node's verifier and stores pin.
    pub fn audience(&self) -> &str {
        self.inner.audience()
    }

    /// The declared-locality source for
    /// [`TopologySwarmOptions::locality`](crate::topology::TopologySwarmOptions::locality):
    /// publishes this node's locality statement (never its credential) and
    /// verifies each peer's for that peer's endpoint, as a credential would
    /// be verified, falling back to the root's registration record for a
    /// node that publishes none.
    ///
    /// The statement is a locality link carrying only the region and zone,
    /// which the root or a delegated issuer signs into the credential
    /// whenever its claims carry the reserved `aster.locality.region` /
    /// `aster.locality.zone` attributes, so it is renewed with the
    /// credential and expires with it.
    pub fn topology_locality(&self) -> crate::topology::LocalitySource {
        let own = self.inner.clone();
        let verify = self.inner.clone();
        crate::topology::LocalitySource {
            own_statement: Arc::new(move || own.locality_statement()),
            verify: Arc::new(move |node_hex: &str, statement: &str| {
                let endpoint: [u8; 32] = hex::decode(node_hex).ok()?.try_into().ok()?;
                verify.declared_locality(&endpoint, statement)
            }),
        }
    }

    /// Verify `chain` as a credential for `subject` the way admission would:
    /// the trusted root and audience, signed validity, and the root's
    /// delegate revocations. Root records are not consulted.
    pub fn verify_credential(
        &self,
        subject: &[u8; 32],
        chain: &str,
    ) -> Result<VerifiedAdmissionEvidence, EnrollmentError> {
        self.inner.verify_credential(subject, chain)
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

    /// Apply root rotation statements (for example one an operator received
    /// out of band) that continue this node's pinned root. Requires
    /// [`EnrollmentOptions::rotation`]. Peers also deliver them during the
    /// enrollment handshake.
    pub fn apply_root_rotations(&self, statements: &[String]) -> anyhow::Result<usize> {
        self.inner.apply_root_rotations(statements)
    }

    /// Replace this node's credential, for example with one the new root
    /// issued after a rotation, and present it again to every peer this node
    /// has a session with. Returns the peers that could not be reached.
    pub async fn replace_credential(
        &self,
        chain: String,
    ) -> anyhow::Result<Vec<([u8; 32], String)>> {
        self.inner.replace_credential(chain).await
    }

    /// The root key this node trusts now; after a rotation, the new root.
    pub fn trusted_root(&self) -> [u8; 32] {
        self.inner.trusted_root()
    }

    /// The fleet namespace pinned to [`Self::trusted_root`].
    pub fn fleet_namespace(&self) -> [u8; 32] {
        self.inner.fleet_namespace()
    }

    /// The root this node trusts now, with its next-root commitment and
    /// rotation count, when rotation is configured.
    pub fn root_state(&self) -> Option<crate::delegation::RootState> {
        self.inner.root_state()
    }

    /// The rotation statements this node has applied, oldest first.
    pub fn root_rotations(&self) -> Vec<String> {
        self.inner.root_rotations()
    }

    pub fn revocations(&self) -> tokio::sync::broadcast::Receiver<EnrollmentRevoked> {
        self.inner.revocations()
    }

    pub async fn reconcile(&self) {
        self.inner.reconcile().await;
    }
}

/// Delegates the root has revoked in the fleet namespace (`IssuerStatusV1`),
/// for consumers that verify their own delegation chains with
/// [`crate::delegation::verify_chain`].
impl crate::delegation::RevokedDelegates for Enrollment {
    fn is_revoked(&self, key: &[u8; 32]) -> bool {
        self.inner.delegate_revoked(key)
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
