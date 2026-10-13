//! Chain enrollment, durable Docs policy and opt-in reciprocal admission.
//! Legacy signing stays separate.
//!
//! A credential is a delegation chain (`crate::delegation`) whose leaf is an
//! enrollment link: one link when the root issues it, more when an online
//! issuer does. Links are compact Ed25519 JWTs (`EdDSA`,
//! `typ = aster-enrollment-v1+jwt` for the leaf), with canonical lowercase hex
//! `iss`/`sub` and a `cnf.jwk` equal to both `sub` and the authenticated peer.
//! Trust comes exclusively from the configured root key, the chain's anchor;
//! headers never select keys or cause network access. Every link requires
//! `aud`, `jti`, `iat`, `exp` and `cnf`, and unknown claims are rejected.
//! NumericDates are nonnegative integral seconds. Activation tolerates a
//! configurable leeway for an issuer clock running ahead (default 60 s, at most
//! 300 s): `iat <= now + leeway`, `nbf <= now + leeway`. Expiry is exact,
//! `now < exp`. Callers provide synchronized, trusted UTC; see
//! [`local_clock_synchronization`].
//!
//! The namespaced `aster_enrollment_v1` extension contains optional complete
//! scope/tag/attribute sets, a required `mandatory` list and a `registration`
//! of `ephemeral` (default) or `persistent`. An issuer may sign a persistent
//! credential only if its delegation has `may_register`. This slice supports
//! no additional mandatory restrictions, so every nonempty list is rejected.
//!
//! [`EnrollmentPolicyStore`] verifies exact-root Docs records and retains
//! revision watermarks and denials across restart. Reopened positive records
//! require local Docs reauthentication; a plaintext snapshot cannot create
//! verified authority. Whole-machine snapshot rollback needs an external trust
//! anchor if the consumer requires protection against it.
//!
//! [`EnrollmentService`] installs explicit, fail-closed reciprocal admission
//! and can synchronize policy through configured online replicas while the root
//! is offline. Consumers choose their admission policy and offline tolerance;
//! operation authorization remains separate. Lower-level callers must obtain
//! peer identities from authenticated transport and supply trustworthy clocks
//! and synchronization observations. Existing built-in protocol sessions need
//! consumer-owned teardown when membership is withdrawn.
//!
//! Lower-level [`admit`] accepts a trusted local [`AdmissionGuard`].
//! [`KnownRevocations`] is a valid monotonic in-memory guard for applications
//! that authenticate and retain their own denial state; it makes no durability
//! or synchronization claim. Applications using Docs policy should pass
//! [`EnrollmentPolicyStore`] instead, as [`EnrollmentService`] always does.
//! The consumer-controlled policy handler cannot override a denial reported by
//! the selected guard or the evidence's signed validity bounds. Choosing an
//! incomplete guard is a trusted host decision, not a peer-selectable bypass.

mod bootstrap;
mod clock;
mod jwt;
mod observations;
mod policy;

pub use bootstrap::{
    AdmissionError, EnrollmentOptions, EnrollmentReadiness, EnrollmentRevoked, EnrollmentService,
    SubjectStanding, ENROLLMENT_ALPN,
};
pub mod issuer;
pub use issuer::{DelegatedIssuer, IssuanceError};
mod records;
mod root_records;
mod root_revisions;
mod rotation;
mod store;
mod stores;

pub(crate) use jwt::canonical_identity;

pub use clock::{local_clock_synchronization, ClockSynchronization};

pub use jwt::{
    AdmissionClaims, AdmissionEvidenceSource, Audience, Confirmation, EnrollmentError,
    EnrollmentExtensionV1, EnrollmentIssueRequest, IssuedEnrollmentCredential, JwtEnrollmentIssuer,
    JwtEnrollmentVerifier, PublicConfirmationKey, Registration, TypedAttribute,
    VerifiedAdmissionEvidence, DEFAULT_ACTIVATION_LEEWAY_SECONDS, ENROLLMENT_JWT_TYPE,
    MAX_ACTIVATION_LEEWAY_SECONDS, MAX_CHAIN_BYTES,
};
pub use policy::{
    admit, AdmissionContext, AdmissionDecision, AdmissionPolicy, DenialReason, KnownRevocations,
    MatchState, PolicyError, PolicyExpressionV1, PolicyPredicate, PolicySynchronization,
    SynchronizationSource, MAX_POLICY_DEPTH, MAX_POLICY_NODES,
};

pub use observations::{
    issue_observation, open_observer_grant, promotion_record, review_observations,
    verify_observation, ObservationError, ObservationKind, ObservationRejection, ObservationReview,
    ObservationV1, ObserverGrant, ReviewedObservation, MAX_OBSERVATION_LIFETIME,
    OBSERVATION_JWT_TYPE,
};
pub use policy::{AdmissionGuard, ObservedSourceAddress, SourceAddressProvenance, SourceCidr};
pub use records::{
    credential_status_key, issuer_status_key, namespace_binding_key, node_registration_key,
    observation_key, BindingStatus, CredentialStatusV1, IssuerStatus, IssuerStatusV1,
    NamespaceBindingV1, NamespacePurpose, NodeRegistrationStatus, NodeRegistrationV1,
    RecordAvailability, MAX_POLICY_RECORDS, MAX_POLICY_RECORD_BYTES, MAX_REGISTRATION_REASON_BYTES,
    OBSERVER_GRANT_APP, OBSERVER_GRANT_PREFIX, OBSERVER_SCOPE, POLICY_RECORD_PREFIX,
    POLICY_RECORD_VERSION,
};
pub use root_records::{
    validate_root_record_json, RootAuthoredDocView, RootRecordError, RootRecordRead,
};
pub use root_revisions::{check_root_revision, should_record_rejection, RootRevisionError};
pub use rotation::{RotationPin, MAX_FORWARDED_ROTATIONS};
pub use store::{EnrollmentPolicyStore, PolicyRefreshIssue, PolicyRefreshReport, RecordError};
pub use stores::PolicyStores;
