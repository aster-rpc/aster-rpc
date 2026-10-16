//! Gateway identity: calls an edge (a gateway, an HTTP front end) makes on
//! behalf of a user who holds no Aster key. Design:
//! `docs/_internal/web_gateway/gateway-identity.md`; this is slice 1
//! (edge-minted tokens only).
//!
//! Three questions, each answered once:
//!
//! - **May this edge connect?** Gate 0, its peer identity. Unchanged.
//! - **May it forward users' identities?** The role [`GATEWAY_ROLE`] among
//!   the edge's own Gate-3 roles (from its enrollment credential).
//! - **Who is the user?** One JWT per call in the [`ON_BEHALF_OF_METADATA`]
//!   metadata entry, verified before Gate 3. In this slice the JWT is
//!   edge-minted: signed by the edge's node key (`iss` = the edge's node id,
//!   which must be the transport peer), with roles inside the issuer
//!   delegation its credential carries ([`ISSUABLE_ROLES_ATTRIBUTE`]).
//!
//! A call without the entry is authorised as the peer, as before. A call with
//! it is authorised as the user: Gate 3's `requires` checks the token's
//! roles, as they are (no intersection with the edge's roles), and the
//! handler's [`CallContext`](super::CallContext) reports
//! `principal()` = `iss|sub`, `actor()` = the edge, `auth_methods()` = `amr`.
//!
//! Refusals fail closed:
//!
//! | Case | Status |
//! |---|---|
//! | The server does not accept the entry (`[identity] accept_on_behalf_of = false`, the default) | `PERMISSION_DENIED` |
//! | The transport did not authenticate the peer (HTTP, WebTransport) | `PERMISSION_DENIED` |
//! | The peer lacks `aster.gateway` | `PERMISSION_DENIED` |
//! | The entry repeats, or the token does not verify (malformed, wrong type, `iss` not the peer, bad signature, expired, not yet valid, lifetime above 5 min) | `UNAUTHENTICATED` |
//! | The peer's credential has no issuer delegation, or the token claims a role outside it | `PERMISSION_DENIED` |
//!
//! There is no per-call audit hook: an accepted call records `principal`,
//! `actor`, `obo_iss` and `obo_jti` on its `rpc` span (OTel:
//! `aster.principal`, `aster.actor`, `aster.on_behalf_of.iss`,
//! `aster.on_behalf_of.jti`) and logs one debug event under
//! [`IDENTITY`](crate::log_targets::IDENTITY). Token contents beyond `iss`,
//! `sub` and `jti` are never logged.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use super::auth::{AttributeStore, ROLE_ATTRIBUTE};
use super::codec::{RpcStatus, StreamHeader};
use super::server::PeerProvenance;
use super::status::StatusCode;
use crate::log_targets::IDENTITY;

/// The call-metadata key that carries the on-behalf-of JWT. Matched ASCII
/// case-insensitively, so no spelling of it slips past the check.
pub const ON_BEHALF_OF_METADATA: &str = "aster-on-behalf-of";
/// The role that lets a peer forward users' identities.
pub const GATEWAY_ROLE: &str = "aster.gateway";
/// The enrollment-credential attribute that is an edge's issuer delegation:
/// a string of comma-separated roles the edge may put in a token it mints,
/// e.g. `"data.read,data.admin"`. Signed by the edge's issuer, so a delegated
/// issuer needs it in its delegation's `attribute_keys`
/// (`aster enroll delegate --attribute-key aster.identity.issuable_roles`).
/// Read only from the verified credential, never from runtime attributes.
pub const ISSUABLE_ROLES_ATTRIBUTE: &str = "aster.identity.issuable_roles";
/// Default tolerance for the edge's clock running ahead (`iat`, `nbf` only;
/// expiry is never extended). Fixed in this slice; configurable later.
pub const DEFAULT_CLOCK_LEEWAY: Duration = Duration::from_secs(60);
/// Longest lifetime (`exp - iat`) of an edge-minted token. Fixed in this
/// slice; configurable (down) later.
pub const MAX_EDGE_TOKEN_LIFETIME: Duration = Duration::from_secs(300);

/// A forwarded user identity, verified for one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardedIdentity {
    principal: String,
    actor: String,
    issuer: String,
    subject: String,
    roles: Vec<String>,
    auth_methods: Vec<String>,
    jti: String,
    expires_at: u64,
}

