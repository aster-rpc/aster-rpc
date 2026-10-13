//! Opt-in reciprocal enrollment on a restricted ALPN.
//!
//! Install before dialing, route only this ALPN to `accept`, and keep the
//! returned service alive. Ordinary protocols stay gated until verified
//! admission. Configured replica addresses are discovery hints, never trust.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot, Semaphore, SemaphorePermit};
use tokio::task::AbortHandle;

use super::{
    admit, local_clock_synchronization, AdmissionContext, AdmissionDecision,
    AdmissionEvidenceSource, AdmissionPolicy, ClockSynchronization, DenialReason, EnrollmentError,
    EnrollmentPolicyStore, JwtEnrollmentVerifier, ObservedSourceAddress, PolicySynchronization,
    RecordAvailability, RecordError, SourceAddressProvenance, SynchronizationSource,
    VerifiedAdmissionEvidence, MAX_CHAIN_BYTES,
};
use crate::{
    CoreAfterHandshakeDecision, CoreConnection, CoreDoc, CoreHookHandshakeInfo, CoreHookReceiver,
    CoreNode, CoreNodeAddr, CoreRecvStream, CoreSendStream, CoreWeakConnection,
};

pub const ENROLLMENT_ALPN: &[u8] = b"aster.enrollment/1";
const MAX_ENVELOPE: usize = MAX_CHAIN_BYTES + 8192;
const REJECT: u64 = 403;
/// Close reason prefix carrying the observed offset (remote minus local
/// seconds) back to a peer whose clock is outside the activation leeway.
const CLOCK_SKEW_REASON: &str = "enrollment clock skew ";
const MAX_RATE_KEYS: usize = 8192;

#[derive(Clone, Debug)]
pub struct EnrollmentOptions {
    /// Bounds opening the first stream and receiving the credential-free probe.
    pub first_message_timeout: Duration,
    /// Accepted enrollment connections that have not yet sent a complete
    /// probe. They hold no handshake slot or per-peer/per-source budget; when
    /// the pool is full the oldest is closed. Starving joiners therefore needs
    /// more than `max_pending_probes / first_message_timeout` new connections
    /// per second.
    pub max_pending_probes: usize,
    /// Bounds each later receive once a handshake slot is held, so a peer
    /// that completes its probe and then stalls frees the slot within one
    /// step. It must exceed `policy_timeout`, because an honest peer
    /// evaluates the other side's evidence between steps.
    pub step_timeout: Duration,
    pub handshake_timeout: Duration,
    pub policy_timeout: Duration,
    pub reconciliation_interval: Duration,
    pub max_concurrent_handshakes: usize,
    pub max_concurrent_handshakes_per_peer: usize,
    pub max_concurrent_handshakes_per_source: usize,
    /// Separate capacity reserved for already-admitted ordinary connections.
    pub max_concurrent_gate_checks: usize,
    pub max_concurrent_reconciliations: usize,
    /// Per authenticated peer identity, rather than one globally shared budget.
    pub max_handshakes_per_second: u32,
    pub max_handshakes_per_second_per_source: u32,
    pub max_admitted_peers: usize,
    /// At `max_admitted_peers`, a new admission may evict the least recently
    /// checked session idle at least this long. Eviction is not revocation:
    /// the evicted peer re-enrolls on its next connection.
    pub session_idle_eviction: Duration,
    /// Refuse to install while the kernel reports the local clock
    /// unsynchronized. When false the status is only reported in readiness.
    pub require_synchronized_clock: bool,
}

impl Default for EnrollmentOptions {
    fn default() -> Self {
        Self {
            first_message_timeout: Duration::from_secs(2),
            max_pending_probes: 256,
            step_timeout: Duration::from_secs(3),
            handshake_timeout: Duration::from_secs(10),
            policy_timeout: Duration::from_secs(2),
            reconciliation_interval: Duration::from_secs(1),
            max_concurrent_handshakes: 16,
            max_concurrent_handshakes_per_peer: 1,
            max_concurrent_handshakes_per_source: 4,
            max_concurrent_gate_checks: 64,
            max_concurrent_reconciliations: 16,
            max_handshakes_per_second: 32,
            max_handshakes_per_second_per_source: 128,
            max_admitted_peers: 1024,
            session_idle_eviction: Duration::from_secs(300),
            require_synchronized_clock: false,
        }
    }
}

/// Readiness describes membership only, never operation/Exec permission.
#[derive(Clone, Debug)]
pub struct EnrollmentReadiness {
    pub peer: [u8; 32],
    pub source: AdmissionEvidenceSource,
    pub policy_sync: Option<PolicySynchronization>,
    /// Direct selected-path observation at admission/check time; relay paths
    /// provide no originating-peer address.
    pub observed_source: Option<ObservedSourceAddress>,
    /// This node's clock status when enrollment was installed.
    pub local_clock: ClockSynchronization,
}

/// Consumers receive this after a definite denial revokes a formerly admitted
/// peer, and should cancel their own jobs/output access as required. A lagged
/// receiver has missed revocations and must recheck every peer it relies on.
#[derive(Clone, Debug)]
pub struct EnrollmentRevoked {
    pub peer: [u8; 32],
    pub reason: DenialReason,
}

/// Why a peer is not admitted right now. Only [`AdmissionError::Denied`]
/// revokes a session; consumers should cancel work only on that variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    /// No admission session exists, for example after this process restarted
    /// or an idle session was evicted. The peer must enroll again.
    NotAdmitted,
    /// Root policy, signed validity or the consumer policy denies the peer.
    Denied(DenialReason),
    /// The peer's clock, or its credential issuer's, is outside the activation
    /// leeway by `offset_seconds` (remote minus local). Not definite: any
    /// session is kept; synchronize clocks and retry.
    ClockSkew { offset_seconds: i64 },
    /// Policy could not be evaluated now (local storage, pending content, a
    /// timeout or a handler error). Any session is kept; retry later.
    Unavailable(String),
}

impl AdmissionError {
    fn from_reason(reason: DenialReason) -> Self {
        if let DenialReason::ClockSkew { offset_seconds } = reason {
            Self::ClockSkew { offset_seconds }
        } else if reason.is_definite() {
            Self::Denied(reason)
        } else {
            Self::Unavailable(format!("{reason:?}"))
        }
    }

    fn from_record(error: RecordError) -> Self {
        match error {
            RecordError::Denied(reason) => Self::from_reason(reason),
            other => Self::Unavailable(other.to_string()),
        }
    }
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAdmitted => write!(f, "peer not admitted; enrollment handshake required"),
            Self::Denied(reason) => write!(f, "enrollment denied: {reason:?}"),
            Self::ClockSkew { offset_seconds } => write!(
                f,
                "enrollment clock skew: peer clock differs by {offset_seconds}s"
            ),
            Self::Unavailable(detail) => write!(f, "enrollment policy unavailable: {detail}"),
        }
    }
}

impl std::error::Error for AdmissionError {}

#[derive(Clone)]
struct Session {
    generation: u64,
    evidence: VerifiedAdmissionEvidence,
    /// Connections registered with `track_connection`.
    connections: Vec<CoreConnection>,
    /// Every ordinary connection Gate 0 admitted for this peer, in either
    /// direction. A definite denial closes them all.
    admitted: Vec<CoreWeakConnection>,
    observed_source: Option<ObservedSourceAddress>,
    last_used: Instant,
}

/// Recent definite revocations, so a peer whose session was revoked is still
/// reported as denied rather than merely not admitted. Oldest entries drop
/// first; a later successful admission clears the peer.
#[derive(Default)]
struct RevokedPeers {
    order: VecDeque<[u8; 32]>,
    reasons: BTreeMap<[u8; 32], DenialReason>,
}

impl RevokedPeers {
    fn insert(&mut self, peer: [u8; 32], reason: DenialReason, capacity: usize) {
        if self.reasons.insert(peer, reason).is_none() {
            self.order.push_back(peer);
        }
        while self.order.len() > capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.reasons.remove(&oldest);
            }
        }
    }

    fn remove(&mut self, peer: &[u8; 32]) {
        if self.reasons.remove(peer).is_some() {
            self.order.retain(|known| known != peer);
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    version: u8,
    /// Sender's Unix seconds, for clock-skew diagnosis.
    sent_at: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaProof {
    version: u8,
    /// The responder's own delegation chain, in text form.
    chain: Option<String>,
    sent_at: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Presentation {
    /// The joining node's delegation chain, in text form.
    chain: Option<String>,
    sent_at: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Welcome {
    version: u8,
    policy_ticket: Option<String>,
}

struct RateState {
    started: Instant,
    peers: BTreeMap<[u8; 32], u32>,
    sources: BTreeMap<IpAddr, u32>,
}

/// Enrollment connections waiting for their probe, oldest first.
#[derive(Default)]
struct PendingProbes {
    next: u64,
    waiting: VecDeque<(u64, CoreConnection)>,
}

/// Membership in the pre-probe pool, released on drop.
struct PendingProbe<'a> {
    pool: &'a Mutex<PendingProbes>,
    id: u64,
}

impl Drop for PendingProbe<'_> {
    fn drop(&mut self) {
        self.pool
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .waiting
            .retain(|(id, _)| *id != self.id);
    }
}

#[derive(Default)]
struct HandshakeCounts {
    peers: BTreeMap<[u8; 32], usize>,
    sources: BTreeMap<IpAddr, usize>,
}

struct HandshakeSlot<'a> {
    counts: &'a Mutex<HandshakeCounts>,
    peer: [u8; 32],
    source: Option<IpAddr>,
    _permit: SemaphorePermit<'a>,
}

impl Drop for HandshakeSlot<'_> {
    fn drop(&mut self) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        decrement_count(&mut counts.peers, &self.peer);
        if let Some(source) = self.source {
            decrement_count(&mut counts.sources, &source);
        }
    }
}

