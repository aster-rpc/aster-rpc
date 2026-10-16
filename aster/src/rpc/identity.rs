//! Gateway identity: calls an edge (a gateway, an HTTP front end) makes on
//! behalf of a user who holds no Aster key. Design:
//! `docs/_internal/web_gateway/gateway-identity.md`; slices 1 (edge-minted
//! tokens) and 2 (provider tokens).
//!
//! Three questions, each answered once:
//!
//! - **May this edge connect?** Gate 0, its peer identity. Unchanged.
//! - **May it forward users' identities?** The role [`GATEWAY_ROLE`] among
//!   the edge's own Gate-3 roles (from its enrollment credential).
//! - **Who is the user?** One JWT per call in the [`ON_BEHALF_OF_METADATA`]
//!   metadata entry, verified before Gate 3. The token's `iss` chooses how:
//!   - **Edge-minted** (`iss` is a node id, 64 hex): signed by the edge's
//!     node key (`iss` must be the transport peer), with roles inside the
//!     issuer delegation its credential carries ([`ISSUABLE_ROLES_ATTRIBUTE`]),
//!     at most `[identity] edge_token_max_lifetime_secs` long.
//!   - **Provider** (any other `iss`): an identity provider's own token,
//!     forwarded unchanged. `iss` must be a
//!     [`TrustedIssuer`](crate::TrustedIssuer) (`[[identity.trusted_issuers]]`);
//!     the signature must verify (RS256, PS256 or ES256, matching the key's
//!     algorithm) under the issuer key for `kid`, which comes from the fleet
//!     namespace (see [`issuer_keys`](super::issuer_keys)), never from a
//!     fetch; `aud` must be a configured audience; `exp - iat` at most the
//!     issuer's `max_token_lifetime_secs`. Roles come from the issuer's
//!     `roles_claim` through its `role_map`; **unmapped values are dropped**.
//!     `principal` is `iss|sub`; `auth_methods()` is the token's `amr`, empty
//!     when it has none; `jti` is empty when it has none.
//!
//!   Both: `[identity] clock_leeway_secs` (default 60) applies to `iat` and
//!   `nbf`, never to `exp`.
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
//! | The entry repeats, or the token does not verify (malformed, wrong type, `iss` not the peer, bad signature, expired, not yet valid, lifetime above the limit) | `UNAUTHENTICATED` |
//! | Edge-minted: the peer's credential has no issuer delegation, or the token claims a role outside it | `PERMISSION_DENIED` |
//! | Provider: `iss` not trusted, no key for `kid` (no fetch happens), `alg` refused or not the key's, wrong `aud` | `UNAUTHENTICATED` |
//!
//! There is no per-call audit hook: an accepted call records `principal`,
//! `actor`, `obo_iss` and `obo_jti` on its `rpc` span (OTel:
//! `aster.principal`, `aster.actor`, `aster.on_behalf_of.iss`,
//! `aster.on_behalf_of.jti`) and logs one debug event under
//! [`IDENTITY`](crate::log_targets::IDENTITY). Token contents beyond `iss`,
//! `sub`, `jti` and `kid` are never logged.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{
    AsterConfig, TrustedIssuer, DEFAULT_CLOCK_LEEWAY_SECS, DEFAULT_EDGE_TOKEN_MAX_LIFETIME_SECS,
    DEFAULT_MAX_KEY_SET_AGE_SECS,
};

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
/// The `amr` value of a passkey (WebAuthn) sign-in (RFC 8176 has no entry for
/// it; `hwk`/`swk` describe the key, not the ceremony).
pub const AMR_WEBAUTHN: &str = "webauthn";
/// The `amr` value of a password sign-in (RFC 8176).
pub const AMR_PASSWORD: &str = "pwd";
/// The `amr` value an edge uses for an API-key caller.
pub const AMR_API_KEY: &str = "apikey";

