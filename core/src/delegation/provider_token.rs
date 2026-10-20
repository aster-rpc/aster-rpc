//! Provider tokens: an identity provider's own JWT (an OIDC ID token or an
//! OAuth access token) that an edge forwards, unchanged, in
//! `aster-on-behalf-of`. Design: `docs/_internal/web_gateway/gateway-identity.md`,
//! "External identity providers (forwarded)".
//!
//! Verification is pure: no I/O and no clock read. The caller supplies the
//! trusted issuer's configuration, a key lookup by `kid` (the issuer's keys
//! come from the replicated fleet namespace, never from a fetch), and the
//! clock. Every check fails closed:
//!
//! - **Framing:** at most [`MAX_PROVIDER_TOKEN_BYTES`]; exactly three
//!   non-empty segments of canonical unpadded base64url; a header segment of
//!   at most [`MAX_HEADER_SEGMENT_BYTES`] and a signature of at most
//!   [`MAX_SIGNATURE_BYTES`]; JSON objects without duplicate members.
//! - **Header:** only `alg`, `kid`, `typ`, `x5t` and `x5t#S256`. Anything
//!   else (`crit`, `jku`, `jwk`, `x5u`, `x5c`, `cty`, `b64`, `zip`, `enc`, …)
//!   is refused, so no header member can select a key or change processing.
//!   `kid` is required. `alg` is one of [`ProviderAlg`] (RS256, PS256,
//!   ES256); `none`, every `HS*` and anything else is refused.
//! - **Key:** the issuer key for `kid`; its algorithm must equal the
//!   header's.
//! - **Claims:** `iss` equals the configured issuer; `aud` (a string or an
//!   array) contains a configured audience; `sub` is present; `iat` and `exp`
//!   are required non-negative integers, `nbf` is optional; `exp > iat`,
//!   `exp - iat <= max_lifetime`, `now < exp`, and `max(iat, nbf) <= now +
//!   leeway`. Leeway never extends expiry. Other claims are allowed: provider
//!   tokens carry many.
//! - **Roles:** the configured claim (an array of strings, or one string of
//!   space-separated values such as `scope`) mapped through the role map.
//!   **Values with no mapping are dropped**, so a provider group grants
//!   nothing until an operator maps it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::{Map, Value};

use super::jws::{canonical_segment, validate_unique_json};
use super::on_behalf_of::{
    check_times, valid_role, valid_subject, EdgeTokenError, EdgeTokenLimits,
};
use super::MAX_LEEWAY_SECONDS;

/// Largest token accepted, checked before any decoding.
pub const MAX_PROVIDER_TOKEN_BYTES: usize = 16 * 1024;
/// Largest encoded header segment.
pub const MAX_HEADER_SEGMENT_BYTES: usize = 1024;
/// Largest decoded signature (an RSA-8192 signature).
pub const MAX_SIGNATURE_BYTES: usize = 1024;
/// Default longest `exp - iat` of a provider token.
pub const DEFAULT_PROVIDER_TOKEN_LIFETIME_SECONDS: u64 = 3600;
/// The cap on a configured `max_token_lifetime_secs`.
pub const MAX_PROVIDER_TOKEN_LIFETIME_SECONDS: u64 = 86_400;
/// Most entries in one JWK set.
pub const MAX_JWKS_KEYS: usize = 64;
/// Longest `kid`.
pub const MAX_KID_BYTES: usize = 256;
/// Longest issuer, audience, `jti` or role-claim value.
pub const MAX_CLAIM_STRING_BYTES: usize = 256;
/// Most `aud` entries in one token.
pub const MAX_TOKEN_AUDIENCES: usize = 16;
/// Most values in the roles claim of one token.
pub const MAX_ROLE_CLAIM_VALUES: usize = 256;
/// Most `amr` entries in one token.
pub const MAX_PROVIDER_AMR: usize = 16;

/// The header members a provider token may carry.
const ALLOWED_HEADER: &[&str] = &["alg", "kid", "typ", "x5t", "x5t#S256"];
/// JWK members that hold private key material: a key carrying any of them is
/// never kept, so a misconfigured provider cannot leak a secret into the
/// replicated namespace.
const PRIVATE_JWK_MEMBERS: &[&str] = &["d", "p", "q", "dp", "dq", "qi", "oth", "k"];

/// A provider signature algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProviderAlg {
    /// RSASSA-PKCS1-v1_5 with SHA-256, 2048 to 8192-bit keys.
    Rs256,
    /// RSASSA-PSS with SHA-256, 2048 to 8192-bit keys.
    Ps256,
    /// ECDSA P-256 with SHA-256, fixed-width `r || s`.
    Es256,
}

impl ProviderAlg {
    /// The JOSE name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rs256 => "RS256",
            Self::Ps256 => "PS256",
            Self::Es256 => "ES256",
        }
    }

    /// From the JOSE name, exactly (case-sensitive). `None` for every other
    /// algorithm, including `none` and `HS*`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "RS256" => Some(Self::Rs256),
            "PS256" => Some(Self::Ps256),
            "ES256" => Some(Self::Es256),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum KeyMaterial {
    /// Big-endian modulus and exponent, without leading zeros.
    Rsa { n: Vec<u8>, e: Vec<u8> },
    /// `x || y`, 64 bytes.
    P256 { xy: Vec<u8> },
}

/// One issuer verification key, parsed from a JWK.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderKey {
    kid: String,
    alg: ProviderAlg,
    material: KeyMaterial,
}

impl fmt::Debug for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderKey")
            .field("kid", &self.kid)
            .field("alg", &self.alg)
            .finish_non_exhaustive()
    }
}