fn decrement_count<K: Ord>(counts: &mut BTreeMap<K, usize>, key: &K) {
    if let Some(count) = counts.get_mut(key) {
        *count -= 1;
        if *count == 0 {
            counts.remove(key);
        }
    }
}

/// Shared core implementation; the facade delegates to this type.
pub struct EnrollmentService {
    this: Weak<Self>,
    node: CoreNode,
    store: Arc<EnrollmentPolicyStore>,
    verifier: JwtEnrollmentVerifier,
    credential: Option<String>,
    policy: Arc<dyn AdmissionPolicy>,
    options: EnrollmentOptions,
    document: RwLock<Option<CoreDoc>>,
    policy_watch: Mutex<Option<AbortHandle>>,
    context: RwLock<AdmissionContext>,
    sessions: Mutex<BTreeMap<[u8; 32], Session>>,
    revoked_peers: Mutex<RevokedPeers>,
    local_clock: ClockSynchronization,
    inbound_permits: Semaphore,
    outbound_permits: Semaphore,
    gate_permits: Arc<Semaphore>,
    handshake_counts: Mutex<HandshakeCounts>,
    pending_probes: Mutex<PendingProbes>,
    rate: Mutex<RateState>,
    next_generation: AtomicU64,
    revoked: broadcast::Sender<EnrollmentRevoked>,
}

impl Drop for EnrollmentService {
    fn drop(&mut self) {
        if let Some(watch) = self
            .policy_watch
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            watch.abort();
        }
    }
}