/// The token of an HTTP `Authorization: Bearer <token>` header value: the
/// scheme matched case-insensitively, surrounding whitespace dropped. `None`
/// for another scheme or an empty token. An edge forwards it unchanged with
/// [`CallOptions::on_behalf_of`](super::CallOptions::on_behalf_of).
pub fn bearer_token(authorization: &str) -> Option<&str> {
    let value = authorization.trim();
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty() && !token.contains(char::is_whitespace)).then_some(token)
}
/// The enrollment-credential attribute that is an edge's issuer delegation:
/// a string of comma-separated roles the edge may put in a token it mints,
/// e.g. `"data.read,data.admin"`. Signed by the edge's issuer, so a delegated
/// issuer needs it in its delegation's `attribute_keys`
/// (`aster enroll delegate --attribute-key aster.identity.issuable_roles`).
/// Read only from the verified credential, never from runtime attributes.
pub const ISSUABLE_ROLES_ATTRIBUTE: &str = "aster.identity.issuable_roles";
/// Default tolerance for an issuer's clock running ahead (`iat`, `nbf` only;
/// expiry is never extended): `[identity] clock_leeway_secs`, at most
/// [`MAX_CLOCK_LEEWAY`].
pub const DEFAULT_CLOCK_LEEWAY: Duration = Duration::from_secs(DEFAULT_CLOCK_LEEWAY_SECS);
/// The cap on `[identity] clock_leeway_secs`.
pub const MAX_CLOCK_LEEWAY: Duration = Duration::from_secs(300);
/// Longest lifetime (`exp - iat`) of an edge-minted token: the default and the
/// cap of `[identity] edge_token_max_lifetime_secs` (configurable down).
pub const MAX_EDGE_TOKEN_LIFETIME: Duration =
    Duration::from_secs(DEFAULT_EDGE_TOKEN_MAX_LIFETIME_SECS);
/// The cap on a trusted issuer's `max_token_lifetime_secs`.
pub const MAX_PROVIDER_TOKEN_LIFETIME: Duration = Duration::from_secs(86_400);
/// Most `[[identity.trusted_issuers]]`.
pub const MAX_TRUSTED_ISSUERS: usize = 32;
/// Shortest `[identity] max_key_set_age_secs`.
pub const MIN_KEY_SET_AGE: Duration = Duration::from_secs(60);
/// Longest `[identity] max_key_set_age_secs` (30 days).
pub const MAX_KEY_SET_AGE: Duration = Duration::from_secs(30 * 86_400);
/// Most `audiences` of one trusted issuer.
pub const MAX_ISSUER_AUDIENCES: usize = 16;
/// Most `role_map` entries of one trusted issuer.
pub const MAX_ROLE_MAP_ENTRIES: usize = 256;

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
/// values, with any `aster.ops.NodeConfig` override applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentitySettings {
    /// Accept the `aster-on-behalf-of` entry (`[identity] accept_on_behalf_of`).
    pub accept_on_behalf_of: bool,
    /// External identity providers (`[[identity.trusted_issuers]]`).
    pub trusted_issuers: Vec<TrustedIssuer>,
    /// Longest edge-minted token, 1..=300 (`[identity]
    /// edge_token_max_lifetime_secs`).
    pub edge_token_max_lifetime_secs: u64,
    /// Leeway on `iat` / `nbf`, 0..=300 (`[identity] clock_leeway_secs`).
    pub clock_leeway_secs: u64,
    /// Oldest issuer key set verified with, by its fetch time,
    /// 60..=2592000 (`[identity] max_key_set_age_secs`).
    pub max_key_set_age_secs: u64,
}

impl Default for IdentitySettings {
    fn default() -> Self {
        Self {
            accept_on_behalf_of: false,
            trusted_issuers: Vec::new(),
            edge_token_max_lifetime_secs: DEFAULT_EDGE_TOKEN_MAX_LIFETIME_SECS,
            clock_leeway_secs: DEFAULT_CLOCK_LEEWAY_SECS,
            max_key_set_age_secs: DEFAULT_MAX_KEY_SET_AGE_SECS,
        }
    }
}

