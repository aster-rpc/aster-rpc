use std::collections::BTreeMap;
use std::fmt;

use crate::delegation::{
    self, claimed_start, issue_enrollment, leaf_jti, verify_chain, Chain, ChainError, LeafClaims,
    LeafKind, NoRevocations, RevokedDelegates, TrustContext, Validity, VerifiedChain,
};
// The claim types are shared with delegation chains, where an enrollment
// credential is the leaf of a chain.
pub use crate::delegation::{
    AdmissionClaims, Audience, Confirmation, EnrollmentExtensionV1, PublicConfirmationKey,
    Registration, TypedAttribute, ENROLLMENT_LINK_TYPE as ENROLLMENT_JWT_TYPE, MAX_CHAIN_BYTES,
};

/// Default tolerance for a signer's clock running ahead of the verifier.
/// It applies only to activation (`iat`, `nbf`); expiry is never extended.
pub const DEFAULT_ACTIVATION_LEEWAY_SECONDS: u64 = crate::delegation::DEFAULT_LEEWAY_SECONDS;
pub const MAX_ACTIVATION_LEEWAY_SECONDS: u64 = crate::delegation::MAX_LEEWAY_SECONDS;

/// Signed validity of one statement at a verifier's trusted time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Activation {
    Active,
    /// `iat` or `nbf` is later than `now + leeway`; the statement starts
    /// `offset_seconds` after `now`.
    Early {
        offset_seconds: i64,
    },
    Expired,
}

/// `iat <= now + leeway`, `nbf <= now + leeway` and `now < exp`. Expiry is
/// checked first so leeway can never revive an expired statement.
pub(super) fn activation(
    issued_at: u64,
    not_before: Option<u64>,
    expires_at: Option<u64>,
    now: u64,
    leeway: u64,
) -> Activation {
    if expires_at.is_some_and(|exp| now >= exp) {
        return Activation::Expired;
    }
    let starts = not_before.map_or(issued_at, |nbf| nbf.max(issued_at));
    if starts > now.saturating_add(leeway) {
        return Activation::Early {
            offset_seconds: i64::try_from(starts - now).unwrap_or(i64::MAX),
        };
    }
    Activation::Active
}

pub(super) fn valid_leeway(seconds: u64) -> bool {
    seconds <= MAX_ACTIVATION_LEEWAY_SECONDS
}