impl EnrollmentService {
    /// Requires a node started with hooks and `ENROLLMENT_ALPN` registered.
    /// Consumes its hook receiver and promptly services both hook directions.
    /// The consumer still owns the node's custom-ALPN accept loop.
    pub fn install(
        node: CoreNode,
        store: Arc<EnrollmentPolicyStore>,
        verifier: JwtEnrollmentVerifier,
        credential: Option<String>,
        policy: Arc<dyn AdmissionPolicy>,
        options: EnrollmentOptions,
    ) -> Result<Arc<Self>> {
        ensure!(
            options.max_concurrent_handshakes > 0
                && options.max_concurrent_handshakes_per_peer > 0
                && options.max_concurrent_handshakes_per_source > 0
                && options.max_pending_probes > 0
                && options.max_concurrent_gate_checks > 0
                && options.max_concurrent_reconciliations > 0
                && options.max_admitted_peers > 0
                && options.max_handshakes_per_second > 0
                && options.max_handshakes_per_second_per_source > 0
                && !options.first_message_timeout.is_zero()
                && !options.handshake_timeout.is_zero()
                && !options.policy_timeout.is_zero()
                && options.step_timeout > options.policy_timeout
                && !options.reconciliation_interval.is_zero()
                && !options.session_idle_eviction.is_zero(),
            "invalid enrollment bounds"
        );
        ensure!(
            node.inner.enrollment_hook_failure_mode == crate::trust::HookFailureMode::FailClosed,
            "enrollment requires fail-closed handshake hooks"
        );
        ensure!(
            verifier.trusted_root() == store.root()
                && verifier.intended_audience() == store.audience(),
            "enrollment verifier and policy store trust contexts differ"
        );
        ensure!(
            verifier.activation_leeway() == store.activation_leeway(),
            "enrollment verifier and policy store activation leeways differ"
        );
        if local_clock_synchronization() == ClockSynchronization::Unsynchronized {
            ensure!(
                !options.require_synchronized_clock,
                "local clock is not synchronized; enable systemd-timesyncd or chronyd"
            );
            tracing::warn!("enrollment: local clock is not synchronized; admission may fail");
        }
        if let Some(token) = &credential {
            verifier.verify(token, &identity(&node.node_id())?, unix_now())?;
        }
        let hooks = node
            .take_hook_receiver()
            .ok_or_else(|| anyhow!("enrollment requires unused node hooks"))?;
        let service = Self::new(node, store, verifier, credential, policy, options);
        service.spawn_hook_loop(hooks);
        let weak = Arc::downgrade(&service);
        let interval = service.options.reconciliation_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let Some(service) = weak.upgrade() else {
                    break;
                };
                service.reconcile().await;
            }
        });
        Ok(service)
    }

    fn new(
        node: CoreNode,
        store: Arc<EnrollmentPolicyStore>,
        verifier: JwtEnrollmentVerifier,
        credential: Option<String>,
        policy: Arc<dyn AdmissionPolicy>,
        options: EnrollmentOptions,
    ) -> Arc<Self> {
        // One reconciliation can revoke every admitted peer; size the channel
        // so that does not lag a promptly polling consumer.
        let (revoked, _) = broadcast::channel(options.max_admitted_peers.max(128));
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            node,
            store,
            verifier,
            credential,
            policy,
            inbound_permits: Semaphore::new(options.max_concurrent_handshakes),
            outbound_permits: Semaphore::new(options.max_concurrent_handshakes),
            gate_permits: Arc::new(Semaphore::new(options.max_concurrent_gate_checks)),
            handshake_counts: Mutex::new(HandshakeCounts::default()),
            pending_probes: Mutex::new(PendingProbes::default()),
            options,
            document: RwLock::new(None),
            policy_watch: Mutex::new(None),
            context: RwLock::new(AdmissionContext::default()),
            sessions: Mutex::new(BTreeMap::new()),
            revoked_peers: Mutex::new(RevokedPeers::default()),
            local_clock: local_clock_synchronization(),
            rate: Mutex::new(RateState {
                started: Instant::now(),
                peers: BTreeMap::new(),
                sources: BTreeMap::new(),
            }),
            next_generation: AtomicU64::new(1),
            revoked,
        })
    }

    fn spawn_hook_loop(&self, mut hooks: CoreHookReceiver) {
        let weak = self.this.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((_, reply)) = hooks.before_connect_rx.recv() => {
                        let _ = reply.send(true);
                    }
                    Some((info, reply)) = hooks.after_handshake_rx.recv() => {
                        let Some(service) = weak.upgrade() else {
                            break;
                        };
                        service.after_handshake(info, reply);
                    }
                    else => break,
                }
            }
        });
    }

    /// Enrollment connections are only rate limited. Every other ALPN needs an
    /// admitted session whose policy still allows the observed source.
    fn after_handshake(
        self: Arc<Self>,
        info: CoreHookHandshakeInfo,
        reply: oneshot::Sender<CoreAfterHandshakeDecision>,
    ) {
        let Ok(peer) = identity(&info.remote_endpoint_id) else {
            let _ = reply.send(hook_decision(false));
            return;
        };
        let address = info.observed_peer_addr;
        if info.alpn == ENROLLMENT_ALPN {
            let allowed = self.take_rate_slot(peer, address.map(|address| address.ip()));
            let _ = reply.send(hook_decision(allowed));
            return;
        }
        let permit = if self.has_session(&peer) {
            self.gate_permits.clone().try_acquire_owned().ok()
        } else {
            None
        };
        let Some(permit) = permit else {
            let _ = reply.send(hook_decision(false));
            return;
        };
        let connection = info.connection;
        tokio::spawn(async move {
            let _permit = permit;
            let source = address.map(|address| ObservedSourceAddress {
                address: address.ip(),
                provenance: SourceAddressProvenance::DirectPeer,
            });
            let allowed = self.gate_connection(&peer, source, connection).await;
            let _ = reply.send(hook_decision(allowed));
        });
    }

    /// Gate 0 for one ordinary connection: admit it only under the session it
    /// was evaluated against, and remember it in the same step so a later
    /// definite denial closes it.
    async fn gate_connection(
        &self,
        peer: &[u8; 32],
        source: Option<ObservedSourceAddress>,
        connection: CoreWeakConnection,
    ) -> bool {
        let Some(session) = self.session(peer) else {
            return false;
        };
        if self.evaluate(&session.evidence, source).await.is_err() {
            return false;
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = sessions
            .get_mut(peer)
            .filter(|current| current.generation == session.generation)
        else {
            return false;
        };
        current.last_used = Instant::now();
        current.admitted.retain(CoreWeakConnection::is_alive);
        current.admitted.push(connection);
        true
    }

    pub fn revocations(&self) -> broadcast::Receiver<EnrollmentRevoked> {
        self.revoked.subscribe()
    }

    /// Attach the pinned policy namespace. It may be a read-only replica.
    /// Live syncs with the root or an admitted replica keep policy freshness
    /// current from then on.
    pub fn set_policy_document(&self, doc: CoreDoc) -> Result<()> {
        ensure!(
            namespace(&doc.doc_id())? == *self.store.namespace(),
            "wrong policy namespace"
        );
        *self.document.write().unwrap_or_else(|e| e.into_inner()) = Some(doc.clone());
        let weak = self.this.clone();
        let watch = tokio::spawn(async move {
            use futures_lite::StreamExt;
            let Ok(events) = doc.doc.subscribe().await else {
                return;
            };
            futures_lite::pin!(events);
            while let Some(Ok(event)) = events.next().await {
                let iroh_docs::engine::LiveEvent::SyncFinished(sync) = event else {
                    continue;
                };
                let Some(service) = weak.upgrade() else {
                    break;
                };
                if sync.result.is_ok() {
                    service
                        .observe_policy_sync(&doc, &sync.peer.to_string())
                        .await;
                }
            }
        });
        if let Some(previous) = self
            .policy_watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(watch.abort_handle())
        {
            previous.abort();
        }
        Ok(())
    }

    /// Record a completed policy sync with the root or an admitted replica
    /// once no policy content is still pending. Other peers prove nothing.
    async fn observe_policy_sync(&self, doc: &CoreDoc, peer: &str) {
        let Ok(peer) = identity(peer) else {
            return;
        };
        if peer != *self.store.root() && !self.has_session(&peer) {
            return;
        }
        let Ok(report) = self.store.refresh(doc).await else {
            return;
        };
        let mut context = self.context.write().unwrap_or_else(|e| e.into_inner());
        context.pending_policy_content = report.has_uncertainty();
        if report.pending == 0 {
            context.policy_sync = Some(self.synchronization(peer));
        }
    }

    fn synchronization(&self, peer: [u8; 32]) -> PolicySynchronization {
        PolicySynchronization {
            completed_at: unix_now(),
            source: if peer == *self.store.root() {
                SynchronizationSource::Root
            } else {
                SynchronizationSource::Replica(peer)
            },
            observed_revision: None,
        }
    }

    fn note_pending_policy_content(&self) {
        self.context
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .pending_policy_content = true;
    }

    fn take_rate_slot(&self, peer: [u8; 32], source: Option<IpAddr>) -> bool {
        let mut rate = self.rate.lock().unwrap_or_else(|e| e.into_inner());
        if rate.started.elapsed() >= Duration::from_secs(1) {
            rate.started = Instant::now();
            rate.peers.clear();
            rate.sources.clear();
        }
        if rate.peers.get(&peer).copied().unwrap_or_default()
            >= self.options.max_handshakes_per_second
            || (!rate.peers.contains_key(&peer) && rate.peers.len() >= MAX_RATE_KEYS)
            || source.is_some_and(|source| {
                rate.sources.get(&source).copied().unwrap_or_default()
                    >= self.options.max_handshakes_per_second_per_source
                    || (!rate.sources.contains_key(&source) && rate.sources.len() >= MAX_RATE_KEYS)
            })
        {
            return false;
        }
        *rate.peers.entry(peer).or_default() += 1;
        if let Some(source) = source {
            *rate.sources.entry(source).or_default() += 1;
        }
        true
    }

    /// Join the pre-probe pool, closing the oldest waiting connection when it
    /// is full. Waiting costs no handshake slot.
    fn pending_probe(&self, connection: &CoreConnection) -> PendingProbe<'_> {
        let mut pool = self
            .pending_probes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while pool.waiting.len() >= self.options.max_pending_probes {
            if let Some((_, oldest)) = pool.waiting.pop_front() {
                let _ = oldest.close(REJECT, b"enrollment busy".to_vec());
            }
        }
        let id = pool.next;
        pool.next += 1;
        pool.waiting.push_back((id, connection.clone()));
        PendingProbe {
            pool: &self.pending_probes,
            id,
        }
    }

    fn inbound_slot(&self, peer: [u8; 32], source: Option<IpAddr>) -> Result<HandshakeSlot<'_>> {
        let permit = self
            .inbound_permits
            .try_acquire()
            .map_err(|_| anyhow!("enrollment busy"))?;
        let mut counts = self
            .handshake_counts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            counts.peers.get(&peer).copied().unwrap_or_default()
                < self.options.max_concurrent_handshakes_per_peer,
            "peer enrollment concurrency limit"
        );
        ensure!(
            source.is_none_or(
                |source| counts.sources.get(&source).copied().unwrap_or_default()
                    < self.options.max_concurrent_handshakes_per_source
            ),
            "source enrollment concurrency limit"
        );
        *counts.peers.entry(peer).or_default() += 1;
        if let Some(source) = source {
            *counts.sources.entry(source).or_default() += 1;
        }
        Ok(HandshakeSlot {
            counts: &self.handshake_counts,
            peer,
            source,
            _permit: permit,
        })
    }

    /// Derive this peer's current evidence from root records and an optional
    /// presented chain, then evaluate it. A record read failure that recorded
    /// no definite denial leaves the peer's standing unknown.
    async fn evidence(
        &self,
        peer: &[u8; 32],
        token: Option<&str>,
        observed_source: Option<ObservedSourceAddress>,
    ) -> Result<VerifiedAdmissionEvidence, AdmissionError> {
        let doc = self
            .document
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut unresolved = None;
        if let Some(doc) = &doc {
            unresolved = self.refresh_issue(self.store.refresh_node(doc, peer).await);
        }
        // Independent root registration replaces presentation evidence. An
        // obsolete/expired chain must not block a newer valid registration.
        if let Ok(registration) = self.store.select_evidence(peer, None, unix_now()) {
            if let Some(issue) = unresolved {
                return Err(issue);
            }
            self.evaluate(&registration, observed_source).await?;
            return Ok(registration);
        }
        let chain = token
            .map(|token| self.verifier.verify(token, peer, unix_now()))
            .transpose()
            .map_err(|error| match error {
                EnrollmentError::ClockSkew { offset_seconds } => {
                    AdmissionError::ClockSkew { offset_seconds }
                }
                _ => AdmissionError::Denied(DenialReason::InvalidValidity),
            })?;
        if let (Some(doc), Some(id)) =
            (&doc, chain.as_ref().and_then(|chain| chain.credential_id()))
        {
            let issue = self.refresh_issue(self.store.refresh_credential(doc, id).await);
            unresolved = unresolved.or(issue);
        }
        let evidence = self
            .store
            .select_evidence(peer, chain.as_ref(), unix_now())
            .map_err(AdmissionError::from_record)?;
        if let Some(issue) = unresolved {
            return Err(issue);
        }
        self.evaluate(&evidence, observed_source).await?;
        Ok(evidence)
    }

    /// Pending or stale content only marks policy uncertain. Any other read
    /// failure is returned so the caller cannot admit on partial evidence.
    fn refresh_issue(
        &self,
        result: std::result::Result<RecordAvailability, RecordError>,
    ) -> Option<AdmissionError> {
        match result {
            Ok(RecordAvailability::Pending) | Err(RecordError::StaleRevision) => {
                self.note_pending_policy_content();
                None
            }
            Ok(_) => None,
            Err(error) => Some(AdmissionError::from_record(error)),
        }
    }

    async fn evaluate(
        &self,
        evidence: &VerifiedAdmissionEvidence,
        observed_source: Option<ObservedSourceAddress>,
    ) -> Result<(), AdmissionError> {
        self.evaluate_with_policy(
            evidence,
            observed_source,
            self.policy.as_ref(),
            self.options.policy_timeout,
        )
        .await
    }

    async fn evaluate_with_policy(
        &self,
        evidence: &VerifiedAdmissionEvidence,
        observed_source: Option<ObservedSourceAddress>,
        policy: &dyn AdmissionPolicy,
        timeout: Duration,
    ) -> Result<(), AdmissionError> {
        let mut context = self
            .context
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        context.observed_source = observed_source;
        match admit(
            evidence,
            self.store.as_ref(),
            &context,
            policy,
            timeout,
            unix_now,
        )
        .await
        {
            AdmissionDecision::Allow => Ok(()),
            AdmissionDecision::Deny(reason) => Err(AdmissionError::from_reason(reason)),
        }
    }

    fn remember(
        &self,
        evidence: VerifiedAdmissionEvidence,
        observed_source: Option<ObservedSourceAddress>,
    ) -> Result<EnrollmentReadiness> {
        self.replace_session(evidence, observed_source, None)
    }

    /// Reconciliation may replace exactly the generation it evaluated, never
    /// resurrect a removed peer or overwrite a concurrent fresh handshake.
    fn replace_session(
        &self,
        evidence: VerifiedAdmissionEvidence,
        observed_source: Option<ObservedSourceAddress>,
        expected_generation: Option<u64>,
    ) -> Result<EnrollmentReadiness> {
        let peer = *evidence.subject();
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(expected) = expected_generation {
            ensure!(
                sessions
                    .get(&peer)
                    .is_some_and(|session| session.generation == expected),
                "admission changed during reconciliation"
            );
        }
        if let Some(session) = sessions.get_mut(&peer).filter(|session| {
            session.evidence.source() == evidence.source()
                && session.evidence.presented_chain() == evidence.presented_chain()
                && session.observed_source == observed_source
        }) {
            session.last_used = Instant::now();
            return Ok(self.readiness(peer, evidence.source(), observed_source));
        }
        if !sessions.contains_key(&peer) && sessions.len() >= self.options.max_admitted_peers {
            let idle = self.options.session_idle_eviction;
            let victim = sessions
                .iter()
                .filter(|(_, session)| {
                    session.last_used.elapsed() >= idle
                        && session
                            .connections
                            .iter()
                            .all(|connection| !connection.connection_info().is_connected)
                        && !session.admitted.iter().any(CoreWeakConnection::is_alive)
                })
                .min_by_key(|(_, session)| session.last_used)
                .map(|(peer, _)| *peer);
            let Some(victim) = victim else {
                bail!("admitted peer limit");
            };
            sessions.remove(&victim);
        }
        let source = evidence.source();
        let generation = self
            .next_generation
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow!("admission generation exhausted"))?;
        let (connections, admitted) = sessions
            .remove(&peer)
            .map(|session| (session.connections, session.admitted))
            .unwrap_or_default();
        sessions.insert(
            peer,
            Session {
                generation,
                evidence,
                connections,
                admitted,
                observed_source,
                last_used: Instant::now(),
            },
        );
        self.revoked_peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&peer);
        Ok(self.readiness(peer, source, observed_source))
    }

    fn readiness(
        &self,
        peer: [u8; 32],
        source: AdmissionEvidenceSource,
        observed_source: Option<ObservedSourceAddress>,
    ) -> EnrollmentReadiness {
        EnrollmentReadiness {
            peer,
            source,
            policy_sync: self
                .context
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .policy_sync
                .clone(),
            observed_source,
            local_clock: self.local_clock,
        }
    }

    /// Compare a peer's handshake timestamp with this clock. Beyond the
    /// activation leeway, fresh credentials would be misjudged as well, so
    /// report the offset rather than an opaque denial.
    fn check_skew(&self, sent_at: u64) -> Result<(), AdmissionError> {
        let offset_seconds = i64::try_from(sent_at)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(unix_now()).unwrap_or(i64::MAX));
        if offset_seconds.unsigned_abs() > self.verifier.activation_leeway() {
            tracing::warn!(offset_seconds, "enrollment: peer clock outside leeway");
            return Err(AdmissionError::ClockSkew { offset_seconds });
        }
        Ok(())
    }

    fn session(&self, peer: &[u8; 32]) -> Option<Session> {
        self.sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(peer)
            .cloned()
    }

    fn has_session(&self, peer: &[u8; 32]) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(peer)
    }

    /// Without a session, report a known definite denial so consumers can act
    /// on it after a revocation or restart; otherwise the peer must re-enroll.
    fn sessionless_standing(&self, peer: &[u8; 32]) -> AdmissionError {
        if let Some(reason) = self
            .revoked_peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reasons
            .get(peer)
        {
            return AdmissionError::Denied(reason.clone());
        }
        match self.store.select_evidence(peer, None, unix_now()) {
            Err(RecordError::Denied(reason)) if reason.is_definite() => {
                AdmissionError::Denied(reason)
            }
            _ => AdmissionError::NotAdmitted,
        }
    }

    /// Accept a check only if the evaluated generation is still current, and
    /// record the use for idle eviction.
    fn confirm(&self, peer: &[u8; 32], generation: u64) -> Result<(), AdmissionError> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match sessions.get_mut(peer) {
            Some(session) if session.generation == generation => {
                session.last_used = Instant::now();
                Ok(())
            }
            Some(_) => Err(AdmissionError::Unavailable(
                "admission changed during evaluation".into(),
            )),
            None => {
                drop(sessions);
                Err(self.sessionless_standing(peer))
            }
        }
    }

    /// Handle one connection selected by the caller's sole ALPN accept loop.
    /// Failure closes it. At most one bounded handshake is consumed.
    pub async fn accept(&self, connection: &CoreConnection) -> Result<EnrollmentReadiness> {
        let result = tokio::time::timeout(self.options.handshake_timeout, async {
            ensure!(
                connection.connection_info().alpn == ENROLLMENT_ALPN,
                "wrong enrollment ALPN"
            );
            let peer = identity(&connection.remote_id())?;
            let source = observed_source(connection);
            // A connection holds a handshake slot only once its probe has
            // arrived, so stalled probes cannot exhaust the slots.
            let pending = self.pending_probe(connection);
            let (send, recv, hello) =
                tokio::time::timeout(self.options.first_message_timeout, async {
                    let (send, recv) = connection.accept_bi().await?;
                    let hello: Hello = receive(&recv).await?;
                    Ok::<_, anyhow::Error>((send, recv, hello))
                })
                .await
                .map_err(|_| anyhow!("enrollment first message timeout"))??;
            drop(pending);
            let _slot = self.inbound_slot(peer, source.map(|source| source.address))?;
            ensure!(hello.version == 1, "unsupported enrollment version");
            self.check_skew(hello.sent_at)?;
            // Joining credentials are withheld until the client has verified
            // this responder. The responder's own identity proof is explicitly
            // public to bounded unauthenticated probes; no policy ticket yet.
            transmit(
                &send,
                &ReplicaProof {
                    version: 1,
                    chain: self.credential.clone(),
                    sent_at: unix_now(),
                },
            )
            .await?;
            let presentation: Presentation = self.receive_step(&recv).await?;
            self.check_skew(presentation.sent_at)?;
            let evidence = self
                .evidence(&peer, presentation.chain.as_deref(), source)
                .await?;
            let doc = self
                .document
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let ticket = match doc {
                Some(doc) => Some(doc.share_with_addr("read".into()).await?),
                None => None,
            };
            transmit(
                &send,
                &Welcome {
                    version: 1,
                    policy_ticket: ticket,
                },
            )
            .await?;
            // Explicit reciprocal confirmation: a peer that rejects our identity
            // must not leave a one-way admission behind.
            let confirmed: bool = self.receive_step(&recv).await?;
            ensure!(confirmed, "reciprocal enrollment rejected");
            self.evaluate(&evidence, source).await?;
            transmit(&send, &true).await?;
            let received: bool = self.receive_step(&recv).await?;
            ensure!(received, "reciprocal confirmation missing");
            self.evaluate(&evidence, source).await?;
            self.remember(evidence, source)
        })
        .await
        .map_err(|_| anyhow!("enrollment handshake timeout"))
        .and_then(|x| x);
        let (code, reason) = match &result {
            Ok(_) => (0, b"enrollment complete".to_vec()),
            Err(error) => (REJECT, denial_close_reason(error)),
        };
        let _ = connection.close(code, reason);
        result
    }

    /// One handshake step after the probe: a stalled peer costs at most
    /// `step_timeout` of a held slot, not the whole handshake deadline.
    async fn receive_step<T: for<'de> Deserialize<'de>>(&self, recv: &CoreRecvStream) -> Result<T> {
        tokio::time::timeout(self.options.step_timeout, receive(recv))
            .await
            .map_err(|_| anyhow!("enrollment step timeout"))?
    }

    /// Try configured reachable replicas in order; addresses carry no authority.
    pub async fn connect_replicas(
        &self,
        replicas: Vec<CoreNodeAddr>,
    ) -> Result<(EnrollmentReadiness, Option<String>)> {
        for replica in replicas {
            if let Ok(value) = self.connect(replica).await {
                return Ok(value);
            }
        }
        bail!("no configured replica completed reciprocal enrollment")
    }

    pub async fn connect(
        &self,
        replica: CoreNodeAddr,
    ) -> Result<(EnrollmentReadiness, Option<String>)> {
        tokio::time::timeout(self.options.handshake_timeout, async {
            let _permit = self
                .outbound_permits
                .try_acquire()
                .map_err(|_| anyhow!("enrollment busy"))?;
            let connection = self
                .node
                .net_client()
                .connect_node_addr(replica, ENROLLMENT_ALPN.to_vec())
                .await?;
            let _close_on_completion_or_cancel = CloseEnrollmentConnection(connection.clone());
            let result = async {
                let (send, recv) = connection.open_bi().await?;
                transmit(
                    &send,
                    &Hello {
                        version: 1,
                        sent_at: unix_now(),
                    },
                )
                .await?;
                let proof: ReplicaProof =
                    tokio::time::timeout(self.options.first_message_timeout, receive(&recv))
                        .await
                        .map_err(|_| anyhow!("replica identity proof timeout"))??;
                ensure!(proof.version == 1, "unsupported enrollment version");
                self.check_skew(proof.sent_at)?;
                let peer = identity(&connection.remote_id())?;
                let source = observed_source(&connection);
                let evidence = self.evidence(&peer, proof.chain.as_deref(), source).await?;
                transmit(
                    &send,
                    &Presentation {
                        chain: self.credential.clone(),
                        sent_at: unix_now(),
                    },
                )
                .await?;
                let welcome: Welcome = self.receive_step(&recv).await?;
                ensure!(welcome.version == 1, "unsupported enrollment version");
                transmit(&send, &true).await?;
                let confirmed: bool = self.receive_step(&recv).await?;
                ensure!(confirmed, "reciprocal enrollment rejected");
                self.evaluate(&evidence, source).await?;
                transmit(&send, &true).await?;
                let closed = tokio::time::timeout(self.options.step_timeout, connection.closed())
                    .await
                    .map_err(|_| anyhow!("enrollment step timeout"))?;
                ensure!(
                    closed.code == Some(0),
                    "replica did not complete enrollment"
                );
                self.evaluate(&evidence, source).await?;
                Ok((self.remember(evidence, source)?, welcome.policy_ticket))
            }
            .await;
            match result {
                Ok(value) => Ok(value),
                Err(error) => {
                    // Read the replica's close reason before closing locally.
                    let error = reported_skew(&connection).map_or(error, anyhow::Error::from);
                    let _ = connection.close(REJECT, denial_close_reason(&error));
                    Err(error)
                }
            }
        })
        .await
        .map_err(|_| anyhow!("enrollment handshake timeout"))?
    }

    /// Import only a read capability for the pinned namespace, sync from the
    /// authenticated replica, and wait for successful reconciliation. A replica
    /// timestamp never proves current root policy. Missing content stays unknown.
    pub async fn synchronize(
        &self,
        peer: [u8; 32],
        ticket: String,
        timeout: Duration,
    ) -> Result<CoreDoc> {
        use iroh_docs::{Capability, DocTicket};
        use iroh_tickets::Ticket;
        let mut decoded = DocTicket::decode_string(&ticket)?;
        ensure!(
            matches!(decoded.capability, Capability::Read(_)),
            "bootstrap requires read-only policy capability"
        );
        ensure!(
            namespace(&decoded.capability.id().to_string())? == *self.store.namespace(),
            "wrong policy namespace"
        );
        decoded.nodes.retain(|node| node.id.as_bytes() == &peer);
        ensure!(
            !decoded.nodes.is_empty(),
            "policy ticket has no authenticated replica address"
        );
        self.check_peer(&peer).await?;
        let doc = self
            .node
            .docs_client()
            .join(decoded.encode_string())
            .await?;
        let events = doc.doc.subscribe().await?;
        futures_lite::pin!(events);
        self.set_policy_document(doc.clone())?;
        self.note_pending_policy_content();
        doc.start_sync(vec![hex::encode(peer)]).await?;
        let result = tokio::time::timeout(timeout, async {
            use futures_lite::StreamExt;
            let mut synchronized = false;
            let mut backoff = crate::CrossedSyncBackoff::default();
            loop {
                match events.next().await {
                    Some(Ok(iroh_docs::engine::LiveEvent::SyncFinished(event)))
                        if identity(&event.peer.to_string())? == peer =>
                    {
                        match &event.result {
                            Ok(_) => synchronized = true,
                            // The replica's own sync on gossip neighbour-up
                            // crossed our direct join. The other direction
                            // normally completes on its own; dial again in
                            // case the replica cannot reach us. The deadline
                            // still bounds it.
                            Err(error) if crate::crossed_sync_abort(error) => {
                                tokio::time::sleep(backoff.next()).await;
                                doc.start_sync(vec![hex::encode(peer)]).await?;
                            }
                            Err(error) => bail!("policy sync failed: {error}"),
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error),
                    None => bail!("policy sync ended"),
                }
                if synchronized {
                    let report = self.store.refresh(&doc).await?;
                    self.context
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .pending_policy_content = report.has_uncertainty();
                    if report.pending == 0 {
                        break;
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        })
        .await;
        result.map_err(|_| anyhow!("policy synchronization timeout"))??;
        self.reconcile().await;
        // This observes successful transport reconciliation only. Individual
        // policy reads still report pending content and must not be fabricated.
        self.context
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .policy_sync = Some(self.synchronization(peer));
        self.check_peer(&peer).await?;
        Ok(doc)
    }

    /// Recheck before each consumer operation. Gate-0 also calls this before
    /// accepting each new ordinary connection. This does not authorize the
    /// operation itself. Only a definite denial revokes the session.
    pub async fn check_peer(
        &self,
        peer: &[u8; 32],
    ) -> std::result::Result<EnrollmentReadiness, AdmissionError> {
        let Some(session) = self.session(peer) else {
            return Err(self.sessionless_standing(peer));
        };
        if let Err(error) = self
            .evaluate(&session.evidence, session.observed_source)
            .await
        {
            if let AdmissionError::Denied(reason) = &error {
                self.revoke_generation(peer, session.generation, reason.clone());
            }
            return Err(error);
        }
        self.confirm(peer, session.generation)?;
        Ok(self.readiness(*peer, session.evidence.source(), session.observed_source))
    }

    /// Recheck a live connection's current selected path at operation time.
    /// Consumers call this after path changes or before sensitive operations.
    pub async fn check_connection(
        &self,
        connection: &CoreConnection,
    ) -> std::result::Result<EnrollmentReadiness, AdmissionError> {
        let peer = identity(&connection.remote_id())
            .map_err(|error| AdmissionError::Unavailable(error.to_string()))?;
        self.check_peer_at_source(&peer, observed_source(connection))
            .await
    }

    /// Evaluate a stronger consumer operation policy against the same opaque
    /// evidence, real pending/freshness state, and current connection path.
    /// Mandatory validity and known revocations still surround that handler.
    pub async fn check_connection_with_policy(
        &self,
        connection: &CoreConnection,
        policy: &dyn AdmissionPolicy,
        timeout: Duration,
    ) -> std::result::Result<EnrollmentReadiness, AdmissionError> {
        self.check_connection(connection).await?;
        let peer = identity(&connection.remote_id())
            .map_err(|error| AdmissionError::Unavailable(error.to_string()))?;
        let Some(session) = self.session(&peer) else {
            return Err(self.sessionless_standing(&peer));
        };
        let source = observed_source(connection);
        self.evaluate_with_policy(&session.evidence, source, policy, timeout)
            .await?;
        self.confirm(&peer, session.generation)?;
        Ok(self.readiness(peer, session.evidence.source(), source))
    }

    /// The handshake gate denies a connection without revoking the session:
    /// its source may differ from the one admitted.
    async fn check_peer_at_source(
        &self,
        peer: &[u8; 32],
        source: Option<ObservedSourceAddress>,
    ) -> std::result::Result<EnrollmentReadiness, AdmissionError> {
        let Some(session) = self.session(peer) else {
            return Err(self.sessionless_standing(peer));
        };
        self.evaluate(&session.evidence, source).await?;
        self.confirm(peer, session.generation)?;
        Ok(self.readiness(*peer, session.evidence.source(), source))
    }

    /// Opt into closing a connection that did not pass this node's Gate 0
    /// hook when this service learns denial. Connections Gate 0 admitted,
    /// including built-in Docs/blobs/gossip/RPC ones, are closed already;
    /// consumers still cancel their own work on revocation notifications.
    pub async fn track_connection(&self, connection: CoreConnection) -> Result<()> {
        let peer = identity(&connection.remote_id())?;
        self.check_connection(&connection).await?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions
            .get_mut(&peer)
            .ok_or_else(|| anyhow!("peer revoked"))?;
        session
            .connections
            .retain(|connection| connection.connection_info().is_connected);
        ensure!(session.connections.len() < 64, "tracked connection limit");
        session.connections.push(connection);
        Ok(())
    }

    fn revoke_generation(&self, peer: &[u8; 32], expected_generation: u64, reason: DenialReason) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if sessions
            .get(peer)
            .is_none_or(|session| session.generation != expected_generation)
        {
            return;
        }
        let Some(session) = sessions.remove(peer) else {
            return;
        };
        self.revoked_peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(*peer, reason.clone(), self.options.max_admitted_peers);
        drop(sessions);
        for connection in session.connections {
            let _ = connection.close(REJECT, b"enrollment revoked".to_vec());
        }
        for connection in session.admitted {
            connection.close(REJECT, b"enrollment revoked");
        }
        let _ = self.revoked.send(EnrollmentRevoked {
            peer: *peer,
            reason,
        });
    }

    /// Refresh currently admitted identities from the local Docs replica.
    /// Only a definite denial revokes; storage errors, pending content and
    /// policy timeouts keep the session for the next pass.
    pub async fn reconcile(&self) {
        let doc = self
            .document
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(doc) = doc {
            let pending = match self.store.refresh(&doc).await {
                Ok(report) => report.has_uncertainty(),
                Err(_) => true,
            };
            self.context
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .pending_policy_content = pending;
        }
        let peers: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(peer, session)| {
                (
                    *peer,
                    session.evidence.clone(),
                    session.observed_source,
                    session.generation,
                )
            })
            .collect();
        // Enforce all already-observed mandatory denials before awaiting any
        // consumer handler. A slow unrelated policy cannot defer revocations.
        let mut pending = VecDeque::new();
        for (peer, evidence, source, generation) in peers {
            match self
                .store
                .select_evidence(&peer, Some(&evidence), unix_now())
            {
                Err(RecordError::Denied(reason)) if reason.is_definite() => {
                    self.revoke_generation(&peer, generation, reason);
                }
                _ => pending.push_back((peer, evidence, source, generation)),
            }
        }
        let count = pending
            .len()
            .min(self.options.max_concurrent_reconciliations);
        let pending = Arc::new(Mutex::new(pending));
        let mut workers: Pin<Box<dyn Future<Output = ()> + Send + '_>> = Box::pin(async {});
        for _ in 0..count {
            let pending = pending.clone();
            let worker = async move {
                loop {
                    let next = pending
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .pop_front();
                    let Some((peer, old, source, generation)) = next else {
                        break;
                    };
                    match self.evidence(&peer, old.presented_chain(), source).await {
                        Ok(evidence) => {
                            let _ = self.replace_session(evidence, source, Some(generation));
                        }
                        Err(AdmissionError::Denied(reason)) => {
                            self.revoke_generation(&peer, generation, reason);
                        }
                        Err(_) => {}
                    }
                }
            };
            workers = Box::pin(async move {
                futures_lite::future::zip(workers, worker).await;
            });
        }
        workers.await;
    }
}

fn identity(hex: &str) -> Result<[u8; 32]> {
    Ok(super::canonical_identity(hex)?)
}
fn namespace(hex: &str) -> Result<[u8; 32]> {
    hex::decode(hex)?
        .try_into()
        .map_err(|_| anyhow!("invalid policy namespace"))
}
fn observed_source(connection: &CoreConnection) -> Option<ObservedSourceAddress> {
    connection
        .transport_snapshot()
        .peer_addr
        .map(|address| ObservedSourceAddress {
            address: address.ip(),
            provenance: SourceAddressProvenance::DirectPeer,
        })
}
fn hook_decision(allow: bool) -> CoreAfterHandshakeDecision {
    if allow {
        CoreAfterHandshakeDecision::Accept
    } else {
        CoreAfterHandshakeDecision::Reject {
            error_code: REJECT as u32,
            reason: b"enrollment required".to_vec(),
        }
    }
}
fn denial_close_reason(error: &anyhow::Error) -> Vec<u8> {
    match error.downcast_ref::<AdmissionError>() {
        Some(AdmissionError::ClockSkew { offset_seconds }) => {
            format!("{CLOCK_SKEW_REASON}{offset_seconds}").into_bytes()
        }
        _ => b"enrollment denied".to_vec(),
    }
}
/// A replica that closed with a skew reason measured our clock against its
/// own; restate the offset from our side (remote minus local).
fn reported_skew(connection: &CoreConnection) -> Option<AdmissionError> {
    let closed = connection.close_info()?;
    if closed.code != Some(REJECT) {
        return None;
    }
    let reason = String::from_utf8(closed.reason?).ok()?;
    let theirs: i64 = reason.strip_prefix(CLOCK_SKEW_REASON)?.parse().ok()?;
    let offset_seconds = theirs.checked_neg()?;
    tracing::warn!(offset_seconds, "enrollment: replica reports clock skew");
    Some(AdmissionError::ClockSkew { offset_seconds })
}
struct CloseEnrollmentConnection(CoreConnection);
impl Drop for CloseEnrollmentConnection {
    fn drop(&mut self) {
        let _ = self.0.close(0, b"enrollment complete".to_vec());
    }
}
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn transmit(send: &CoreSendStream, value: &impl Serialize) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    ensure!(
        payload.len() <= MAX_ENVELOPE,
        "enrollment envelope too large"
    );
    send.write_all((payload.len() as u32).to_be_bytes().to_vec())
        .await?;
    send.write_all(payload).await?;
    Ok(())
}
async fn receive<T: for<'de> Deserialize<'de>>(recv: &CoreRecvStream) -> Result<T> {
    let mut prefix = [0; 4];
    prefix.copy_from_slice(&recv.read_exact(4).await?);
    let len = u32::from_be_bytes(prefix) as usize;
    ensure!(len <= MAX_ENVELOPE, "enrollment envelope too large");
    Ok(serde_json::from_slice(&recv.read_exact(len).await?)?)
}

