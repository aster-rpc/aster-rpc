//! Call-time authorization (Gate 3).
//!
//! The RPC server enforces a per-call capability check *before* dispatch: it
//! reads the service/method [`CapabilityRequirement`] (declared on the service
//! via [`ServiceDispatch`](super::server::ServiceDispatch)) and evaluates it
//! against the caller's attributes from an [`AttributeStore`]. The store is
//! populated by the **application** — e.g. a Gate-0 admission loop that verifies
//! an attestation chain assigns the peer a role. Gate 1 (enrollment-credential
//! verification) and Gate 2 (session authorize → rcan) are deferred; see
//! `docs/_internal/aster-rust.md` Step 2 "Outstanding".

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub use aster_transport_core::contract::{CapabilityKind, CapabilityRequirement, ROLE_ATTRIBUTE};

/// A clone-shared map from peer id (hex `NodeId`) to that peer's attributes
/// (e.g. `{"aster.role": "operator"}`). The application injects entries — the
/// RPC server only reads them. Cheap to clone (`Arc` bump).
#[derive(Clone, Default)]
pub struct AttributeStore {
    inner: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
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

    /// Remove all attributes for `peer`.
    pub fn remove(&self, peer: &str) {
        self.inner.write().unwrap().remove(peer);
    }

    /// The attributes recorded for `peer` (empty map if none).
    pub fn get(&self, peer: &str) -> HashMap<String, String> {
        self.inner
            .read()
            .unwrap()
            .get(peer)
            .cloned()
            .unwrap_or_default()
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
