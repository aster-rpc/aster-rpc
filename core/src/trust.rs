//! Shared trust and admission primitives.
//!
//! This module owns the framework-level pieces that should not be reimplemented
//! independently by each language binding: admission ALPN classification,
//! admitted-peer state, expiry handling, and the default Gate-0 decision rule.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

/// Producer mesh admission ALPN.
pub const ALPN_PRODUCER_ADMISSION: &[u8] = b"aster.producer_admission";
/// Consumer admission ALPN.
pub const ALPN_CONSUMER_ADMISSION: &[u8] = b"aster.consumer_admission";
/// Delegated admission ALPN.
pub const ALPN_DELEGATED_ADMISSION: &[u8] = b"aster.admission";

/// The default set of ALPNs that remain reachable before a peer is admitted.
pub fn default_admission_alpns() -> Vec<Vec<u8>> {
    vec![
        ALPN_PRODUCER_ADMISSION.to_vec(),
        ALPN_CONSUMER_ADMISSION.to_vec(),
        ALPN_DELEGATED_ADMISSION.to_vec(),
    ]
}

/// Coarse trust posture for Gate 0.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TrustMode {
    /// Local/dev mode: unknown peers may use any ALPN.
    OpenDev,
    /// Protected mode: unknown peers may only use admission ALPNs.
    #[default]
    Protected,
}

/// What the core hook adapter should do if the host callback is unavailable or
/// does not answer before the configured timeout.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HookFailureMode {
    /// Preserve historical behavior: let the connection through.
    #[default]
    FailOpen,
    /// Protected behavior: reject the connection.
    FailClosed,
}

/// Where an admission came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionSource {
    /// Direct operator or test admission.
    Manual,
    /// A signed enrollment credential.
    Credential,
    /// An ownership-attestation chain.
    Attestation,
    /// A root policy docs namespace.
    RootPolicyDoc,
    /// Workload identity proof such as OIDC.
    WorkloadIdentity,
    /// Application-owned admission proof.
    Custom(String),
}

/// Stored result of a successful peer admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerAdmission {
    /// Remote endpoint/node id.
    pub endpoint_id: String,
    /// Optional root/owner id responsible for this peer.
    pub owner_id: Option<String>,
    /// Attributes used by later RPC/capability checks.
    pub attributes: HashMap<String, String>,
    /// When this admission was accepted.
    pub admitted_at: SystemTime,
    /// Optional expiry; expired records are ignored by lookup and Gate 0.
    pub expires_at: Option<SystemTime>,
    /// Source that produced this admission.
    pub source: AdmissionSource,
    /// Optional hash/fingerprint of the proof that produced this admission.
    pub proof_hash: Option<Vec<u8>>,
    /// Optional policy epoch/version that produced this admission.
    pub policy_epoch: Option<u64>,
}

impl PeerAdmission {
    /// Create a manual admission for `endpoint_id`.
    pub fn new(endpoint_id: impl Into<String>) -> Self {
        Self::from_source(endpoint_id, AdmissionSource::Manual)
    }

    /// Create an admission with an explicit source.
    pub fn from_source(endpoint_id: impl Into<String>, source: AdmissionSource) -> Self {
        Self {
            endpoint_id: endpoint_id.into(),
            owner_id: None,
            attributes: HashMap::new(),
            admitted_at: SystemTime::now(),
            expires_at: None,
            source,
            proof_hash: None,
            policy_epoch: None,
        }
    }

    /// Set the owner/root id.
    pub fn with_owner_id(mut self, owner_id: impl Into<String>) -> Self {
        self.owner_id = Some(owner_id.into());
        self
    }

    /// Add one admission attribute.
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Replace admission attributes.
    pub fn with_attributes(mut self, attributes: HashMap<String, String>) -> Self {
        self.attributes = attributes;
        self
    }

    /// Set an absolute expiry.
    pub fn with_expires_at(mut self, expires_at: SystemTime) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Set expiry relative to `admitted_at`.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.expires_at = self.admitted_at.checked_add(ttl);
        self
    }

    /// Set proof hash/fingerprint material.
    pub fn with_proof_hash(mut self, proof_hash: Vec<u8>) -> Self {
        self.proof_hash = Some(proof_hash);
        self
    }

    /// Set policy epoch/version material.
    pub fn with_policy_epoch(mut self, policy_epoch: u64) -> Self {
        self.policy_epoch = Some(policy_epoch);
        self
    }

    /// Whether this admission is expired at `now`.
    pub fn is_expired_at(&self, now: SystemTime) -> bool {
        self.expires_at.is_some_and(|expires_at| now >= expires_at)
    }
}

/// Shared in-memory admitted-peer store.
#[derive(Clone, Debug, Default)]
pub struct PeerAdmissionStore {
    inner: Arc<RwLock<HashMap<String, PeerAdmission>>>,
}