/// Whether `value` looks like a node id (64 hex): such an `iss` selects the
/// edge-minted path, so no trusted issuer may use one.
fn node_id_shaped(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn check_text(what: &str, value: &str, max: usize) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if value.len() > max {
        return Err(format!("{what} is longer than {max} bytes"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{what} contains a control character"));
    }
    Ok(())
}

impl IdentitySettings {
    /// The settings `config` gives (file, environment, builder).
    pub fn from_config(config: &AsterConfig) -> Self {
        Self {
            accept_on_behalf_of: config.identity_accept_on_behalf_of,
            trusted_issuers: config.identity_trusted_issuers.clone(),
            edge_token_max_lifetime_secs: config.identity_edge_token_max_lifetime_secs,
            clock_leeway_secs: config.identity_clock_leeway_secs,
            max_key_set_age_secs: config.identity_max_key_set_age_secs,
        }
    }

    /// The trusted issuer with this exact `iss`.
    pub fn trusted_issuer(&self, iss: &str) -> Option<&TrustedIssuer> {
        self.trusted_issuers.iter().find(|issuer| issuer.iss == iss)
    }

    /// Validate the whole section. `AsterServer` refuses to start, and
    /// `aster.ops.NodeConfig` refuses an override, that fails it.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_EDGE_TOKEN_LIFETIME.as_secs()).contains(&self.edge_token_max_lifetime_secs) {
            return Err(format!(
                "identity.edge_token_max_lifetime_secs must be in 1..={}",
                MAX_EDGE_TOKEN_LIFETIME.as_secs()
            ));
        }
        if self.clock_leeway_secs > MAX_CLOCK_LEEWAY.as_secs() {
            return Err(format!(
                "identity.clock_leeway_secs must be at most {}",
                MAX_CLOCK_LEEWAY.as_secs()
            ));
        }
        if !(MIN_KEY_SET_AGE.as_secs()..=MAX_KEY_SET_AGE.as_secs())
            .contains(&self.max_key_set_age_secs)
        {
            return Err(format!(
                "identity.max_key_set_age_secs must be in {}..={}",
                MIN_KEY_SET_AGE.as_secs(),
                MAX_KEY_SET_AGE.as_secs()
            ));
        }
        if self.trusted_issuers.len() > MAX_TRUSTED_ISSUERS {
            return Err(format!(
                "identity.trusted_issuers: at most {MAX_TRUSTED_ISSUERS} issuers"
            ));
        }
        let mut seen = BTreeSet::new();
        for issuer in &self.trusted_issuers {
            let iss = &issuer.iss;
            let at = |what: &str| format!("identity.trusted_issuers[{iss:?}].{what}");
            check_text("identity.trusted_issuers.iss", iss, 256)?;
            if iss.contains('|') {
                return Err(format!(
                    "{}: may not contain '|' (the principal is iss|sub)",
                    at("iss")
                ));
            }
            if node_id_shaped(iss) {
                return Err(format!(
                    "{}: a node id is an edge issuer, not a trusted issuer",
                    at("iss")
                ));
            }
            if !seen.insert(iss.as_str()) {
                return Err(format!("{}: listed twice", at("iss")));
            }
            if issuer.audiences.is_empty() {
                return Err(format!(
                    "{} is required: an issuer without audiences would accept a token \
                     issued to anyone",
                    at("audiences")
                ));
            }
            if issuer.audiences.len() > MAX_ISSUER_AUDIENCES {
                return Err(format!(
                    "{}: at most {MAX_ISSUER_AUDIENCES}",
                    at("audiences")
                ));
            }
            for audience in &issuer.audiences {
                check_text(&at("audiences"), audience, 256)?;
            }
            if let Some(claim) = &issuer.roles_claim {
                check_text(&at("roles_claim"), claim, 128)?;
            } else if !issuer.role_map.is_empty() {
                return Err(format!("{} needs roles_claim", at("role_map")));
            }
            if issuer.role_map.len() > MAX_ROLE_MAP_ENTRIES {
                return Err(format!(
                    "{}: at most {MAX_ROLE_MAP_ENTRIES} entries",
                    at("role_map")
                ));
            }
            for (value, role) in &issuer.role_map {
                check_text(&at("role_map key"), value, 256)?;
                check_text(&at("role_map value"), role, 128)?;
                if role.trim() != role || role.contains(',') {
                    return Err(format!(
                        "{}: {role:?} is not a role (no comma, no surrounding space)",
                        at("role_map")
                    ));
                }
                if is_framework_role(role) {
                    return Err(format!(
                        "{}: {role:?}: roles starting \"aster.\" are the framework's and are \
                         never granted by an identity provider",
                        at("role_map")
                    ));
                }
            }
            if !(1..=MAX_PROVIDER_TOKEN_LIFETIME.as_secs())
                .contains(&issuer.max_token_lifetime_secs)
            {
                return Err(format!(
                    "{} must be in 1..={}",
                    at("max_token_lifetime_secs"),
                    MAX_PROVIDER_TOKEN_LIFETIME.as_secs()
                ));
            }
        }
        Ok(())
    }
}

