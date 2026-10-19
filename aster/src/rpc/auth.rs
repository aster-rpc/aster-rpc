//! Call-time authorization (Gate 3).
//!
//! The RPC server enforces a per-call capability check *before* dispatch: it
//! reads the service/method [`CapabilityRequirement`] (declared on the service
//! via [`ServiceDispatch`](super::server::ServiceDispatch)) and evaluates it
//! against the caller's attributes from an [`AttributeStore`]. The store is
//! populated by the **application**, and an `AsterServer` with enrollment
//! layers each enrolled peer's credential roles over it (`aster::enrollment`).
//! For an enrolled peer the credential sets the maximum and runtime settings
//! can only restrict it ([`AttributeStore::restrict_roles`]).
//! Gate 2 (session authorize → rcan) is deferred; see
//! `docs/_internal/aster-rust.md` Step 2 "Outstanding".

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

pub use aster_transport_core::contract::{CapabilityKind, CapabilityRequirement, ROLE_ATTRIBUTE};

/// A source of per-peer attributes computed when a call arrives rather than
/// stored, such as the claims of a peer's current enrollment session. It is
/// consulted on every Gate-3 check, so a revoked or expired session stops
/// contributing at once. It must be cheap and must not block.
///
/// A peer the source knows is *credentialed*: its `aster.role` comes from the
/// source alone, narrowed by any [`AttributeStore::restrict_roles`]
/// restriction, and runtime attributes can never add a role to it.
pub trait AttributeSource: Send + Sync + 'static {
    /// `peer`'s verified attributes, or `None` when this source knows
    /// nothing of it.
    fn attributes(&self, peer: &str) -> Option<SourcedAttributes>;

    /// The roles `peer` may issue in an edge-minted on-behalf-of token: the
    /// issuer delegation in its verified credential
    /// ([`ISSUABLE_ROLES_ATTRIBUTE`](super::identity::ISSUABLE_ROLES_ATTRIBUTE)).
    /// `None` (the default) means the peer holds no issuer delegation and
    /// cannot mint at all. Never derived from runtime attributes.
    fn issuable_roles(&self, peer: &str) -> Option<BTreeSet<String>> {
        let _ = peer;
        None
    }
}

/// What an [`AttributeSource`] reports for a peer it knows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourcedAttributes {
    /// Identifies the peer's session. A role restriction set under one
    /// session never applies under another: it is dropped as soon as the
    /// source reports a different value. Enrollment keeps it stable across
    /// renewals of the same subject and changes it once the session ends.
    pub session: u64,
    /// The verified attributes, including `aster.role` when the credential
    /// grants any roles.
    pub attributes: HashMap<String, String>,
}

/// Why a runtime role change was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoleChangeError {
    /// `peer` has a live enrollment session, so its credential owns
    /// `aster.role`; runtime code can only narrow it with
    /// [`AttributeStore::restrict_roles`].
    #[error(
        "peer {0} has a live enrollment session: its credential sets aster.role; \
         use restrict_roles to narrow it"
    )]
    CredentialOwnsRoles(String),
    /// `peer` has no live enrollment session, so there are no credential
    /// roles to restrict; set its roles with [`AttributeStore::set`] instead.
    #[error("peer {0} has no live enrollment session to restrict")]
    NotEnrolled(String),
}

/// A role restriction: the source session it was set under, and the roles
/// allowed.
type Restriction = (u64, BTreeSet<String>);

/// A clone-shared map from peer id (hex `NodeId`) to that peer's attributes
/// (e.g. `{"aster.role": "operator"}`). The application injects entries — the
/// RPC server only reads them. Cheap to clone (`Arc` bump).
///
/// An optional [`AttributeSource`] (an `AsterServer` with enrollment installs
/// one) is layered over the stored entries. For a peer the source knows (a
/// peer with a live enrollment session) **the credential sets the maximum and
/// runtime settings can only restrict**:
///
/// - effective roles = credential roles ∩ the [`restrict_roles`] restriction,
///   or the credential roles when no restriction is set;
/// - a stored `aster.role` never applies, and [`set`] / [`set_role`] refuse
///   one with [`RoleChangeError::CredentialOwnsRoles`];
/// - other stored keys still apply; the source's value wins on a collision;
/// - a restriction applies from the next call (calls in flight keep the
///   attributes they were admitted with) and ends with the session
///   (revocation, eviction, expiry); it survives renewals of the same
///   subject.
///
/// For a peer the source does not know (no enrollment, or a custom gate)
/// stored attributes, `aster.role` included, apply unchanged.
///
/// [`restrict_roles`]: Self::restrict_roles
/// [`set`]: Self::set
/// [`set_role`]: Self::set_role
#[derive(Clone, Default)]
pub struct AttributeStore {
    inner: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
    source: Arc<RwLock<Option<Arc<dyn AttributeSource>>>>,
    /// Per-peer role restriction, keyed to the source session it was set in.
    restrictions: Arc<RwLock<HashMap<String, Restriction>>>,
}