#[cfg(test)]
mod tests {
    use super::super::{DenialReason, EnrollmentIssueRequest, JwtEnrollmentIssuer, PolicyError};
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Notify;

    const AUDIENCE: &str = "bootstrap-races";

    fn public(seed: u8) -> [u8; 32] {
        JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
    }

    /// A fresh root-issued credential; each has its own random `jti`.
    fn evidence(seed: u8) -> VerifiedAdmissionEvidence {
        let request = EnrollmentIssueRequest::new(public(seed), AUDIENCE, unix_now() + 3600);
        let issued = JwtEnrollmentIssuer::from_seed(&[1; 32])
            .issue(&request, unix_now())
            .unwrap();
        JwtEnrollmentVerifier::new(public(1), AUDIENCE)
            .unwrap()
            .verify(issued.chain(), &public(seed), unix_now())
            .unwrap()
    }

    struct PausedPolicy {
        pause_next: AtomicBool,
        evaluated: AtomicU64,
        entered: Notify,
        release: Notify,
        mixed: bool,
    }

    impl PausedPolicy {
        fn new(mixed: bool) -> Arc<Self> {
            Arc::new(Self {
                pause_next: AtomicBool::new(true),
                evaluated: AtomicU64::new(0),
                entered: Notify::new(),
                release: Notify::new(),
                mixed,
            })
        }
    }