impl ForwardedIdentity {
    /// `iss|sub`: the user's stable identity.
    pub fn principal(&self) -> &str {
        &self.principal
    }
    /// The edge that forwarded the call (its node id, the transport peer).
    pub fn actor(&self) -> &str {
        &self.actor
    }
    /// The token's `iss`. For an edge-minted token, the edge's node id.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }
    /// The token's `sub`.
    pub fn subject(&self) -> &str {
        &self.subject
    }
    /// The token's roles, sorted: what Gate 3 checked.
    pub fn roles(&self) -> &[String] {
        &self.roles
    }
    /// The token's `amr`: how the user signed in.
    pub fn auth_methods(&self) -> &[String] {
        &self.auth_methods
    }
    /// The token's `jti`.
    pub fn jti(&self) -> &str {
        &self.jti
    }
    /// The token's `exp`, in Unix seconds.
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// The Gate-3 attributes of the forwarded call: the token's roles only.
    /// The edge's own attributes describe the edge, not the user.
    pub(crate) fn attributes(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if !self.roles.is_empty() {
            out.insert(ROLE_ATTRIBUTE.to_string(), self.roles.join(","));
        }
        out
    }
}

/// The `[identity]` settings a server has in force: the file and environment
/// values, with any `aster.ops.NodeConfig` override applied. Slice 2 adds the
/// trusted issuers here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentitySettings {
    /// Accept the `aster-on-behalf-of` entry (`[identity] accept_on_behalf_of`).
    pub accept_on_behalf_of: bool,
}

/// The [`IdentitySettings`] every dispatcher of one server reads, per call.
/// Swapped atomically by `aster.ops.NodeConfig`: a call routed after
/// [`store`](Self::store) returns sees the new settings, with no restart.
#[derive(Debug, Default)]
pub(crate) struct LiveIdentity {
    current: arc_swap::ArcSwap<IdentitySettings>,
}

impl LiveIdentity {
    pub(crate) fn load(&self) -> IdentitySettings {
        IdentitySettings::clone(&self.current.load())
    }

    pub(crate) fn store(&self, settings: IdentitySettings) {
        self.current.store(std::sync::Arc::new(settings));
    }

    /// The verification policy in force, `None` when the entry is refused.
    pub(crate) fn policy(&self) -> Option<OnBehalfOfPolicy> {
        self.current
            .load()
            .accept_on_behalf_of
            .then(OnBehalfOfPolicy::default)
    }
}

/// How a server verifies `aster-on-behalf-of`. Present only when the server
/// accepts the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OnBehalfOfPolicy {
    pub(crate) leeway: Duration,
    pub(crate) max_lifetime: Duration,
}

impl Default for OnBehalfOfPolicy {
    fn default() -> Self {
        Self {
            leeway: DEFAULT_CLOCK_LEEWAY,
            max_lifetime: MAX_EDGE_TOKEN_LIFETIME,
        }
    }
}

/// Parse an [`ISSUABLE_ROLES_ATTRIBUTE`] value: comma-separated, trimmed,
/// empty entries dropped.
pub fn parse_issuable_roles(value: &str) -> BTreeSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|role| !role.is_empty())
        .map(str::to_string)
        .collect()
}

fn denied(message: &str) -> RpcStatus {
    RpcStatus::error(StatusCode::PermissionDenied, message)
}

fn unauthenticated(message: &str) -> RpcStatus {
    RpcStatus::error(StatusCode::Unauthenticated, message)
}

/// Whether `attributes` grants `role` in `aster.role`.
fn has_role(attributes: &HashMap<String, String>, role: &str) -> bool {
    attributes
        .get(ROLE_ATTRIBUTE)
        .is_some_and(|roles| roles.split(',').map(str::trim).any(|r| r == role))
}