impl PeerAdmissionStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace an admission. Returns the previous admission, if any.
    pub fn admit(&self, admission: PeerAdmission) -> Option<PeerAdmission> {
        self.inner
            .write()
            .expect("peer admission store poisoned")
            .insert(admission.endpoint_id.clone(), admission)
    }

    /// Insert a manual admission for one peer.
    pub fn admit_peer(&self, endpoint_id: impl Into<String>) -> Option<PeerAdmission> {
        self.admit(PeerAdmission::new(endpoint_id))
    }

    /// Revoke one peer admission.
    pub fn revoke(&self, endpoint_id: &str) -> Option<PeerAdmission> {
        self.inner
            .write()
            .expect("peer admission store poisoned")
            .remove(endpoint_id)
    }

    /// Return an unexpired admission at the current time.
    pub fn get(&self, endpoint_id: &str) -> Option<PeerAdmission> {
        self.get_at(endpoint_id, SystemTime::now())
    }

    /// Return an unexpired admission at `now`.
    pub fn get_at(&self, endpoint_id: &str, now: SystemTime) -> Option<PeerAdmission> {
        let admission = self
            .inner
            .read()
            .expect("peer admission store poisoned")
            .get(endpoint_id)
            .cloned()?;
        (!admission.is_expired_at(now)).then_some(admission)
    }

    /// Whether the peer has an unexpired admission now.
    pub fn is_admitted(&self, endpoint_id: &str) -> bool {
        self.is_admitted_at(endpoint_id, SystemTime::now())
    }

    /// Whether the peer has an unexpired admission at `now`.
    pub fn is_admitted_at(&self, endpoint_id: &str, now: SystemTime) -> bool {
        self.get_at(endpoint_id, now).is_some()
    }

    /// Return admission attributes for an unexpired peer.
    pub fn attributes(&self, endpoint_id: &str) -> HashMap<String, String> {
        self.attributes_at(endpoint_id, SystemTime::now())
    }

    /// Return admission attributes for an unexpired peer at `now`.
    pub fn attributes_at(&self, endpoint_id: &str, now: SystemTime) -> HashMap<String, String> {
        self.get_at(endpoint_id, now)
            .map(|admission| admission.attributes)
            .unwrap_or_default()
    }

    /// Current raw record count, including expired records not yet pruned.
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .expect("peer admission store poisoned")
            .len()
    }

    /// Whether the store has no records.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove expired records and return the number removed.
    pub fn prune_expired(&self) -> usize {
        self.prune_expired_at(SystemTime::now())
    }

    /// Remove records expired at `now` and return the number removed.
    pub fn prune_expired_at(&self, now: SystemTime) -> usize {
        let mut guard = self.inner.write().expect("peer admission store poisoned");
        let before = guard.len();
        guard.retain(|_, admission| !admission.is_expired_at(now));
        before - guard.len()
    }
}

/// Result of evaluating Gate 0.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GateDecision {
    /// Allow the connection to continue.
    Allow,
    /// Reject the connection with an application close code and reason.
    Reject { error_code: u32, reason: Vec<u8> },
}

impl GateDecision {
    /// Whether the decision allows the connection.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Shared Gate-0 policy.
#[derive(Clone, Debug)]
pub struct GatePolicy {
    mode: TrustMode,
    store: PeerAdmissionStore,
    admission_alpns: Arc<Vec<Vec<u8>>>,
    reject_code: u32,
    reject_reason: Arc<Vec<u8>>,
}

impl Default for GatePolicy {
    fn default() -> Self {
        Self::protected()
    }
}

impl GatePolicy {
    /// Protected policy: admission ALPNs are open, all other ALPNs require an
    /// unexpired peer admission.
    pub fn protected() -> Self {
        Self::with_mode(TrustMode::Protected)
    }

    /// Open/dev policy: all ALPNs are reachable.
    pub fn open_dev() -> Self {
        Self::with_mode(TrustMode::OpenDev)
    }

    /// Create a policy with the supplied trust mode.
    pub fn with_mode(mode: TrustMode) -> Self {
        Self {
            mode,
            store: PeerAdmissionStore::new(),
            admission_alpns: Arc::new(default_admission_alpns()),
            reject_code: 403,
            reject_reason: Arc::new(b"peer not admitted".to_vec()),
        }
    }

    /// Change the trust mode while preserving store and ALPN settings.
    pub fn set_mode(mut self, mode: TrustMode) -> Self {
        self.mode = mode;
        self
    }

    /// Share an existing admission store.
    pub fn with_store(mut self, store: PeerAdmissionStore) -> Self {
        self.store = store;
        self
    }

    /// Return the shared admission store.
    pub fn store(&self) -> PeerAdmissionStore {
        self.store.clone()
    }

    /// Current trust mode.
    pub fn mode(&self) -> TrustMode {
        self.mode
    }

    /// Replace the always-open admission ALPN set.
    pub fn with_admission_alpns(mut self, admission_alpns: Vec<Vec<u8>>) -> Self {
        self.admission_alpns = Arc::new(admission_alpns);
        self
    }

