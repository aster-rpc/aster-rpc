use std::collections::BTreeMap;
use std::fmt;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};

use crate::delegation::jws::{self, JwsError};
// The claim types are shared with delegation chains, where an enrollment
// credential is a one-link chain.
pub use crate::delegation::{
    AdmissionClaims, Audience, Confirmation, EnrollmentExtensionV1, PublicConfirmationKey,
    TypedAttribute, ENROLLMENT_LINK_TYPE as ENROLLMENT_JWT_TYPE,
};

pub const MAX_JWT_BYTES: usize = 16 * 1024;

impl AdmissionClaims {
    /// Parse an attribute file before authorized issuance. The input is bounded
    /// to [`MAX_JWT_BYTES`], must be an object of strictly typed values, and may
    /// not contain duplicate keys at any depth. Issuance separately checks the
    /// size of the complete JWT, so a parsed map can still exceed that limit.
    pub fn parse_attributes_json(
        bytes: &[u8],
    ) -> Result<BTreeMap<String, TypedAttribute>, EnrollmentError> {
        if bytes.len() > MAX_JWT_BYTES {
            return Err(EnrollmentError::TokenTooLarge);
        }
        validate_unique_json(bytes).map_err(|_| EnrollmentError::InvalidClaims)?;
        serde_json::from_slice(bytes).map_err(|_| EnrollmentError::InvalidClaims)
    }
}

/// Untrusted issuance/serialization data. This type never grants admission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrollmentClaimsV1 {
    pub iss: String,
    pub sub: String,
    pub aud: Audience,
    pub jti: String,
    pub iat: u64,
    #[serde(
        default,
        deserialize_with = "optional_numeric_date",
        skip_serializing_if = "Option::is_none"
    )]
    pub nbf: Option<u64>,
    #[serde(
        default,
        deserialize_with = "optional_numeric_date",
        skip_serializing_if = "Option::is_none"
    )]
    pub exp: Option<u64>,
    pub cnf: Confirmation,
    pub aster_enrollment_v1: EnrollmentExtensionV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionEvidenceSource {
    PresentedJwt,
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

/// Immutable verified claims. Only signature verification or the pinned-root
/// Docs path can construct this type; deserializing copied claims cannot
/// manufacture verified evidence.
#[derive(Debug, Clone)]
pub struct VerifiedAdmissionEvidence {
    pub(super) issuer: [u8; 32],
    pub(super) subject: [u8; 32],
    claims: AdmissionClaims,
    audience: Audience,
    issued_at: u64,
    not_before: Option<u64>,
    expires_at: Option<u64>,
    credential_id: Option<String>,
    original_jwt: Option<String>,
    source: AdmissionEvidenceSource,
}

impl VerifiedAdmissionEvidence {
    pub fn issuer(&self) -> &[u8; 32] {
        &self.issuer
    }
    pub fn subject(&self) -> &[u8; 32] {
        &self.subject
    }
    /// Registration is a separate statement and has no JWT credential id.
    pub fn credential_id(&self) -> Option<&str> {
        self.credential_id.as_deref()
    }
    pub fn claims(&self) -> &AdmissionClaims {
        &self.claims
    }
    pub fn audience(&self) -> &Audience {
        &self.audience
    }
    pub fn issued_at(&self) -> u64 {
        self.issued_at
    }
    pub fn expires_at(&self) -> Option<u64> {
        self.expires_at
    }
    pub fn not_before(&self) -> Option<u64> {
        self.not_before
    }
    pub fn source(&self) -> AdmissionEvidenceSource {
        self.source
    }
    /// Preserve the signed artifact and provenance, never re-sign mapped claims.
    pub fn original_jwt(&self) -> Option<&str> {
        self.original_jwt.as_deref()
    }

    pub(super) fn valid_at(&self, now: u64) -> bool {
        self.issued_at <= now
            && self.not_before.is_none_or(|nbf| nbf <= now)
            && self.expires_at.is_none_or(|exp| now < exp)
    }

    pub(super) fn from_registration(
        data: RegistrationEvidenceData,
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
            original_jwt: None,
            source: AdmissionEvidenceSource::RootAuthoredRecord {
                document_id: data.document_id,
                revision: data.revision,
            },
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
    UnsupportedMandatoryRestriction,
    InvalidClaims,
    SigningFailed,
    RandomnessUnavailable,
}

impl fmt::Display for EnrollmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "enrollment verification failed: {self:?}")
    }
}
impl std::error::Error for EnrollmentError {}

