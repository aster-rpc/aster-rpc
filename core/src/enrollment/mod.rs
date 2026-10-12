//! JWT enrollment, durable Docs policy and opt-in reciprocal admission.
//! Legacy signing and attestation stay separate.
//!
//! This first profile uses compact signed JWTs (RFC 7519/8725), `EdDSA`
//! (Ed25519), `typ = aster-enrollment-v1+jwt`, and canonical lowercase hex
//! Ed25519 public keys for `iss` and `sub`. `cnf.jwk` (RFC 7800/8037) must be
//! an `OKP`/`Ed25519` public key equal to both `sub` and the authenticated peer.
//! Trust comes exclusively from the configured root key; headers never select
//! keys or cause network access. `aud`, `jti`, `iat` and `cnf` are required.
//! NumericDates are nonnegative integral seconds, with exact `iat <= now`,
//! `nbf <= now < exp` semantics and zero clock skew. V1 offers no configurable
//! iat margin or validity leeway; callers provide synchronized, trusted UTC.
//! Expiry may be omitted.
//!
//! The namespaced `aster_enrollment_v1` extension contains optional complete
//! scope/tag/attribute sets and a required `mandatory` list. This slice supports
//! no additional mandatory restrictions, so every nonempty list is rejected.
//! Unknown optional JWT claims are ignored. JOSE critical extensions are not
//! payload restrictions and are rejected separately.
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
mod jwt;
mod policy;

pub use bootstrap::{
    EnrollmentOptions, EnrollmentReadiness, EnrollmentRevoked, EnrollmentService, ENROLLMENT_ALPN,
};
mod records;
mod root_records;
mod root_revisions;
mod store;

pub(crate) use jwt::canonical_identity;

pub use jwt::{
    AdmissionClaims, AdmissionEvidenceSource, Audience, Confirmation, EnrollmentClaimsV1,
    EnrollmentError, EnrollmentExtensionV1, EnrollmentIssueRequest, IssuedEnrollmentCredential,
    JwtEnrollmentIssuer, JwtEnrollmentVerifier, PublicConfirmationKey, TypedAttribute,
    VerifiedAdmissionEvidence, ENROLLMENT_JWT_TYPE, MAX_JWT_BYTES,
};
pub use policy::{
    admit, AdmissionContext, AdmissionDecision, AdmissionPolicy, DenialReason, KnownRevocations,
    MatchState, PolicyError, PolicyExpressionV1, PolicyPredicate, PolicySynchronization,
    SynchronizationSource, MAX_POLICY_DEPTH, MAX_POLICY_NODES,
};

pub use policy::{AdmissionGuard, ObservedSourceAddress, SourceAddressProvenance, SourceCidr};
pub use records::{
    credential_status_key, node_registration_key, CredentialStatusV1, NodeRegistrationStatus,
    NodeRegistrationV1, RecordAvailability, MAX_POLICY_RECORDS, MAX_POLICY_RECORD_BYTES,
    POLICY_RECORD_PREFIX, POLICY_RECORD_VERSION,
};
pub use root_records::{
    validate_root_record_json, RootAuthoredDocView, RootRecordError, RootRecordRead,
};
pub use root_revisions::{check_root_revision, should_record_rejection, RootRevisionError};
pub use store::{EnrollmentPolicyStore, PolicyRefreshIssue, PolicyRefreshReport, RecordError};