/// The `[identity.publisher]` section `config` asks for, checked for start-up:
/// the build must have the `identity-publisher` feature, the node Enrollment
/// (the keys go into its fleet namespace), and the section must validate.
pub(crate) fn publisher_plan(
    config: &AsterConfig,
    has_enrollment: bool,
) -> crate::Result<Option<crate::config::IdentityPublisherConfig>> {
    let Some(publisher) = &config.identity_publisher else {
        return Ok(None);
    };
    if !cfg!(feature = "identity-publisher") {
        return Err(crate::Error::InvalidArgument(
            "[identity.publisher] is configured but aster was built without the \
             `identity-publisher` feature"
                .into(),
        ));
    }
    if !has_enrollment {
        return Err(crate::Error::InvalidArgument(
            "[identity.publisher] needs [enrollment]: issuer keys are written into its fleet \
             namespace"
                .into(),
        ));
    }
    publisher.validate(config.identity_max_key_set_age_secs)?;
    Ok(Some(publisher.clone()))
}

/// The [`IdentitySettings`] every dispatcher of one server reads, per call.
/// Swapped atomically by `aster.ops.NodeConfig`: a call routed after
/// [`store`](Self::store) returns sees the new settings, with no restart.
#[derive(Debug, Default)]
pub(crate) struct LiveIdentity {
    current: arc_swap::ArcSwap<IdentitySettings>,
    /// The issuer keys from the fleet namespace, when the server has
    /// Enrollment.
    #[cfg(feature = "delegation")]
    keys: arc_swap::ArcSwapOption<super::issuer_keys::IssuerKeys>,
}

impl LiveIdentity {
    pub(crate) fn load(&self) -> IdentitySettings {
        IdentitySettings::clone(&self.current.load())
    }

    pub(crate) fn store(&self, settings: IdentitySettings) {
        self.current.store(Arc::new(settings));
    }

    #[cfg(all(feature = "enrollment", feature = "docs"))]
    pub(crate) fn set_keys(&self, keys: Arc<super::issuer_keys::IssuerKeys>) {
        self.keys.store(Some(keys));
    }

    /// The verification policy in force, `None` when the entry is refused.
    pub(crate) fn policy(&self) -> Option<OnBehalfOfPolicy> {
        let settings = self.current.load_full();
        if !settings.accept_on_behalf_of {
            return None;
        }
        Some(OnBehalfOfPolicy {
            settings,
            #[cfg(feature = "delegation")]
            keys: self.keys.load_full(),
        })
    }
}

/// How a server verifies `aster-on-behalf-of`. Present only when the server
/// accepts the entry.
#[derive(Debug, Clone, Default)]
pub(crate) struct OnBehalfOfPolicy {
    #[cfg_attr(not(feature = "delegation"), allow(dead_code))]
    pub(crate) settings: Arc<IdentitySettings>,
    #[cfg(feature = "delegation")]
    pub(crate) keys: Option<Arc<super::issuer_keys::IssuerKeys>>,
}

/// Whether `role` is a framework role (`aster.` prefix, ASCII case ignored),
/// which only a node's own credential grants. The token verifier applies the
/// same rule (`aster_transport_core::delegation::on_behalf_of`).
fn is_framework_role(role: &str) -> bool {
    role.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("aster."))
}

/// Parse an [`ISSUABLE_ROLES_ATTRIBUTE`] value: comma-separated, trimmed,
/// empty entries dropped. Framework roles (`aster.` prefix) are dropped too:
/// as with `role_map`, they are never granted to a user, so a delegation
/// listing one cannot make it issuable (the token verifier refuses them
/// regardless).
pub fn parse_issuable_roles(value: &str) -> BTreeSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|role| !role.is_empty() && !is_framework_role(role))
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