/// Authorized issuance inputs. The issuer derives root/subject identifiers,
/// confirmation key, algorithm and token type rather than accepting them from
/// caller-provided JWT claims. A missing credential id draws 256 random bits.
#[derive(Debug, Clone)]
pub struct EnrollmentIssueRequest {
    pub subject: [u8; 32],
    pub audience: Audience,
    pub credential_id: Option<String>,
    pub not_before: Option<u64>,
    pub expires_at: Option<u64>,
    pub claims: AdmissionClaims,
}

impl EnrollmentIssueRequest {
    pub fn new(subject: [u8; 32], audience: impl Into<String>) -> Self {
        Self {
            subject,
            audience: Audience::One(audience.into()),
            credential_id: None,
            not_before: None,
            expires_at: None,
            claims: AdmissionClaims::default(),
        }
    }
}

/// The public signed artifact and its identifier, containing no root secret.
#[derive(Debug, Clone)]
pub struct IssuedEnrollmentCredential {
    jwt: String,
    credential_id: String,
}

impl IssuedEnrollmentCredential {
    pub fn jwt(&self) -> &str {
        &self.jwt
    }
    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }
    pub fn into_jwt(self) -> String {
        self.jwt
    }
}

/// An authorized local root signer. Possession of the root's existing secret
/// key authorizes issuance; this type has no secret-export, Debug or Clone API.
/// Dalek zeroizes the signing key on drop.
pub struct JwtEnrollmentIssuer {
    key: ed25519_dalek::SigningKey,
}