impl ProviderKey {
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The one algorithm this key verifies.
    pub fn alg(&self) -> ProviderAlg {
        self.alg
    }

    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        use aws_lc_rs::signature::{
            RsaPublicKeyComponents, UnparsedPublicKey, ECDSA_P256_SHA256_FIXED,
            RSA_PKCS1_2048_8192_SHA256, RSA_PSS_2048_8192_SHA256,
        };
        match (&self.material, self.alg) {
            (KeyMaterial::Rsa { n, e }, ProviderAlg::Rs256 | ProviderAlg::Ps256) => {
                let params = if self.alg == ProviderAlg::Rs256 {
                    &RSA_PKCS1_2048_8192_SHA256
                } else {
                    &RSA_PSS_2048_8192_SHA256
                };
                RsaPublicKeyComponents { n, e }
                    .verify(params, message, signature)
                    .is_ok()
            }
            (KeyMaterial::P256 { xy }, ProviderAlg::Es256) => {
                if signature.len() != 64 {
                    return false;
                }
                let mut point = Vec::with_capacity(65);
                point.push(0x04);
                point.extend_from_slice(xy);
                UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &point)
                    .verify(message, signature)
                    .is_ok()
            }
            _ => false,
        }
    }

    /// The key as a minimal public JWK: `kty`, `kid`, `alg`, `use` and the
    /// public parameters only.
    pub fn to_jwk(&self) -> Value {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let mut jwk = Map::new();
        jwk.insert("kid".into(), self.kid.clone().into());
        jwk.insert("alg".into(), self.alg.as_str().into());
        jwk.insert("use".into(), "sig".into());
        match &self.material {
            KeyMaterial::Rsa { n, e } => {
                jwk.insert("kty".into(), "RSA".into());
                jwk.insert("n".into(), URL_SAFE_NO_PAD.encode(n).into());
                jwk.insert("e".into(), URL_SAFE_NO_PAD.encode(e).into());
            }
            KeyMaterial::P256 { xy } => {
                jwk.insert("kty".into(), "EC".into());
                jwk.insert("crv".into(), "P-256".into());
                jwk.insert("x".into(), URL_SAFE_NO_PAD.encode(&xy[..32]).into());
                jwk.insert("y".into(), URL_SAFE_NO_PAD.encode(&xy[32..]).into());
            }
        }
        Value::Object(jwk)
    }
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn base64_member(jwk: &Map<String, Value>, name: &str) -> Option<Vec<u8>> {
    canonical_segment(jwk.get(name)?.as_str()?).ok()
}

fn strip_leading_zeros(bytes: Vec<u8>) -> Vec<u8> {
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len());
    bytes[start..].to_vec()
}

/// One JWK, or `None` when it is not a usable signature key: private
/// material, `use` other than `sig`, `key_ops` without `verify`, an
/// algorithm other than RS256 / PS256 / ES256 or one that does not fit the
/// key type, an RSA modulus outside 2048..=8192 bits, a curve other than
/// P-256. An RSA key without `alg` is RS256; an EC P-256 key without `alg` is
/// ES256 (a PS256 key must say so).
fn parse_jwk(jwk: &Map<String, Value>) -> Option<ProviderKey> {
    if PRIVATE_JWK_MEMBERS.iter().any(|m| jwk.contains_key(*m)) {
        return None;
    }
    let kid = jwk.get("kid")?.as_str()?;
    if !valid_text(kid, MAX_KID_BYTES) {
        return None;
    }
    if let Some(usage) = jwk.get("use") {
        if usage.as_str()? != "sig" {
            return None;
        }
    }
    if let Some(ops) = jwk.get("key_ops") {
        if !ops
            .as_array()?
            .iter()
            .any(|op| op.as_str() == Some("verify"))
        {
            return None;
        }
    }
    let alg = match jwk.get("alg") {
        None => None,
        Some(value) => Some(ProviderAlg::from_name(value.as_str()?)?),
    };
    let (alg, material) = match jwk.get("kty")?.as_str()? {
        "RSA" => {
            let alg = alg.unwrap_or(ProviderAlg::Rs256);
            if alg == ProviderAlg::Es256 {
                return None;
            }
            let n = strip_leading_zeros(base64_member(jwk, "n")?);
            let e = strip_leading_zeros(base64_member(jwk, "e")?);
            if !(256..=1024).contains(&n.len()) || !(1..=8).contains(&e.len()) {
                return None;
            }
            (alg, KeyMaterial::Rsa { n, e })
        }
        "EC" => {
            if jwk.get("crv")?.as_str()? != "P-256" {
                return None;
            }
            let alg = alg.unwrap_or(ProviderAlg::Es256);
            if alg != ProviderAlg::Es256 {
                return None;
            }
            let x = base64_member(jwk, "x")?;
            let y = base64_member(jwk, "y")?;
            if x.len() != 32 || y.len() != 32 {
                return None;
            }
            let mut xy = x;
            xy.extend_from_slice(&y);
            (alg, KeyMaterial::P256 { xy })
        }
        _ => return None,
    };
    Some(ProviderKey {
        kid: kid.to_string(),
        alg,
        material,
    })
}

/// Why a JWK set was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwkSetError {
    /// Not `{"keys": [...]}`.
    Malformed,
    /// More than [`MAX_JWKS_KEYS`] entries.
    TooManyKeys,
}

impl fmt::Display for JwkSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => write!(f, "JWK set is not {{\"keys\": [...]}}"),
            Self::TooManyKeys => write!(f, "JWK set has more than {MAX_JWKS_KEYS} keys"),
        }
    }
}

impl std::error::Error for JwkSetError {}