/// Resolve a call's `aster-on-behalf-of` entry. `Ok(None)`: no entry, the
/// call is authorised as the peer. `peer_attributes` are the peer's own
/// Gate-3 attributes (for the gateway role); `store` supplies its issuer
/// delegation.
pub(crate) fn resolve(
    policy: Option<&OnBehalfOfPolicy>,
    header: &StreamHeader,
    provenance: PeerProvenance,
    transport_peer: &str,
    peer_attributes: &HashMap<String, String>,
    store: &AttributeStore,
) -> Result<Option<ForwardedIdentity>, RpcStatus> {
    let mut entries = header
        .metadata_keys
        .iter()
        .zip(header.metadata_values.iter())
        .filter(|(key, _)| key.eq_ignore_ascii_case(ON_BEHALF_OF_METADATA))
        .map(|(_, value)| value.as_str());
    let Some(token) = entries.next() else {
        return Ok(None);
    };
    let refuse = |status: RpcStatus, reason: &str| {
        tracing::debug!(
            target: IDENTITY,
            actor = transport_peer,
            service = header.service.as_str(),
            method = header.method.as_str(),
            reason,
            "on-behalf-of refused"
        );
        Err(status)
    };
    if entries.next().is_some() {
        return refuse(
            unauthenticated("more than one aster-on-behalf-of entry"),
            "repeated entry",
        );
    }
    let Some(policy) = policy else {
        return refuse(
            denied("this server does not accept aster-on-behalf-of"),
            "not accepted",
        );
    };
    if !provenance.is_authenticated() {
        return refuse(
            denied("aster-on-behalf-of requires a transport-authenticated peer"),
            "unauthenticated transport",
        );
    }
    if !has_role(peer_attributes, GATEWAY_ROLE) {
        return refuse(
            denied("caller may not forward identities (lacks aster.gateway)"),
            "no gateway role",
        );
    }
    verify(policy, token, transport_peer, store).or_else(|(status, reason)| refuse(status, reason))
}

#[cfg(feature = "delegation")]
fn verify(
    policy: &OnBehalfOfPolicy,
    token: &str,
    transport_peer: &str,
    store: &AttributeStore,
) -> Result<Option<ForwardedIdentity>, (RpcStatus, &'static str)> {
    use aster_transport_core::delegation::on_behalf_of::{
        verify_edge_token, EdgeTokenError, EdgeTokenLimits,
    };

    let Some(peer_key) = parse_hex32(transport_peer) else {
        return Err((unauthenticated("invalid transport peer id"), "peer id"));
    };
    let Some(issuable) = store.issuable_roles(transport_peer) else {
        return Err((
            denied("caller's credential carries no issuer delegation"),
            "no issuer delegation",
        ));
    };
    let limits = EdgeTokenLimits {
        now: unix_now(),
        leeway: policy.leeway.as_secs(),
        max_lifetime: policy.max_lifetime.as_secs(),
    };
    let verified = match verify_edge_token(token, &peer_key, &issuable, &limits) {
        Ok(verified) => verified,
        Err(EdgeTokenError::RoleNotIssuable) => {
            return Err((
                denied("on-behalf-of token claims a role outside the issuer delegation"),
                "role not issuable",
            ))
        }
        Err(EdgeTokenError::Configuration) => {
            return Err((
                unauthenticated("on-behalf-of verification misconfigured"),
                "configuration",
            ))
        }
        Err(error) => return Err((unauthenticated(&error.to_string()), reason_of(error))),
    };
    let issuer = hex::encode(verified.issuer);
    let identity = ForwardedIdentity {
        principal: format!("{issuer}|{}", verified.subject),
        actor: transport_peer.to_string(),
        issuer,
        subject: verified.subject,
        roles: verified.roles.into_iter().collect(),
        auth_methods: verified.amr,
        jti: verified.jti,
        expires_at: verified.expires_at,
    };
    tracing::debug!(
        target: IDENTITY,
        principal = identity.principal.as_str(),
        actor = identity.actor.as_str(),
        iss = identity.issuer.as_str(),
        sub = identity.subject.as_str(),
        jti = identity.jti.as_str(),
        "on-behalf-of accepted"
    );
    Ok(Some(identity))
}

#[cfg(feature = "delegation")]
fn reason_of(
    error: aster_transport_core::delegation::on_behalf_of::EdgeTokenError,
) -> &'static str {
    use aster_transport_core::delegation::on_behalf_of::EdgeTokenError as E;
    match error {
        E::Configuration => "configuration",
        E::Size => "size",
        E::Malformed => "malformed",
        E::Header => "header",
        E::IssuerMismatch => "issuer mismatch",
        E::Signature => "signature",
        E::NotYetValid => "not yet valid",
        E::Expired => "expired",
        E::Lifetime => "lifetime",
        E::RoleNotIssuable => "role not issuable",
        E::Issuance => "issuance",
    }
}

