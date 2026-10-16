//! Opt-in reciprocal enrollment on a restricted ALPN.
//!
//! Install before dialing, route only this ALPN to `accept`, and keep the
//! returned service alive. Ordinary protocols stay gated until verified
//! admission. Configured replica addresses are discovery hints, never trust:
//! a configured replica this node has no evidence for yet is trusted for
//! synchronizing the fleet namespace only (see [`EnrollmentService::attach_replicas`]).

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
use tokio::sync::{broadcast, oneshot, watch, Semaphore, SemaphorePermit};
use tokio::task::AbortHandle;

use super::rotation::{verified_history, RotationJournal, RotationPin, MAX_FORWARDED_ROTATIONS};
use super::{
    admit, local_clock_synchronization, AdmissionContext, AdmissionDecision,
    AdmissionEvidenceSource, AdmissionPolicy, ClockSynchronization, DenialReason, EnrollmentError,
    EnrollmentPolicyStore, JwtEnrollmentVerifier, NamespaceBindingV1, NamespacePurpose,
    ObservationKind, ObservedSourceAddress, ObserverGrant, PolicyStores, PolicySynchronization,
    RecordAvailability, RecordError, Registration, SourceAddressProvenance, SynchronizationSource,
    VerifiedAdmissionEvidence, MAX_CHAIN_BYTES,
};
use crate::delegation::{
    claimed_rotation_seq, verify_root_rotation, ChainError, RootState, MAX_ROTATION_BYTES,
};
use crate::{
    CoreAfterHandshakeDecision, CoreConnection, CoreDoc, CoreHookConnectInfo,
    CoreHookHandshakeInfo, CoreHookReceiver, CoreNode, CoreNodeAddr, CoreRecvStream,
    CoreSendStream, CoreWeakConnection,
};

pub const ENROLLMENT_ALPN: &[u8] = b"aster.enrollment/1";
const MAX_ENVELOPE: usize = MAX_CHAIN_BYTES + 8192 + MAX_FORWARDED_ROTATIONS * MAX_ROTATION_BYTES;
const REJECT: u64 = 403;
/// Close reason prefix carrying the observed offset (remote minus local
/// seconds) back to a peer whose clock is outside the activation leeway.
const CLOCK_SKEW_REASON: &str = "enrollment clock skew ";
/// Close reason for a definite denial ([`AdmissionError::Denied`]).
const DENIED_REASON: &str = "enrollment denied";
/// Close reason when policy could not be evaluated now
/// ([`AdmissionError::Unavailable`] / [`AdmissionError::NotAdmitted`]).
const UNAVAILABLE_REASON: &str = "enrollment unavailable";
/// Close reason for a failure that is not an admission decision (protocol,
/// transport, a timeout or a busy responder).
const FAILED_REASON: &str = "enrollment failed";
const MAX_RATE_KEYS: usize = 8192;
/// Target of the per-connection Gate-0 decision events, shared with the
/// facade's own admission log (`aster::log_targets::ADMISSION`).
const ADMISSION_TARGET: &str = "aster::admission";
/// A denied peer gets at most one info-level line per interval; the rest of
/// its denials log at debug. Remote peers choose how often they dial.
const DENY_LOG_PEER_INTERVAL: Duration = Duration::from_secs(10);
/// Info-level denial lines per second across all peers, so fresh identities
/// cannot flood the log either.
const DENY_LOG_PER_SECOND: u32 = 32;
/// After an on-demand handshake with a peer fails, further dials to or from
/// it fail at once for this long instead of handshaking again, so a protocol
/// that redials in a loop (or a stranger that keeps dialling) costs at most
/// one handshake per interval.
const ON_DEMAND_RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// The ALPNs synchronizing the fleet namespace from a replica needs: Docs set
/// reconciliation and the blob fetches for its entries' content.
fn replica_sync_alpn(alpn: &[u8]) -> bool {
    alpn == iroh_docs::ALPN || alpn == iroh_blobs::ALPN || alpn == crate::BLOBS_ZSTD_ALPN
}

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
    /// Accept root rotations: the commitment to the next root key the
    /// consumer pins beside its root. `None` refuses every rotation
    /// statement, and the root can then only change by reinstalling.
    pub rotation: Option<RotationPin>,
    /// Rotation statements that produced the pinned root, when it was moved
    /// on out of band (`aster trust rotate --apply`, a configuration update),
    /// oldest first. They are verified back from the pin and journaled, so
    /// this node relays them to peers still on an older root. Ignored
    /// without [`Self::rotation`]; a statement that is not part of the pinned
    /// history fails installation.
    pub applied_rotations: Vec<String>,
    /// After [`EnrollmentService::attach_replicas`]: how often each configured
    /// replica is synchronized again (one with a session) or retried (one
    /// without), so later registrations and revocations arrive without a
    /// root publication session even when a live Docs subscription dropped.
    pub replica_sync_interval: Duration,
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
            rotation: None,
            applied_rotations: Vec::new(),
            replica_sync_interval: Duration::from_secs(30),
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

/// The root's statement about a subject, as this node last reconciled it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubjectStanding {
    /// An active, currently valid registration admits it.
    Registered,
    /// A definite denial: revoked, superseded, rejected or expired.
    Denied(DenialReason),
    /// No registration anywhere; a chain is its only possible evidence.
    Unregistered,
    /// Policy could not be evaluated (pending content, store unavailable).
    Unknown(RecordError),
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
    /// Stable across renewals of the same subject while the session lives:
    /// the generation that started it. A session that ended (revoked,
    /// evicted, expired) starts a new lineage when the peer is admitted again.
    lineage: u64,
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
    /// Root rotations this node applied, oldest first. Self-certifying, so a
    /// peer applies any that continue its own pinned root.
    #[serde(default)]
    rotations: Vec<String>,
    /// Sender's Unix seconds, for clock-skew diagnosis.
    sent_at: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaProof {
    version: u8,
    #[serde(default)]
    rotations: Vec<String>,
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
    /// Read ticket for the fleet namespace.
    policy_ticket: Option<String>,
    /// Read tickets for bound namespaces whose `read_scopes` intersect the
    /// joining node's verified scopes and which this replica holds.
    #[serde(default)]
    bound_tickets: Vec<String>,
}

/// At most one `admitted` observation per subject per hour (enrollment B10).
const OBSERVATION_INTERVAL_SECONDS: u64 = 3600;

/// This node as an observer: the observations namespace it may write, once
/// the root has bound one and sealed its write capability to this node.
#[derive(Default)]
struct Observer {
    doc: Option<([u8; 32], CoreDoc)>,
    /// Per subject: first observation and latest write, Unix seconds.
    seen: BTreeMap<[u8; 32], (u64, u64)>,
}

/// Bound-namespace tickets kept from one replica between `connect` and
/// `synchronize`; each is honoured only for a namespace the fleet binds.
const MAX_OFFERED_TICKETS: usize = 32;

struct RateState {
    started: Instant,
    peers: BTreeMap<[u8; 32], u32>,
    sources: BTreeMap<IpAddr, u32>,
}

/// The outcome of one on-demand handshake, shared by every dial waiting on it.
type OnDemand = watch::Receiver<Option<std::result::Result<(), String>>>;

/// On-demand handshakes in flight per peer, and recent failures.
#[derive(Default)]
struct OnDemandHandshakes {
    in_flight: BTreeMap<[u8; 32], OnDemand>,
    failed: BTreeMap<[u8; 32], Instant>,
}

/// Bounds info-level Gate-0 denial lines (see [`DENY_LOG_PEER_INTERVAL`]).
struct DenyLog {
    window: Instant,
    lines: u32,
    /// When each recently denied peer last got an info line.
    peers: BTreeMap<[u8; 32], Instant>,
    /// Denials logged at debug only since the last info line.
    suppressed: u64,
}

impl DenyLog {
    fn new() -> Self {
        Self {
            window: Instant::now(),
            lines: 0,
            peers: BTreeMap::new(),
            suppressed: 0,
        }
    }

    /// Whether this denial gets an info line; if so, how many were demoted
    /// to debug since the previous one.
    fn permit(&mut self, peer: &[u8; 32]) -> Option<u64> {
        let now = Instant::now();
        if now.duration_since(self.window) >= Duration::from_secs(1) {
            self.window = now;
            self.lines = 0;
        }
        if self.peers.len() >= MAX_RATE_KEYS {
            self.peers
                .retain(|_, last| now.duration_since(*last) < DENY_LOG_PEER_INTERVAL);
        }
        let recent = self
            .peers
            .get(peer)
            .is_some_and(|last| now.duration_since(*last) < DENY_LOG_PEER_INTERVAL);
        if recent || self.lines >= DENY_LOG_PER_SECOND || self.peers.len() >= MAX_RATE_KEYS {
            self.suppressed += 1;
            return None;
        }
        self.lines += 1;
        self.peers.insert(*peer, now);
        Some(std::mem::take(&mut self.suppressed))
    }
}

/// Why Gate 0 refused a connection, for its decision log.
struct GateDenial {
    reason: &'static str,
    detail: Option<String>,
}

impl GateDenial {
    fn new(reason: &'static str) -> Self {
        Self {
            reason,
            detail: None,
        }
    }

    fn from_admission(error: AdmissionError) -> Self {
        let reason = match &error {
            AdmissionError::Denied(
                DenialReason::NodeRevoked
                | DenialReason::CredentialRevoked
                | DenialReason::IssuerRevoked
                | DenialReason::RootRotated,
            ) => "revoked",
            AdmissionError::Denied(_) => "denied by policy",
            AdmissionError::ClockSkew { .. } => "clock skew",
            AdmissionError::Unavailable(_) => "policy unavailable",
            AdmissionError::NotAdmitted => "no session",
        };
        Self {
            reason,
            detail: Some(error.to_string()),
        }
    }
}

/// Why Gate 0 allowed a connection, for its decision log: the reason and the
/// evidence source it rests on.
type GateAllow = (&'static str, &'static str);

/// An enrollment connection that passed its rate limit.
const HANDSHAKE_ALLOW: GateAllow = ("enrollment handshake", "none");

/// A configured replica this node holds no evidence for, trusted for
/// synchronizing the fleet namespace only: the enrollment handshake's replica
/// leg, and this node's own outbound Docs and blob connections while that
/// synchronization runs. It grants no session, roles or other ALPN.
const REPLICA_SYNC_ALLOW: GateAllow = ("replica sync", "none");

fn admitted(source: &'static str) -> GateAllow {
    ("admitted", source)
}

/// The admission source a Gate-0 allow rests on, for its decision log.
fn source_label(source: &AdmissionEvidenceSource) -> &'static str {
    match source {
        AdmissionEvidenceSource::PresentedChain { .. } => "credential",
        AdmissionEvidenceSource::RootAuthoredRecord { .. } => "registration",
    }
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