/// Choose the verifier from the token's (unverified) `iss`: a node id is an
/// edge-minted token, anything else a provider token.
#[cfg(feature = "delegation")]
fn verify(
    policy: &OnBehalfOfPolicy,
    token: &str,
    transport_peer: &str,
    store: &AttributeStore,
) -> Result<Option<ForwardedIdentity>, (RpcStatus, &'static str)> {
    use aster_transport_core::delegation::provider_token::peek_issuer;
    let iss =
        peek_issuer(token).map_err(|error| (unauthenticated(&error.to_string()), "malformed"))?;
    if node_id_shaped(&iss) {
        verify_edge(policy, token, transport_peer, store)
    } else {
        verify_provider(policy, token, &iss, transport_peer)
    }
}

#[cfg(feature = "delegation")]
fn verify_provider(
    policy: &OnBehalfOfPolicy,
    token: &str,
    iss: &str,
    transport_peer: &str,
) -> Result<Option<ForwardedIdentity>, (RpcStatus, &'static str)> {
    use aster_transport_core::delegation::provider_token::{
        verify_provider_token, ProviderIssuerPolicy, ProviderTokenError, ProviderTokenLimits,
    };
    let Some(issuer) = policy.settings.trusted_issuer(iss) else {
        return Err((
            unauthenticated("on-behalf-of token issuer is not trusted"),
            "issuer not trusted",
        ));
    };
    let Some(keys) = policy.keys.as_ref() else {
        return Err((
            unauthenticated("this server holds no issuer keys (it needs Enrollment)"),
            "no issuer keys",
        ));
    };
    let now = unix_now();
    let max_age = policy.settings.max_key_set_age_secs;
    // The kid that had no key, for the publisher's unknown-kid refresh.
    let missing = std::cell::RefCell::new(None::<String>);
    let lookup = |kid: &str| {
        let key = keys.key(iss, kid, now, max_age);
        if key.is_none() {
            *missing.borrow_mut() = Some(kid.to_string());
        }
        key
    };
    let verified = verify_provider_token(
        token,
        &ProviderIssuerPolicy {
            iss: &issuer.iss,
            audiences: &issuer.audiences,
            roles_claim: issuer.roles_claim.as_deref(),
            role_map: &issuer.role_map,
            max_lifetime: issuer.max_token_lifetime_secs,
        },
        &lookup,
        &ProviderTokenLimits {
            now,
            leeway: policy.settings.clock_leeway_secs,
        },
    )
    .map_err(|error| {
        use ProviderTokenError as E;
        if error == E::UnknownKey {
            if let Some(kid) = missing.borrow().as_deref() {
                keys.notice_unknown_kid(iss, kid);
            }
        }
        let reason = match error {
            E::Configuration => "configuration",
            E::Size => "size",
            E::Malformed => "malformed",
            E::Header => "header",
            E::Algorithm => "algorithm",
            E::Issuer => "issuer mismatch",
            E::UnknownKey => "unknown kid",
            E::AlgorithmMismatch => "algorithm mismatch",
            E::Signature => "signature",
            E::Audience => "audience",
            E::NotYetValid => "not yet valid",
            E::Expired => "expired",
            E::Lifetime => "lifetime",
        };
        (unauthenticated(&error.to_string()), reason)
    })?;
    let identity = ForwardedIdentity {
        principal: format!("{}|{}", verified.issuer, verified.subject),
        actor: transport_peer.to_string(),
        issuer: verified.issuer,
        subject: verified.subject,
        roles: verified.roles.into_iter().collect(),
        auth_methods: verified.amr,
        jti: verified.jti.unwrap_or_default(),
        expires_at: verified.expires_at,
    };
    tracing::debug!(
        target: IDENTITY,
        principal = identity.principal.as_str(),
        actor = identity.actor.as_str(),
        iss = identity.issuer.as_str(),
        sub = identity.subject.as_str(),
        jti = identity.jti.as_str(),
        kid = verified.kid.as_str(),
        "on-behalf-of accepted (provider token)"
    );
    Ok(Some(identity))
}