    #[async_trait::async_trait]
    impl AdmissionPolicy for PausedPolicy {
        async fn evaluate(
            &self,
            evidence: &VerifiedAdmissionEvidence,
            _: &AdmissionContext,
            _: u64,
        ) -> std::result::Result<AdmissionDecision, PolicyError> {
            if evidence.subject() == &public(3) && self.pause_next.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.evaluated.fetch_add(1, Ordering::SeqCst);
            if self.mixed && evidence.subject() == &public(4) {
                Ok(AdmissionDecision::Deny(DenialReason::Consumer(
                    "test denial".into(),
                )))
            } else if self.mixed && evidence.subject() == &public(5) {
                Err(PolicyError::Handler("test error".into()))
            } else {
                Ok(AdmissionDecision::Allow)
            }
        }
    }

    // Bypass install's periodic task so each interleaving is explicitly driven.
    async fn service(
        policy: Arc<dyn AdmissionPolicy>,
        options: EnrollmentOptions,
    ) -> (Arc<EnrollmentService>, std::path::PathBuf) {
        let node = raw_node(2).await;
        let mut nonce = [0; 8];
        getrandom::getrandom(&mut nonce).unwrap();
        let path = std::env::temp_dir().join(format!(
            "aster-bootstrap-{}-{}.json",
            std::process::id(),
            hex::encode(nonce)
        ));
        let store =
            Arc::new(EnrollmentPolicyStore::open(&path, public(1), [42; 32], AUDIENCE).unwrap());
        let verifier = JwtEnrollmentVerifier::new(public(1), AUDIENCE).unwrap();
        (
            EnrollmentService::new(node, store, verifier, None, policy, options),
            path,
        )
    }

