//! Edge-minted on-behalf-of tokens: the per-call JWT an edge (a gateway, an
//! HTTP front end) signs with its own node key for a user it authenticated
//! itself (session cookie, API key, WebAuthn). Design:
//! `docs/_internal/web_gateway/gateway-identity.md`, "Edge-minted tokens".
//!
//! The token is a compact JWS from [`super::jws`], the same profile as every
//! Aster credential (`EdDSA`, header exactly `alg` + `typ`, canonical
//! base64url, no duplicate JSON members), with `typ`
//! [`EDGE_TOKEN_JWT_TYPE`] and this payload:
//!
//! | Claim | Meaning |
//! |---|---|
//! | `iss` | The edge's node id (64 lowercase hex). Must equal the transport peer |
//! | `sub` | The user, as the edge names them |
//! | `roles` | The user's roles; each must be in the edge's issuable set |
//! | `amr` | How the user signed in (RFC 8176 style: `webauthn`, `pwd`, `apikey`, …) |
//! | `iat`, `nbf` (optional), `exp` | Validity; `exp - iat` is bounded |
//! | `jti` | Unique id, for the audit |
//!
//! Verification is pure: no I/O, no clock read, no key lookup. The caller
//! supplies the transport peer's key and the issuable roles from the edge's
//! verified enrollment credential. Every check fails closed.

use std::collections::BTreeSet;
use std::fmt;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};

use super::jws;
use super::links::{parse_key, some, valid_jti};
use super::MAX_LEEWAY_SECONDS;

/// The JWS `typ` of an edge-minted token.
pub const EDGE_TOKEN_JWT_TYPE: &str = "aster-on-behalf-of-v1+jwt";
/// Largest token accepted, checked before any decoding.
pub const MAX_EDGE_TOKEN_BYTES: usize = 4096;
/// Default tolerance for the edge's clock running ahead: applies to `iat` and
/// `nbf` only. Expiry is never extended, as for delegation chains.
pub const DEFAULT_EDGE_TOKEN_LEEWAY_SECONDS: u64 = 60;
/// Longest `exp - iat` an edge-minted token may carry. Configurable down only.
pub const MAX_EDGE_TOKEN_LIFETIME_SECONDS: u64 = 300;
/// Most roles one token may carry.
pub const MAX_EDGE_TOKEN_ROLES: usize = 64;
/// Most `amr` entries one token may carry.
pub const MAX_EDGE_TOKEN_AMR: usize = 16;
/// Longest `sub`.
pub const MAX_SUBJECT_BYTES: usize = 256;
/// Longest role or `amr` entry.
pub const MAX_ROLE_BYTES: usize = 128;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    iss: String,
    sub: String,
    roles: Vec<String>,
    amr: Vec<String>,
    iat: u64,
    #[serde(
        default,
        deserialize_with = "some",
        skip_serializing_if = "Option::is_none"
    )]
    nbf: Option<u64>,
    exp: u64,
    jti: String,
}

/// Why a token was refused or could not be minted. Each variant names the
/// failing check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeTokenError {
    /// The limits are outside the documented caps.
    Configuration,
    /// Larger than [`MAX_EDGE_TOKEN_BYTES`].
    Size,
    /// Not a well-formed token: segments, base64url, JSON, or a claim value
    /// outside its bounds (empty, too long, a control character, a comma in
    /// a role, a duplicate role or `amr` entry).
    Malformed,
    /// A header that is not exactly `{"alg":"EdDSA","typ":…}`, or the wrong
    /// `typ`. Provider tokens (`RS256`, `kid`, …) land here.
    Header,
    /// `iss` is not the transport peer.
    IssuerMismatch,
    /// The signature does not verify under the transport peer's key.
    Signature,
    /// `iat` or `nbf` is later than `now + leeway`.
    NotYetValid,
    /// `now >= exp`.
    Expired,
    /// `exp <= iat`, `nbf >= exp`, or `exp - iat` above the limit.
    Lifetime,
    /// A role outside the edge's issuable set, or a framework (`aster.`)
    /// role, which no delegation can make issuable.
    RoleNotIssuable,
    /// Minting could not sign or draw randomness.
    Issuance,
}