impl AdmissionClaims {
    /// Parse an attribute file before authorized issuance. The input is bounded
    /// to one chain link, must be an object of strictly typed values, and may
    /// not contain duplicate keys at any depth. Issuance separately checks the
    /// size of the complete link, so a parsed map can still exceed that limit.
    pub fn parse_attributes_json(
        bytes: &[u8],
    ) -> Result<BTreeMap<String, TypedAttribute>, EnrollmentError> {
        if bytes.len() > delegation::MAX_LINK_BYTES {
            return Err(EnrollmentError::TokenTooLarge);
        }
        validate_unique_json(bytes).map_err(|_| EnrollmentError::InvalidClaims)?;
        serde_json::from_slice(bytes).map_err(|_| EnrollmentError::InvalidClaims)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionEvidenceSource {
    /// A delegation chain the peer presented, verified for its key.
    PresentedChain {
        /// The pinned root the chain is anchored at.
        anchor: [u8; 32],
        /// Each link's issuer, leaf first; the last is the anchor. A
        /// root-issued credential has exactly one.
        issuers: Vec<[u8; 32]>,
        /// `BLAKE3("aster.chain.v1\0" || text)`.
        digest: [u8; 32],
    },
    RootAuthoredRecord {
        document_id: [u8; 32],
        revision: u64,
    },
}

/// Only the exact-root Docs verification path may supply registration data.
/// This is deliberately not a public conversion from caller-asserted records.
pub(super) struct RegistrationEvidenceData {
    pub issuer: [u8; 32],
    pub subject: [u8; 32],
    pub audience: Audience,
    pub issued_at: u64,
    pub not_before: Option<u64>,
    pub expires_at: Option<u64>,
    pub claims: AdmissionClaims,
    pub document_id: [u8; 32],
    pub revision: u64,
}

/// Immutable verified claims. Only chain verification or the pinned-root
/// Docs path can construct this type; deserializing copied claims cannot
/// manufacture verified evidence.
#[derive(Debug, Clone)]
pub struct VerifiedAdmissionEvidence {
    /// The trusted root: the chain's anchor, or the record's author.
    pub(super) issuer: [u8; 32],
    pub(super) subject: [u8; 32],
    claims: AdmissionClaims,
    audience: Audience,
    issued_at: u64,
    not_before: Option<u64>,
    expires_at: Option<u64>,
    credential_id: Option<String>,
    presented_chain: Option<String>,
    registration: Option<Registration>,
    source: AdmissionEvidenceSource,
    /// The verifier's activation leeway, so later rechecks use the same rule.
    activation_leeway: u64,
}

impl VerifiedAdmissionEvidence {
    /// The trusted root this evidence is anchored at. For a delegated
    /// credential the issuer path is in [`Self::source`].
    pub fn issuer(&self) -> &[u8; 32] {
        &self.issuer
    }
    pub fn subject(&self) -> &[u8; 32] {
        &self.subject
    }
    /// The leaf link's `jti`. Registration is a separate statement and has
    /// no credential id.
    pub fn credential_id(&self) -> Option<&str> {
        self.credential_id.as_deref()
    }
    pub fn claims(&self) -> &AdmissionClaims {
        &self.claims
    }
    pub fn audience(&self) -> &Audience {
        &self.audience
    }
    /// The leaf's `iat`; ancestors were issued no later.
    pub fn issued_at(&self) -> u64 {
        self.issued_at
    }
    /// The earliest expiry of any link, or the record's expiry.
    pub fn expires_at(&self) -> Option<u64> {
        self.expires_at
    }
    pub fn not_before(&self) -> Option<u64> {
        self.not_before
    }
    pub fn source(&self) -> AdmissionEvidenceSource {
        self.source.clone()
    }
    /// What a presented credential asks for. `None` for a registration
    /// record, which is itself the long-lived authority.
    pub fn registration(&self) -> Option<Registration> {
        self.registration
    }
    /// Preserve the signed artifact and provenance, never re-sign mapped claims.
    pub fn presented_chain(&self) -> Option<&str> {
        self.presented_chain.as_deref()
    }

    pub(super) fn activation_at(&self, now: u64) -> Activation {
        activation(
            self.issued_at,
            self.not_before,
            self.expires_at,
            now,
            self.activation_leeway,
        )
    }

    pub(super) fn from_registration(
        data: RegistrationEvidenceData,
        activation_leeway: u64,
    ) -> Result<Self, EnrollmentError> {
        validate_key(&data.issuer)?;
        validate_key(&data.subject)?;
        validate_evidence_shape(
            &data.audience,
            data.issued_at,
            data.not_before,
            data.expires_at,
            &data.claims,
        )?;
        Ok(Self {
            issuer: data.issuer,
            subject: data.subject,
            claims: data.claims,
            audience: data.audience,
            issued_at: data.issued_at,
            not_before: data.not_before,
            expires_at: data.expires_at,
            credential_id: None,
            presented_chain: None,
            registration: None,
            source: AdmissionEvidenceSource::RootAuthoredRecord {
                document_id: data.document_id,
                revision: data.revision,
            },
            activation_leeway,
        })
    }

    fn from_chain(
        verified: VerifiedChain,
        text: &str,
        activation_leeway: u64,
    ) -> Result<Self, EnrollmentError> {
        let LeafClaims::Enrollment(extension) = verified.leaf() else {
            return Err(EnrollmentError::InvalidJwt);
        };
        validate_evidence_shape(
            verified.audience(),
            verified.issued_at(),
            verified.not_before(),
            Some(verified.not_after()),
            &extension.claims,
        )?;
        Ok(Self {
            issuer: *verified.anchor(),
            subject: *verified.subject(),
            claims: extension.claims.clone(),
            audience: verified.audience().clone(),
            issued_at: verified.issued_at(),
            not_before: verified.not_before(),
            expires_at: Some(verified.not_after()),
            credential_id: verified.jtis().first().cloned(),
            presented_chain: Some(text.to_owned()),
            registration: Some(extension.registration),
            source: AdmissionEvidenceSource::PresentedChain {
                anchor: *verified.anchor(),
                issuers: verified.issuers().to_vec(),
                digest: *verified.digest(),
            },
            activation_leeway,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollmentError {
    InvalidConfiguration,
    TokenTooLarge,
    InvalidJwt,
    UnsupportedHeader,
    InvalidIdentity,
    PeerKeyMismatch,
    InvalidValidity,
    /// Valid except that `iat`/`nbf` is beyond the activation leeway: the
    /// issuer's clock is ahead of this verifier's by about `offset_seconds`.
    ClockSkew {
        offset_seconds: i64,
    },
    UnsupportedMandatoryRestriction,
    InvalidClaims,
    /// A chain failed a structural, attenuation or revocation gate; the
    /// variant names the gate.
    Chain(ChainError),
    SigningFailed,
}

impl fmt::Display for EnrollmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "enrollment verification failed: {self:?}")
    }
}
impl std::error::Error for EnrollmentError {}

impl From<ChainError> for EnrollmentError {
    fn from(error: ChainError) -> Self {
        match error {
            ChainError::Size => Self::TokenTooLarge,
            ChainError::Malformed | ChainError::Signature => Self::InvalidJwt,
            ChainError::Header => Self::UnsupportedHeader,
            ChainError::LeafMismatch => Self::PeerKeyMismatch,
            ChainError::NotYetValid | ChainError::Expired | ChainError::Lifetime => {
                Self::InvalidValidity
            }
            ChainError::UnsupportedRestriction => Self::UnsupportedMandatoryRestriction,
            ChainError::Issuance => Self::SigningFailed,
            ChainError::Configuration => Self::InvalidConfiguration,
            other => Self::Chain(other),
        }
    }
}

/// Authorized issuance inputs. The issuer derives its identity, the
/// confirmation key, algorithm, token type and a random `jti` rather than
/// accepting them from caller-provided claims. Every credential expires.
#[derive(Debug, Clone)]
pub struct EnrollmentIssueRequest {
    pub subject: [u8; 32],
    pub audience: Audience,
    pub not_before: Option<u64>,
    pub expires_at: u64,
    pub claims: AdmissionClaims,
    pub registration: Registration,
}

impl EnrollmentIssueRequest {
    /// An ephemeral credential with no claims, valid until `expires_at`.
    pub fn new(subject: [u8; 32], audience: impl Into<String>, expires_at: u64) -> Self {
        Self {
            subject,
            audience: Audience::One(audience.into()),
            not_before: None,
            expires_at,
            claims: AdmissionClaims::default(),
            registration: Registration::Ephemeral,
        }
    }
}

/// The public signed chain and its leaf identifier, containing no secret.
#[derive(Debug, Clone)]
pub struct IssuedEnrollmentCredential {
    chain: Chain,
    credential_id: String,
}

impl IssuedEnrollmentCredential {
    /// The chain's text form, as presented in the enrollment handshake.
    pub fn chain(&self) -> &str {
        self.chain.to_text()
    }
    /// The leaf link's `jti`.
    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }
    pub fn into_chain(self) -> String {
        self.chain.into_text()
    }
}

/// An authorized local signer: the root, or a delegated issuer holding its
/// own delegation chain. Possession of the secret key authorizes issuance;
/// this type has no secret-export, Debug or Clone API. Dalek zeroizes the
/// signing key on drop.
pub struct JwtEnrollmentIssuer {
    key: ed25519_dalek::SigningKey,
}

impl JwtEnrollmentIssuer {
    pub fn from_seed(secret: &[u8; 32]) -> Self {
        Self {
            key: ed25519_dalek::SigningKey::from_bytes(secret),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// Issue a one-link credential as the root. `issued_at` is the root's
    /// trusted UTC seconds. The caller remains responsible for authorizing
    /// the requested grants.
    pub fn issue(
        &self,
        request: &EnrollmentIssueRequest,
        issued_at: u64,
    ) -> Result<IssuedEnrollmentCredential, EnrollmentError> {
        self.issue_chain(request, None, issued_at)
    }

    /// Issue a credential under `parent`, this issuer's own delegation
    /// chain. Refuses anything the delegation does not allow, including a
    /// persistent credential without `may_register`.
    pub fn issue_delegated(
        &self,
        request: &EnrollmentIssueRequest,
        parent: &Chain,
        issued_at: u64,
    ) -> Result<IssuedEnrollmentCredential, EnrollmentError> {
        self.issue_chain(request, Some(parent), issued_at)
    }

    fn issue_chain(
        &self,
        request: &EnrollmentIssueRequest,
        parent: Option<&Chain>,
        issued_at: u64,
    ) -> Result<IssuedEnrollmentCredential, EnrollmentError> {
        validate_key(&request.subject)?;
        validate_evidence_shape(
            &request.audience,
            issued_at,
            request.not_before,
            Some(request.expires_at),
            &request.claims,
        )?;
        let chain = issue_enrollment(
            self.key.as_bytes(),
            parent,
            &request.subject,
            &request.audience,
            &EnrollmentExtensionV1 {
                claims: request.claims.clone(),
                mandatory: vec![],
                registration: request.registration,
            },
            Validity {
                issued_at,
                not_before: request.not_before,
                expires_at: request.expires_at,
            },
        )?;
        let credential_id = leaf_jti(&chain).ok_or(EnrollmentError::SigningFailed)?;
        Ok(IssuedEnrollmentCredential {
            chain,
            credential_id,
        })
    }
}

/// A pinned Ed25519 root and an explicitly selected intended audience.
/// Configure a separate verifier for each trusted root; chains cannot add
/// roots, change algorithms, or supply key URLs.
pub struct JwtEnrollmentVerifier {
    root: [u8; 32],
    audience: String,
    activation_leeway: u64,
}

impl JwtEnrollmentVerifier {
    pub fn trusted_root(&self) -> &[u8; 32] {
        &self.root
    }

    pub fn intended_audience(&self) -> &str {
        &self.audience
    }

    pub fn activation_leeway(&self) -> u64 {
        self.activation_leeway
    }

    /// Tolerate an issuer clock up to `seconds` ahead (0 to
    /// [`MAX_ACTIVATION_LEEWAY_SECONDS`]). Expiry never receives leeway.
    pub fn with_activation_leeway(mut self, seconds: u64) -> Result<Self, EnrollmentError> {
        if !valid_leeway(seconds) {
            return Err(EnrollmentError::InvalidConfiguration);
        }
        self.activation_leeway = seconds;
        Ok(self)
    }

    pub fn new(root: [u8; 32], audience: impl Into<String>) -> Result<Self, EnrollmentError> {
        let audience = audience.into();
        if !valid_audience(&audience) || validate_key(&root).is_err() {
            return Err(EnrollmentError::InvalidConfiguration);
        }
        Ok(Self {
            root,
            audience,
            activation_leeway: DEFAULT_ACTIVATION_LEEWAY_SECONDS,
        })
    }

    /// Verify a presented chain with no delegate revocations.
    /// See [`Self::verify_with`].
    pub fn verify(
        &self,
        chain: &str,
        authenticated_peer: &[u8; 32],
        now: u64,
    ) -> Result<VerifiedAdmissionEvidence, EnrollmentError> {
        self.verify_with(chain, authenticated_peer, now, &NoRevocations)
    }

    /// `authenticated_peer` MUST come from the authenticated transport, never
    /// from chain or request data. Comparing this key supplies proof of
    /// possession; the chain alone is not a bearer credential. `now` is
    /// trusted UTC seconds. The chain must end at the pinned root, every link
    /// must name this audience, and every gate of `verify_chain` applies.
    /// Activation tolerates the configured leeway; expiry is exact. A chain
    /// that fails only on activation reports [`EnrollmentError::ClockSkew`].
    pub fn verify_with(
        &self,
        chain: &str,
        authenticated_peer: &[u8; 32],
        now: u64,
        revoked: &dyn RevokedDelegates,
    ) -> Result<VerifiedAdmissionEvidence, EnrollmentError> {
        let anchors = [self.root];
        let verify_at = |now: u64| {
            let mut trust = TrustContext::new(&anchors, &self.audience, now, revoked);
            trust.leeway = self.activation_leeway;
            verify_chain(
                chain.as_bytes(),
                LeafKind::Enrollment,
                authenticated_peer,
                &trust,
            )
        };
        match verify_at(now) {
            Ok(verified) => {
                VerifiedAdmissionEvidence::from_chain(verified, chain, self.activation_leeway)
            }
            // Time is checked before structure and signatures. Report skew
            // only if the chain is otherwise valid when it claims to start.
            Err(ChainError::NotYetValid) => {
                let start = claimed_start(chain.as_bytes())
                    .filter(|start| *start > now)
                    .ok_or(EnrollmentError::InvalidValidity)?;
                verify_at(start)?;
                Err(EnrollmentError::ClockSkew {
                    offset_seconds: i64::try_from(start - now).unwrap_or(i64::MAX),
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Verify a locality statement (see [`delegation::locality_statement`])
    /// for `endpoint` exactly as a credential is verified: pinned root and
    /// audience, delegation bounds and revocations, signed validity, and a
    /// leaf naming `endpoint`. Returns the declaration and when the
    /// statement stops being valid.
    pub fn verify_locality(
        &self,
        statement: &str,
        endpoint: &[u8; 32],
        now: u64,
        revoked: &dyn RevokedDelegates,
    ) -> Result<crate::topology::locality::CoreVerifiedLocality, EnrollmentError> {
        let anchors = [self.root];
        let mut trust = TrustContext::new(&anchors, &self.audience, now, revoked);
        trust.leeway = self.activation_leeway;
        let verified = verify_chain(statement.as_bytes(), LeafKind::Locality, endpoint, &trust)?;
        let LeafClaims::Locality(claims) = verified.leaf() else {
            return Err(EnrollmentError::InvalidJwt);
        };
        Ok(crate::topology::locality::CoreVerifiedLocality {
            declared: claims.declared().ok_or(EnrollmentError::InvalidClaims)?,
            expires_at: Some(verified.not_after()),
        })
    }
}

/// Parse an enrollment identity consistently across JWTs, Docs and transport
/// adapters. Strings are canonical lowercase hex, and keys must be valid,
/// nonweak Ed25519 points; merely decoding 32 bytes is insufficient.
pub(crate) fn canonical_identity(value: &str) -> Result<[u8; 32], EnrollmentError> {
    let bytes: [u8; 32] = hex::decode(value)
        .map_err(|_| EnrollmentError::InvalidIdentity)?
        .try_into()
        .map_err(|_| EnrollmentError::InvalidIdentity)?;
    if hex::encode(bytes) != value {
        return Err(EnrollmentError::InvalidIdentity);
    }
    validate_key(&bytes)?;
    Ok(bytes)
}

fn validate_key(value: &[u8; 32]) -> Result<(), EnrollmentError> {
    crate::delegation::links::valid_key(value)
        .then_some(())
        .ok_or(EnrollmentError::InvalidIdentity)
}

fn valid_audience(value: &str) -> bool {
    if value.is_empty() || value.chars().any(char::is_control) {
        return false;
    }
    if !value.contains(':') {
        return true;
    }
    // RFC 7519's StringOrURI requires a URI when a colon occurs. Check the
    // original string, not a URL parser's normalized/escaped replacement.
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else if byte.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=".contains(&byte) {
            index += 1;
        } else {
            return false;
        }
    }
    url::Url::parse(value).is_ok()
}

fn validate_evidence_shape(
    audience: &Audience,
    issued_at: u64,
    not_before: Option<u64>,
    expires_at: Option<u64>,
    claims: &AdmissionClaims,
) -> Result<(), EnrollmentError> {
    let valid_audiences = match audience {
        Audience::One(value) => valid_audience(value),
        Audience::Many(values) => !values.is_empty() && values.iter().all(|v| valid_audience(v)),
    };
    if !valid_audiences
        || claims
            .attributes
            .iter()
            .flat_map(|attrs| attrs.values())
            .any(|attr| matches!(attr, TypedAttribute::Float(value) if !value.is_finite()))
    {
        return Err(EnrollmentError::InvalidClaims);
    }
    if expires_at.is_some_and(|exp| exp <= issued_at)
        || matches!((not_before, expires_at), (Some(nbf), Some(exp)) if nbf >= exp)
    {
        return Err(EnrollmentError::InvalidValidity);
    }
    Ok(())
}

/// Reject duplicate JSON object keys at any depth (the shared JWS check).
pub(super) fn validate_unique_json(bytes: &[u8]) -> Result<(), EnrollmentError> {
    delegation::jws::validate_unique_json(bytes).map_err(|_| EnrollmentError::InvalidJwt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_identity_parser_rejects_noncanonical_and_weak_encodings() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[10; 32])
            .verifying_key()
            .to_bytes();
        let encoded = hex::encode(key);
        assert_eq!(canonical_identity(&encoded).unwrap(), key);
        for invalid in [
            encoded.to_uppercase(),
            format!(" {encoded}"),
            encoded[..62].into(),
            "zz".repeat(32),
            hex::encode([0; 32]),
            hex::encode([255; 32]),
        ] {
            assert_eq!(
                canonical_identity(&invalid).unwrap_err(),
                EnrollmentError::InvalidIdentity,
                "{invalid}"
            );
        }
        let mut identity_point = [0; 32];
        identity_point[0] = 1;
        assert_eq!(
            canonical_identity(&hex::encode(identity_point)).unwrap_err(),
            EnrollmentError::InvalidIdentity
        );
    }

    #[test]
    fn shared_identity_parser_rejects_strong_zip215_compressed_aliases() {
        // RFC 8032 rejects y >= p, where p = 2^255 - 19. ZIP-215 instead
        // accepts some of these encodings as aliases of canonical points.
        // Establish that this exercises strong points accepted by Dalek, not
        // merely weak points already rejected by the other key check.
        let mut aliases = 0;
        for low_byte in 0xed..=0xff {
            let mut encoded = [0xff; 32];
            encoded[0] = low_byte;
            encoded[31] = 0x7f;
            let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(&encoded) else {
                continue;
            };
            if key.is_weak() {
                continue;
            }
            let canonical = key.to_edwards().compress().to_bytes();
            assert_ne!(canonical, encoded);
            assert_eq!(
                canonical_identity(&hex::encode(encoded)).unwrap_err(),
                EnrollmentError::InvalidIdentity
            );
            assert_eq!(
                canonical_identity(&hex::encode(canonical)).unwrap(),
                canonical
            );
            aliases += 1;
        }
        assert!(
            aliases > 0,
            "fixture must exercise strong compressed aliases"
        );
    }
}