impl AttributeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the full attribute map for `peer` (hex node id), replacing any
    /// prior. Refused with [`RoleChangeError::CredentialOwnsRoles`] when the
    /// map carries `aster.role` and `peer` has a live enrollment session (use
    /// [`restrict_roles`](Self::restrict_roles)). A stored `aster.role` is
    /// also ignored while a peer that enrolls later has a session.
    pub fn set(
        &self,
        peer: impl Into<String>,
        attributes: HashMap<String, String>,
    ) -> Result<(), RoleChangeError> {
        let peer = peer.into();
        if attributes.contains_key(ROLE_ATTRIBUTE) && self.sourced(&peer).is_some() {
            return Err(RoleChangeError::CredentialOwnsRoles(peer));
        }
        self.inner.write().unwrap().insert(peer, attributes);
        Ok(())
    }

    /// Convenience: set `peer`'s `aster.role` attribute (comma-join multiple
    /// roles yourself). Same rules as [`set`](Self::set).
    pub fn set_role(
        &self,
        peer: impl Into<String>,
        role: impl Into<String>,
    ) -> Result<(), RoleChangeError> {
        let mut attrs = HashMap::new();
        attrs.insert(ROLE_ATTRIBUTE.to_string(), role.into());
        self.set(peer, attrs)
    }

    /// Remove all stored attributes for `peer`. A source's attributes and a
    /// role restriction are not affected.
    pub fn remove(&self, peer: &str) {
        self.inner.write().unwrap().remove(peer);
    }

    /// Narrow an enrolled `peer`'s roles to `roles` ∩ its credential roles,
    /// from its next call. Replaces any earlier restriction; an empty set
    /// refuses every role-gated call. A role the credential lacks is never
    /// granted. The restriction ends with the enrollment session and
    /// survives renewals of the same subject. Refused with
    /// [`RoleChangeError::NotEnrolled`] when `peer` has no live session.
    pub fn restrict_roles<I, S>(&self, peer: &str, roles: I) -> Result<(), RoleChangeError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let Some(sourced) = self.sourced(peer) else {
            return Err(RoleChangeError::NotEnrolled(peer.to_string()));
        };
        let roles = roles
            .into_iter()
            .map(|role| role.into().trim().to_string())
            .filter(|role| !role.is_empty())
            .collect();
        self.restrictions
            .write()
            .unwrap()
            .insert(peer.to_string(), (sourced.session, roles));
        Ok(())
    }

    /// Remove `peer`'s role restriction, restoring its credential roles from
    /// the next call. Returns whether a restriction was in place.
    pub fn clear_restriction(&self, peer: &str) -> bool {
        self.restrictions.write().unwrap().remove(peer).is_some()
    }

    /// The roles `peer` is restricted to (sorted), or `None` when no
    /// restriction applies — none was set, it was cleared, or the session it
    /// was set under has ended.
    pub fn restricted_roles(&self, peer: &str) -> Option<Vec<String>> {
        let current = self.sourced(peer).map(|sourced| sourced.session);
        let mut restrictions = self.restrictions.write().unwrap();
        match restrictions.get(peer) {
            Some((session, roles)) if Some(*session) == current => {
                Some(roles.iter().cloned().collect())
            }
            Some(_) => {
                restrictions.remove(peer);
                None
            }
            None => None,
        }
    }

    /// Layer `source` over the stored entries for this store and every clone
    /// of it, replacing any previous source.
    pub fn set_source(&self, source: Arc<dyn AttributeSource>) {
        *self.source.write().unwrap() = Some(source);
    }

    /// The roles `peer` may issue in an edge-minted on-behalf-of token, from
    /// the source alone (see [`AttributeSource::issuable_roles`]).
    #[cfg_attr(not(feature = "delegation"), allow(dead_code))]
    pub(crate) fn issuable_roles(&self, peer: &str) -> Option<BTreeSet<String>> {
        let source = self.source.read().unwrap().clone();
        source.and_then(|source| source.issuable_roles(peer))
    }

    /// Whether an [`AttributeSource`] (enrollment) is installed.
    pub(crate) fn has_source(&self) -> bool {
        self.source.read().unwrap().is_some()
    }

    fn sourced(&self, peer: &str) -> Option<SourcedAttributes> {
        let source = self.source.read().unwrap().clone();
        source.and_then(|source| source.attributes(peer))
    }

    /// The attributes Gate 3 sees for `peer` (empty map if none): the stored
    /// entries with the source's attributes and the role rule applied (see
    /// the type docs).
    pub fn get(&self, peer: &str) -> HashMap<String, String> {
        self.resolve(peer).0
    }

    /// [`get`](Self::get), and whether `peer` is credentialed: its roles come
    /// from the source, and nothing at runtime may add one.
    pub(crate) fn resolve(&self, peer: &str) -> (HashMap<String, String>, bool) {
        let mut attributes = self
            .inner
            .read()
            .unwrap()
            .get(peer)
            .cloned()
            .unwrap_or_default();
        let Some(sourced) = self.sourced(peer) else {
            // No session: a restriction left here belonged to one that ended.
            if self.restrictions.read().unwrap().contains_key(peer) {
                self.restrictions.write().unwrap().remove(peer);
            }
            return (attributes, false);
        };
        attributes.remove(ROLE_ATTRIBUTE);
        attributes.extend(sourced.attributes);
        let restriction = self.restrictions.read().unwrap().get(peer).cloned();
        match restriction {
            Some((session, allowed)) if session == sourced.session => {
                let roles: Vec<&str> = attributes
                    .get(ROLE_ATTRIBUTE)
                    .map(|value| {
                        value
                            .split(',')
                            .map(str::trim)
                            .filter(|role| allowed.contains(*role))
                            .collect()
                    })
                    .unwrap_or_default();
                if roles.is_empty() {
                    attributes.remove(ROLE_ATTRIBUTE);
                } else {
                    let joined = roles.join(",");
                    attributes.insert(ROLE_ATTRIBUTE.to_string(), joined);
                }
            }
            Some(_) => {
                // Set under a session that has since ended.
                let mut restrictions = self.restrictions.write().unwrap();
                if restrictions
                    .get(peer)
                    .is_some_and(|(session, _)| *session != sourced.session)
                {
                    restrictions.remove(peer);
                }
            }
            None => {}
        }
        (attributes, true)
    }
}