impl fmt::Display for EdgeTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Configuration => "limits out of range",
            Self::Size => "token too large",
            Self::Malformed => "malformed token",
            Self::Header => "unsupported token header or type",
            Self::IssuerMismatch => "iss is not the transport peer",
            Self::Signature => "signature does not verify",
            Self::NotYetValid => "token not yet valid",
            Self::Expired => "token expired",
            Self::Lifetime => "token lifetime out of range",
            Self::RoleNotIssuable => "role outside the issuer delegation",
            Self::Issuance => "token issuance failed",
        };
        write!(f, "on-behalf-of token rejected: {reason}")
    }
}

impl std::error::Error for EdgeTokenError {}

/// The verifier's clock and bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeTokenLimits {
    /// Trusted UTC seconds.
    pub now: u64,
    /// Applies to `iat` and `nbf` only. At most [`MAX_LEEWAY_SECONDS`].
    pub leeway: u64,
    /// Longest `exp - iat`; 1..=[`MAX_EDGE_TOKEN_LIFETIME_SECONDS`].
    pub max_lifetime: u64,
}

impl EdgeTokenLimits {
    /// The defaults at `now`.
    pub fn at(now: u64) -> Self {
        Self {
            now,
            leeway: DEFAULT_EDGE_TOKEN_LEEWAY_SECONDS,
            max_lifetime: MAX_EDGE_TOKEN_LIFETIME_SECONDS,
        }
    }

    fn check(&self) -> Result<(), EdgeTokenError> {
        let ok = self.leeway <= MAX_LEEWAY_SECONDS
            && (1..=MAX_EDGE_TOKEN_LIFETIME_SECONDS).contains(&self.max_lifetime);
        ok.then_some(()).ok_or(EdgeTokenError::Configuration)
    }
}

/// What to mint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeTokenRequest {
    pub subject: String,
    pub roles: BTreeSet<String>,
    pub amr: Vec<String>,
    pub issued_at: u64,
    pub not_before: Option<u64>,
    pub expires_at: u64,
}

/// A token that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedEdgeToken {
    /// The edge's key, equal to the transport peer's.
    pub issuer: [u8; 32],
    pub subject: String,
    pub roles: BTreeSet<String>,
    pub amr: Vec<String>,
    pub issued_at: u64,
    pub not_before: Option<u64>,
    pub expires_at: u64,
    pub jti: String,
    /// Every claim of the token (`iss`, `sub`, `roles`, `amr`, `iat`, `nbf`
    /// when set, `exp`, `jti`), as signed.
    pub claims: std::sync::Arc<serde_json::Map<String, serde_json::Value>>,
}

/// Whether `role` is one of the framework's (`aster.` prefix, ASCII case
/// ignored). Those are granted only by a node's own credential: never by an
/// identity provider's `role_map`, and never through an issuer delegation, so
/// an edge-minted token carrying one is refused however the delegation reads.
pub fn is_framework_role(role: &str) -> bool {
    role.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("aster."))
}

/// A role as Gate 3 compares it: non-empty, bounded, no control character,
/// no comma (roles are comma-joined into `aster.role`), no surrounding space.
pub(super) fn valid_role(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ROLE_BYTES
        && value.trim() == value
        && !value.contains(',')
        && !value.chars().any(char::is_control)
}

pub(super) fn valid_subject(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_SUBJECT_BYTES && !value.chars().any(char::is_control)
}

/// Bounds on values, shared by minting and verification.
fn check_values(
    subject: &str,
    roles: &[&str],
    amr: &[&str],
    jti: &str,
) -> Result<(), EdgeTokenError> {
    let unique = |values: &[&str]| values.iter().collect::<BTreeSet<_>>().len() == values.len();
    let ok = valid_subject(subject)
        && roles.len() <= MAX_EDGE_TOKEN_ROLES
        && roles.iter().all(|role| valid_role(role))
        && unique(roles)
        && !amr.is_empty()
        && amr.len() <= MAX_EDGE_TOKEN_AMR
        && amr.iter().all(|method| valid_role(method))
        && unique(amr)
        && valid_jti(jti);
    ok.then_some(()).ok_or(EdgeTokenError::Malformed)
}