/// The usable signature keys of a JWK set (`{"keys": [...]}`), in order.
/// Unusable entries are skipped (see the module docs). A `kid` that more than
/// one usable key carries is dropped altogether, so a lookup by `kid` is never
/// ambiguous.
pub fn parse_jwk_set(set: &Value) -> Result<Vec<ProviderKey>, JwkSetError> {
    let entries = set
        .as_object()
        .and_then(|object| object.get("keys"))
        .and_then(Value::as_array)
        .ok_or(JwkSetError::Malformed)?;
    if entries.len() > MAX_JWKS_KEYS {
        return Err(JwkSetError::TooManyKeys);
    }
    let keys: Vec<ProviderKey> = entries
        .iter()
        .filter_map(|entry| entry.as_object().and_then(parse_jwk))
        .collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for key in &keys {
        *counts.entry(key.kid.as_str()).or_default() += 1;
    }
    let unique: BTreeSet<String> = counts
        .into_iter()
        .filter(|(_, count)| *count == 1)
        .map(|(kid, _)| kid.to_string())
        .collect();
    Ok(keys
        .into_iter()
        .filter(|key| unique.contains(&key.kid))
        .collect())
}

/// Why a provider token was refused. Each variant names the failing check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTokenError {
    /// The issuer configuration or limits are outside the documented caps.
    Configuration,
    /// The token, its header or its signature is larger than allowed.
    Size,
    /// Not a well-formed token: segments, base64url, JSON, duplicate members,
    /// or a claim missing or outside its bounds.
    Malformed,
    /// A header member outside the allowed set, or a missing `kid`.
    Header,
    /// `alg` is not RS256, PS256 or ES256 (`none`, `HS256`, …).
    Algorithm,
    /// `iss` is not the configured issuer.
    Issuer,
    /// No key for `kid`.
    UnknownKey,
    /// The key for `kid` verifies another algorithm.
    AlgorithmMismatch,
    /// The signature does not verify.
    Signature,
    /// No `aud` entry is a configured audience.
    Audience,
    /// `iat` or `nbf` is later than `now + leeway`.
    NotYetValid,
    /// `now >= exp`.
    Expired,
    /// `exp <= iat`, `nbf >= exp`, or `exp - iat` above the issuer's maximum.
    Lifetime,
}

impl fmt::Display for ProviderTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Configuration => "issuer configuration out of range",
            Self::Size => "token too large",
            Self::Malformed => "malformed token",
            Self::Header => "unsupported token header",
            Self::Algorithm => "unsupported signature algorithm",
            Self::Issuer => "issuer not trusted",
            Self::UnknownKey => "no issuer key for kid",
            Self::AlgorithmMismatch => "alg does not match the issuer key",
            Self::Signature => "signature does not verify",
            Self::Audience => "audience not accepted",
            Self::NotYetValid => "token not yet valid",
            Self::Expired => "token expired",
            Self::Lifetime => "token lifetime out of range",
        };
        write!(f, "provider token rejected: {reason}")
    }
}

impl std::error::Error for ProviderTokenError {}

/// A decoded token whose signature and claims have not been checked.
struct Unverified<'a> {
    header: Map<String, Value>,
    claims: Map<String, Value>,
    signing_input: &'a str,
    signature: Vec<u8>,
}

fn decode(token: &str) -> Result<Unverified<'_>, ProviderTokenError> {
    use ProviderTokenError::{Malformed, Size};
    if token.len() > MAX_PROVIDER_TOKEN_BYTES {
        return Err(Size);
    }
    let mut segments = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(Malformed);
    };
    if header.len() > MAX_HEADER_SEGMENT_BYTES
        || signature.len() > (MAX_SIGNATURE_BYTES * 4).div_ceil(3)
    {
        return Err(Size);
    }
    let header = canonical_segment(header).map_err(|_| Malformed)?;
    let payload = canonical_segment(payload).map_err(|_| Malformed)?;
    let signature = canonical_segment(signature).map_err(|_| Malformed)?;
    validate_unique_json(&header).map_err(|_| Malformed)?;
    validate_unique_json(&payload).map_err(|_| Malformed)?;
    let header = serde_json::from_slice(&header).map_err(|_| Malformed)?;
    let claims = serde_json::from_slice(&payload).map_err(|_| Malformed)?;
    let signing_input = &token[..token.rfind('.').ok_or(Malformed)?];
    Ok(Unverified {
        header,
        claims,
        signing_input,
        signature,
    })
}

/// The unverified `iss` of a compact JWT, to choose how to verify it. Applies
/// the provider framing gates (size, segments, base64url, JSON) and nothing
/// else; the result is never trusted on its own.
pub fn peek_issuer(token: &str) -> Result<String, ProviderTokenError> {
    let unverified = decode(token)?;
    unverified
        .claims
        .get("iss")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(ProviderTokenError::Malformed)
}

/// The unverified `iss` and header `kid` of a compact JWT, for a publisher
/// deciding whether the token names a key it has not fetched yet. The same
/// framing gates as [`peek_issuer`], plus the `kid` bounds; never trusted on
/// its own.
pub fn peek_issuer_and_kid(token: &str) -> Result<(String, String), ProviderTokenError> {
    let unverified = decode(token)?;
    let iss = unverified
        .claims
        .get("iss")
        .and_then(Value::as_str)
        .ok_or(ProviderTokenError::Malformed)?;
    let kid = unverified
        .header
        .get("kid")
        .and_then(Value::as_str)
        .filter(|kid| valid_text(kid, MAX_KID_BYTES))
        .ok_or(ProviderTokenError::Header)?;
    Ok((iss.to_string(), kid.to_string()))
}

fn check_header(header: &Map<String, Value>) -> Result<(ProviderAlg, String), ProviderTokenError> {
    use ProviderTokenError::Header;
    if header
        .keys()
        .any(|member| !ALLOWED_HEADER.contains(&member.as_str()))
    {
        return Err(Header);
    }
    let alg = header.get("alg").and_then(Value::as_str).ok_or(Header)?;
    let alg = ProviderAlg::from_name(alg).ok_or(ProviderTokenError::Algorithm)?;
    for member in ["typ", "x5t", "x5t#S256"] {
        if let Some(value) = header.get(member) {
            if !value.as_str().is_some_and(|v| valid_text(v, 128)) {
                return Err(Header);
            }
        }
    }
    let kid = header.get("kid").and_then(Value::as_str).ok_or(Header)?;
    if !valid_text(kid, MAX_KID_BYTES) {
        return Err(Header);
    }
    Ok((alg, kid.to_string()))
}