/// Everything bound to one root: the fleet store, the stores for the
/// membership namespaces it binds, and the chain verifier anchored at it.
struct Trust {
    store: Arc<EnrollmentPolicyStore>,
    stores: Arc<PolicyStores>,
    verifier: Arc<JwtEnrollmentVerifier>,
}

impl Trust {
    fn new(store: Arc<EnrollmentPolicyStore>, verifier: JwtEnrollmentVerifier) -> Self {
        Self {
            stores: Arc::new(PolicyStores::new(store.clone())),
            store,
            verifier: Arc::new(verifier),
        }
    }
}

struct RotationState {
    root: RootState,
    journal: RotationJournal,
}

struct OwnCredential {
    chain: Option<String>,
    evidence: Option<VerifiedAdmissionEvidence>,
}

/// The outcome of the joining side of the enrollment handshake.
enum Joined {
    /// Both sides admitted each other: a session, and the replica's fleet
    /// namespace ticket.
    Admitted(EnrollmentReadiness, Option<String>),
    /// A configured replica that presented no credential, and that this
    /// node's policy has no record for yet. It admitted this node; this node
    /// trusts it only to synchronize the fleet namespace from.
    SyncOnly {
        peer: [u8; 32],
        ticket: Option<String>,
    },
}

/// What [`EnrollmentService::attach_replicas`] attached policy from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplicaAttachment {
    /// The fleet namespace was synchronized from this replica, which holds an
    /// ordinary admitted session.
    Synchronized([u8; 32]),
    /// No replica completed; this node serves admission from the root-authored
    /// records it already held (or received this start from a replica it could
    /// not admit). Policy freshness stays unsynchronized
    /// ([`EnrollmentReadiness::policy_sync`] is `None`) until a replica
    /// synchronizes. `error` is the last replica's failure.
    LocalCopy { error: String },
}

/// Fleet synchronizations in flight with one replica that holds no session,
/// and the outbound connections Gate 0 let through for them.
#[derive(Default)]
struct ReplicaSync {
    open: usize,
    connections: Vec<CoreWeakConnection>,
}

/// One outbound fleet synchronization in flight with a replica that holds no
/// session. When the last one for that replica ends, the connections it let
/// through are closed, so a pooled Docs or blob connection cannot carry later
/// traffic past Gate 0.
struct ReplicaWindow<'a> {
    windows: &'a Mutex<BTreeMap<[u8; 32], ReplicaSync>>,
    peer: [u8; 32],
}

impl Drop for ReplicaWindow<'_> {
    fn drop(&mut self) {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sync) = windows.get_mut(&self.peer) else {
            return;
        };
        sync.open -= 1;
        if sync.open == 0 {
            if let Some(sync) = windows.remove(&self.peer) {
                for connection in sync.connections {
                    connection.close(0, b"replica sync complete");
                }
            }
        }
    }
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
    /// The root this node trusts and the policy pinned to it. An accepted
    /// root rotation replaces it as a whole.
    trust: RwLock<Arc<Trust>>,
    /// The pinned next-root commitment and the applied-rotation journal;
    /// `None` when the consumer configured no rotation.
    rotation: Mutex<Option<RotationState>>,
    audience: String,
    /// This node's own credential, as presented in handshakes, and its
    /// verified evidence. Renewal replaces both together.
    own: RwLock<OwnCredential>,
    policy: Arc<dyn AdmissionPolicy>,
    options: EnrollmentOptions,
    document: RwLock<Option<CoreDoc>>,
    /// Documents for namespaces the fleet binds, by namespace id.
    bound_documents: RwLock<BTreeMap<[u8; 32], CoreDoc>>,
    /// Bound-namespace read tickets each replica offered at `connect`.
    offered_tickets: Mutex<BTreeMap<[u8; 32], Vec<String>>>,
    observer: Mutex<Observer>,
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
    deny_log: Mutex<DenyLog>,
    on_demand: Mutex<OnDemandHandshakes>,
    /// Configured replicas without a session that this node is synchronizing
    /// the fleet namespace from right now (see [`REPLICA_SYNC_ALLOW`]).
    replica_windows: Mutex<BTreeMap<[u8; 32], ReplicaSync>>,
    replica_maintenance: Mutex<Option<AbortHandle>>,
    next_generation: AtomicU64,
    revoked: broadcast::Sender<EnrollmentRevoked>,
}

impl Drop for EnrollmentService {
    fn drop(&mut self) {
        for task in [&mut self.policy_watch, &mut self.replica_maintenance] {
            if let Some(task) = task
                .get_mut()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                task.abort();
            }
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
        Self::install_inner(
            node,
            |node| node.take_hook_receiver(),
            store,
            verifier,
            credential,
            policy,
            options,
        )
    }

    /// [`Self::install`] for a host that already drains the node's hook
    /// receiver and forwards both hook directions into `hooks` (the C ABI's
    /// admission drainers). The node must still be started with fail-closed
    /// hooks; the host must not answer forwarded invocations itself.
    pub fn install_with_hooks(
        node: CoreNode,
        hooks: CoreHookReceiver,
        store: Arc<EnrollmentPolicyStore>,
        verifier: JwtEnrollmentVerifier,
        credential: Option<String>,
        policy: Arc<dyn AdmissionPolicy>,
        options: EnrollmentOptions,
    ) -> Result<Arc<Self>> {
        ensure!(
            node.has_hooks(),
            "enrollment requires a node started with hooks"
        );
        Self::install_inner(
            node,
            move |_| Some(hooks),
            store,
            verifier,
            credential,
            policy,
            options,
        )
    }