#[cfg(feature = "delegation")]
fn verify_edge(
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
        leeway: policy.settings.clock_leeway_secs,
        max_lifetime: policy.settings.edge_token_max_lifetime_secs,
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
    fn bearer_tokens_are_read_from_the_authorization_header() {
        assert_eq!(bearer_token("Bearer abc.def.ghi"), Some("abc.def.ghi"));
        assert_eq!(bearer_token("  bearer   abc  "), Some("abc"));
        assert_eq!(bearer_token("BEARER abc"), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Bearer   "), None);
        assert_eq!(bearer_token("Bearer a b"), None);
        assert_eq!(bearer_token("Bearerabc"), None);
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
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let edge_shaped = format!(
            "{}.{}.AAAA",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"EdDSA","typ":"aster-on-behalf-of-v1+jwt"}"#),
            URL_SAFE_NO_PAD.encode(format!(r#"{{"iss":"{peer}"}}"#))
        );
        let edge_entry = header(&[(ON_BEHALF_OF_METADATA, edge_shaped.as_str())]);
        assert_eq!(
            code(resolve(
                Some(&policy),
                &edge_entry,
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
        // Garbage, and a provider token from an issuer nobody trusts.
        assert_eq!(
            code(resolve(
                Some(&policy),
                &entry,
                iroh,
                &peer,
                &gateway(),
                &store
            )),
            StatusCode::Unauthenticated
        );
        let provider = format!(
            "{}.{}.AAAA",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","kid":"k"}"#),
            URL_SAFE_NO_PAD.encode(r#"{"iss":"https://idp.example.com"}"#)
        );
        assert_eq!(
            code(resolve(
                Some(&policy),
                &header(&[(ON_BEHALF_OF_METADATA, provider.as_str())]),
                iroh,
                &peer,
                &gateway(),
                &store
            )),
            StatusCode::Unauthenticated
        );
    }

    #[test]
    fn settings_validate_as_a_whole() {
        use crate::TrustedIssuer;
        let ok = || {
            TrustedIssuer::new("https://idp.example.com", ["api"])
                .roles_claim("groups")
                .map_role("g", "data.read")
        };
        let with = |issuers: Vec<TrustedIssuer>| IdentitySettings {
            trusted_issuers: issuers,
            ..Default::default()
        };
        with(vec![ok()]).validate().unwrap();
        IdentitySettings::default().validate().unwrap();
        let refused = [
            with(vec![TrustedIssuer::new(
                "https://idp.example.com",
                Vec::<String>::new(),
            )]),
            with(vec![ok(), ok()]),
            with(vec![TrustedIssuer::new("ab".repeat(32), ["api"])]),
            with(vec![TrustedIssuer::new("a|b", ["api"])]),
            with(vec![TrustedIssuer::new("", ["api"])]),
            with(vec![ok().map_role("x", "a,b")]),
            with(vec![ok().map_role("x", "aster.config-admin")]),
            with(vec![
                TrustedIssuer::new("https://i", ["api"]).map_role("x", "y")
            ]),
            with(vec![ok().max_token_lifetime_secs(0)]),
            with(vec![ok().max_token_lifetime_secs(86_401)]),
            IdentitySettings {
                edge_token_max_lifetime_secs: 301,
                ..Default::default()
            },
            IdentitySettings {
                edge_token_max_lifetime_secs: 0,
                ..Default::default()
            },
            IdentitySettings {
                clock_leeway_secs: 301,
                ..Default::default()
            },
        ];
        for settings in refused {
            assert!(settings.validate().is_err(), "{settings:?}");
        }
        let err = with(vec![TrustedIssuer::new("https://i", Vec::<String>::new())])
            .validate()
            .unwrap_err();
        assert!(err.contains("audiences"), "{err}");
    }

    #[test]
    fn issuable_roles_parse_like_aster_role() {
        assert_eq!(
            parse_issuable_roles(" a, b ,,a"),
            ["a".to_string(), "b".to_string()].into()
        );
        assert_eq!(
            parse_issuable_roles("aster.gateway,data.read, ASTER.config-admin"),
            ["data.read".to_string()].into(),
            "framework roles are never issuable"
        );
    }
}