/// Require a single role (`CapabilityKind::Role`).
pub fn require_role(role: impl Into<String>) -> CapabilityRequirement {
    CapabilityRequirement {
        kind: CapabilityKind::Role,
        roles: vec![role.into()],
    }
}

/// Require at least one of the listed roles (`CapabilityKind::AnyOf`).
pub fn require_any_of<I, S>(roles: I) -> CapabilityRequirement
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    CapabilityRequirement {
        kind: CapabilityKind::AnyOf,
        roles: roles.into_iter().map(Into::into).collect(),
    }
}

/// Require all of the listed roles (`CapabilityKind::AllOf`).
pub fn require_all_of<I, S>(roles: I) -> CapabilityRequirement
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    CapabilityRequirement {
        kind: CapabilityKind::AllOf,
        roles: roles.into_iter().map(Into::into).collect(),
    }
}

// ── Pre-dispatch authentication hook ────────────────────────────────────────

/// Context handed to an [`Authenticator`] before dispatch.
pub struct AuthContext {
    /// The transport-provided caller id (Iroh: peer node id; HTTP: remote addr
    /// until the authenticator resolves a real principal).
    pub peer_id: String,
    pub service: String,
    pub method: String,
    /// `0` for stateless calls; the session anchor otherwise.
    pub session_id: i32,
    /// Request metadata — transport headers (e.g. `authorization`). Over HTTP
    /// these are the request's HTTP headers (lowercased names).
    pub metadata: HashMap<String, String>,
}

/// What an [`Authenticator`] resolved for a call.
#[derive(Default)]
pub struct AuthOutcome {
    /// Replace the principal used for the rest of dispatch (e.g. the
    /// authenticated pubkey). `None` keeps the transport-provided `peer_id`.
    pub principal: Option<String>,
    /// Attributes resolved for this call (e.g. `aster.role`). Merged under the
    /// [`AttributeStore`]'s attributes for the peer: the store wins on a key
    /// collision, and for a peer with a live enrollment session an
    /// `aster.role` here is ignored, because the credential sets that peer's
    /// maximum roles (see [`AttributeStore`]).
    pub attributes: HashMap<String, String>,
}