    async fn cleanup(service: Arc<EnrollmentService>, path: std::path::PathBuf) {
        service.node.close().await;
        drop(service);
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(lock_path);
    }

    #[tokio::test]
    async fn reconciliation_cannot_resurrect_or_overwrite_reenrollment() {
        for reenroll in [false, true] {
            let policy = PausedPolicy::new(false);
            let (service, path) = service(policy.clone(), EnrollmentOptions::default()).await;
            service.remember(evidence(3), None).unwrap();
            let old_generation = service.session(&public(3)).unwrap().generation;
            let task = tokio::spawn({
                let service = service.clone();
                async move { service.reconcile().await }
            });
            policy.entered.notified().await;
            service.revoke_generation(&public(3), old_generation, DenialReason::NodeRevoked);
            let renewed = evidence(3);
            let renewed_id = renewed.credential_id().map(str::to_owned);
            if reenroll {
                service.remember(renewed, None).unwrap();
            }
            policy.release.notify_one();
            task.await.unwrap();
            if reenroll {
                assert_eq!(
                    service
                        .session(&public(3))
                        .unwrap()
                        .evidence
                        .credential_id(),
                    renewed_id.as_deref()
                );
            } else {
                assert!(service.session(&public(3)).is_none());
            }
            cleanup(service, path).await;
        }
    }