/// A trusted issuer's configuration, as verification needs it.
#[derive(Debug, Clone, Copy)]
pub struct ProviderIssuerPolicy<'a> {
    /// The exact `iss`.
    pub iss: &'a str,
    /// Accepted `aud` values; at least one.
    pub audiences: &'a [String],
    /// The claim roles are read from; `None`: the token grants no roles.
    pub roles_claim: Option<&'a str>,
    /// Provider value → Aster role. Unmapped values are dropped.
    pub role_map: &'a BTreeMap<String, String>,
    /// Longest `exp - iat`; 1..=[`MAX_PROVIDER_TOKEN_LIFETIME_SECONDS`].
    pub max_lifetime: u64,
}

/// The verifier's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderTokenLimits {
    /// Trusted UTC seconds.
    pub now: u64,
    /// Applies to `iat` and `nbf` only. At most [`MAX_LEEWAY_SECONDS`].
    pub leeway: u64,
}

/// A provider token that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedProviderToken {
    pub issuer: String,
    pub subject: String,
    /// The mapped Aster roles.
    pub roles: BTreeSet<String>,
    /// The token's `amr`, empty when it has none.
    pub amr: Vec<String>,
    pub jti: Option<String>,
    pub kid: String,
    pub alg: ProviderAlg,
    pub issued_at: u64,
    pub not_before: Option<u64>,
    pub expires_at: u64,
    /// Every claim of the token, as signed. Present only on a token that
    /// passed every check above, so it is verified data.
    pub claims: std::sync::Arc<Map<String, Value>>,
}

fn integer(claims: &Map<String, Value>, name: &str) -> Result<Option<u64>, ProviderTokenError> {
    match claims.get(name) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or(ProviderTokenError::Malformed),
    }
}

fn string_list(value: &Value, max: usize) -> Result<Vec<&str>, ProviderTokenError> {
    let values: Vec<&str> = match value {
        Value::String(text) => text.split_whitespace().collect(),
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().ok_or(ProviderTokenError::Malformed))
            .collect::<Result<_, _>>()?,
        _ => return Err(ProviderTokenError::Malformed),
    };
    if values.len() > max
        || !values
            .iter()
            .all(|value| valid_text(value, MAX_CLAIM_STRING_BYTES))
    {
        return Err(ProviderTokenError::Malformed);
    }
    Ok(values)
}

/// Verify `token` as a token from the issuer `policy` describes, with the
/// issuer's keys behind `key_for` (by `kid`).
///
/// Order: configuration, framing, header and algorithm, `iss`, key and
/// algorithm match, signature, then the remaining claims and validity.
pub fn verify_provider_token(
    token: &str,
    policy: &ProviderIssuerPolicy<'_>,
    key_for: &dyn Fn(&str) -> Option<ProviderKey>,
    limits: &ProviderTokenLimits,
) -> Result<VerifiedProviderToken, ProviderTokenError> {
    use ProviderTokenError as E;
    if limits.leeway > MAX_LEEWAY_SECONDS
        || !(1..=MAX_PROVIDER_TOKEN_LIFETIME_SECONDS).contains(&policy.max_lifetime)
        || policy.audiences.is_empty()
    {
        return Err(E::Configuration);
    }
    let unverified = decode(token)?;
    let (alg, kid) = check_header(&unverified.header)?;
    let claims = &unverified.claims;
    let iss = claims
        .get("iss")
        .and_then(Value::as_str)
        .ok_or(E::Malformed)?;
    if iss != policy.iss {
        return Err(E::Issuer);
    }
    let key = key_for(&kid).ok_or(E::UnknownKey)?;
    if key.kid != kid {
        return Err(E::UnknownKey);
    }
    if key.alg != alg {
        return Err(E::AlgorithmMismatch);
    }
    if !key.verify(unverified.signing_input.as_bytes(), &unverified.signature) {
        return Err(E::Signature);
    }

    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .ok_or(E::Malformed)?;
    if !valid_subject(subject) {
        return Err(E::Malformed);
    }
    // `aud` is one string or an array of strings (RFC 7519 §4.1.3); a string
    // is never split.
    let audiences: Vec<&str> = match claims.get("aud") {
        Some(Value::String(aud)) => vec![aud.as_str()],
        Some(value @ Value::Array(_)) => string_list(value, MAX_TOKEN_AUDIENCES)?,
        _ => return Err(E::Malformed),
    };
    if audiences.is_empty()
        || !audiences
            .iter()
            .all(|aud| valid_text(aud, MAX_CLAIM_STRING_BYTES))
    {
        return Err(E::Malformed);
    }
    let issued_at = integer(claims, "iat")?.ok_or(E::Malformed)?;
    let expires_at = integer(claims, "exp")?.ok_or(E::Malformed)?;
    let not_before = integer(claims, "nbf")?;
    let jti = match claims.get("jti") {
        None => None,
        Some(value) => {
            let jti = value.as_str().ok_or(E::Malformed)?;
            if !valid_text(jti, MAX_CLAIM_STRING_BYTES) {
                return Err(E::Malformed);
            }
            Some(jti.to_string())
        }
    };
    let amr = match claims.get("amr") {
        None => Vec::new(),
        Some(value @ Value::Array(_)) => {
            let methods = string_list(value, MAX_PROVIDER_AMR)?;
            if !methods.iter().all(|method| valid_role(method)) {
                return Err(E::Malformed);
            }
            methods.into_iter().map(str::to_string).collect()
        }
        Some(_) => return Err(E::Malformed),
    };
    let roles = match policy.roles_claim.and_then(|claim| claims.get(claim)) {
        None => BTreeSet::new(),
        Some(value) => string_list(value, MAX_ROLE_CLAIM_VALUES)?
            .into_iter()
            .filter_map(|value| policy.role_map.get(value))
            .filter(|role| valid_role(role))
            .cloned()
            .collect(),
    };

    if !audiences
        .iter()
        .any(|aud| policy.audiences.iter().any(|accepted| accepted == aud))
    {
        return Err(E::Audience);
    }
    let times = EdgeTokenLimits {
        now: limits.now,
        leeway: limits.leeway,
        max_lifetime: policy.max_lifetime,
    };
    check_times(issued_at, not_before, expires_at, &times).map_err(|error| match error {
        EdgeTokenError::Expired => E::Expired,
        EdgeTokenError::NotYetValid => E::NotYetValid,
        _ => E::Lifetime,
    })?;
    let issuer = iss.to_string();
    let subject = subject.to_string();
    Ok(VerifiedProviderToken {
        issuer,
        subject,
        claims: std::sync::Arc::new(unverified.claims),
        roles,
        amr,
        jti,
        kid,
        alg,
        issued_at,
        not_before,
        expires_at,
    })
}

