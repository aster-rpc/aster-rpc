//! Online issuance under a root delegation (enrollment §4, B11).
//!
//! An issuer is a node holding a delegation chain from the root. It enrolls
//! new nodes with short-lived credentials while the root is offline, and
//! renews them on request. It refuses any subject the root has denied, and
//! nothing it signs can exceed its delegation: every credential is checked
//! against the issuer's chain at signing and verified, as admission would,
//! before it is returned. Signing uses the issuer node's own key, so its
//! `issued` observations name the key that signed the leaf.

use super::{
    AdmissionClaims, AdmissionEvidenceSource, DenialReason, EnrollmentError,
    EnrollmentIssueRequest, EnrollmentService, IssuedEnrollmentCredential, JwtEnrollmentIssuer,
    Registration, SubjectStanding,
};
use crate::delegation::{delegation_limits, Chain, ChainError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuanceError {
    /// The root has denied the subject; it must not be enrolled or renewed.
    SubjectDenied(DenialReason),
    /// A registration admits the subject; it needs no credential renewal.
    Registered,
    /// Root policy for the subject could not be evaluated right now.
    PolicyUnavailable,
    /// The presented credential was not signed by this issuer.
    ForeignCredential,
    /// The root has revoked this issuer, or a delegate above it.
    IssuerRevoked,
    /// The credential could not be issued or does not verify.
    Credential(EnrollmentError),
}

impl std::fmt::Display for IssuanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "enrollment issuance refused: {self:?}")
    }
}
impl std::error::Error for IssuanceError {}

impl From<EnrollmentError> for IssuanceError {
    fn from(error: EnrollmentError) -> Self {
        match error {
            EnrollmentError::Chain(ChainError::Revoked) => Self::IssuerRevoked,
            other => Self::Credential(other),
        }
    }
}

/// Signs leaves under this node's delegation chain from the root.
pub struct DelegatedIssuer {
    signer: JwtEnrollmentIssuer,
    delegation: Chain,
    audience: String,
    expires_at: u64,
    max_child_lifetime: u64,
    lifetime: u64,
}

impl DelegatedIssuer {
    /// `secret` is the issuer node's key; `delegation` its chain from the
    /// root, whose leaf names that key. New credentials last `lifetime`
    /// seconds, shortened to fit the delegation.
    pub fn new(
        secret: &[u8; 32],
        delegation: Chain,
        audience: impl Into<String>,
        lifetime: u64,
    ) -> Result<Self, EnrollmentError> {
        let signer = JwtEnrollmentIssuer::from_seed(secret);
        let limits = delegation_limits(&delegation).ok_or(EnrollmentError::InvalidClaims)?;
        if limits.subject != signer.public_key() || lifetime == 0 {
            return Err(EnrollmentError::InvalidConfiguration);
        }
        Ok(Self {
            signer,
            delegation,
            audience: audience.into(),
            expires_at: limits.expires_at,
            max_child_lifetime: limits.max_child_lifetime,
            lifetime,
        })
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signer.public_key()
    }

    /// When this issuer's own delegation expires; only the root renews it.
    pub fn delegation_expires_at(&self) -> u64 {
        self.expires_at
    }

    fn sign(
        &self,
        subject: &[u8; 32],
        claims: AdmissionClaims,
        registration: Registration,
        now: u64,
    ) -> Result<IssuedEnrollmentCredential, EnrollmentError> {
        let expires_at = (now + self.lifetime.min(self.max_child_lifetime)).min(self.expires_at);
        let mut request = EnrollmentIssueRequest::new(*subject, self.audience.clone(), expires_at);
        request.claims = claims;
        request.registration = registration;
        self.signer.issue_delegated(&request, &self.delegation, now)
    }
}

/// Enroll `subject` (orchestrator provisioning). Refuses a subject the
/// root has denied or already registered, and returns only a credential
/// that verifies here as admission would. A persistent credential is
/// observed for the root when this node is an observer.
pub fn issue_for(
    service: &EnrollmentService,
    issuer: &DelegatedIssuer,
    subject: &[u8; 32],
    claims: AdmissionClaims,
    registration: Registration,
    now: u64,
) -> Result<IssuedEnrollmentCredential, IssuanceError> {
    issue(service, issuer, subject, claims, registration, now, false)
}

/// Renew the credential an admitted `caller` presents: same claims and
/// registration, new expiry. Only credentials this issuer signed are
/// renewed, and only while they still verify.
pub fn renew(
    service: &EnrollmentService,
    issuer: &DelegatedIssuer,
    caller: &[u8; 32],
    chain: &str,
    now: u64,
) -> Result<IssuedEnrollmentCredential, IssuanceError> {
    let current = service.verify_credential(caller, chain)?;
    let AdmissionEvidenceSource::PresentedChain { issuers, .. } = current.source() else {
        return Err(IssuanceError::ForeignCredential);
    };
    if issuers.first() != Some(&issuer.public_key()) {
        return Err(IssuanceError::ForeignCredential);
    }
    let registration = current.registration().unwrap_or_default();
    issue(
        service,
        issuer,
        caller,
        current.claims().clone(),
        registration,
        now,
        true,
    )
}

fn issue(
    service: &EnrollmentService,
    issuer: &DelegatedIssuer,
    subject: &[u8; 32],
    claims: AdmissionClaims,
    registration: Registration,
    now: u64,
    renewal: bool,
) -> Result<IssuedEnrollmentCredential, IssuanceError> {
    match service.subject_standing(subject) {
        SubjectStanding::Denied(reason) => return Err(IssuanceError::SubjectDenied(reason)),
        SubjectStanding::Registered => return Err(IssuanceError::Registered),
        SubjectStanding::Unknown(_) => return Err(IssuanceError::PolicyUnavailable),
        SubjectStanding::Unregistered => {}
    }
    let issued = issuer.sign(subject, claims, registration, now)?;
    // Gate G6 here catches a revoked issuer before anything is handed out.
    service.verify_credential(subject, issued.chain())?;
    if registration == Registration::Persistent {
        service.observe_issuance(subject, issued.chain(), renewal);
    }
    Ok(issued)
}