/// Without the `delegation` feature there is no JWS verifier: refuse.
#[cfg(not(feature = "delegation"))]
fn verify(
    _policy: &OnBehalfOfPolicy,
    _token: &str,
    _transport_peer: &str,
    _store: &AttributeStore,
) -> Result<Option<ForwardedIdentity>, (RpcStatus, &'static str)> {
    Err((
        unauthenticated("on-behalf-of tokens need the delegation feature"),
        "no verifier",
    ))
}

#[cfg(feature = "delegation")]
fn parse_hex32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    hex::decode(value).ok()?.try_into().ok()
}

#[cfg(feature = "delegation")]
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

// ── Edge side ───────────────────────────────────────────────────────────────

/// Mints edge-minted on-behalf-of tokens, signed with this node's key.
///
/// It refuses roles outside its issuer delegation and lifetimes above
/// [`MAX_EDGE_TOKEN_LIFETIME`], exactly as the receiving core would. Attach a
/// token to a call with
/// [`CallOptions::on_behalf_of`](super::CallOptions::on_behalf_of).
#[cfg(feature = "delegation")]
#[derive(Clone)]
pub struct Issuer {
    secret: crate::SecretKey,
    source: IssuableSource,
}

#[cfg(feature = "delegation")]
#[derive(Clone)]
enum IssuableSource {
    Fixed(BTreeSet<String>),
    /// Read from the current credential at every mint, so a renewal or a
    /// replaced credential applies at once.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    Enrollment(crate::enrollment::Enrollment),
}

#[cfg(feature = "delegation")]
impl Issuer {
    /// An issuer with an explicit issuable-role set. The receiving core still
    /// checks the roles against the delegation in this node's credential;
    /// prefer [`from_enrollment`](Self::from_enrollment), which reads it.
    pub fn new<I, S>(secret: crate::SecretKey, issuable_roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            secret,
            source: IssuableSource::Fixed(issuable_roles.into_iter().map(Into::into).collect()),
        }
    }

    /// An issuer for `node`, bounded by the issuer delegation
    /// ([`ISSUABLE_ROLES_ATTRIBUTE`]) in the credential `enrollment` holds,
    /// read at each mint. With an `AsterClient`:
    /// `Issuer::from_enrollment(client.node(), client.enrollment().unwrap())`.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    pub fn from_enrollment(
        node: &crate::Node,
        enrollment: &crate::enrollment::Enrollment,
    ) -> crate::Result<Self> {
        Ok(Self {
            secret: node.export_secret_key()?,
            source: IssuableSource::Enrollment(enrollment.clone()),
        })
    }

    /// This issuer's node id (the tokens' `iss`), 64 hex.
    pub fn issuer_id(&self) -> String {
        hex::encode(self.secret.public_key().to_bytes())
    }

    /// The roles this issuer may put in a token now.
    pub fn issuable_roles(&self) -> BTreeSet<String> {
        match &self.source {
            IssuableSource::Fixed(roles) => roles.clone(),
            #[cfg(all(feature = "enrollment", feature = "docs"))]
            IssuableSource::Enrollment(enrollment) => enrollment
                .credential()
                .and_then(|(_, evidence)| issuable_from_claims(evidence.claims()))
                .unwrap_or_default(),
        }
    }

    /// Mint a token for user `sub` with `roles`, signed in by `amr` (e.g.
    /// `["webauthn"]`; at least one entry), valid for `ttl` from now.
    ///
    /// Refused with [`Error::InvalidArgument`](crate::Error::InvalidArgument)
    /// when a role is outside [`issuable_roles`](Self::issuable_roles),
    /// `ttl` is zero or above [`MAX_EDGE_TOKEN_LIFETIME`], or a value is out of
    /// bounds (empty `sub`, a role with a comma, …).
    pub fn mint_user_token<R, RS, A, AS>(
        &self,
        sub: &str,
        roles: R,
        amr: A,
        ttl: Duration,
    ) -> crate::Result<String>
    where
        R: IntoIterator<Item = RS>,
        RS: Into<String>,
        A: IntoIterator<Item = AS>,
        AS: Into<String>,
    {
        use aster_transport_core::delegation::on_behalf_of::{
            mint_edge_token, EdgeTokenLimits, EdgeTokenRequest,
        };
        let invalid = |message: String| crate::Error::InvalidArgument(message);
        if ttl.is_zero() || ttl > MAX_EDGE_TOKEN_LIFETIME {
            return Err(invalid(format!(
                "on-behalf-of token ttl must be in 1s..={}s",
                MAX_EDGE_TOKEN_LIFETIME.as_secs()
            )));
        }
        let now = unix_now();
        let request = EdgeTokenRequest {
            subject: sub.to_string(),
            roles: roles.into_iter().map(Into::into).collect(),
            amr: amr.into_iter().map(Into::into).collect(),
            issued_at: now,
            not_before: None,
            expires_at: now + ttl.as_secs().max(1),
        };
        let limits = EdgeTokenLimits::at(now);
        mint_edge_token(
            &self.secret.to_bytes(),
            &request,
            &self.issuable_roles(),
            &limits,
        )
        .map_err(|error| invalid(error.to_string()))
    }
}