/// `exp > iat`, `nbf < exp`, `exp - iat <= max_lifetime`, then
/// `now < exp` and `max(iat, nbf) <= now + leeway`. Leeway never extends
/// expiry.
pub(super) fn check_times(
    issued_at: u64,
    not_before: Option<u64>,
    expires_at: u64,
    limits: &EdgeTokenLimits,
) -> Result<(), EdgeTokenError> {
    if expires_at <= issued_at
        || not_before.is_some_and(|nbf| nbf >= expires_at)
        || expires_at - issued_at > limits.max_lifetime
    {
        return Err(EdgeTokenError::Lifetime);
    }
    if limits.now >= expires_at {
        return Err(EdgeTokenError::Expired);
    }
    let starts = not_before.map_or(issued_at, |nbf| nbf.max(issued_at));
    if starts > limits.now.saturating_add(limits.leeway) {
        return Err(EdgeTokenError::NotYetValid);
    }
    Ok(())
}

/// Verify `token` as an edge-minted token from `peer`, the transport-
/// authenticated key of the caller, whose verified credential lets it issue
/// `issuable` roles.
///
/// Order: limits, size, JWS framing and header, payload bounds, `iss` equal to
/// `peer`, signature, validity, roles within `issuable`.
pub fn verify_edge_token(
    token: &str,
    peer: &[u8; 32],
    issuable: &BTreeSet<String>,
    limits: &EdgeTokenLimits,
) -> Result<VerifiedEdgeToken, EdgeTokenError> {
    limits.check()?;
    if token.len() > MAX_EDGE_TOKEN_BYTES {
        return Err(EdgeTokenError::Size);
    }
    let compact = jws::decode(token).map_err(|error| match error {
        jws::JwsError::Header => EdgeTokenError::Header,
        _ => EdgeTokenError::Malformed,
    })?;
    if compact.typ != EDGE_TOKEN_JWT_TYPE {
        return Err(EdgeTokenError::Header);
    }
    let payload: Payload =
        serde_json::from_slice(&compact.payload).map_err(|_| EdgeTokenError::Malformed)?;
    let roles: Vec<&str> = payload.roles.iter().map(String::as_str).collect();
    let amr: Vec<&str> = payload.amr.iter().map(String::as_str).collect();
    check_values(&payload.sub, &roles, &amr, &payload.jti)?;
    // `iss` must be the canonical hex of the transport peer's key. Comparing
    // the canonical form first means a key in another encoding never reaches
    // signature verification.
    let issuer = parse_key(&payload.iss).ok_or(EdgeTokenError::IssuerMismatch)?;
    if issuer != *peer {
        return Err(EdgeTokenError::IssuerMismatch);
    }
    compact
        .verify(peer)
        .map_err(|_| EdgeTokenError::Signature)?;
    check_times(payload.iat, payload.nbf, payload.exp, limits)?;
    if !roles
        .iter()
        .all(|role| issuable.contains(*role) && !is_framework_role(role))
    {
        return Err(EdgeTokenError::RoleNotIssuable);
    }
    let claims = match serde_json::to_value(&payload) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => return Err(EdgeTokenError::Malformed),
    };
    Ok(VerifiedEdgeToken {
        claims: std::sync::Arc::new(claims),
        issuer,
        subject: payload.sub,
        roles: payload.roles.into_iter().collect(),
        amr: payload.amr,
        issued_at: payload.iat,
        not_before: payload.nbf,
        expires_at: payload.exp,
        jti: payload.jti,
    })
}

