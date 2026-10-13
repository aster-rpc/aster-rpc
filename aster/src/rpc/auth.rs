//! Call-time authorization (Gate 3).
//!
//! The RPC server enforces a per-call capability check *before* dispatch: it
//! reads the service/method [`CapabilityRequirement`] (declared on the service
//! via [`ServiceDispatch`](super::server::ServiceDispatch)) and evaluates it
//! against the caller's attributes from an [`AttributeStore`]. The store is
//! populated by the **application** — e.g. a Gate-0 admission loop that verifies
//! an enrollment credential assigns the peer a role (`aster::enrollment`).
//! Gate 2 (session authorize → rcan) is deferred; see
//! `docs/_internal/aster-rust.md` Step 2 "Outstanding".

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub use aster_transport_core::contract::{CapabilityKind, CapabilityRequirement, ROLE_ATTRIBUTE};

/// A source of per-peer attributes computed when a call arrives rather than
/// stored, such as the claims of a peer's current enrollment session. It is
/// consulted on every Gate-3 check, so a revoked or expired session stops
/// contributing at once. It must be cheap and must not block.
pub trait AttributeSource: Send + Sync + 'static {
    /// `peer`'s attributes, or `None` when this source knows nothing of it.
    fn attributes(&self, peer: &str) -> Option<HashMap<String, String>>;
}

/// A clone-shared map from peer id (hex `NodeId`) to that peer's attributes
/// (e.g. `{"aster.role": "operator"}`). The application injects entries — the
/// RPC server only reads them. Cheap to clone (`Arc` bump).
///
/// An optional [`AttributeSource`] (an `AsterServer` with enrollment installs
/// one) is layered over the stored entries: its attributes win on a key
/// collision, because they are verified by the root.
#[derive(Clone, Default)]
pub struct AttributeStore {
    inner: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
    source: Arc<RwLock<Option<Arc<dyn AttributeSource>>>>,
}

impl AttributeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the full attribute map for `peer` (hex node id), replacing any prior.
    pub fn set(&self, peer: impl Into<String>, attributes: HashMap<String, String>) {
        self.inner.write().unwrap().insert(peer.into(), attributes);
    }

    /// Convenience: set `peer`'s `aster.role` attribute (comma-join multiple
    /// roles yourself).
    pub fn set_role(&self, peer: impl Into<String>, role: impl Into<String>) {
        let mut attrs = HashMap::new();
        attrs.insert(ROLE_ATTRIBUTE.to_string(), role.into());
        self.set(peer, attrs);
    }

    /// Remove all stored attributes for `peer`. A source's attributes are
    /// not affected.
    pub fn remove(&self, peer: &str) {
        self.inner.write().unwrap().remove(peer);
    }

    /// Layer `source` over the stored entries for this store and every clone
    /// of it, replacing any previous source.
    pub fn set_source(&self, source: Arc<dyn AttributeSource>) {
        *self.source.write().unwrap() = Some(source);
    }

    /// The attributes recorded for `peer` (empty map if none), with the
    /// source's attributes layered over them.
    pub fn get(&self, peer: &str) -> HashMap<String, String> {
        let mut attributes = self
            .inner
            .read()
            .unwrap()
            .get(peer)
            .cloned()
            .unwrap_or_default();
        let source = self.source.read().unwrap().clone();
        if let Some(derived) = source.and_then(|source| source.attributes(peer)) {
            attributes.extend(derived);
        }
        attributes
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
    /// Attributes resolved for this call (e.g. `aster.role`). Merged with the
    /// enrollment-derived attributes from the [`AttributeStore`] — **enrollment
    /// wins on key collision** (a per-call delegate is lower-assurance than an
    /// admission-vouched attribute and must not overwrite it).
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