#[cfg(test)]
pub(crate) mod test_keys {
    //! Provider keys and token signing for tests, over aws-lc-rs.
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{
        EcdsaKeyPair, KeyPair, RsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING,
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde_json::Value;

    pub enum TestKey {
        Rsa(RsaKeyPair, bool),
        Ec(EcdsaKeyPair),
    }

    impl TestKey {
        pub fn rsa(pss: bool) -> Self {
            Self::Rsa(
                RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap(),
                pss,
            )
        }

        pub fn ec() -> Self {
            Self::Ec(EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap())
        }

        pub fn alg(&self) -> &'static str {
            match self {
                Self::Rsa(_, false) => "RS256",
                Self::Rsa(_, true) => "PS256",
                Self::Ec(_) => "ES256",
            }
        }

        pub fn jwk(&self, kid: &str, with_alg: bool) -> Value {
            let mut jwk = match self {
                Self::Rsa(pair, _) => {
                    let public = pair.public_key();
                    serde_json::json!({
                        "kty": "RSA",
                        "n": URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),
                        "e": URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero()),
                    })
                }
                Self::Ec(pair) => {
                    let point = pair.public_key().as_ref();
                    serde_json::json!({
                        "kty": "EC",
                        "crv": "P-256",
                        "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
                        "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
                    })
                }
            };
            jwk["kid"] = kid.into();
            if with_alg {
                jwk["alg"] = self.alg().into();
            }
            jwk
        }

        pub fn sign_input(&self, input: &[u8]) -> Vec<u8> {
            let rng = SystemRandom::new();
            match self {
                Self::Rsa(pair, pss) => {
                    let mut signature = vec![0; pair.public_modulus_len()];
                    let encoding = if *pss {
                        &aws_lc_rs::signature::RSA_PSS_SHA256
                    } else {
                        &aws_lc_rs::signature::RSA_PKCS1_SHA256
                    };
                    pair.sign(encoding, &rng, input, &mut signature).unwrap();
                    signature
                }
                Self::Ec(pair) => pair.sign(&rng, input).unwrap().as_ref().to_vec(),
            }
        }

        /// A compact JWT with `header` and `claims`, signed by this key.
        pub fn sign(&self, header: &Value, claims: &Value) -> String {
            let input = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).unwrap()),
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap())
            );
            let signature = self.sign_input(input.as_bytes());
            format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_keys::TestKey;
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    const NOW: u64 = 1_800_000_000;
    const ISS: &str = "https://accounts.example.com";

    struct Fixture {
        audiences: Vec<String>,
        role_map: BTreeMap<String, String>,
        keys: Vec<ProviderKey>,
    }

    impl Fixture {
        fn new(keys: &[(&TestKey, &str)]) -> Self {
            let set = serde_json::json!({
                "keys": keys.iter().map(|(key, kid)| key.jwk(kid, true)).collect::<Vec<_>>()
            });
            Self {
                audiences: vec!["odata-core".into(), "other".into()],
                role_map: [
                    ("data-readers".to_string(), "data.read".to_string()),
                    ("data-admins".to_string(), "data.admin".to_string()),
                ]
                .into(),
                keys: parse_jwk_set(&set).unwrap(),
            }
        }

        fn policy(&self) -> ProviderIssuerPolicy<'_> {
            ProviderIssuerPolicy {
                iss: ISS,
                audiences: &self.audiences,
                roles_claim: Some("groups"),
                role_map: &self.role_map,
                max_lifetime: DEFAULT_PROVIDER_TOKEN_LIFETIME_SECONDS,
            }
        }

        fn verify(&self, token: &str) -> Result<VerifiedProviderToken, ProviderTokenError> {
            self.verify_at(token, NOW)
        }

        fn verify_at(
            &self,
            token: &str,
            now: u64,
        ) -> Result<VerifiedProviderToken, ProviderTokenError> {
            let lookup = |kid: &str| self.keys.iter().find(|k| k.kid() == kid).cloned();
            verify_provider_token(
                token,
                &self.policy(),
                &lookup,
                &ProviderTokenLimits { now, leeway: 60 },
            )
        }
    }

    fn header(alg: &str, kid: &str) -> Value {
        serde_json::json!({"alg": alg, "kid": kid, "typ": "JWT"})
    }

    fn claims() -> Value {
        serde_json::json!({
            "iss": ISS,
            "sub": "alice",
            "aud": "odata-core",
            "iat": NOW - 10,
            "exp": NOW + 300,
            "jti": "j-1",
            "amr": ["pwd", "mfa"],
            "groups": ["data-readers", "unmapped-group"],
            "email": "alice@example.com",
        })
    }

    fn with(f: impl FnOnce(&mut Value)) -> Value {
        let mut c = claims();
        f(&mut c);
        c
    }

    #[test]
    fn peek_reads_issuer_and_kid_without_trusting_them() {
        let key = TestKey::ec();
        let token = key.sign(&header("ES256", "k-new"), &claims());
        assert_eq!(
            peek_issuer_and_kid(&token).unwrap(),
            (ISS.to_string(), "k-new".to_string())
        );
        let no_kid = key.sign(&serde_json::json!({"alg": "ES256"}), &claims());
        assert_eq!(
            peek_issuer_and_kid(&no_kid),
            Err(ProviderTokenError::Header)
        );
        let long_kid = key.sign(&header("ES256", &"k".repeat(MAX_KID_BYTES + 1)), &claims());
        assert!(peek_issuer_and_kid(&long_kid).is_err());
        assert!(peek_issuer_and_kid("not.a.token").is_err());
    }

    #[test]
    fn rs256_ps256_and_es256_tokens_verify() {
        for key in [TestKey::rsa(false), TestKey::rsa(true), TestKey::ec()] {
            let fixture = Fixture::new(&[(&key, "k1")]);
            let token = key.sign(&header(key.alg(), "k1"), &claims());
            let verified = fixture.verify(&token).unwrap();
            assert_eq!(verified.issuer, ISS);
            assert_eq!(verified.subject, "alice");
            // Unmapped values are dropped.
            assert_eq!(verified.roles, BTreeSet::from(["data.read".to_string()]));
            assert_eq!(verified.amr, ["pwd", "mfa"]);
            assert_eq!(verified.jti.as_deref(), Some("j-1"));
            assert_eq!(verified.alg.as_str(), key.alg());
        }
    }

    #[test]
    fn a_verified_token_carries_every_claim_and_a_refused_one_none() {
        let key = TestKey::ec();
        let fixture = Fixture::new(&[(&key, "k1")]);
        let signed = with(|c| c["tenant"] = "acme-synthetic".into());
        let verified = fixture
            .verify(&key.sign(&header("ES256", "k1"), &signed))
            .unwrap();
        // Everything as signed, including the raw (unmapped) group values.
        assert_eq!(Value::Object((*verified.claims).clone()), signed);
        // A forged signature, a wrong audience and an expired token are
        // errors: the result type holds no claims on any refusal.
        let forged = key.sign(&header("ES256", "k1"), &signed);
        let mut parts: Vec<&str> = forged.split('.').collect();
        let other = TestKey::ec().sign(&header("ES256", "k1"), &signed);
        parts[2] = other.rsplit('.').next().unwrap();
        assert_eq!(
            fixture.verify(&parts.join(".")).unwrap_err(),
            ProviderTokenError::Signature
        );
        let wrong_aud = with(|c| c["aud"] = "nobody".into());
        assert_eq!(
            fixture
                .verify(&key.sign(&header("ES256", "k1"), &wrong_aud))
                .unwrap_err(),
            ProviderTokenError::Audience
        );
        assert_eq!(
            fixture
                .verify_at(&key.sign(&header("ES256", "k1"), &signed), NOW + 10_000)
                .unwrap_err(),
            ProviderTokenError::Expired
        );
    }

    #[test]
    fn jwk_without_alg_defaults_by_key_type() {
        let rsa = TestKey::rsa(false);
        let ec = TestKey::ec();
        let pss = TestKey::rsa(true);
        let set = serde_json::json!({"keys": [rsa.jwk("r", false), ec.jwk("e", false), pss.jwk("p", false)]});
        let keys = parse_jwk_set(&set).unwrap();
        let alg = |kid: &str| keys.iter().find(|k| k.kid() == kid).unwrap().alg();
        assert_eq!(alg("r"), ProviderAlg::Rs256);
        assert_eq!(alg("e"), ProviderAlg::Es256);
        // A PS256 key must say so: without `alg` it is RS256 only.
        assert_eq!(alg("p"), ProviderAlg::Rs256);
        let mut fixture = Fixture::new(&[]);
        fixture.keys = keys;
        let token = pss.sign(&header("PS256", "p"), &claims());
        assert_eq!(
            fixture.verify(&token),
            Err(ProviderTokenError::AlgorithmMismatch)
        );
    }

    #[test]
    fn algorithms_none_hs256_and_mismatches_are_refused() {
        let rsa = TestKey::rsa(false);
        let ec = TestKey::ec();
        let fixture = Fixture::new(&[(&rsa, "rsa"), (&ec, "ec")]);
        let unsigned = |alg: &str| {
            format!(
                "{}.{}.",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header(alg, "rsa")).unwrap()),
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims()).unwrap())
            )
        };
        // `none` with an empty signature segment is not even well framed;
        // with a signature it is an unsupported algorithm.
        assert_eq!(
            fixture.verify(&unsigned("none")),
            Err(ProviderTokenError::Malformed)
        );
        assert_eq!(
            fixture.verify(&format!("{}AAAA", unsigned("none"))),
            Err(ProviderTokenError::Algorithm)
        );
        // HS256 "signed" with the RSA public key as the HMAC secret (the
        // classic confusion attack) never reaches a key.
        for alg in ["HS256", "HS384", "RS512", "ES384", "EdDSA", "rs256"] {
            let token = rsa.sign(&header(alg, "rsa"), &claims());
            assert_eq!(
                fixture.verify(&token),
                Err(ProviderTokenError::Algorithm),
                "{alg}"
            );
        }
        // The header names an algorithm another key uses.
        let token = rsa.sign(&header("PS256", "rsa"), &claims());
        assert_eq!(
            fixture.verify(&token),
            Err(ProviderTokenError::AlgorithmMismatch)
        );
        let token = ec.sign(&header("RS256", "rsa"), &claims());
        assert_eq!(fixture.verify(&token), Err(ProviderTokenError::Signature));
        let token = rsa.sign(&header("ES256", "ec"), &claims());
        assert_eq!(fixture.verify(&token), Err(ProviderTokenError::Signature));
        let token = rsa.sign(&header("RS256", "ec"), &claims());
        assert_eq!(
            fixture.verify(&token),
            Err(ProviderTokenError::AlgorithmMismatch)
        );
        // Signed by a key that is not the issuer's.
        let other = TestKey::rsa(false);
        let token = other.sign(&header("RS256", "rsa"), &claims());
        assert_eq!(fixture.verify(&token), Err(ProviderTokenError::Signature));
    }

    #[test]
    fn claims_and_validity_fail_closed() {
        let key = TestKey::ec();
        let fixture = Fixture::new(&[(&key, "k")]);
        let verify = |claims: Value| fixture.verify(&key.sign(&header("ES256", "k"), &claims));
        assert_eq!(
            verify(with(|c| c["iss"] = "https://evil.example.com".into())),
            Err(ProviderTokenError::Issuer)
        );
        assert_eq!(
            verify(with(|c| c["aud"] = "someone-else".into())),
            Err(ProviderTokenError::Audience)
        );
        // An array is fine when one entry matches.
        verify(with(|c| c["aud"] = serde_json::json!(["x", "other"]))).unwrap();
        assert_eq!(
            verify(with(|c| c["aud"] = serde_json::json!(["x", 1]))),
            Err(ProviderTokenError::Malformed)
        );
        assert_eq!(
            verify(with(|c| {
                c.as_object_mut().unwrap().remove("aud");
            })),
            Err(ProviderTokenError::Malformed)
        );
        assert_eq!(
            verify(with(|c| {
                c["iat"] = (NOW - 400).into();
                c["exp"] = (NOW - 1).into();
            })),
            Err(ProviderTokenError::Expired)
        );
        // Expiry has no leeway.
        assert_eq!(
            verify(with(|c| {
                c["iat"] = (NOW - 400).into();
                c["exp"] = NOW.into();
            })),
            Err(ProviderTokenError::Expired)
        );
        assert_eq!(
            verify(with(|c| c["nbf"] = (NOW + 61).into())),
            Err(ProviderTokenError::NotYetValid)
        );
        verify(with(|c| c["nbf"] = (NOW + 60).into())).unwrap();
        assert_eq!(
            verify(with(|c| c["iat"] = (NOW + 61).into())),
            Err(ProviderTokenError::NotYetValid)
        );
        assert_eq!(DEFAULT_PROVIDER_TOKEN_LIFETIME_SECONDS, 3600);
        verify(with(|c| {
            c["iat"] = NOW.into();
            c["exp"] = (NOW + DEFAULT_PROVIDER_TOKEN_LIFETIME_SECONDS).into();
        }))
        .unwrap();
        assert_eq!(
            verify(with(|c| {
                c["iat"] = NOW.into();
                c["exp"] = (NOW + DEFAULT_PROVIDER_TOKEN_LIFETIME_SECONDS + 1).into();
            })),
            Err(ProviderTokenError::Lifetime)
        );
        for missing in ["iat", "exp", "sub", "iss"] {
            assert_eq!(
                verify(with(|c| {
                    c.as_object_mut().unwrap().remove(missing);
                })),
                Err(ProviderTokenError::Malformed),
                "{missing}"
            );
        }
        assert_eq!(
            verify(with(|c| c["exp"] = serde_json::json!(1.8e9))),
            Err(ProviderTokenError::Malformed)
        );
        assert_eq!(
            verify(with(|c| c["exp"] = "1800000300".into())),
            Err(ProviderTokenError::Malformed)
        );
        assert_eq!(
            verify(with(|c| c["groups"] = serde_json::json!({"a": 1}))),
            Err(ProviderTokenError::Malformed)
        );
        // Roles: a space-separated string, no roles claim, nothing mapped.
        let roles = |claims: Value| verify(claims).unwrap().roles;
        assert_eq!(
            roles(with(|c| c["groups"] = "data-admins data-readers".into())),
            BTreeSet::from(["data.admin".to_string(), "data.read".to_string()])
        );
        assert!(roles(with(|c| {
            c.as_object_mut().unwrap().remove("groups");
        }))
        .is_empty());
        assert!(roles(with(|c| c["groups"] = serde_json::json!(["nobody"]))).is_empty());
        // `amr` and `jti` are optional.
        let bare = verify(with(|c| {
            let c = c.as_object_mut().unwrap();
            c.remove("amr");
            c.remove("jti");
        }))
        .unwrap();
        assert!(bare.amr.is_empty());
        assert_eq!(bare.jti, None);
    }

    #[test]
    fn headers_and_keys_fail_closed() {
        let key = TestKey::rsa(false);
        let fixture = Fixture::new(&[(&key, "k")]);
        let sign = |header: Value| key.sign(&header, &claims());
        for (member, value) in [
            ("crit", serde_json::json!(["exp"])),
            ("jku", "https://evil.example.com/jwks".into()),
            ("jwk", key.jwk("k", true)),
            ("x5u", "https://evil.example.com/x5u".into()),
            ("x5c", serde_json::json!(["AAAA"])),
            ("cty", "JWT".into()),
            ("b64", false.into()),
        ] {
            let mut h = header("RS256", "k");
            h[member] = value;
            assert_eq!(
                fixture.verify(&sign(h)),
                Err(ProviderTokenError::Header),
                "{member}"
            );
        }
        let mut h = header("RS256", "k");
        h["x5t"] = "thumb".into();
        fixture.verify(&sign(h)).unwrap();
        let no_kid = serde_json::json!({"alg": "RS256"});
        assert_eq!(
            fixture.verify(&sign(no_kid)),
            Err(ProviderTokenError::Header)
        );
        assert_eq!(
            fixture.verify(&sign(header("RS256", "unknown"))),
            Err(ProviderTokenError::UnknownKey)
        );
        // JWK sets: private material, other uses and curves, duplicate kids.
        let mut private = key.jwk("p", true);
        private["d"] = "AAAA".into();
        let mut enc = key.jwk("enc", true);
        enc["use"] = "enc".into();
        let mut ops = key.jwk("ops", true);
        ops["key_ops"] = serde_json::json!(["encrypt"]);
        let mut p384 = TestKey::ec().jwk("p384", true);
        p384["crv"] = "P-384".into();
        let hmac = serde_json::json!({"kty": "oct", "kid": "h", "k": "c2VjcmV0"});
        let set = serde_json::json!({"keys": [
            private, enc, ops, p384, hmac,
            key.jwk("dup", true), TestKey::ec().jwk("dup", true),
            key.jwk("ok", true), "not an object",
        ]});
        let keys = parse_jwk_set(&set).unwrap();
        assert_eq!(
            keys.iter().map(ProviderKey::kid).collect::<Vec<_>>(),
            ["ok"]
        );
        // Round trip through the minimal form.
        let again = parse_jwk_set(&serde_json::json!({"keys": [keys[0].to_jwk()]})).unwrap();
        assert_eq!(again, keys);
        let too_many = serde_json::json!({"keys": vec![key.jwk("x", true); MAX_JWKS_KEYS + 1]});
        assert_eq!(parse_jwk_set(&too_many), Err(JwkSetError::TooManyKeys));
        assert_eq!(
            parse_jwk_set(&serde_json::json!([])),
            Err(JwkSetError::Malformed)
        );
    }

    #[test]
    fn malformed_framing_is_refused() {
        let key = TestKey::ec();
        let fixture = Fixture::new(&[(&key, "k")]);
        let good = key.sign(&header("ES256", "k"), &claims());
        fixture.verify(&good).unwrap();
        let parts: Vec<&str> = good.split('.').collect();
        let cases = [
            format!("{good}.extra"),
            format!("{}.{}", parts[0], parts[1]),
            format!("{}..{}", parts[0], parts[2]),
            format!("{}={}", good, ""),
            good.replacen('.', "+", 1),
            format!("{}.{}.{}=", parts[0], parts[1], parts[2]),
            format!(" {good}"),
            String::new(),
        ];
        for case in &cases {
            assert!(
                matches!(
                    fixture.verify(case),
                    Err(ProviderTokenError::Malformed | ProviderTokenError::Size)
                ),
                "{case}"
            );
        }
        // Non-canonical base64url: nonzero trailing bits in the last char.
        let mut sig = parts[2].to_string();
        let last = sig.pop().unwrap();
        sig.push(if last == 'A' { 'B' } else { 'A' });
        let tweaked = format!("{}.{}.{sig}", parts[0], parts[1]);
        assert!(fixture.verify(&tweaked).is_err());
        // Duplicate members.
        let dup_header = r#"{"alg":"ES256","kid":"k","alg":"RS256"}"#;
        let token = format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(dup_header),
            parts[1],
            parts[2]
        );
        assert_eq!(fixture.verify(&token), Err(ProviderTokenError::Malformed));
        // Oversized: the whole token, the header segment, the signature.
        assert_eq!(
            fixture.verify(&"a".repeat(MAX_PROVIDER_TOKEN_BYTES + 1)),
            Err(ProviderTokenError::Size)
        );
        let big_header = format!("{}.{}.{}", "A".repeat(2000), parts[1], parts[2]);
        assert_eq!(fixture.verify(&big_header), Err(ProviderTokenError::Size));
        let big_sig = format!("{}.{}.{}", parts[0], parts[1], "A".repeat(2000));
        assert_eq!(fixture.verify(&big_sig), Err(ProviderTokenError::Size));
        // A JSON array, not an object.
        let array = format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode("[1]"),
            parts[1],
            parts[2]
        );
        assert_eq!(fixture.verify(&array), Err(ProviderTokenError::Malformed));
        assert_eq!(peek_issuer(&good).unwrap(), ISS);
        assert_eq!(peek_issuer("x.y"), Err(ProviderTokenError::Malformed));
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

        /// Arbitrary input never panics and never verifies.
        #[test]
        fn arbitrary_strings_never_verify(input in "[A-Za-z0-9_.=+/-]{0,600}") {
            let fixture = Fixture::new(&[]);
            proptest::prop_assert!(fixture.verify(&input).is_err());
            let _ = peek_issuer(&input);
        }

        /// Any single-byte change to a valid token is refused.
        #[test]
        fn mutated_tokens_never_verify(position in 0usize..4096, byte in 0x21u8..0x7f) {
            use std::sync::OnceLock;
            static FIXTURE: OnceLock<(String, Vec<ProviderKey>)> = OnceLock::new();
            let (token, keys) = FIXTURE.get_or_init(|| {
                let key = TestKey::ec();
                let keys = parse_jwk_set(&serde_json::json!({"keys": [key.jwk("k", true)]})).unwrap();
                (key.sign(&header("ES256", "k"), &claims()), keys)
            });
            let mut bytes = token.clone().into_bytes();
            let position = position % bytes.len();
            proptest::prop_assume!(bytes[position] != byte);
            bytes[position] = byte;
            let mutated = String::from_utf8(bytes).unwrap();
            let mut fixture = Fixture::new(&[]);
            fixture.keys = keys.clone();
            proptest::prop_assert!(fixture.verify(&mutated).is_err());
        }
    }
}