/// Mint a token signed with `seed` (the edge's Ed25519 secret key; not
/// retained). Refuses anything [`verify_edge_token`] would reject for this
/// issuer at `limits.now`: a role outside `issuable`, a lifetime above
/// `limits.max_lifetime`, an `nbf` later than `now + leeway`, bad values.
pub fn mint_edge_token(
    seed: &[u8; 32],
    request: &EdgeTokenRequest,
    issuable: &BTreeSet<String>,
    limits: &EdgeTokenLimits,
) -> Result<String, EdgeTokenError> {
    limits.check()?;
    let issuer = jws::public_key(seed).map_err(|_| EdgeTokenError::Issuance)?;
    let mut jti = [0; 16];
    getrandom::getrandom(&mut jti).map_err(|_| EdgeTokenError::Issuance)?;
    let payload = Payload {
        iss: hex::encode(issuer),
        sub: request.subject.clone(),
        roles: request.roles.iter().cloned().collect(),
        amr: request.amr.clone(),
        iat: request.issued_at,
        nbf: request.not_before,
        exp: request.expires_at,
        jti: URL_SAFE_NO_PAD.encode(jti),
    };
    let token = jws::sign(
        EDGE_TOKEN_JWT_TYPE,
        &serde_json::to_vec(&payload).map_err(|_| EdgeTokenError::Issuance)?,
        seed,
    )
    .map_err(|_| EdgeTokenError::Issuance)?;
    // The verifier's own gates over the result.
    verify_edge_token(&token, &issuer, issuable, limits)?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn roles(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    fn request(roles_: &[&str]) -> EdgeTokenRequest {
        EdgeTokenRequest {
            subject: "alice@example.com".into(),
            roles: roles(roles_),
            amr: vec!["webauthn".into()],
            issued_at: NOW,
            not_before: None,
            expires_at: NOW + 120,
        }
    }

    fn key(seed: u8) -> [u8; 32] {
        jws::public_key(&[seed; 32]).unwrap()
    }

    /// Sign an arbitrary payload, bypassing minting's own checks.
    fn forge(seed: u8, payload: serde_json::Value) -> String {
        jws::sign(
            EDGE_TOKEN_JWT_TYPE,
            &serde_json::to_vec(&payload).unwrap(),
            &[seed; 32],
        )
        .unwrap()
    }

    fn payload(seed: u8) -> serde_json::Value {
        serde_json::json!({
            "iss": hex::encode(key(seed)),
            "sub": "bob",
            "roles": ["data.read"],
            "amr": ["pwd"],
            "iat": NOW,
            "exp": NOW + 60,
            "jti": "AAAAAAAAAAAAAAAAAAAAAA",
        })
    }

    #[test]
    fn mint_and_verify_round_trip() {
        let issuable = roles(&["data.read", "data.write"]);
        let limits = EdgeTokenLimits::at(NOW);
        let token =
            mint_edge_token(&[7; 32], &request(&["data.read"]), &issuable, &limits).expect("mint");
        let verified = verify_edge_token(&token, &key(7), &issuable, &limits).unwrap();
        assert_eq!(verified.issuer, key(7));
        assert_eq!(verified.subject, "alice@example.com");
        assert_eq!(verified.roles, roles(&["data.read"]));
        assert_eq!(verified.amr, ["webauthn"]);
        assert_eq!(verified.jti.len(), 22);
    }

    #[test]
    fn minting_refuses_what_verification_would() {
        let issuable = roles(&["data.read"]);
        let limits = EdgeTokenLimits::at(NOW);
        let mint = |r: &EdgeTokenRequest| mint_edge_token(&[7; 32], r, &issuable, &limits);
        assert_eq!(
            mint(&request(&["data.admin"])),
            Err(EdgeTokenError::RoleNotIssuable)
        );
        // A framework role, even one the delegation lists.
        let framework = roles(&["data.read", "aster.gateway", "Aster.Config-Admin"]);
        for role in ["aster.gateway", "Aster.Config-Admin"] {
            assert_eq!(
                mint_edge_token(&[7; 32], &request(&[role]), &framework, &limits),
                Err(EdgeTokenError::RoleNotIssuable),
                "{role}"
            );
        }
        let mut long = request(&["data.read"]);
        long.expires_at = NOW + MAX_EDGE_TOKEN_LIFETIME_SECONDS + 1;
        assert_eq!(mint(&long), Err(EdgeTokenError::Lifetime));
        let mut no_amr = request(&[]);
        no_amr.amr.clear();
        assert_eq!(mint(&no_amr), Err(EdgeTokenError::Malformed));
        let mut comma = request(&[]);
        comma.roles = roles(&["a,b"]);
        assert_eq!(mint(&comma), Err(EdgeTokenError::Malformed));
        let bad_limits = EdgeTokenLimits {
            max_lifetime: MAX_EDGE_TOKEN_LIFETIME_SECONDS + 1,
            ..limits
        };
        assert_eq!(
            mint_edge_token(&[7; 32], &request(&[]), &issuable, &bad_limits),
            Err(EdgeTokenError::Configuration)
        );
    }

    #[test]
    fn verification_fails_closed() {
        let issuable = roles(&["data.read"]);
        let limits = EdgeTokenLimits::at(NOW);
        let verify =
            |token: &str, peer: u8| verify_edge_token(token, &key(peer), &issuable, &limits);
        let good = forge(7, payload(7));
        verify(&good, 7).unwrap();
        // Another peer replaying it, or a token naming another issuer.
        assert_eq!(verify(&good, 8), Err(EdgeTokenError::IssuerMismatch));
        // Signed by 8 but claiming 7.
        assert_eq!(
            verify(&forge(8, payload(7)), 7),
            Err(EdgeTokenError::Signature)
        );
        // Times.
        let with = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut p = payload(7);
            f(&mut p);
            forge(7, p)
        };
        assert_eq!(
            verify(&with(&|p| p["exp"] = (NOW - 1).into()), 7),
            Err(EdgeTokenError::Lifetime)
        );
        assert_eq!(
            verify(
                &with(&|p| {
                    p["iat"] = (NOW - 120).into();
                    p["exp"] = NOW.into();
                }),
                7
            ),
            Err(EdgeTokenError::Expired)
        );
        assert_eq!(
            verify(
                &with(&|p| {
                    p["iat"] = (NOW + 61).into();
                    p["exp"] = (NOW + 200).into();
                }),
                7
            ),
            Err(EdgeTokenError::NotYetValid)
        );
        // Within leeway is fine.
        verify(
            &with(&|p| {
                p["iat"] = (NOW + 60).into();
                p["exp"] = (NOW + 200).into();
            }),
            7,
        )
        .unwrap();
        assert_eq!(
            verify(
                &with(&|p| {
                    p["nbf"] = (NOW + 61).into();
                    p["exp"] = (NOW + 200).into();
                }),
                7
            ),
            Err(EdgeTokenError::NotYetValid)
        );
        assert_eq!(
            verify(
                &with(&|p| p["exp"] = (NOW + MAX_EDGE_TOKEN_LIFETIME_SECONDS + 1).into()),
                7
            ),
            Err(EdgeTokenError::Lifetime)
        );
        // Roles and values.
        assert_eq!(
            verify(
                &with(&|p| p["roles"] = serde_json::json!(["data.admin"])),
                7
            ),
            Err(EdgeTokenError::RoleNotIssuable)
        );
        // A framework role is refused even when the delegation lists it.
        let framework = roles(&["data.read", "aster.gateway"]);
        assert_eq!(
            verify_edge_token(
                &with(&|p| p["roles"] = serde_json::json!(["aster.gateway"])),
                &key(7),
                &framework,
                &limits
            ),
            Err(EdgeTokenError::RoleNotIssuable)
        );
        assert_eq!(
            verify(
                &with(&|p| p["roles"] = serde_json::json!(["data.read", "data.read"])),
                7
            ),
            Err(EdgeTokenError::Malformed)
        );
        assert_eq!(
            verify(&with(&|p| p["extra"] = 1.into()), 7),
            Err(EdgeTokenError::Malformed)
        );
        assert_eq!(
            verify(&with(&|p| p["nbf"] = serde_json::Value::Null), 7),
            Err(EdgeTokenError::Malformed)
        );
        assert_eq!(
            verify(
                &with(&|p| p["iss"] = hex::encode(key(7)).to_uppercase().into()),
                7
            ),
            Err(EdgeTokenError::IssuerMismatch)
        );
        // Wrong typ, and an oversized token, before any parsing.
        let other = jws::sign(
            "aster-enrollment-v1+jwt",
            &serde_json::to_vec(&payload(7)).unwrap(),
            &[7; 32],
        )
        .unwrap();
        assert_eq!(verify(&other, 7), Err(EdgeTokenError::Header));
        assert_eq!(
            verify(&"a".repeat(MAX_EDGE_TOKEN_BYTES + 1), 7),
            Err(EdgeTokenError::Size)
        );
        assert_eq!(verify("not.a.token", 7), Err(EdgeTokenError::Malformed));
    }
}