impl JwtEnrollmentIssuer {
    pub fn from_seed(root_secret: &[u8; 32]) -> Self {
        Self {
            key: ed25519_dalek::SigningKey::from_bytes(root_secret),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// Issue a reusable proof-of-possession credential with no implicit expiry
    /// or maximum lifetime. `issued_at` is the root's trusted UTC seconds.
    /// The caller remains responsible for authorizing the requested grants.
    pub fn issue(
        &self,
        request: &EnrollmentIssueRequest,
        issued_at: u64,
    ) -> Result<IssuedEnrollmentCredential, EnrollmentError> {
        validate_key(&request.subject)?;
        validate_evidence_shape(
            &request.audience,
            issued_at,
            request.not_before,
            request.expires_at,
            &request.claims,
        )?;
        // Bound copied caller data before constructing owned JWT claims. The
        // writer stops serialization at the limit without allocating a payload.
        let mut size = BoundedJsonSize(0);
        serde_json::to_writer(&mut size, &(&request.audience, &request.claims)).map_err(
            |error| {
                if error.is_io() {
                    EnrollmentError::TokenTooLarge
                } else {
                    EnrollmentError::InvalidClaims
                }
            },
        )?;
        let credential_id = match &request.credential_id {
            Some(value) if valid_credential_id(value) => value.clone(),
            Some(_) => return Err(EnrollmentError::InvalidClaims),
            None => {
                let mut bytes = [0; 32];
                getrandom::getrandom(&mut bytes)
                    .map_err(|_| EnrollmentError::RandomnessUnavailable)?;
                URL_SAFE_NO_PAD.encode(bytes)
            }
        };
        let claims = EnrollmentClaimsV1 {
            iss: hex::encode(self.public_key()),
            sub: hex::encode(request.subject),
            aud: request.audience.clone(),
            jti: credential_id.clone(),
            iat: issued_at,
            nbf: request.not_before,
            exp: request.expires_at,
            cnf: Confirmation::ed25519(&request.subject),
            aster_enrollment_v1: EnrollmentExtensionV1 {
                claims: request.claims.clone(),
                mandatory: vec![],
            },
        };
        // Check the complete artifact before signing.
        let payload = serde_json::to_vec(&claims).map_err(|_| EnrollmentError::InvalidClaims)?;
        if jws::encoded_len(ENROLLMENT_JWT_TYPE, payload.len()) > MAX_JWT_BYTES {
            return Err(EnrollmentError::TokenTooLarge);
        }
        let jwt = jws::sign(ENROLLMENT_JWT_TYPE, &payload, self.key.as_bytes())
            .map_err(|_| EnrollmentError::SigningFailed)?;
        if jwt.len() > MAX_JWT_BYTES {
            return Err(EnrollmentError::TokenTooLarge);
        }
        Ok(IssuedEnrollmentCredential { jwt, credential_id })
    }
}

struct BoundedJsonSize(usize);

impl std::io::Write for BoundedJsonSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_JWT_BYTES)
            .ok_or_else(|| std::io::Error::other("enrollment claims exceed size limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A pinned Ed25519 root and an explicitly selected intended audience.
/// Configure a separate verifier for each trusted root; token headers cannot
/// add roots, change algorithms, or supply key URLs.
pub struct JwtEnrollmentVerifier {
    root: [u8; 32],
    audience: String,
}

impl JwtEnrollmentVerifier {
    pub fn trusted_root(&self) -> &[u8; 32] {
        &self.root
    }

    pub fn intended_audience(&self) -> &str {
        &self.audience
    }

    pub fn new(root: [u8; 32], audience: impl Into<String>) -> Result<Self, EnrollmentError> {
        let audience = audience.into();
        if !valid_audience(&audience) || validate_key(&root).is_err() {
            return Err(EnrollmentError::InvalidConfiguration);
        }
        Ok(Self { root, audience })
    }

    /// `authenticated_peer` MUST come from the authenticated transport, never
    /// from JWT/request data. Comparing this key supplies proof of possession;
    /// the JWT alone is not a bearer credential. `now` is trusted UTC seconds.
    /// V1 applies zero clock skew: `iat <= now`, `nbf <= now`, and `now < exp`.
    /// Future issuance is inactive even without `nbf`; no implicit iat margin or
    /// configurable validity leeway is applied. Synchronize the trusted clock
    /// rather than extending a signed expiration to hide clock differences.
    pub fn verify(
        &self,
        token: &str,
        authenticated_peer: &[u8; 32],
        now: u64,
    ) -> Result<VerifiedAdmissionEvidence, EnrollmentError> {
        if token.len() > MAX_JWT_BYTES {
            return Err(EnrollmentError::TokenTooLarge);
        }
        // The shared JWS profile rejects JOSE extensions (only alg and typ;
        // notably no crit/b64/jku/jwk/kid/x5u), noncanonical encodings and
        // ambiguous objects everywhere, including optional application
        // claims, attributes and nested confirmation data, before any claim
        // is processed.
        let compact = jws::decode(token).map_err(|error| match error {
            JwsError::Header => EnrollmentError::UnsupportedHeader,
            _ => EnrollmentError::InvalidJwt,
        })?;
        if compact.typ != ENROLLMENT_JWT_TYPE {
            return Err(EnrollmentError::UnsupportedHeader);
        }
        compact
            .verify(&self.root)
            .map_err(|_| EnrollmentError::InvalidJwt)?;
        // Time semantics are applied below against the same trusted clock
        // that policy uses, including equality at exp and optional exp.
        let claims: EnrollmentClaimsV1 =
            serde_json::from_slice(&compact.payload).map_err(|_| EnrollmentError::InvalidJwt)?;
        if claims.iss != hex::encode(self.root) || !claims.aud.contains(&self.audience) {
            return Err(EnrollmentError::InvalidJwt);
        }
        let subject = canonical_identity(&claims.sub)?;
        if canonical_identity(&claims.iss)? != self.root {
            return Err(EnrollmentError::InvalidIdentity);
        }
        let cnf = &claims.cnf.jwk;
        let confirmation = URL_SAFE_NO_PAD
            .decode(&cnf.x)
            .map_err(|_| EnrollmentError::PeerKeyMismatch)?;
        if cnf.kty != "OKP"
            || cnf.crv != "Ed25519"
            || confirmation.as_slice() != subject
            || subject != *authenticated_peer
            || URL_SAFE_NO_PAD.encode(&confirmation) != cnf.x
        {
            return Err(EnrollmentError::PeerKeyMismatch);
        }
        if !valid_credential_id(&claims.jti) || !claims.aud.contains(&self.audience) {
            return Err(EnrollmentError::InvalidClaims);
        }
        if !claims.aster_enrollment_v1.mandatory.is_empty() {
            return Err(EnrollmentError::UnsupportedMandatoryRestriction);
        }
        validate_evidence_shape(
            &claims.aud,
            claims.iat,
            claims.nbf,
            claims.exp,
            &claims.aster_enrollment_v1.claims,
        )?;
        let evidence = VerifiedAdmissionEvidence {
            issuer: self.root,
            subject,
            claims: claims.aster_enrollment_v1.claims,
            audience: claims.aud,
            issued_at: claims.iat,
            not_before: claims.nbf,
            expires_at: claims.exp,
            credential_id: Some(claims.jti),
            original_jwt: Some(token.to_owned()),
            source: AdmissionEvidenceSource::PresentedJwt,
        };
        if !evidence.valid_at(now) {
            return Err(EnrollmentError::InvalidValidity);
        }
        Ok(evidence)
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

fn optional_numeric_date<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    // Missing is allowed; an explicit null/string/fraction is not NumericDate
    // under this integral-seconds profile and cannot erase a signed bound.
    u64::deserialize(deserializer).map(Some)
}

fn validate_key(value: &[u8; 32]) -> Result<(), EnrollmentError> {
    crate::delegation::links::valid_key(value)
        .then_some(())
        .ok_or(EnrollmentError::InvalidIdentity)
}

fn valid_credential_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
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
    jws::validate_unique_json(bytes).map_err(|_| EnrollmentError::InvalidJwt)
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