#[cfg(feature = "delegation")]
impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issuer")
            .field("issuer_id", &self.issuer_id())
            .finish_non_exhaustive()
    }
}

/// The issuer delegation in verified admission claims, when present.
#[cfg(all(feature = "delegation", feature = "enrollment"))]
pub(crate) fn issuable_from_claims(
    claims: &crate::enrollment::AdmissionClaims,
) -> Option<BTreeSet<String>> {
    match claims.attributes.as_ref()?.get(ISSUABLE_ROLES_ATTRIBUTE)? {
        crate::enrollment::TypedAttribute::String(value) => Some(parse_issuable_roles(value)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(entries: &[(&str, &str)]) -> StreamHeader {
        StreamHeader {
            service: "S".into(),
            method: "m".into(),
            metadata_keys: entries.iter().map(|(k, _)| k.to_string()).collect(),
            metadata_values: entries.iter().map(|(_, v)| v.to_string()).collect(),
            ..Default::default()
        }
    }

    fn gateway() -> HashMap<String, String> {
        [(ROLE_ATTRIBUTE.to_string(), "x, aster.gateway".to_string())].into()
    }

    fn code(result: Result<Option<ForwardedIdentity>, RpcStatus>) -> StatusCode {
        result.unwrap_err().status_code()
    }

    #[test]
    fn no_entry_is_the_peer_and_refusals_fail_closed() {
        let store = AttributeStore::new();
        let peer = "ab".repeat(32);
        let policy = OnBehalfOfPolicy::default();
        let iroh = PeerProvenance::IrohAuthenticated;
        assert_eq!(
            resolve(None, &header(&[]), iroh, &peer, &HashMap::new(), &store),
            Ok(None)
        );
        let entry = header(&[("Aster-On-Behalf-Of", "t")]);
        assert_eq!(
            code(resolve(None, &entry, iroh, &peer, &gateway(), &store)),
            StatusCode::PermissionDenied
        );
        assert_eq!(
            code(resolve(
                Some(&policy),
                &entry,
                PeerProvenance::Other,
                &peer,
                &gateway(),
                &store
            )),
            StatusCode::PermissionDenied
        );
        assert_eq!(
            code(resolve(
                Some(&policy),
                &entry,
                iroh,
                &peer,
                &HashMap::new(),
                &store
            )),
            StatusCode::PermissionDenied
        );
        let twice = header(&[(ON_BEHALF_OF_METADATA, "t"), (ON_BEHALF_OF_METADATA, "u")]);
        assert_eq!(
            code(resolve(
                Some(&policy),
                &twice,
                iroh,
                &peer,
                &gateway(),
                &store
            )),
            StatusCode::Unauthenticated
        );
        // A gateway without an issuer delegation cannot mint.
        assert_eq!(
            code(resolve(
                Some(&policy),
                &entry,
                iroh,
                &peer,
                &gateway(),
                &store
            )),
            if cfg!(feature = "delegation") {
                StatusCode::PermissionDenied
            } else {
                StatusCode::Unauthenticated
            }
        );
    }

    #[test]
    fn issuable_roles_parse_like_aster_role() {
        assert_eq!(
            parse_issuable_roles(" a, b ,,a"),
            ["a".to_string(), "b".to_string()].into()
        );
    }
}