    /// Return the configured always-open admission ALPNs.
    pub fn admission_alpns(&self) -> Vec<Vec<u8>> {
        self.admission_alpns.as_ref().clone()
    }

    /// Replace the default rejection code/reason.
    pub fn with_reject(mut self, error_code: u32, reason: impl Into<Vec<u8>>) -> Self {
        self.reject_code = error_code;
        self.reject_reason = Arc::new(reason.into());
        self
    }

    /// Insert or replace an admission.
    pub fn admit(&self, admission: PeerAdmission) -> Option<PeerAdmission> {
        self.store.admit(admission)
    }

    /// Insert a manual admission for one peer.
    pub fn admit_peer(&self, endpoint_id: impl Into<String>) -> Option<PeerAdmission> {
        self.store.admit_peer(endpoint_id)
    }

    /// Revoke one peer admission.
    pub fn revoke(&self, endpoint_id: &str) -> Option<PeerAdmission> {
        self.store.revoke(endpoint_id)
    }

    /// Whether a peer has an unexpired admission now.
    pub fn is_admitted(&self, endpoint_id: &str) -> bool {
        self.store.is_admitted(endpoint_id)
    }

    /// Admission attributes for an unexpired peer.
    pub fn attributes(&self, endpoint_id: &str) -> HashMap<String, String> {
        self.store.attributes(endpoint_id)
    }

    /// Evaluate Gate 0 at the current time.
    pub fn should_allow(&self, endpoint_id: &str, alpn: &[u8]) -> GateDecision {
        self.should_allow_at(endpoint_id, alpn, SystemTime::now())
    }

    /// Evaluate Gate 0 at `now`.
    pub fn should_allow_at(
        &self,
        endpoint_id: &str,
        alpn: &[u8],
        now: SystemTime,
    ) -> GateDecision {
        if self
            .admission_alpns
            .iter()
            .any(|admission_alpn| admission_alpn.as_slice() == alpn)
        {
            return GateDecision::Allow;
        }

        if self.mode == TrustMode::OpenDev || self.store.is_admitted_at(endpoint_id, now) {
            return GateDecision::Allow;
        }

        GateDecision::Reject {
            error_code: self.reject_code,
            reason: self.reject_reason.as_ref().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const RPC_ALPN: &[u8] = b"aster.rpc/1";

    #[test]
    fn protected_policy_allows_admission_alpns_for_unknown_peer() {
        let policy = GatePolicy::protected();

        assert!(policy.should_allow(PEER, ALPN_CONSUMER_ADMISSION).is_allowed());
        assert!(policy.should_allow(PEER, ALPN_PRODUCER_ADMISSION).is_allowed());
        assert!(policy.should_allow(PEER, ALPN_DELEGATED_ADMISSION).is_allowed());
    }

    #[test]
    fn protected_policy_denies_unknown_peer_on_normal_alpn() {
        let policy = GatePolicy::protected();

        assert_eq!(
            policy.should_allow(PEER, RPC_ALPN),
            GateDecision::Reject {
                error_code: 403,
                reason: b"peer not admitted".to_vec(),
            }
        );
    }

    #[test]
    fn open_dev_policy_allows_unknown_peer_on_normal_alpn() {
        let policy = GatePolicy::open_dev();

        assert!(policy.should_allow(PEER, RPC_ALPN).is_allowed());
    }

    #[test]
    fn admitted_peer_is_allowed_until_revoked() {
        let policy = GatePolicy::protected();

        policy.admit_peer(PEER);
        assert!(policy.should_allow(PEER, RPC_ALPN).is_allowed());

        policy.revoke(PEER);
        assert!(!policy.should_allow(PEER, RPC_ALPN).is_allowed());
    }

    #[test]
    fn expired_peer_is_denied_and_pruned() {
        let store = PeerAdmissionStore::new();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        store.admit(
            PeerAdmission::new(PEER)
                .with_expires_at(now - Duration::from_secs(1))
                .with_attribute("role", "writer"),
        );

        assert!(!store.is_admitted_at(PEER, now));
        assert!(store.attributes_at(PEER, now).is_empty());
        assert_eq!(store.len(), 1);
        assert_eq!(store.prune_expired_at(now), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn unexpired_attributes_are_returned() {
        let store = PeerAdmissionStore::new();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        store.admit(
            PeerAdmission::from_source(PEER, AdmissionSource::Credential)
                .with_owner_id(OTHER)
                .with_expires_at(now + Duration::from_secs(60))
                .with_attribute("capability", "read"),
        );

        let attrs = store.attributes_at(PEER, now);
        assert_eq!(attrs.get("capability"), Some(&"read".to_string()));
        assert!(store.is_admitted_at(PEER, now));
    }

    #[test]
    fn custom_admission_alpn_is_open() {
        let policy = GatePolicy::protected().with_admission_alpns(vec![b"custom.admit".to_vec()]);

        assert!(policy.should_allow(PEER, b"custom.admit").is_allowed());
        assert!(!policy
            .should_allow(PEER, ALPN_CONSUMER_ADMISSION)
            .is_allowed());
    }
}