    fn install_inner(
        node: CoreNode,
        take_hooks: impl FnOnce(&CoreNode) -> Option<CoreHookReceiver>,
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
                && !options.replica_sync_interval.is_zero()
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
        let (store, verifier, rotation) = resume_rotation(
            store,
            verifier,
            options.rotation,
            &options.applied_rotations,
        )?;
        let own = identity(&node.node_id())?;
        // A credential only admits a node until the root registers it (design
        // §4), so a node never needs a valid one to start: one that no longer
        // verifies (expired, revoked issuer path, anchored at a root this node
        // rotated away from) is dropped and never presented, and peers admit
        // the node by its registration or refuse it. Dropping confers nothing:
        // the decision stays with each peer's own records. A credential that
        // is only early (`ClockSkew`) still fails, since the local clock, not
        // the credential, is what needs fixing.
        let own_evidence = match credential.as_deref() {
            Some(token) => match verifier.verify(token, &own, unix_now()) {
                Ok(evidence) => Some(evidence),
                Err(error @ EnrollmentError::ClockSkew { .. }) => return Err(error.into()),
                Err(error) if rotation.as_ref().is_some_and(|r| r.root.seq > 0) => {
                    tracing::warn!(?error, "enrollment: credential predates the current root");
                    None
                }
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        "enrollment: this node's credential does not verify and is not \
                         presented; peers admit it only by a root registration"
                    );
                    None
                }
            },
            None => {
                tracing::info!(
                    "enrollment: no credential; peers admit this node only by a root registration"
                );
                None
            }
        };
        let credential = credential.filter(|_| own_evidence.is_some());
        let hooks =
            take_hooks(&node).ok_or_else(|| anyhow!("enrollment requires unused node hooks"))?;
        let service = Self::new(
            node,
            Trust::new(store, verifier),
            OwnCredential {
                chain: credential,
                evidence: own_evidence,
            },
            policy,
            options,
            rotation,
        );
        // Bound stores keep their own durable denials across restart.
        service.trust().stores.sync_bindings()?;
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
                // A pass reading the Docs store when the node closes may
                // never finish. It must not keep the service, and with it the
                // policy store's file lock, alive after the node is gone.
                let endpoint = service.node.inner.endpoint.clone();
                tokio::select! {
                    () = service.reconcile() => {}
                    () = endpoint.closed() => break,
                }
            }
        });
        Ok(service)
    }

    fn new(
        node: CoreNode,
        trust: Trust,
        own: OwnCredential,
        policy: Arc<dyn AdmissionPolicy>,
        options: EnrollmentOptions,
        rotation: Option<RotationState>,
    ) -> Arc<Self> {
        // One reconciliation can revoke every admitted peer; size the channel
        // so that does not lag a promptly polling consumer.
        let (revoked, _) = broadcast::channel(options.max_admitted_peers.max(128));
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            node,
            audience: trust.store.audience().to_owned(),
            trust: RwLock::new(Arc::new(trust)),
            rotation: Mutex::new(rotation),
            bound_documents: RwLock::new(BTreeMap::new()),
            offered_tickets: Mutex::new(BTreeMap::new()),
            observer: Mutex::new(Observer::default()),
            own: RwLock::new(own),
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
            deny_log: Mutex::new(DenyLog::new()),
            on_demand: Mutex::new(OnDemandHandshakes::default()),
            replica_windows: Mutex::new(BTreeMap::new()),
            replica_maintenance: Mutex::new(None),
            next_generation: AtomicU64::new(1),
            revoked,
        })
    }

    fn spawn_hook_loop(&self, mut hooks: CoreHookReceiver) {
        // Outbound handshakes too: replica sync tracks and gates its own
        // connections in `after_handshake`.
        hooks.include_outbound_handshakes();
        let weak = self.this.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((info, reply)) = hooks.before_connect_rx.recv() => {
                        let Some(service) = weak.upgrade() else {
                            let _ = reply.send(false);
                            break;
                        };
                        service.before_connect(info, reply);
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
    /// admitted session whose policy still allows the observed source. Every
    /// decision is logged once (see [`Self::log_decision`]).
    fn after_handshake(
        self: Arc<Self>,
        info: CoreHookHandshakeInfo,
        reply: oneshot::Sender<CoreAfterHandshakeDecision>,
    ) {
        let alpn = info.alpn;
        let Ok(peer) = identity(&info.remote_endpoint_id) else {
            tracing::info!(
                target: ADMISSION_TARGET,
                peer = %info.remote_endpoint_id,
                alpn = %String::from_utf8_lossy(&alpn),
                decision = "deny",
                reason = "invalid peer identity",
                "gate 0: enrollment refused connection"
            );
            let _ = reply.send(hook_decision(false));
            return;
        };
        let address = info.observed_peer_addr;
        if alpn == ENROLLMENT_ALPN {
            let allowed = self.take_rate_slot(peer, address.map(|address| address.ip()));
            let decision = if allowed {
                Ok(HANDSHAKE_ALLOW)
            } else {
                Err(GateDenial::new("rate limited"))
            };
            self.log_decision(&peer, &alpn, decision);
            let _ = reply.send(hook_decision(allowed));
            return;
        }
        // This node's own synchronization of the fleet namespace from a
        // configured replica it holds no evidence for: only its outbound Docs
        // and blob connections, and only while that synchronization runs.
        if info.outbound && !self.has_session(&peer) && self.replica_window_open(&peer) {
            let allowed = replica_sync_alpn(&alpn);
            let decision = if allowed {
                Ok(REPLICA_SYNC_ALLOW)
            } else {
                Err(GateDenial::new("replica sync only"))
            };
            self.log_replica_decision(&peer, &alpn, decision, false);
            if allowed && !self.track_replica_connection(&peer, info.connection.clone()) {
                // The window closed while the dial was in flight.
                self.log_replica_decision(
                    &peer,
                    &alpn,
                    Err(GateDenial::new("replica sync only")),
                    false,
                );
                let _ = reply.send(hook_decision(false));
                return;
            }
            let _ = reply.send(hook_decision(allowed));
            return;
        }
        let connection = info.connection;
        let source = address.map(|address| ObservedSourceAddress {
            address: address.ip(),
            provenance: SourceAddressProvenance::DirectPeer,
        });
        if self.has_session(&peer) {
            let Ok(permit) = self.gate_permits.clone().try_acquire_owned() else {
                self.log_decision(&peer, &alpn, Err(GateDenial::new("gate busy")));
                let _ = reply.send(hook_decision(false));
                return;
            };
            tokio::spawn(async move {
                let _permit = permit;
                let decision = self.gate_connection(&peer, source, connection).await;
                let allowed = decision.is_ok();
                self.log_decision(&peer, &alpn, decision.map(admitted));
                let _ = reply.send(hook_decision(allowed));
            });
            return;
        }
        // No session: a known definite denial is final. Otherwise this node
        // has lost the session (it restarted, or evicted an idle one) while
        // the dialer still holds its own, so enroll with the dialer now,
        // within the hook's deadline, and gate the connection on the result.
        // That costs the same handshake budget as an inbound enrollment
        // connection from this peer and source.
        let denial = self.sessionless_denial(&peer);
        if denial.reason != "no session" {
            self.log_decision(&peer, &alpn, Err(denial));
            let _ = reply.send(hook_decision(false));
            return;
        }
        if !self.take_rate_slot(peer, address.map(|address| address.ip())) {
            self.log_decision(&peer, &alpn, Err(GateDenial::new("rate limited")));
            let _ = reply.send(hook_decision(false));
            return;
        }
        let dial_back = CoreNodeAddr {
            endpoint_id: hex::encode(peer),
            relay_url: None,
            direct_addresses: address
                .map(|address| vec![address.to_string()])
                .unwrap_or_default(),
        };
        tokio::spawn(async move {
            let decision = match self.ensure_session(peer, dial_back).await {
                Err(error) => Err(GateDenial {
                    reason: "enrollment failed",
                    detail: Some(error),
                }),
                Ok(()) => match self.gate_permits.clone().try_acquire_owned() {
                    Err(_) => Err(GateDenial::new("gate busy")),
                    Ok(_permit) => self.gate_connection(&peer, source, connection).await,
                },
            };
            let allowed = decision.is_ok();
            self.log_decision(&peer, &alpn, decision.map(admitted));
            let _ = reply.send(hook_decision(allowed));
        });
    }

    /// Outbound Gate 0. A dial on an ordinary ALPN to a peer this node holds
    /// no session with (after either side restarted, or before any explicit
    /// `connect`) first runs the enrollment handshake with that peer, which
    /// leaves both sides with sessions; the dial proceeds only if it
    /// succeeds. Enrollment dials themselves pass straight through.
    fn before_connect(self: Arc<Self>, info: CoreHookConnectInfo, reply: oneshot::Sender<bool>) {
        if info.alpn == ENROLLMENT_ALPN {
            let _ = reply.send(true);
            return;
        }
        let Ok(peer) = identity(&info.remote_endpoint_id) else {
            let _ = reply.send(false);
            return;
        };
        if self.has_session(&peer) {
            let _ = reply.send(true);
            return;
        }
        if self.replica_window_open(&peer) {
            let allowed = replica_sync_alpn(&info.alpn);
            let decision = if allowed {
                Ok(REPLICA_SYNC_ALLOW)
            } else {
                Err(GateDenial::new("replica sync only"))
            };
            self.log_replica_decision(&peer, &info.alpn, decision, false);
            let _ = reply.send(allowed);
            return;
        }
        tokio::spawn(async move {
            let result = self.ensure_session(peer, info.remote_addr).await;
            if let Err(error) = &result {
                tracing::debug!(
                    target: ADMISSION_TARGET,
                    peer = %hex::encode(peer),
                    alpn = %String::from_utf8_lossy(&info.alpn),
                    %error,
                    "outbound connect blocked: on-demand enrollment failed"
                );
            }
            let _ = reply.send(result.is_ok());
        });
    }

    /// How long an on-demand handshake may take: the handshake deadline, cut
    /// to leave a fifth of the hook timeout for the decision itself, since a
    /// hook that runs out falls back to its failure mode (open, for an
    /// outbound dial).
    fn on_demand_deadline(&self) -> Duration {
        let hook = self.node.inner.enrollment_hook_timeout;
        self.options
            .handshake_timeout
            .min(hook.saturating_sub(hook / 5))
    }

    /// Run the enrollment handshake with `peer` unless a session exists.
    /// Concurrent callers for one peer share one handshake, which runs to
    /// completion in its own task even if they give up, and is bounded by
    /// [`Self::on_demand_deadline`]. A failure is remembered for
    /// [`ON_DEMAND_RETRY_BACKOFF`].
    async fn ensure_session(
        self: &Arc<Self>,
        peer: [u8; 32],
        address: CoreNodeAddr,
    ) -> std::result::Result<(), String> {
        if self.has_session(&peer) {
            return Ok(());
        }
        let deadline = self.on_demand_deadline();
        let mut outcome = {
            let mut on_demand = self.on_demand.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(waiting) = on_demand.in_flight.get(&peer) {
                waiting.clone()
            } else {
                if let Some(failed) = on_demand.failed.get(&peer) {
                    if failed.elapsed() < ON_DEMAND_RETRY_BACKOFF {
                        return Err("a recent enrollment handshake with this peer failed".into());
                    }
                }
                let (sender, receiver) = watch::channel(None);
                on_demand.in_flight.insert(peer, receiver.clone());
                let service = self.clone();
                tokio::spawn(async move {
                    let result =
                        match tokio::time::timeout(deadline, service.connect(address)).await {
                            Ok(Ok(_)) => Ok(()),
                            Ok(Err(error)) => Err(format!("{error:#}")),
                            Err(_) => Err("enrollment handshake timeout".to_string()),
                        };
                    let mut on_demand = service.on_demand.lock().unwrap_or_else(|e| e.into_inner());
                    on_demand.in_flight.remove(&peer);
                    if result.is_ok() {
                        on_demand.failed.remove(&peer);
                    } else {
                        if on_demand.failed.len() >= MAX_RATE_KEYS {
                            on_demand
                                .failed
                                .retain(|_, at| at.elapsed() < ON_DEMAND_RETRY_BACKOFF);
                        }
                        if on_demand.failed.len() < MAX_RATE_KEYS {
                            on_demand.failed.insert(peer, Instant::now());
                        }
                    }
                    drop(on_demand);
                    let _ = sender.send(Some(result));
                });
                receiver
            }
        };
        let waited = tokio::time::timeout(deadline, outcome.wait_for(Option::is_some)).await;
        match waited {
            Ok(Ok(result)) => result.clone().unwrap_or(Ok(())),
            Ok(Err(_)) => Err("enrollment handshake abandoned".into()),
            Err(_) => Err("enrollment handshake timeout".into()),
        }
    }

    /// Why a peer with no session is refused: a recent definite revocation
    /// is reported as such, anything else as a missing session.
    fn sessionless_denial(&self, peer: &[u8; 32]) -> GateDenial {
        match self.sessionless_standing(peer) {
            AdmissionError::NotAdmitted => GateDenial::new("no session"),
            other => GateDenial::from_admission(other),
        }
    }

    /// One structured event per Gate-0 decision under `aster::admission`:
    /// allows at debug, denials at info. A peer gets at most one info-level
    /// denial per [`DENY_LOG_PEER_INTERVAL`] and all peers together at most
    /// [`DENY_LOG_PER_SECOND`] a second; the rest log at debug and are
    /// counted in the next info line's `suppressed`, since a remote decides
    /// how often it dials.
    fn log_decision(&self, peer: &[u8; 32], alpn: &[u8], decision: Result<GateAllow, GateDenial>) {
        let peer_hex = hex::encode(peer);
        let alpn = String::from_utf8_lossy(alpn);
        match decision {
            Ok((reason, source)) => tracing::debug!(
                target: ADMISSION_TARGET,
                peer = %peer_hex,
                alpn = %alpn,
                decision = "allow",
                reason,
                source,
                "gate 0: enrollment admitted connection"
            ),
            Err(denial) => {
                let permitted = self
                    .deny_log
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .permit(peer);
                let detail = denial.detail.as_deref().unwrap_or("");
                match permitted {
                    Some(suppressed) => tracing::info!(
                        target: ADMISSION_TARGET,
                        peer = %peer_hex,
                        alpn = %alpn,
                        decision = "deny",
                        reason = denial.reason,
                        detail,
                        suppressed,
                        "gate 0: enrollment refused connection"
                    ),
                    None => tracing::debug!(
                        target: ADMISSION_TARGET,
                        peer = %peer_hex,
                        alpn = %alpn,
                        decision = "deny",
                        reason = denial.reason,
                        detail,
                        "gate 0: enrollment refused connection"
                    ),
                }
            }
        }
    }

    /// Replica-trust decisions, which this node makes about its own dials and
    /// its own configured replicas rather than at a remote's pace: allows at
    /// debug like any other; a judgement refusing a replica after its fleet
    /// synchronization (`judgement`) at info and unthrottled, since it runs
    /// at most once per replica per sync interval; a dial beyond what the
    /// synchronization needs at debug, like any blocked outbound dial.
    fn log_replica_decision(
        &self,
        peer: &[u8; 32],
        alpn: &[u8],
        decision: Result<GateAllow, GateDenial>,
        judgement: bool,
    ) {
        let denial = match decision {
            Ok(allow) => return self.log_decision(peer, alpn, Ok(allow)),
            Err(denial) => denial,
        };
        let peer = hex::encode(peer);
        let alpn = String::from_utf8_lossy(alpn);
        let detail = denial.detail.as_deref().unwrap_or("");
        if judgement {
            tracing::info!(
                target: ADMISSION_TARGET,
                peer = %peer,
                alpn = %alpn,
                decision = "deny",
                reason = denial.reason,
                detail,
                "gate 0: enrollment refused replica"
            );
        } else {
            tracing::debug!(
                target: ADMISSION_TARGET,
                peer = %peer,
                alpn = %alpn,
                decision = "deny",
                reason = denial.reason,
                detail,
                "gate 0: enrollment refused connection"
            );
        }
    }

    /// Gate 0 for one ordinary connection: admit it only under the session it
    /// was evaluated against, and remember it in the same step so a later
    /// definite denial closes it. Returns the admitting evidence's kind.
    async fn gate_connection(
        &self,
        peer: &[u8; 32],
        source: Option<ObservedSourceAddress>,
        connection: CoreWeakConnection,
    ) -> Result<&'static str, GateDenial> {
        let Some(session) = self.session(peer) else {
            return Err(self.sessionless_denial(peer));
        };
        self.evaluate(&session.evidence, source)
            .await
            .map_err(GateDenial::from_admission)?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = sessions
            .get_mut(peer)
            .filter(|current| current.generation == session.generation)
        else {
            return Err(GateDenial::new("session changed"));
        };
        current.last_used = Instant::now();
        current.admitted.retain(CoreWeakConnection::is_alive);
        current.admitted.push(connection);
        Ok(source_label(&session.evidence.source()))
    }

    /// Attach a document for a namespace the fleet binds, such as one this
    /// node holds as a writer. Membership registrations in it take part in
    /// admission, and joining nodes whose verified scopes intersect the
    /// binding's `read_scopes` receive a read ticket for it.
    pub fn add_policy_document(&self, doc: CoreDoc) -> Result<NamespaceBindingV1> {
        let namespace = namespace(&doc.doc_id())?;
        let binding = self
            .trust()
            .stores
            .sync_bindings()?
            .into_iter()
            .find(|binding| binding.namespace == hex::encode(namespace))
            .ok_or_else(|| anyhow!("namespace is not bound by the fleet"))?;
        self.bound_documents
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(namespace, doc);
        Ok(binding)
    }

    /// Documents this node holds for currently bound namespaces of `purpose`.
    pub fn policy_documents(
        &self,
        purpose: NamespacePurpose,
    ) -> Vec<(NamespaceBindingV1, CoreDoc)> {
        let documents = self
            .bound_documents
            .read()
            .unwrap_or_else(|e| e.into_inner());
        self.trust()
            .store
            .bindings()
            .into_iter()
            .filter(|binding| binding.purpose == purpose)
            .filter_map(|binding| {
                let namespace = namespace(&binding.namespace).ok()?;
                let doc = documents.get(&namespace)?.clone();
                Some((binding, doc))
            })
            .collect()
    }

    fn membership_documents(&self) -> Vec<(Arc<EnrollmentPolicyStore>, CoreDoc)> {
        let documents = self
            .bound_documents
            .read()
            .unwrap_or_else(|e| e.into_inner());
        documents
            .iter()
            .filter_map(|(namespace, doc)| {
                Some((self.trust().stores.membership(namespace)?, doc.clone()))
            })
            .collect()
    }

    /// Read tickets for bound namespaces this replica holds whose
    /// `read_scopes` intersect the peer's verified scopes. The observations
    /// namespace is never shared this way: observers get write access by a
    /// sealed grant and only the root reads it.
    async fn bound_tickets(&self, evidence: &VerifiedAdmissionEvidence) -> Result<Vec<String>> {
        let scopes = evidence.claims().scopes.clone().unwrap_or_default();
        let mut tickets = Vec::new();
        for (binding, doc) in [NamespacePurpose::Membership, NamespacePurpose::ExecGrants]
            .into_iter()
            .flat_map(|purpose| self.policy_documents(purpose))
        {
            if !binding.read_scopes.is_disjoint(&scopes) {
                tickets.push(doc.share_with_addr("read".into()).await?);
            }
        }
        Ok(tickets)
    }

    /// Join and sync the bound namespaces `peer` offered at `connect`. Only a
    /// read capability for a namespace the fleet binds, from that peer, is
    /// accepted; the replica's offer carries no authority of its own.
    async fn synchronize_bound(&self, peer: [u8; 32]) -> Result<()> {
        use iroh_docs::{Capability, DocTicket};
        use iroh_tickets::Ticket;
        let offered = self
            .offered_tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&peer)
            .unwrap_or_default();
        let bindings = self.trust().stores.sync_bindings()?;
        for ticket in offered {
            let mut decoded = DocTicket::decode_string(&ticket)?;
            ensure!(
                matches!(decoded.capability, Capability::Read(_)),
                "bound policy tickets must be read-only"
            );
            let bound = namespace(&decoded.capability.id().to_string())?;
            let Some(binding) = bindings.iter().find(|binding| {
                binding.namespace == hex::encode(bound)
                    && binding.purpose != NamespacePurpose::Observations
            }) else {
                continue;
            };
            decoded.nodes.retain(|node| node.id.as_bytes() == &peer);
            ensure!(
                !decoded.nodes.is_empty(),
                "bound policy ticket has no authenticated replica address"
            );
            let doc = self
                .node
                .docs_client()
                .join(decoded.encode_string())
                .await?;
            let store = (binding.purpose == NamespacePurpose::Membership)
                .then(|| self.trust().stores.membership(&bound))
                .flatten();
            self.bound_documents
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(bound, doc.clone());
            self.sync_document(&doc, peer, store.as_deref(), false)
                .await?;
        }
        Ok(())
    }

    /// One sync round with `peer`, waiting until it completes and, when a
    /// store is given, until that store has no pending content.
    async fn sync_document(
        &self,
        doc: &CoreDoc,
        peer: [u8; 32],
        store: Option<&EnrollmentPolicyStore>,
        fleet: bool,
    ) -> Result<()> {
        use futures_lite::StreamExt;
        let events = doc.doc.subscribe().await?;
        futures_lite::pin!(events);
        doc.start_sync(vec![hex::encode(peer)]).await?;
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
                        // case the replica cannot reach us. The caller's
                        // deadline still bounds it.
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
                let Some(store) = store else {
                    return Ok(());
                };
                let report = store.refresh(doc).await?;
                if fleet {
                    self.context
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .pending_policy_content = report.has_uncertainty();
                }
                if report.pending == 0 {
                    return Ok(());
                }
            }
        }
    }

    /// Whether the root has revoked `key` as a delegate (`IssuerStatusV1` in
    /// the fleet namespace), as this node last reconciled it. Consumers that
    /// verify their own chains, such as Exec's on-behalf-of action chains,
    /// apply the same withdrawals enrollment does.
    pub fn delegate_revoked(&self, key: &[u8; 32]) -> bool {
        use crate::delegation::RevokedDelegates;
        self.trust().stores.is_revoked(key)
    }

    /// The fleet audience this node's verifier and stores pin.
    pub fn audience(&self) -> &str {
        &self.audience
    }

    fn trust(&self) -> Arc<Trust> {
        self.trust.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The chain this node presents: its credential while unexpired. An
    /// expired chain admits nothing, and presenting one would only shadow the
    /// registration a peer falls back to (declared locality, for one).
    fn own_chain(&self) -> Option<String> {
        let own = self.own.read().unwrap_or_else(|e| e.into_inner());
        let current = own
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.expires_at().is_none_or(|exp| unix_now() < exp));
        own.chain.clone().filter(|_| current)
    }

    /// The credential chain this node presents to peers now: its credential
    /// while unexpired, `None` once it expired or was dropped at install.
    /// [`Self::credential`] keeps returning an expired credential so renewal
    /// can tell expiry from absence.
    pub fn presented_chain(&self) -> Option<String> {
        self.own_chain()
    }

    fn own_evidence(&self) -> Option<VerifiedAdmissionEvidence> {
        self.own
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .evidence
            .clone()
    }

    /// This node's current credential chain and its verified evidence.
    pub fn credential(&self) -> Option<(String, VerifiedAdmissionEvidence)> {
        let own = self.own.read().unwrap_or_else(|e| e.into_inner());
        own.chain.clone().zip(own.evidence.clone())
    }

    /// Verify `chain` for `subject` as admission would: pinned root and
    /// audience, signed validity, and the root's delegate revocations.
    pub fn verify_credential(
        &self,
        subject: &[u8; 32],
        chain: &str,
    ) -> Result<VerifiedAdmissionEvidence, super::EnrollmentError> {
        self.trust()
            .verifier
            .verify_with(chain, subject, unix_now(), self.trust().stores.as_ref())
    }

    /// What the root's records, as this node last reconciled them across
    /// every bound namespace, say about `subject`.
    pub fn subject_standing(&self, subject: &[u8; 32]) -> SubjectStanding {
        match self
            .trust()
            .stores
            .select_evidence(subject, None, unix_now())
        {
            Ok(_) => SubjectStanding::Registered,
            Err(RecordError::Denied(reason)) if reason.is_definite() => {
                SubjectStanding::Denied(reason)
            }
            Err(RecordError::MissingEvidence) => SubjectStanding::Unregistered,
            Err(error) => SubjectStanding::Unknown(error),
        }
    }

    /// This node's locality statement: the locality link embedded in its
    /// current credential, with the credential's delegation links. `None`
    /// when it holds no credential or the credential declares no locality.
    /// This, not the credential, is what the node publishes.
    pub fn locality_statement(&self) -> Option<String> {
        let chain = crate::delegation::Chain::from_text(&self.own_chain()?).ok()?;
        crate::delegation::locality_statement(&chain).map(|statement| statement.into_text())
    }

    /// `endpoint`'s verified declared locality, with admission's precedence:
    /// an active registration record for `endpoint` answers (its
    /// `aster.locality.*` claims, the statement ignored); a definite denial
    /// answers nothing; otherwise `statement` must verify for `endpoint` as a
    /// credential would (root or delegated issuer within its grant, delegate
    /// revocations, expiry). `None` for anything else, including a statement
    /// issued to another endpoint.
    pub fn declared_locality(
        &self,
        endpoint: &[u8; 32],
        statement: &str,
    ) -> Option<crate::topology::locality::CoreVerifiedLocality> {
        let trust = self.trust();
        let now = unix_now();
        match trust.stores.select_evidence(endpoint, None, now) {
            Ok(record) => {
                return Some(crate::topology::locality::CoreVerifiedLocality {
                    declared: record.claims().declared_locality().ok().flatten()?,
                    expires_at: record.expires_at(),
                });
            }
            Err(RecordError::Denied(reason)) if reason.is_definite() => return None,
            Err(_) if statement.is_empty() => return None,
            Err(_) => {}
        }
        trust
            .verifier
            .verify_locality(statement, endpoint, now, trust.stores.as_ref())
            .ok()
    }

    /// Replace this node's credential with a renewed chain, then present it
    /// again to every peer this node has a session with, so their sessions
    /// rest on it before the old chain expires. The chain must verify for
    /// this node. Returns the peers that could not be reached; their
    /// sessions lapse with the old chain unless the peer re-enrolls.
    pub async fn replace_credential(&self, chain: String) -> Result<Vec<([u8; 32], String)>> {
        let own = identity(&self.node.node_id())?;
        let evidence = self.verify_credential(&own, &chain)?;
        *self.own.write().unwrap_or_else(|e| e.into_inner()) = OwnCredential {
            chain: Some(chain),
            evidence: Some(evidence),
        };
        let peers: Vec<[u8; 32]> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        let mut unreached = Vec::new();
        for peer in peers {
            let address = CoreNodeAddr {
                endpoint_id: hex::encode(peer),
                relay_url: None,
                direct_addresses: Vec::new(),
            };
            if let Err(error) = self.connect(address).await {
                unreached.push((peer, error.to_string()));
            }
        }
        Ok(unreached)
    }

    /// The root key this node currently trusts.
    pub fn trusted_root(&self) -> [u8; 32] {
        *self.trust().store.root()
    }

    /// The fleet namespace pinned to the current root.
    pub fn fleet_namespace(&self) -> [u8; 32] {
        *self.trust().store.namespace()
    }

    /// The root this node currently trusts, with its next-root commitment
    /// and rotation count. `None` when the consumer configured no rotation.
    pub fn root_state(&self) -> Option<RootState> {
        self.rotation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|rotation| rotation.root)
    }

    /// The rotation statements this node has applied, oldest first, as it
    /// offers them to peers in the enrollment handshake.
    pub fn root_rotations(&self) -> Vec<String> {
        self.rotation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|rotation| rotation.journal.statements().to_vec())
            .unwrap_or_default()
    }

    /// Statements a peer offered in a handshake. Each is self-certifying, so
    /// the peer needs no trust; one that does not continue this node's root
    /// is ignored.
    fn offered_rotations(&self, statements: &[String]) {
        if statements.is_empty() {
            return;
        }
        if let Err(error) = self.apply_root_rotations(statements) {
            tracing::debug!(%error, "enrollment: offered root rotation not applied");
        }
    }

    /// Apply the statements that continue this node's pinned root, in
    /// sequence order: each must be signed by the key the current
    /// commitment names, name the current root as `prev`, and be exactly
    /// the next sequence number. Each one is journaled durably before it
    /// takes effect. Then the root's statement replaces the trusted root,
    /// the policy stores re-target to its fleet namespace with no authority
    /// carried over, and every session (all of them rest on the previous
    /// root) is closed with [`DenialReason::RootRotated`]. Returns how many
    /// were applied; statements already applied are skipped.
    pub fn apply_root_rotations(&self, statements: &[String]) -> Result<usize> {
        let mut ordered: Vec<(u64, &String)> = statements
            .iter()
            .take(MAX_FORWARDED_ROTATIONS)
            .filter_map(|statement| Some((claimed_rotation_seq(statement)?, statement)))
            .collect();
        ordered.sort_by_key(|(seq, _)| *seq);
        let mut applied = 0;
        {
            let mut rotation = self.rotation.lock().unwrap_or_else(|e| e.into_inner());
            let rotation = rotation
                .as_mut()
                .ok_or_else(|| anyhow!("root rotation is not configured"))?;
            for (seq, statement) in ordered {
                if seq <= rotation.root.seq {
                    continue;
                }
                let trust = self.trust();
                let verified = verify_root_rotation(
                    statement,
                    &rotation.root,
                    unix_now(),
                    trust.verifier.activation_leeway(),
                )
                .map_err(|error| anyhow!("root rotation rejected: {error:?}"))?;
                let store = Arc::new(
                    trust
                        .store
                        .open_rotated(*verified.new_root(), verified.namespaces()[0])?,
                );
                let verifier = rotated_verifier(&trust.verifier, *verified.new_root())?;
                rotation.journal.append(verified.statement())?;
                rotation.root = verified.apply();
                *self.trust.write().unwrap_or_else(|e| e.into_inner()) =
                    Arc::new(Trust::new(store, verifier));
                tracing::info!(
                    root = %hex::encode(verified.new_root()),
                    seq = verified.seq(),
                    "enrollment: root rotated"
                );
                applied += 1;
            }
        }
        if applied > 0 {
            self.after_rotation();
        }
        Ok(applied)
    }

    /// Drop everything held under the previous root: its policy documents,
    /// observer grant and freshness, this node's own credential if the new
    /// root does not anchor it, and every session.
    fn after_rotation(&self) {
        if let Some(watch) = self
            .policy_watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            watch.abort();
        }
        *self.document.write().unwrap_or_else(|e| e.into_inner()) = None;
        self.bound_documents
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.offered_tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self.observer.lock().unwrap_or_else(|e| e.into_inner()) = Observer::default();
        {
            let mut context = self.context.write().unwrap_or_else(|e| e.into_inner());
            context.policy_sync = None;
            context.pending_policy_content = true;
        }
        let own = identity(&self.node.node_id()).ok();
        let mut credential = self.own.write().unwrap_or_else(|e| e.into_inner());
        let still_valid = credential
            .chain
            .as_deref()
            .zip(own)
            .is_some_and(|(chain, own)| {
                self.trust()
                    .verifier
                    .verify(chain, &own, unix_now())
                    .is_ok()
            });
        if !still_valid {
            *credential = OwnCredential {
                chain: None,
                evidence: None,
            };
        }
        drop(credential);
        let sessions: Vec<([u8; 32], u64)> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(peer, session)| (*peer, session.generation))
            .collect();
        for (peer, generation) in sessions {
            self.revoke_generation(&peer, generation, DenialReason::RootRotated);
        }
    }

    pub fn revocations(&self) -> broadcast::Receiver<EnrollmentRevoked> {
        self.revoked.subscribe()
    }

    /// Attach the pinned policy namespace. It may be a read-only replica.
    /// Live syncs with the root or an admitted replica keep policy freshness
    /// current from then on.
    pub fn set_policy_document(&self, doc: CoreDoc) -> Result<()> {
        ensure!(
            namespace(&doc.doc_id())? == *self.trust().store.namespace(),
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
                    // As for reconciliation: never outlive the node.
                    let endpoint = service.node.inner.endpoint.clone();
                    let peer = sync.peer.to_string();
                    tokio::select! {
                        () = service.observe_policy_sync(&doc, &peer) => {}
                        () = endpoint.closed() => break,
                    }
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
        if peer != *self.trust().store.root() && !self.has_session(&peer) {
            return;
        }
        let Ok(report) = self.trust().store.refresh(doc).await else {
            return;
        };
        let _ = self.trust().stores.sync_bindings();
        let mut context = self.context.write().unwrap_or_else(|e| e.into_inner());
        context.pending_policy_content = report.has_uncertainty();
        if report.pending == 0 {
            context.policy_sync = Some(self.synchronization(peer));
        }
    }

    fn synchronization(&self, peer: [u8; 32]) -> PolicySynchronization {
        PolicySynchronization {
            completed_at: unix_now(),
            source: if peer == *self.trust().store.root() {
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
            unresolved = self.refresh_issue(self.trust().store.refresh_node(doc, peer).await);
        }
        for (store, doc) in self.membership_documents() {
            let issue = self.refresh_issue(store.refresh_node(&doc, peer).await);
            unresolved = unresolved.or(issue);
        }
        // Independent root registration replaces presentation evidence. An
        // obsolete/expired chain must not block a newer valid registration.
        if let Ok(registration) = self.trust().stores.select_evidence(peer, None, unix_now()) {
            if let Some(issue) = unresolved {
                return Err(issue);
            }
            self.evaluate(&registration, observed_source).await?;
            return Ok(registration);
        }
        // Gate G6 rejects a chain through a delegate the root has revoked.
        let chain = token
            .map(|token| {
                self.trust().verifier.verify_with(
                    token,
                    peer,
                    unix_now(),
                    self.trust().stores.as_ref(),
                )
            })
            .transpose()
            .map_err(|error| match error {
                EnrollmentError::ClockSkew { offset_seconds } => {
                    AdmissionError::ClockSkew { offset_seconds }
                }
                EnrollmentError::Chain(ChainError::Revoked) => {
                    AdmissionError::Denied(DenialReason::IssuerRevoked)
                }
                _ => AdmissionError::Denied(DenialReason::InvalidValidity),
            })?;
        if let (Some(doc), Some(chain)) = (&doc, chain.as_ref()) {
            if let Some(id) = chain.credential_id() {
                let issue =
                    self.refresh_issue(self.trust().store.refresh_credential(doc, id).await);
                unresolved = unresolved.or(issue);
            }
            // Read the root's current statement about every delegate on the
            // path, so a revocation this node has not reconciled yet applies.
            if let AdmissionEvidenceSource::PresentedChain { issuers, .. } = chain.source() {
                for delegate in issuers
                    .iter()
                    .filter(|key| *key != self.trust().store.root())
                {
                    let issue =
                        self.refresh_issue(self.trust().store.refresh_issuer(doc, delegate).await);
                    unresolved = unresolved.or(issue);
                }
            }
        }
        let evidence = self
            .trust()
            .stores
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
            self.trust().stores.as_ref(),
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
        let readiness = self.replace_session(evidence.clone(), observed_source, None)?;
        self.observe_admission(&evidence);
        Ok(readiness)
    }

    /// As an observer, record a fresh admission on a persistent chain for the
    /// root: on first admission, then at most hourly. Ephemeral credentials
    /// and registrations are never observed. The write is best effort.
    fn observe_admission(&self, evidence: &VerifiedAdmissionEvidence) {
        let (Some(chain), Some(Registration::Persistent)) =
            (evidence.presented_chain(), evidence.registration())
        else {
            return;
        };
        self.observe(evidence.subject(), chain, ObservationKind::Admitted, false);
    }

    /// As an observer, record that this node signed a persistent credential
    /// for `subject` (an issuer, enrollment §5): always for a new issuance,
    /// and for a renewal at most hourly. Returns whether one was written,
    /// which needs this node to hold the observer scope and sealed grant.
    pub fn observe_issuance(&self, subject: &[u8; 32], chain: &str, renewal: bool) -> bool {
        self.observe(subject, chain, ObservationKind::Issued, !renewal)
    }

    fn observe(&self, subject: &[u8; 32], chain: &str, kind: ObservationKind, force: bool) -> bool {
        let Ok(own) = identity(&self.node.node_id()) else {
            return false;
        };
        let subject = *subject;
        if subject == own {
            return false;
        }
        let now = unix_now();
        let (doc, first_seen) = {
            let mut observer = self.observer.lock().unwrap_or_else(|e| e.into_inner());
            let Some((_, doc)) = observer.doc.clone() else {
                return false;
            };
            let first_seen = match observer.seen.get(&subject) {
                Some((_, written)) if !force && now < written + OBSERVATION_INTERVAL_SECONDS => {
                    return false
                }
                Some((first, _)) => *first,
                None => now,
            };
            if !observer.seen.contains_key(&subject)
                && observer.seen.len() >= self.options.max_admitted_peers
            {
                observer.seen.pop_first();
            }
            observer.seen.insert(subject, (first_seen, now));
            (doc, first_seen)
        };
        let Ok(secret) = <[u8; 32]>::try_from(self.node.export_secret_key()) else {
            return false;
        };
        let Ok(token) =
            super::issue_observation(&secret, &subject, chain, kind, first_seen, now, now)
        else {
            return false;
        };
        let docs = self.node.docs_client();
        tokio::spawn(async move {
            let written = async {
                let author = docs.default_author().await?;
                doc.set_bytes(
                    author,
                    super::observation_key(&own, &subject),
                    token.into_bytes(),
                )
                .await
            }
            .await;
            if let Err(error) = written {
                tracing::warn!(%error, "enrollment: observation write failed");
            }
        });
        true
    }

    /// Become (or stop being) an observer. This node observes only while the
    /// fleet binds an observations namespace, its own verified scopes include
    /// one of the binding's `write_scopes`, and the root's sealed grant opens
    /// to that namespace's write capability.
    async fn refresh_observer(&self, fleet: &CoreDoc) {
        let binding = self
            .trust()
            .store
            .bindings()
            .into_iter()
            .find(|binding| binding.purpose == NamespacePurpose::Observations);
        let qualified = binding.as_ref().is_some_and(|binding| {
            self.own_scopes()
                .is_some_and(|scopes| !scopes.is_disjoint(&binding.write_scopes))
        });
        let target = binding
            .as_ref()
            .filter(|_| qualified)
            .and_then(|binding| namespace(&binding.namespace).ok());
        let Some(target) = target else {
            self.observer.lock().unwrap_or_else(|e| e.into_inner()).doc = None;
            return;
        };
        let Ok(secret) = <[u8; 32]>::try_from(self.node.export_secret_key()) else {
            return;
        };
        let held = self
            .observer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .doc
            .as_ref()
            .is_some_and(|(namespace, _)| *namespace == target);
        let grant =
            super::open_observer_grant(fleet, self.trust().store.root(), &secret, &target).await;
        let doc = match grant {
            Ok(ObserverGrant::Active(_)) if held => return,
            Ok(ObserverGrant::Active(write)) => {
                let opened = async {
                    let doc = self
                        .node
                        .docs_client()
                        .import_write_namespace(write.to_vec())
                        .await?;
                    ensure!(
                        namespace(&doc.doc_id())? == target,
                        "observer grant names another namespace"
                    );
                    // Serve the namespace so the root can sync observations.
                    doc.start_sync(vec![]).await?;
                    Ok::<_, anyhow::Error>(doc)
                }
                .await;
                match opened {
                    Ok(doc) => Some((target, doc)),
                    Err(error) => {
                        tracing::warn!(%error, "enrollment: observer grant unusable");
                        None
                    }
                }
            }
            Ok(ObserverGrant::Absent) => return,
            Ok(ObserverGrant::Revoked) => None,
            Err(error) => {
                tracing::warn!(%error, "enrollment: observer grant unusable");
                None
            }
        };
        self.observer.lock().unwrap_or_else(|e| e.into_inner()).doc = doc;
    }

    /// This node's current verified scopes: its registration's if the root
    /// has published one, else its own chain's.
    fn own_scopes(&self) -> Option<std::collections::BTreeSet<String>> {
        let own = identity(&self.node.node_id()).ok()?;
        self.trust()
            .stores
            .select_evidence(&own, self.own_evidence().as_ref(), unix_now())
            .ok()?
            .claims()
            .scopes
            .clone()
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
        let previous = sessions.remove(&peer);
        let lineage = previous
            .as_ref()
            .filter(|session| {
                session
                    .evidence
                    .expires_at()
                    .is_none_or(|exp| unix_now() < exp)
            })
            .map_or(generation, |session| session.lineage);
        let (connections, admitted) = previous
            .map(|session| (session.connections, session.admitted))
            .unwrap_or_default();
        sessions.insert(
            peer,
            Session {
                generation,
                lineage,
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
        if offset_seconds.unsigned_abs() > self.trust().verifier.activation_leeway() {
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
        match self.trust().stores.select_evidence(peer, None, unix_now()) {
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
            self.offered_rotations(&hello.rotations);
            // Joining credentials are withheld until the client has verified
            // this responder. The responder's own identity proof is explicitly
            // public to bounded unauthenticated probes; no policy ticket yet.
            transmit(
                &send,
                &ReplicaProof {
                    version: 1,
                    rotations: self.root_rotations(),
                    chain: self.own_chain(),
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
            let bound_tickets = self.bound_tickets(&evidence).await?;
            transmit(
                &send,
                &Welcome {
                    version: 1,
                    policy_ticket: ticket,
                    bound_tickets,
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
        match self.handshake(replica, false).await? {
            Joined::Admitted(readiness, ticket) => Ok((readiness, ticket)),
            Joined::SyncOnly { .. } => bail!("replica not admitted"),
        }
    }

    /// The joining side of the handshake. With `sync_only`, a configured
    /// replica that presents no credential and that this node's policy has
    /// no record for at all yields [`Joined::SyncOnly`] instead of a refusal;
    /// a credential that fails verification, or any record that denies it,
    /// is still refused.
    async fn handshake(&self, replica: CoreNodeAddr, sync_only: bool) -> Result<Joined> {
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
                        rotations: self.root_rotations(),
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
                self.offered_rotations(&proof.rotations);
                let peer = identity(&connection.remote_id())?;
                let source = observed_source(&connection);
                let evidence = match self.evidence(&peer, proof.chain.as_deref(), source).await {
                    Ok(evidence) => Some(evidence),
                    Err(error)
                        if sync_only && proof.chain.is_none() && self.unknown(&peer, &error) =>
                    {
                        self.log_replica_decision(
                            &peer,
                            ENROLLMENT_ALPN,
                            Ok(REPLICA_SYNC_ALLOW),
                            false,
                        );
                        None
                    }
                    Err(error) => {
                        if sync_only {
                            let denial = GateDenial::from_admission(error.clone());
                            self.log_replica_decision(&peer, ENROLLMENT_ALPN, Err(denial), true);
                        }
                        return Err(error.into());
                    }
                };
                transmit(
                    &send,
                    &Presentation {
                        chain: self.own_chain(),
                        sent_at: unix_now(),
                    },
                )
                .await?;
                let welcome: Welcome = self.receive_step(&recv).await?;
                ensure!(welcome.version == 1, "unsupported enrollment version");
                transmit(&send, &true).await?;
                let confirmed: bool = self.receive_step(&recv).await?;
                ensure!(confirmed, "reciprocal enrollment rejected");
                if let Some(evidence) = &evidence {
                    self.evaluate(evidence, source).await?;
                }
                transmit(&send, &true).await?;
                let closed = tokio::time::timeout(self.options.step_timeout, connection.closed())
                    .await
                    .map_err(|_| anyhow!("enrollment step timeout"))?;
                ensure!(
                    closed.code == Some(0),
                    "replica did not complete enrollment"
                );
                // The replica admitted this node; this node admits nothing
                // in return and ignores the bound-namespace tickets, which
                // only an admitted replica's session may use.
                let Some(evidence) = evidence else {
                    return Ok(Joined::SyncOnly {
                        peer,
                        ticket: welcome.policy_ticket,
                    });
                };
                self.evaluate(&evidence, source).await?;
                let mut offered = welcome.bound_tickets;
                offered.truncate(MAX_OFFERED_TICKETS);
                {
                    let mut tickets = self
                        .offered_tickets
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if tickets.len() >= MAX_OFFERED_TICKETS && !tickets.contains_key(&peer) {
                        tickets.pop_first();
                    }
                    tickets.insert(peer, offered);
                }
                Ok(Joined::Admitted(
                    self.remember(evidence, source)?,
                    welcome.policy_ticket,
                ))
            }
            .await;
            match result {
                Ok(value) => Ok(value),
                Err(error) => {
                    // Read the replica's close reason before closing locally:
                    // its admission decision about this node.
                    let error = reported_refusal(&connection).map_or(error, anyhow::Error::from);
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
        let ticket = self.fleet_ticket(peer, &ticket)?;
        self.check_peer(&peer).await?;
        let doc = self.node.docs_client().join(ticket).await?;
        self.set_policy_document(doc.clone())?;
        self.note_pending_policy_content();
        let result = tokio::time::timeout(timeout, async {
            self.sync_document(&doc, peer, Some(self.trust().store.as_ref()), true)
                .await?;
            self.trust().stores.sync_bindings()?;
            self.synchronize_bound(peer).await
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

    /// A replica's fleet ticket narrowed to what it may name: a read
    /// capability for the pinned namespace, at `peer`'s address only.
    fn fleet_ticket(&self, peer: [u8; 32], ticket: &str) -> Result<String> {
        use iroh_docs::{Capability, DocTicket};
        use iroh_tickets::Ticket;
        let mut decoded = DocTicket::decode_string(ticket)?;
        ensure!(
            matches!(decoded.capability, Capability::Read(_)),
            "bootstrap requires read-only policy capability"
        );
        ensure!(
            namespace(&decoded.capability.id().to_string())? == *self.trust().store.namespace(),
            "wrong policy namespace"
        );
        decoded.nodes.retain(|node| node.id.as_bytes() == &peer);
        ensure!(
            !decoded.nodes.is_empty(),
            "policy ticket has no authenticated replica address"
        );
        Ok(decoded.encode_string())
    }

    /// Whether `error` means only that this node's policy holds nothing at
    /// all about `peer` yet (an empty or not yet synchronized fleet copy), as
    /// opposed to a denial, a skewed clock or a failed credential.
    fn unknown(&self, peer: &[u8; 32], error: &AdmissionError) -> bool {
        matches!(error, AdmissionError::Unavailable(_))
            && matches!(
                self.trust().stores.select_evidence(peer, None, unix_now()),
                Err(RecordError::MissingEvidence)
            )
    }

    fn replica_window_open(&self, peer: &[u8; 32]) -> bool {
        self.replica_windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(peer)
    }

    fn open_replica_window(&self, peer: [u8; 32]) -> ReplicaWindow<'_> {
        self.replica_windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(peer)
            .or_default()
            .open += 1;
        ReplicaWindow {
            windows: &self.replica_windows,
            peer,
        }
    }

    /// Remember a connection a replica window let through, to close with the
    /// window. False when no window for `peer` is open any more.
    fn track_replica_connection(&self, peer: &[u8; 32], connection: CoreWeakConnection) -> bool {
        let mut windows = self
            .replica_windows
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(sync) = windows.get_mut(peer) else {
            return false;
        };
        sync.connections.retain(CoreWeakConnection::is_alive);
        sync.connections.push(connection);
        true
    }

    /// Hold the fleet namespace from the configured `replicas`, the policy
    /// sources a node such as a daemon's child process is pointed at.
    ///
    /// Each replica is tried in order until one completes: enroll with it,
    /// then synchronize the fleet namespace from it. A replica that presents
    /// no credential and that this node's policy has no record for yet (an
    /// empty or stale fleet copy) is trusted for that synchronization only:
    /// it must still admit this node, this node's own outbound Docs and blob
    /// connections to it pass Gate 0 while the synchronization runs, and
    /// nothing else does. Every record is read by exact root author, so such
    /// a replica can withhold records but not forge them. Afterwards local
    /// policy judges it: registered, it is enrolled with as an ordinary
    /// admitted peer; denied or still unknown, it is refused.
    ///
    /// When no replica completes, this node starts from the root-authored
    /// records it holds locally, if any ([`ReplicaAttachment::LocalCopy`]);
    /// with none it fails. Either way it then keeps every configured replica
    /// in sync every [`EnrollmentOptions::replica_sync_interval`]: a replica
    /// with a session is synchronized again, one without is retried as
    /// above. Each step is bounded by `timeout`.
    pub async fn attach_replicas(
        &self,
        replicas: Vec<CoreNodeAddr>,
        timeout: Duration,
    ) -> Result<ReplicaAttachment> {
        ensure!(!replicas.is_empty(), "no policy replicas configured");
        let fleet = hex::encode(self.trust().store.namespace());
        if let Some(local) = self.node.docs_client().open(fleet).await? {
            self.set_policy_document(local)?;
            self.reconcile().await;
        }
        let mut last_error = None;
        let mut attached = None;
        for replica in &replicas {
            match self.attach_replica(replica.clone(), timeout).await {
                Ok(peer) => {
                    attached = Some(peer);
                    break;
                }
                Err(error) => {
                    tracing::debug!(
                        replica = %replica.endpoint_id,
                        error = %format!("{error:#}"),
                        "enrollment: policy replica did not complete"
                    );
                    last_error = Some(error);
                }
            }
        }
        let outcome = match attached {
            Some(peer) => ReplicaAttachment::Synchronized(peer),
            None => {
                let error = last_error.unwrap_or_else(|| anyhow!("no policy replica answered"));
                if !self.holds_local_policy().await {
                    return Err(error.context(
                        "no configured replica completed reciprocal enrollment and synchronization",
                    ));
                }
                let error = format!("{error:#}");
                tracing::warn!(
                    %error,
                    "enrollment: no policy replica completed; serving admission from the local \
                     fleet copy, unsynchronized, and retrying the replicas"
                );
                ReplicaAttachment::LocalCopy { error }
            }
        };
        self.spawn_replica_maintenance(replicas, timeout);
        Ok(outcome)
    }

    /// Whether the fleet copy this node holds has any root-authored record.
    async fn holds_local_policy(&self) -> bool {
        let doc = self
            .document
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match doc {
            Some(doc) => self
                .trust()
                .store
                .refresh(&doc)
                .await
                .is_ok_and(|report| report.available > 0),
            None => false,
        }
    }

    /// Enroll with one replica and synchronize the fleet namespace from it;
    /// see [`Self::attach_replicas`]. Returns the replica's identity.
    async fn attach_replica(&self, replica: CoreNodeAddr, timeout: Duration) -> Result<[u8; 32]> {
        let joined = tokio::time::timeout(timeout, self.handshake(replica.clone(), true))
            .await
            .map_err(|_| anyhow!("policy replica did not answer within the sync timeout"))??;
        let (readiness, ticket) = match joined {
            Joined::Admitted(readiness, ticket) => (readiness, ticket),
            Joined::SyncOnly { peer, ticket } => {
                let ticket = ticket.ok_or_else(|| anyhow!("replica offered no fleet ticket"))?;
                self.replica_sync(peer, &ticket, timeout).await?;
                // Local policy can now judge the replica.
                let judged = match self.subject_standing(&peer) {
                    SubjectStanding::Registered => Ok(()),
                    SubjectStanding::Denied(reason) => Err(AdmissionError::Denied(reason)),
                    SubjectStanding::Unregistered | SubjectStanding::Unknown(_) => {
                        Err(AdmissionError::Unavailable(
                            "the replica's fleet copy holds no registration for it".into(),
                        ))
                    }
                };
                if let Err(error) = judged {
                    let mut denial = GateDenial::from_admission(error.clone());
                    if matches!(error, AdmissionError::Unavailable(_)) {
                        denial.reason = "replica not registered";
                    }
                    self.log_replica_decision(&peer, ENROLLMENT_ALPN, Err(denial), true);
                    return Err(error.into());
                }
                match tokio::time::timeout(timeout, self.handshake(replica, false))
                    .await
                    .map_err(|_| {
                        anyhow!("policy replica did not answer within the sync timeout")
                    })?? {
                    Joined::Admitted(readiness, ticket) => (readiness, ticket),
                    Joined::SyncOnly { .. } => bail!("replica not admitted"),
                }
            }
        };
        let ticket = ticket.ok_or_else(|| anyhow!("replica offered no fleet ticket"))?;
        self.synchronize(readiness.peer, ticket, timeout).await?;
        Ok(readiness.peer)
    }

    /// Synchronize the fleet namespace, and nothing else, from a replica that
    /// admitted this node but holds no session here. Gate 0 lets this node's
    /// own outbound Docs and blob connections to it through while this runs.
    /// No policy freshness is recorded: only a session's sync proves that.
    async fn replica_sync(&self, peer: [u8; 32], ticket: &str, timeout: Duration) -> Result<()> {
        let ticket = self.fleet_ticket(peer, ticket)?;
        {
            // Importing the ticket already dials the replica.
            let _window = self.open_replica_window(peer);
            let doc = self.node.docs_client().join(ticket).await?;
            self.set_policy_document(doc.clone())?;
            self.note_pending_policy_content();
            tokio::time::timeout(
                timeout,
                self.sync_document(&doc, peer, Some(self.trust().store.as_ref()), true),
            )
            .await
            .map_err(|_| anyhow!("fleet synchronization from the replica timed out"))??;
        }
        self.trust().stores.sync_bindings()?;
        self.reconcile().await;
        Ok(())
    }

    /// Keep the fleet namespace synchronized with every configured replica
    /// (see [`Self::attach_replicas`]). Replaces any earlier maintenance.
    fn spawn_replica_maintenance(&self, replicas: Vec<CoreNodeAddr>, timeout: Duration) {
        let weak = self.this.clone();
        let interval = self.options.replica_sync_interval;
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(service) = weak.upgrade() else {
                    break;
                };
                // A closed node has nothing to keep in sync, and must not be
                // kept alive (with its policy store's lock) by trying.
                let endpoint = service.node.inner.endpoint.clone();
                if endpoint.is_closed() {
                    break;
                }
                tokio::select! {
                    _ = tokio::time::timeout(
                        timeout.saturating_mul(3),
                        service.maintain_replicas(&replicas, timeout),
                    ) => {}
                    _ = endpoint.closed() => break,
                }
            }
        });
        if let Some(previous) = self
            .replica_maintenance
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(task.abort_handle())
        {
            previous.abort();
        }
    }

    async fn maintain_replicas(&self, replicas: &[CoreNodeAddr], timeout: Duration) {
        for replica in replicas {
            let Ok(peer) = identity(&replica.endpoint_id) else {
                continue;
            };
            if self.has_session(&peer) {
                let doc = self
                    .document
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(doc) = doc {
                    let started =
                        tokio::time::timeout(timeout, doc.start_sync(vec![hex::encode(peer)]))
                            .await;
                    if !matches!(started, Ok(Ok(()))) {
                        tracing::debug!(
                            replica = %replica.endpoint_id,
                            "enrollment: replica resync did not start"
                        );
                    }
                    continue;
                }
            }
            // A replica this node's policy definitely denies stays refused.
            if let AdmissionError::Denied(_) = self.sessionless_standing(&peer) {
                continue;
            }
            if let Err(error) = self.attach_replica(replica.clone(), timeout).await {
                tracing::debug!(
                    replica = %replica.endpoint_id,
                    error = %format!("{error:#}"),
                    "enrollment: policy replica retry did not complete"
                );
            }
        }
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
        match self
            .evaluate_session(peer, &session, session.observed_source)
            .await
        {
            Ok(Some(readiness)) => Ok(readiness),
            Ok(None) => {
                self.confirm(peer, session.generation)?;
                Ok(self.readiness(*peer, session.evidence.source(), session.observed_source))
            }
            Err(error) => {
                if let AdmissionError::Denied(reason) = &error {
                    self.revoke_generation(peer, session.generation, reason.clone());
                }
                Err(error)
            }
        }
    }

    /// The verified evidence `peer`'s current admission session rests on: the
    /// claims a consumer maps to its own roles. `None` without a session.
    /// This reads the session as last admitted or checked; call
    /// [`Self::check_peer`] first to re-evaluate it against current policy.
    pub fn peer_evidence(&self, peer: &[u8; 32]) -> Option<VerifiedAdmissionEvidence> {
        self.session(peer).map(|session| session.evidence)
    }

    /// [`Self::peer_evidence`] with the session's lineage: an id that stays
    /// the same while `peer`'s session is renewed in place and changes once
    /// the session ends (revocation, eviction, expiry) and the peer is
    /// admitted again. Consumers key per-session state to it.
    pub fn peer_session(&self, peer: &[u8; 32]) -> Option<(u64, VerifiedAdmissionEvidence)> {
        self.session(peer)
            .map(|session| (session.lineage, session.evidence))
    }

    /// Evaluate a session's evidence at `source`. When a newer root record
    /// superseded it (a promotion, or a later registration revision), derive
    /// current evidence as reconciliation would and replace that exact
    /// generation instead of reporting the stale evidence as a denial.
    /// `Ok(Some(_))` is the upgraded session; `Ok(None)` means unchanged.
    async fn evaluate_session(
        &self,
        peer: &[u8; 32],
        session: &Session,
        source: Option<ObservedSourceAddress>,
    ) -> std::result::Result<Option<EnrollmentReadiness>, AdmissionError> {
        match self.evaluate(&session.evidence, source).await {
            Err(AdmissionError::Denied(DenialReason::RegistrationSuperseded)) => {}
            other => return other.map(|()| None),
        }
        let current = self
            .evidence(peer, session.evidence.presented_chain(), source)
            .await?;
        let mut readiness = self
            .replace_session(current, session.observed_source, Some(session.generation))
            .map_err(|error| AdmissionError::Unavailable(error.to_string()))?;
        readiness.observed_source = source;
        Ok(Some(readiness))
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
        if let Some(readiness) = self.evaluate_session(peer, &session, source).await? {
            return Ok(readiness);
        }
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
            let mut pending = match self.trust().store.refresh(&doc).await {
                Ok(report) => report.has_uncertainty(),
                Err(_) => true,
            };
            pending |= self.trust().stores.sync_bindings().is_err();
            for (store, doc) in self.membership_documents() {
                pending |= store
                    .refresh(&doc)
                    .await
                    .map_or(true, |report| report.has_uncertainty());
            }
            self.refresh_observer(&doc).await;
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
                .trust()
                .stores
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

/// Re-apply the rotations journaled beside `store` that continue the pinned
/// root, so a rotation learned at runtime survives restart. Returns the store
/// and verifier for the current root.
fn resume_rotation(
    store: Arc<EnrollmentPolicyStore>,
    verifier: JwtEnrollmentVerifier,
    pin: Option<RotationPin>,
    applied: &[String],
) -> Result<(
    Arc<EnrollmentPolicyStore>,
    JwtEnrollmentVerifier,
    Option<RotationState>,
)> {
    let Some(pin) = pin else {
        return Ok((store, verifier, None));
    };
    let mut journal = RotationJournal::load(store.rotation_journal_path())?;
    let mut root = RootState {
        root: *store.root(),
        next_commitment: pin.next_commitment,
        seq: pin.seq,
    };
    if !applied.is_empty() {
        let history = verified_history(applied, root, unix_now(), verifier.activation_leeway())
            .map_err(|_| anyhow!("applied root rotations are not the pinned root's history"))?;
        journal.seed(&history)?;
    }
    let applied = journal.replay(root, unix_now(), verifier.activation_leeway());
    let Some(last) = applied.last() else {
        return Ok((store, verifier, Some(RotationState { root, journal })));
    };
    root = last.apply();
    let store = Arc::new(store.open_rotated(root.root, last.namespaces()[0])?);
    let verifier = rotated_verifier(&verifier, root.root)?;
    Ok((store, verifier, Some(RotationState { root, journal })))
}

fn rotated_verifier(
    previous: &JwtEnrollmentVerifier,
    root: [u8; 32],
) -> Result<JwtEnrollmentVerifier> {
    Ok(
        JwtEnrollmentVerifier::new(root, previous.intended_audience())?
            .with_activation_leeway(previous.activation_leeway())?,
    )
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
        Some(AdmissionError::Denied(_)) => DENIED_REASON.as_bytes().to_vec(),
        Some(AdmissionError::Unavailable(_) | AdmissionError::NotAdmitted) => {
            UNAVAILABLE_REASON.as_bytes().to_vec()
        }
        None => FAILED_REASON.as_bytes().to_vec(),
    }
}
/// The replica's admission decision about this node, from its close reason:
/// a skew reason measured our clock against its own (restated from our side,
/// remote minus local); a denial or an unavailable policy maps to the same
/// [`AdmissionError`] kind the replica reached. The replica keeps the detailed
/// reason; a peer only learns the kind.
fn reported_refusal(connection: &CoreConnection) -> Option<AdmissionError> {
    let closed = connection.close_info()?;
    if closed.code != Some(REJECT) {
        return None;
    }
    let reason = String::from_utf8(closed.reason?).ok()?;
    if let Some(theirs) = reason.strip_prefix(CLOCK_SKEW_REASON) {
        let offset_seconds = theirs.parse::<i64>().ok()?.checked_neg()?;
        tracing::warn!(offset_seconds, "enrollment: replica reports clock skew");
        return Some(AdmissionError::ClockSkew { offset_seconds });
    }
    match reason.as_str() {
        DENIED_REASON => Some(AdmissionError::Denied(DenialReason::RefusedByPeer)),
        UNAVAILABLE_REASON => Some(AdmissionError::Unavailable(
            "the replica could not evaluate enrollment policy now".into(),
        )),
        _ => None,
    }
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
            EnrollmentService::new(
                node,
                Trust::new(store, verifier),
                OwnCredential {
                    chain: None,
                    evidence: None,
                },
                policy,
                options,
                None,
            ),
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

    /// Promotion race: once this node's store holds a root registration for a
    /// peer admitted on its chain, a check that runs before reconciliation
    /// upgrades the session to the registration instead of revoking the peer
    /// as `RegistrationSuperseded`. Both the operation check and the Gate 0
    /// path are covered.
    #[tokio::test]
    async fn check_before_reconcile_upgrades_a_promoted_chain_session() {
        use super::super::{AdmissionClaims, NodeRegistrationStatus, NodeRegistrationV1};
        for gate in [false, true] {
            // The service runs on the root's node, whose default author is the
            // root key, so its own writes are exact-root records.
            let root = raw_node(1).await;
            let doc = root.docs_client().create().await.unwrap();
            let mut nonce = [0; 8];
            getrandom::getrandom(&mut nonce).unwrap();
            let path = std::env::temp_dir().join(format!(
                "aster-bootstrap-promotion-{}-{}.json",
                std::process::id(),
                hex::encode(nonce)
            ));
            let store = Arc::new(
                EnrollmentPolicyStore::open(&path, public(1), *doc.doc.id().as_bytes(), AUDIENCE)
                    .unwrap(),
            );
            let policy = PausedPolicy::new(false);
            policy.pause_next.store(false, Ordering::SeqCst);
            let service = EnrollmentService::new(
                root,
                Trust::new(
                    store.clone(),
                    JwtEnrollmentVerifier::new(public(1), AUDIENCE).unwrap(),
                ),
                OwnCredential {
                    chain: None,
                    evidence: None,
                },
                policy,
                EnrollmentOptions::default(),
                None,
            );
            service.set_policy_document(doc.clone()).unwrap();
            service.remember(evidence(3), None).unwrap();
            let generation = service.session(&public(3)).unwrap().generation;

            let record = NodeRegistrationV1 {
                version: 1,
                issuer: hex::encode(public(1)),
                subject: hex::encode(public(3)),
                audience: crate::delegation::Audience::One(AUDIENCE.into()),
                revision: 1,
                issued_at: unix_now() - 60,
                not_before: None,
                expires_at: None,
                status: NodeRegistrationStatus::Active,
                claims: AdmissionClaims::default(),
                mandatory: vec![],
                reauthorizes_revision: None,
                reason: None,
            };
            doc.set_bytes_durable(
                hex::encode(public(1)),
                super::super::node_registration_key(&public(3)),
                serde_json::to_vec(&record).unwrap(),
            )
            .await
            .unwrap();
            // A policy sync refreshed the store; reconciliation has not run.
            store.refresh(&doc).await.unwrap();
            assert!(matches!(
                service
                    .trust()
                    .stores
                    .select_evidence(&public(3), None, unix_now())
                    .unwrap()
                    .source(),
                AdmissionEvidenceSource::RootAuthoredRecord { revision: 1, .. }
            ));

            let ready = if gate {
                service
                    .check_peer_at_source(&public(3), None)
                    .await
                    .unwrap()
            } else {
                service.check_peer(&public(3)).await.unwrap()
            };
            assert!(matches!(
                ready.source,
                AdmissionEvidenceSource::RootAuthoredRecord { revision: 1, .. }
            ));
            let session = service.session(&public(3)).expect("session survives");
            assert_ne!(session.generation, generation);
            assert!(matches!(
                session.evidence.source(),
                AdmissionEvidenceSource::RootAuthoredRecord { .. }
            ));
            assert!(service.revoked_peers.lock().unwrap().reasons.is_empty());
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
            rotations: Vec::new(),
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
            rotations: Vec::new(),
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
                rotations: Vec::new(),
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
                            rotations: Vec::new(),
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
                rotations: Vec::new(),
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
                rotations: Vec::new(),
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

    /// Replica trust tolerates only absent evidence. A configured replica
    /// that presents no credential, while this node's store holds nothing
    /// about it, is trusted to the point of receiving this node's
    /// presentation (fleet synchronization would follow). One that presents a
    /// credential that fails verification is refused before that, even with
    /// an empty store: a chain signed by another key, a forged chain naming
    /// the root as issuer, and a valid root credential issued to another key.
    #[tokio::test]
    async fn replica_trust_refuses_a_credential_that_fails_verification() {
        let (service, path) = service(PausedPolicy::new(false), EnrollmentOptions::default()).await;
        let request =
            |subject: u8| EnrollmentIssueRequest::new(public(subject), AUDIENCE, unix_now() + 3600);
        let other_key = JwtEnrollmentIssuer::from_seed(&[8; 32])
            .issue(&request(9), unix_now())
            .unwrap()
            .into_chain();
        let forged = {
            // Another key's link, re-labelled as issued by the root: the
            // payload names the root, the signature is still the other key's.
            use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
            let (prefix, link) = other_key.rsplit_once('~').unwrap();
            let mut parts: Vec<String> = link.split('.').map(str::to_owned).collect();
            let payload = String::from_utf8(URL_SAFE_NO_PAD.decode(&parts[1]).unwrap()).unwrap();
            let relabelled = payload.replace(&hex::encode(public(8)), &hex::encode(public(1)));
            assert_ne!(payload, relabelled);
            parts[1] = URL_SAFE_NO_PAD.encode(relabelled);
            format!("{prefix}~{}", parts.join("."))
        };
        let wrong_subject = JwtEnrollmentIssuer::from_seed(&[1; 32])
            .issue(&request(10), unix_now())
            .unwrap()
            .into_chain();
        for (chain, trusted) in [
            (None, true),
            (Some(other_key), false),
            (Some(forged), false),
            (Some(wrong_subject), false),
        ] {
            let replica = raw_node(9).await;
            let handler = tokio::spawn({
                let replica = replica.clone();
                async move {
                    let (_, connection) = replica.accept_aster().await.unwrap();
                    let (send, recv) = connection.accept_bi().await.unwrap();
                    let _: Hello = receive(&recv).await.unwrap();
                    transmit(
                        &send,
                        &ReplicaProof {
                            version: 1,
                            rotations: Vec::new(),
                            chain,
                            sent_at: unix_now(),
                        },
                    )
                    .await
                    .unwrap();
                    let presented = receive::<Presentation>(&recv).await.is_ok();
                    let _ = connection.close(REJECT, b"test replica done".to_vec());
                    presented
                }
            });
            let attached = service
                .attach_replicas(vec![replica.node_addr_info()], Duration::from_secs(5))
                .await;
            assert!(
                attached.is_err(),
                "nothing completed and nothing is held locally"
            );
            assert_eq!(handler.await.unwrap(), trusted);
            assert!(!service.has_session(&public(9)));
            assert!(!service.replica_window_open(&public(9)));
            replica.close().await;
        }
        cleanup(service, path).await;
    }
}