    #[tokio::test]
    async fn unchanged_reconciliation_preserves_slow_check_generation() {
        let policy = PausedPolicy::new(false);
        let (service, path) = service(policy.clone(), EnrollmentOptions::default()).await;
        service.remember(evidence(3), None).unwrap();
        let generation = service.session(&public(3)).unwrap().generation;
        let task = tokio::spawn({
            let service = service.clone();
            async move { service.check_peer(&public(3)).await }
        });
        policy.entered.notified().await;
        service.reconcile().await;
        assert_eq!(service.session(&public(3)).unwrap().generation, generation);
        policy.release.notify_one();
        assert!(task.await.unwrap().is_ok());
        cleanup(service, path).await;
    }

    #[tokio::test]
    async fn reconciliation_isolates_slow_denied_and_failed_policies() {
        let policy = PausedPolicy::new(true);
        let (service, path) = service(
            policy.clone(),
            EnrollmentOptions {
                max_concurrent_reconciliations: 4,
                ..Default::default()
            },
        )
        .await;
        for seed in [3, 4, 5, 7] {
            service.remember(evidence(seed), None).unwrap();
        }
        for seed in 20..52 {
            service.remember(evidence(seed), None).unwrap();
        }
        let task = tokio::spawn({
            let service = service.clone();
            async move { service.reconcile().await }
        });
        policy.entered.notified().await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while service.has_session(&public(4)) || policy.evaluated.load(Ordering::SeqCst) < 35 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!task.is_finished());
        assert!(service.has_session(&public(3)) && service.has_session(&public(7)));
        policy.release.notify_one();
        task.await.unwrap();
        assert!(service.check_peer(&public(7)).await.is_ok());
        // A definite consumer denial revokes; a failing handler only makes
        // the peer's standing unknown, so its session survives.
        assert!(matches!(
            service.check_peer(&public(4)).await,
            Err(AdmissionError::Denied(DenialReason::Consumer(_)))
        ));
        assert!(service.has_session(&public(5)));
        assert!(matches!(
            service.check_peer(&public(5)).await,
            Err(AdmissionError::Unavailable(_))
        ));
        assert!(service.has_session(&public(5)));
        cleanup(service, path).await;
    }

    #[tokio::test]
    async fn sessionless_peer_reports_not_admitted_until_revoked() {
        let (service, path) = service(PausedPolicy::new(true), EnrollmentOptions::default()).await;
        let mut revocations = service.revocations();
        assert_eq!(
            service.check_peer(&public(4)).await.unwrap_err(),
            AdmissionError::NotAdmitted
        );
        service.remember(evidence(4), None).unwrap();
        assert!(matches!(
            service.check_peer(&public(4)).await,
            Err(AdmissionError::Denied(DenialReason::Consumer(_)))
        ));
        let revoked = revocations.try_recv().unwrap();
        assert_eq!(revoked.peer, public(4));
        assert!(matches!(revoked.reason, DenialReason::Consumer(_)));
        // The revocation outlives the session it removed.
        assert!(matches!(
            service.check_peer(&public(4)).await,
            Err(AdmissionError::Denied(DenialReason::Consumer(_)))
        ));
        // A later successful admission clears it.
        service.remember(evidence(4), None).unwrap();
        assert!(service.revoked_peers.lock().unwrap().reasons.is_empty());
        cleanup(service, path).await;
    }

    #[tokio::test]
    async fn full_session_table_evicts_only_idle_sessions() {
        let (service, path) = service(
            PausedPolicy::new(false),
            EnrollmentOptions {
                max_admitted_peers: 2,
                session_idle_eviction: Duration::from_secs(60),
                ..Default::default()
            },
        )
        .await;
        service.remember(evidence(10), None).unwrap();
        service.remember(evidence(11), None).unwrap();
        assert!(service.remember(evidence(12), None).is_err());
        service
            .sessions
            .lock()
            .unwrap()
            .get_mut(&public(10))
            .unwrap()
            .last_used = Instant::now() - Duration::from_secs(120);
        service.remember(evidence(12), None).unwrap();
        assert!(!service.has_session(&public(10)));
        assert!(service.has_session(&public(11)) && service.has_session(&public(12)));
        // Eviction is not a revocation.
        assert_eq!(
            service.check_peer(&public(10)).await.unwrap_err(),
            AdmissionError::NotAdmitted
        );
        cleanup(service, path).await;
    }

    #[tokio::test]
    async fn handshake_limits_are_independent_and_release_bounded_state() {
        let (service, path) = service(
            PausedPolicy::new(false),
            EnrollmentOptions {
                max_concurrent_handshakes: 2,
                max_concurrent_handshakes_per_source: 1,
                max_handshakes_per_second: 2,
                max_handshakes_per_second_per_source: 3,
                ..Default::default()
            },
        )
        .await;
        let first: IpAddr = "127.0.0.1".parse().unwrap();
        let second: IpAddr = "127.0.0.2".parse().unwrap();
        let one = service.inbound_slot(public(3), Some(first)).unwrap();
        assert!(service.inbound_slot(public(3), Some(second)).is_err());
        assert!(service.inbound_slot(public(4), Some(first)).is_err());
        let two = service.inbound_slot(public(4), Some(second)).unwrap();
        assert!(service.inbound_slot(public(5), None).is_err());
        assert!(service.outbound_permits.try_acquire().is_ok());
        assert!(service.gate_permits.try_acquire().is_ok());
        drop((one, two));
        assert!(service.handshake_counts.lock().unwrap().peers.is_empty());
        assert!(service.handshake_counts.lock().unwrap().sources.is_empty());
        assert!(service.take_rate_slot(public(3), Some(first)));
        assert!(service.take_rate_slot(public(3), Some(first)));
        assert!(!service.take_rate_slot(public(3), Some(first)));
        assert!(service.take_rate_slot(public(4), Some(first)));
        assert!(!service.take_rate_slot(public(5), Some(first)));
        assert!(service.take_rate_slot(public(5), Some(second)));
        assert!(service.take_rate_slot(public(6), None));
        assert!(service.take_rate_slot(public(7), None));
        service.rate.lock().unwrap().started = Instant::now() - Duration::from_secs(2);
        assert!(service.take_rate_slot(public(3), Some(first)));
        assert_eq!(service.rate.lock().unwrap().peers.len(), 1);
        assert_eq!(service.rate.lock().unwrap().sources.len(), 1);
        {
            let mut rate = service.rate.lock().unwrap();
            rate.started = Instant::now() + Duration::from_secs(60);
            rate.peers = (0..MAX_RATE_KEYS)
                .map(|i| {
                    let mut key = [0; 32];
                    key[..8].copy_from_slice(&(i as u64).to_be_bytes());
                    (key, 1)
                })
                .collect();
        }
        assert!(!service.take_rate_slot(public(8), None));
        assert_eq!(service.rate.lock().unwrap().peers.len(), MAX_RATE_KEYS);
        cleanup(service, path).await;
    }

    /// A loopback node serving the enrollment ALPN without admission hooks.
    async fn raw_node(seed: u8) -> CoreNode {
        CoreNode::memory_with_alpns(
            vec![ENROLLMENT_ALPN.to_vec()],
            Some(crate::CoreEndpointConfig {
                relay_mode: Some("disabled".into()),
                bind_addr: Some("127.0.0.1:0".into()),
                secret_key: Some(vec![seed; 32]),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
    }

    /// Open an enrollment connection from `client` and hand the inbound side
    /// to `service.accept`.
    async fn raw_handshake(
        service: &Arc<EnrollmentService>,
        client: &CoreNode,
    ) -> (
        CoreConnection,
        tokio::task::JoinHandle<Result<EnrollmentReadiness>>,
    ) {
        let outbound = client
            .net_client()
            .connect_node_addr(service.node.node_addr_info(), ENROLLMENT_ALPN.to_vec())
            .await
            .unwrap();
        let (_, inbound) = service.node.accept_aster().await.unwrap();
        let accept = tokio::spawn({
            let service = service.clone();
            async move { service.accept(&inbound).await }
        });
        (outbound, accept)
    }

    fn assert_skew(error: &anyhow::Error, expected: i64) {
        match error.downcast_ref::<AdmissionError>() {
            Some(AdmissionError::ClockSkew { offset_seconds }) => assert!(
                (offset_seconds - expected).abs() <= 1,
                "offset {offset_seconds}, expected {expected}"
            ),
            other => panic!("expected clock skew, got {other:?}: {error:#}"),
        }
    }

    #[tokio::test]
    async fn handshake_clock_skew_beyond_leeway_is_diagnosed_on_both_sides() {
        let (service, path) = service(PausedPolicy::new(false), EnrollmentOptions::default()).await;
        let client = raw_node(7).await;
        // Within the 60 s leeway the replica answers with its proof.
        let (connection, accept) = raw_handshake(&service, &client).await;
        let (send, recv) = connection.open_bi().await.unwrap();
        let hello = Hello {
            version: 1,
            sent_at: unix_now() + 30,
        };
        transmit(&send, &hello).await.unwrap();
        let proof: ReplicaProof = receive(&recv).await.unwrap();
        assert!(proof.sent_at.abs_diff(unix_now()) <= 1);
        connection.close(0, b"done".to_vec()).unwrap();
        assert!(accept.await.unwrap().is_err());

        // Beyond it the replica refuses before disclosing its proof, and says
        // why in the close reason.
        let (connection, accept) = raw_handshake(&service, &client).await;
        let (send, recv) = connection.open_bi().await.unwrap();
        let hello = Hello {
            version: 1,
            sent_at: unix_now() + 90,
        };
        transmit(&send, &hello).await.unwrap();
        assert!(receive::<ReplicaProof>(&recv).await.is_err());
        assert_skew(&accept.await.unwrap().unwrap_err(), 90);
        let closed = connection.closed().await;
        assert_eq!(closed.code, Some(REJECT));
        let reason = String::from_utf8(closed.reason.unwrap()).unwrap();
        let reported: i64 = reason
            .strip_prefix(CLOCK_SKEW_REASON)
            .unwrap()
            .parse()
            .unwrap();
        assert!((reported - 90).abs() <= 1, "{reason}");

        // A presented credential issued 90 s ahead is skew too, not a
        // definite denial.
        let (connection, accept) = raw_handshake(&service, &client).await;
        let (send, recv) = connection.open_bi().await.unwrap();
        transmit(
            &send,
            &Hello {
                version: 1,
                sent_at: unix_now(),
            },
        )
        .await
        .unwrap();
        let _: ReplicaProof = receive(&recv).await.unwrap();
        let ahead = JwtEnrollmentIssuer::from_seed(&[1; 32])
            .issue(
                &EnrollmentIssueRequest::new(public(7), AUDIENCE, unix_now() + 3600),
                unix_now() + 90,
            )
            .unwrap();
        transmit(
            &send,
            &Presentation {
                chain: Some(ahead.into_chain()),
                sent_at: unix_now(),
            },
        )
        .await
        .unwrap();
        assert_skew(&accept.await.unwrap().unwrap_err(), 90);
        client.close().await;
        cleanup(service, path).await;
    }

    #[tokio::test]
    async fn joiner_reports_replica_clock_skew_in_its_own_frame() {
        let (service, path) = service(PausedPolicy::new(false), EnrollmentOptions::default()).await;
        for replica_closes in [false, true] {
            let replica = raw_node(9).await;
            let handler = tokio::spawn({
                let replica = replica.clone();
                async move {
                    let (_, connection) = replica.accept_aster().await.unwrap();
                    let (send, recv) = connection.accept_bi().await.unwrap();
                    let hello: Hello = receive(&recv).await.unwrap();
                    if replica_closes {
                        // The replica measured the joiner 90 s ahead of it.
                        let _ =
                            connection.close(REJECT, format!("{CLOCK_SKEW_REASON}90").into_bytes());
                    } else {
                        // The replica's own clock runs 90 s behind.
                        let proof = ReplicaProof {
                            version: 1,
                            chain: None,
                            sent_at: hello.sent_at - 90,
                        };
                        transmit(&send, &proof).await.unwrap();
                    }
                    connection.closed().await
                }
            });
            let error = service.connect(replica.node_addr_info()).await.unwrap_err();
            assert_skew(&error, -90);
            handler.await.unwrap();
            replica.close().await;
        }
        cleanup(service, path).await;
    }

    fn pending(service: &EnrollmentService) -> usize {
        service.pending_probes.lock().unwrap().waiting.len()
    }

    async fn wait_until(condition: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stalled_probes_hold_no_handshake_slot_or_budget() {
        let (service, path) = service(
            PausedPolicy::new(false),
            EnrollmentOptions {
                max_concurrent_handshakes: 1,
                max_concurrent_handshakes_per_source: 1,
                first_message_timeout: Duration::from_secs(5),
                ..Default::default()
            },
        )
        .await;
        let mut stalled = Vec::new();
        for seed in 20..24 {
            let client = raw_node(seed).await;
            let (connection, accept) = raw_handshake(&service, &client).await;
            stalled.push((client, connection, accept));
        }
        wait_until(|| pending(&service) == stalled.len()).await;
        // Every slot and the one per-source allowance are still free: a
        // joiner from the same loopback source completes its probe.
        assert!(service.handshake_counts.lock().unwrap().sources.is_empty());
        assert_eq!(service.inbound_permits.available_permits(), 1);
        let client = raw_node(7).await;
        let (connection, accept) = raw_handshake(&service, &client).await;
        let (send, recv) = connection.open_bi().await.unwrap();
        transmit(
            &send,
            &Hello {
                version: 1,
                sent_at: unix_now(),
            },
        )
        .await
        .unwrap();
        let _: ReplicaProof = receive(&recv).await.unwrap();
        assert_eq!(service.inbound_permits.available_permits(), 0);
        assert_eq!(pending(&service), stalled.len());
        connection.close(0, b"done".to_vec()).unwrap();
        assert!(accept.await.unwrap().is_err());
        for (client, connection, accept) in stalled {
            connection.close(0, b"done".to_vec()).unwrap();
            assert!(accept.await.unwrap().is_err());
            client.close().await;
        }
        assert_eq!(pending(&service), 0);
        client.close().await;
        cleanup(service, path).await;
    }

    /// Regression: `synchronize` (and `CoreDoc::sync_with_timeout`) failed
    /// with "Remote peer aborted sync: AlreadySyncing" when the replica's own
    /// gossip-triggered sync crossed our direct join. Only that exact abort
    /// is waited out; every other failed sync, including other aborts, still
    /// fails closed.
    #[test]
    fn only_a_crossed_sync_abort_is_waited_out() {
        use iroh_docs::net::{AbortReason, ConnectError};
        assert!(crate::crossed_sync_abort(
            &ConnectError::RemoteAbort(AbortReason::AlreadySyncing).to_string()
        ));
        assert_eq!(
            ConnectError::RemoteAbort(AbortReason::AlreadySyncing).to_string(),
            "Remote peer aborted sync: AlreadySyncing"
        );
        for other in [AbortReason::NotFound, AbortReason::InternalServerError] {
            assert!(!crate::crossed_sync_abort(
                &ConnectError::RemoteAbort(other).to_string()
            ));
        }
        assert!(!crate::crossed_sync_abort("failed to sync: AlreadySyncing"));
        let mut backoff = crate::CrossedSyncBackoff::default();
        let waits: Vec<_> = (0..5).map(|_| backoff.next().as_millis()).collect();
        assert_eq!(waits, [250, 500, 1000, 2000, 2000]);
    }

    #[tokio::test]
    async fn probe_then_stall_releases_its_slot_within_one_step() {
        let (service, path) = service(
            PausedPolicy::new(false),
            EnrollmentOptions {
                max_concurrent_handshakes: 1,
                policy_timeout: Duration::from_millis(100),
                step_timeout: Duration::from_millis(300),
                ..Default::default()
            },
        )
        .await;
        let client = raw_node(7).await;
        let (connection, accept) = raw_handshake(&service, &client).await;
        let (send, recv) = connection.open_bi().await.unwrap();
        transmit(
            &send,
            &Hello {
                version: 1,
                sent_at: unix_now(),
            },
        )
        .await
        .unwrap();
        let _: ReplicaProof = receive(&recv).await.unwrap();
        assert_eq!(service.inbound_permits.available_permits(), 0);
        // The client now stalls instead of presenting. The slot is released
        // after one step deadline, well before the 10 s handshake deadline.
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(Duration::from_secs(2), accept)
            .await
            .expect("stalled step must not hold the slot until the handshake deadline")
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("step timeout"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(service.inbound_permits.available_permits(), 1);
        drop((send, recv));
        client.close().await;
        cleanup(service, path).await;
    }

    #[tokio::test]
    async fn full_pre_probe_pool_closes_the_oldest_waiting_connection() {
        let (service, path) = service(
            PausedPolicy::new(false),
            EnrollmentOptions {
                max_pending_probes: 2,
                first_message_timeout: Duration::from_secs(5),
                ..Default::default()
            },
        )
        .await;
        let mut waiting = Vec::new();
        for seed in 20..23 {
            let client = raw_node(seed).await;
            let (connection, accept) = raw_handshake(&service, &client).await;
            waiting.push((client, connection, accept));
            wait_until(|| pending(&service) == waiting.len().min(2)).await;
        }
        let (_, oldest, accept) = &mut waiting[0];
        let closed = tokio::time::timeout(Duration::from_secs(2), oldest.closed())
            .await
            .unwrap();
        assert_eq!(closed.code, Some(REJECT));
        assert_eq!(closed.reason.unwrap(), b"enrollment busy");
        assert!(accept.await.unwrap().is_err());
        for (_, connection, _) in &waiting[1..] {
            assert!(connection.close_info().is_none());
        }
        for (client, connection, accept) in waiting.into_iter().skip(1) {
            connection.close(0, b"done".to_vec()).unwrap();
            assert!(accept.await.unwrap().is_err());
            client.close().await;
        }
        assert_eq!(pending(&service), 0);
        cleanup(service, path).await;
    }
}