/// A server-side pre-dispatch authentication / authorization hook. Runs before
/// the Gate-3 capability check on every call, with the request metadata
/// (transport headers) plus peer / service / method / session. Reject by
/// returning `Err(RpcStatus)` (e.g. `UNAUTHENTICATED`); otherwise return the
/// resolved principal + attributes the capability gate then sees.
///
/// This is the Rust form of Aster's pluggable auth delegate: custom auth is an
/// `Authenticator` impl — exactly where Rust users already write code, no
/// separate plugin system. Bearer-JWT and friends are just impls of this trait.
/// Attach one with [`Server::authenticator`](super::server::Server::authenticator)
/// or [`AsterServerBuilder::authenticator`](super::runtime::AsterServerBuilder::authenticator).
#[async_trait::async_trait]
pub trait Authenticator: Send + Sync + 'static {
    async fn authenticate(&self, ctx: &AuthContext)
        -> Result<AuthOutcome, super::codec::RpcStatus>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A source whose single enrolled peer and session the test controls.
    #[derive(Default)]
    struct FakeSource(Mutex<Option<(String, u64, String)>>);

    impl FakeSource {
        fn enroll(&self, peer: &str, session: u64, roles: &str) {
            *self.0.lock().unwrap() = Some((peer.into(), session, roles.into()));
        }
        fn end(&self) {
            *self.0.lock().unwrap() = None;
        }
    }

    impl AttributeSource for FakeSource {
        fn attributes(&self, peer: &str) -> Option<SourcedAttributes> {
            let current = self.0.lock().unwrap().clone()?;
            (current.0 == peer).then(|| SourcedAttributes {
                session: current.1,
                attributes: [(ROLE_ATTRIBUTE.to_string(), current.2)].into(),
            })
        }
    }

    fn roles(store: &AttributeStore, peer: &str) -> Option<String> {
        store.get(peer).get(ROLE_ATTRIBUTE).cloned()
    }

    fn store() -> (AttributeStore, Arc<FakeSource>) {
        let store = AttributeStore::new();
        let source = Arc::new(FakeSource::default());
        store.set_source(source.clone());
        (store, source)
    }

    #[test]
    fn restriction_intersects_credential_roles() {
        let (store, source) = store();
        source.enroll("p", 1, "a,b");
        assert_eq!(roles(&store, "p").as_deref(), Some("a,b"));
        store.restrict_roles("p", ["a", "c"]).unwrap();
        assert_eq!(roles(&store, "p").as_deref(), Some("a"), "c never granted");
        store.restrict_roles("p", Vec::<String>::new()).unwrap();
        assert_eq!(roles(&store, "p"), None, "empty restriction: no roles");
        assert!(store.clear_restriction("p"));
        assert_eq!(roles(&store, "p").as_deref(), Some("a,b"));
    }

    #[test]
    fn runtime_role_never_applies_to_a_credentialed_peer() {
        let (store, source) = store();
        // Stored before the peer enrolls: ignored while the session lives.
        store.set_role("p", "c").unwrap();
        source.enroll("p", 1, "a");
        assert_eq!(roles(&store, "p").as_deref(), Some("a"));
        assert_eq!(
            store.set_role("p", "c"),
            Err(RoleChangeError::CredentialOwnsRoles("p".into()))
        );
        // Non-role keys still apply.
        store
            .set("p", [("tenant".to_string(), "t1".to_string())].into())
            .unwrap();
        assert_eq!(store.get("p").get("tenant").map(String::as_str), Some("t1"));
        // Once the session ends, the peer is uncredentialed again.
        source.end();
        assert_eq!(roles(&store, "p"), None);
    }

    #[test]
    fn restriction_ends_with_the_session() {
        let (store, source) = store();
        assert_eq!(
            store.restrict_roles("p", ["a"]),
            Err(RoleChangeError::NotEnrolled("p".into()))
        );
        source.enroll("p", 1, "a,b");
        store.restrict_roles("p", ["a"]).unwrap();
        // A renewal keeps the session id: the restriction stays.
        source.enroll("p", 1, "a,b");
        assert_eq!(store.restricted_roles("p"), Some(vec!["a".to_string()]));
        // A new session (after revocation, eviction or expiry) drops it.
        source.enroll("p", 2, "a,b");
        assert_eq!(roles(&store, "p").as_deref(), Some("a,b"));
        assert_eq!(store.restricted_roles("p"), None);
        // So does the session ending.
        store.restrict_roles("p", ["a"]).unwrap();
        source.end();
        assert_eq!(store.restricted_roles("p"), None);
    }

    #[test]
    fn uncredentialed_peers_keep_runtime_roles() {
        let (store, _source) = store();
        store.set_role("q", "operator").unwrap();
        assert_eq!(roles(&store, "q").as_deref(), Some("operator"));
        let plain = AttributeStore::new();
        plain.set_role("q", "operator").unwrap();
        assert_eq!(roles(&plain, "q").as_deref(), Some("operator"));
    }
}
