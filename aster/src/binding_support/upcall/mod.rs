//! Upcalls: Rust asking foreign (binding) code a question -- plan D6 / R6,
//! design `docs/_internal/bindings/upcall-design.md`.
//!
//! Several `aster` extension points are Rust traits or callbacks
//! ([`LeaseSerializer`](crate::lease::LeaseSerializer),
//! [`FencedResource`](crate::lease::FencedResource),
//! [`LeaseAuthorizer`](crate::lease::LeaseAuthorizer), topology
//! [`AdmittedFn`](crate::topology::AdmittedFn), the placement manager's
//! providers, expose's `AuthorizeFn`, Gate 0 admission). A binding cannot
//! hand Rust a function pointer (the no-function-pointer rule), so every one
//! of them is served the same way: **event + respond**.
//!
//! 1. Rust allocates an `invocation_id`, records a pending entry and hands an
//!    [`UpcallEvent`] `{ invocation_id, kind, deadline, payload }` to the
//!    binding's [`UpcallSink`]. Payloads are the canonical UCv1 encodings in
//!    [`codec`].
//! 2. The binding runs its handler wherever it likes (asyncio task, JS
//!    microtask, Java virtual thread) and calls
//!    [`Upcalls::respond`]`(invocation_id, status, bytes)`, from any thread.
//! 3. The waiting Rust future (or blocked thread, for the synchronous
//!    callback types) resumes with the bytes.
//!
//! Per [`UpcallKind`] an [`UpcallPolicy`] sets the timeout and, for the
//! yes/no decision kinds, the [`HookFailureMode`] applied when the binding
//! errors, times out, is overloaded or has shut down. When Rust stops
//! waiting -- timeout, or the waiting future is dropped -- the binding gets
//! an [`UpcallKind::Cancel`] event for the id (or, if the original event is
//! still queued in an [`UpcallQueue`], the event is simply withdrawn), and a
//! later `respond` for it returns [`RespondError::UnknownInvocation`].
//!
//! [`UpcallQueue`] is the shared delivery: a bounded queue drained in batches,
//! awaitable ([`UpcallQueue::ready`], PyO3), blocking with a timeout
//! ([`UpcallQueue::wait_blocking`], C ABI poll), or bypassed entirely by a
//! sink that forwards straight into a NAPI `ThreadsafeFunction`.
//!
//! The adapters that implement the Rust traits on top live in the
//! submodules: [`lease`], [`topology`], [`admission`], `placement`
//! (`coordination-client`) and `expose` (`expose`).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{oneshot, Notify};

pub use aster_transport_core::trust::HookFailureMode;

use crate::log_targets::UPCALL;

pub mod admission;
pub mod codec;
#[cfg(feature = "expose")]
pub mod expose;
pub mod lease;
#[cfg(feature = "coordination-client")]
pub mod placement;
pub mod topology;

// ── kinds and statuses ──────────────────────────────────────────────────────

/// What an upcall asks. The numeric value is the wire/ABI code (`u16`);
/// it never changes once assigned. Ranges: `0` control, `1..=15` leases,
/// `16..=31` topology, `32..=47` placement, `48..=63` expose, `64..=79`
/// Gate 0 admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
#[non_exhaustive]
pub enum UpcallKind {
    /// Control event: Rust stopped waiting for `invocation_id`. Payload is
    /// one [`CancelReason`] byte. Never answered.
    Cancel = 0,
    /// `LeaseSerializer::transition` -> `TransitionOutcome`.
    LeaseTransition = 1,
    /// `LeaseSerializer::snapshot` -> `LeaseSnapshot`.
    LeaseSnapshot = 2,
    /// `LeaseAuthorizer` -> bool decision.
    LeaseAuthorize = 3,
    /// `FencedResource::mutate` -> output bytes or a `FenceRejection`.
    FencedMutate = 4,
    /// Topology `AdmittedFn`, batched: node ids -> one bool each.
    TopologyAdmitted = 16,
    /// Placement `TopologySource::snapshot` -> serialized shared view.
    TopologySnapshot = 17,
    /// `PlacementPolicySource::policy` -> `PlacementPolicy`.
    PlacementPolicy = 32,
    /// `PlacementProvisioner::provision` -> `MemberSeedV1`.
    PlacementProvision = 33,
    /// expose `AuthorizeFn` -> bool decision.
    ExposeAuthorize = 48,
    /// The bytes-level expose `HttpHandler` (R9): request head (+ inline
    /// body or a body handle) -> response head (+ inline body or a body
    /// handle). Bodies stream through `expose::HttpBodies`, not upcalls.
    ExposeHttp = 49,
    /// expose `RoutePolicy` (an edge's `serve_routes`): requested route
    /// specs -> the granted subset.
    ExposeRoutePolicy = 50,
    /// Gate 0 outbound connect hook -> bool decision.
    AdmissionConnect = 64,
    /// Gate 0 inbound after-handshake hook -> accept, or reject with
    /// code + reason.
    AdmissionHandshake = 65,
}

impl UpcallKind {
    /// Every kind, in code order.
    pub const ALL: [UpcallKind; 14] = [
        UpcallKind::Cancel,
        UpcallKind::LeaseTransition,
        UpcallKind::LeaseSnapshot,
        UpcallKind::LeaseAuthorize,
        UpcallKind::FencedMutate,
        UpcallKind::TopologyAdmitted,
        UpcallKind::TopologySnapshot,
        UpcallKind::PlacementPolicy,
        UpcallKind::PlacementProvision,
        UpcallKind::ExposeAuthorize,
        UpcallKind::ExposeHttp,
        UpcallKind::ExposeRoutePolicy,
        UpcallKind::AdmissionConnect,
        UpcallKind::AdmissionHandshake,
    ];

    /// The ABI code.
    pub fn code(self) -> u16 {
        self as u16
    }

    /// Parse an ABI code.
    pub fn from_code(code: u16) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.code() == code)
    }

    /// The stable snake_case name (logs, vectors, binding enums).
    pub fn name(self) -> &'static str {
        match self {
            UpcallKind::Cancel => "cancel",
            UpcallKind::LeaseTransition => "lease_transition",
            UpcallKind::LeaseSnapshot => "lease_snapshot",
            UpcallKind::LeaseAuthorize => "lease_authorize",
            UpcallKind::FencedMutate => "fenced_mutate",
            UpcallKind::TopologyAdmitted => "topology_admitted",
            UpcallKind::TopologySnapshot => "topology_snapshot",
            UpcallKind::PlacementPolicy => "placement_policy",
            UpcallKind::PlacementProvision => "placement_provision",
            UpcallKind::ExposeAuthorize => "expose_authorize",
            UpcallKind::ExposeHttp => "expose_http",
            UpcallKind::ExposeRoutePolicy => "expose_route_policy",
            UpcallKind::AdmissionConnect => "admission_connect",
            UpcallKind::AdmissionHandshake => "admission_handshake",
        }
    }

    /// Whether the kind is a yes/no decision, to which an
    /// [`UpcallPolicy::failure_mode`] applies. Every other kind surfaces a
    /// failure as an error to its Rust caller.
    pub fn is_decision(self) -> bool {
        matches!(
            self,
            UpcallKind::LeaseAuthorize
                | UpcallKind::TopologyAdmitted
                | UpcallKind::ExposeAuthorize
                | UpcallKind::ExposeRoutePolicy
                | UpcallKind::AdmissionConnect
                | UpcallKind::AdmissionHandshake
        )
    }
}

impl fmt::Display for UpcallKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why Rust stopped waiting; the payload byte of a [`UpcallKind::Cancel`]
/// event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CancelReason {
    /// The kind's timeout passed.
    Timeout = 1,
    /// The waiting Rust future was dropped (its caller gave up).
    Abandoned = 2,
    /// [`Upcalls::close`] ran.
    Shutdown = 3,
}

impl CancelReason {
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(CancelReason::Timeout),
            2 => Some(CancelReason::Abandoned),
            3 => Some(CancelReason::Shutdown),
            _ => None,
        }
    }
}

/// How the binding answered. ABI code `u32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum UpcallStatus {
    /// The payload is the kind's UCv1 response.
    Ok = 0,
    /// The handler failed; the payload is a UTF-8 message.
    Error = 1,
    /// The binding has no handler for this kind (payload ignored).
    Unimplemented = 2,
}

impl UpcallStatus {
    /// Parse an ABI code; an unknown code reads as [`UpcallStatus::Error`].
    pub fn from_code(code: u32) -> Self {
        match code {
            0 => UpcallStatus::Ok,
            2 => UpcallStatus::Unimplemented,
            _ => UpcallStatus::Error,
        }
    }

    pub fn code(self) -> u32 {
        self as u32
    }
}

// ── events, sinks, errors ──────────────────────────────────────────────────

/// One upcall handed to the binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpcallEvent {
    /// Correlation key for [`Upcalls::respond`]. Unique per [`Upcalls`],
    /// starts at 1, never reused. For [`UpcallKind::Cancel`] it names the
    /// invocation being cancelled.
    pub invocation_id: u64,
    pub kind: UpcallKind,
    /// When Rust stops waiting (Unix ms). Informational: the binding may use
    /// it to bound its own work; Rust enforces it regardless.
    pub deadline_unix_ms: u64,
    /// The kind's UCv1 request ([`codec`]); for `Cancel`, one
    /// [`CancelReason`] byte.
    pub payload: Vec<u8>,
}

impl UpcallEvent {
    /// For a `Cancel` event: why.
    pub fn cancel_reason(&self) -> Option<CancelReason> {
        if self.kind != UpcallKind::Cancel {
            return None;
        }
        self.payload
            .first()
            .copied()
            .and_then(CancelReason::from_code)
    }
}

/// Where upcall events go. `deliver` must not block and must not call back
/// into [`Upcalls`] (it can run under the caller's locks). Return `false`
/// to refuse the event (queue full, binding closed): the upcall then fails
/// at once with [`UpcallError::Overloaded`]. `Cancel` events should always
/// be accepted; a refused one is dropped.
pub trait UpcallSink: Send + Sync + 'static {
    fn deliver(&self, event: UpcallEvent) -> bool;
}

/// Why an upcall produced no answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UpcallError {
    #[error("upcall timed out")]
    Timeout,
    #[error("upcall refused: binding queue full or closed")]
    Overloaded,
    #[error("upcalls closed")]
    Closed,
    #[error("foreign handler failed: {0}")]
    Foreign(String),
    #[error("foreign handler not implemented for {0}")]
    Unimplemented(UpcallKind),
    #[error("malformed upcall response: {0}")]
    Decode(String),
}

impl From<UpcallError> for crate::Error {
    fn from(e: UpcallError) -> Self {
        crate::Error::Transport(anyhow::Error::new(e))
    }
}

/// Why a [`Upcalls::respond`] was not delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RespondError {
    /// No pending upcall has this id: never issued, already answered,
    /// cancelled, or timed out. Bindings should ignore it (it is the normal
    /// outcome of a late answer).
    #[error("unknown or finished upcall invocation {0}")]
    UnknownInvocation(u64),
}

/// Timeout and failure mode for one [`UpcallKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpcallPolicy {
    /// How long Rust waits for `respond`.
    pub timeout: Duration,
    /// For decision kinds: the answer used when there is none (error,
    /// timeout, overload, shutdown, undecodable response).
    pub failure_mode: HookFailureMode,
}

impl Default for UpcallPolicy {
    /// 5 s, fail closed.
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            failure_mode: HookFailureMode::FailClosed,
        }
    }
}

impl UpcallPolicy {
    pub fn new(timeout: Duration, failure_mode: HookFailureMode) -> Self {
        Self {
            timeout,
            failure_mode,
        }
    }

    /// The decision used when the binding gave none.
    pub fn fallback(&self) -> bool {
        matches!(self.failure_mode, HookFailureMode::FailOpen)
    }
}

// ── the upcall table ───────────────────────────────────────────────────────

type Reply = (UpcallStatus, Vec<u8>);

enum ReplyTx {
    Async(oneshot::Sender<Reply>),
    Blocking(std::sync::mpsc::SyncSender<Reply>),
}

impl ReplyTx {
    fn send(self, reply: Reply) -> bool {
        match self {
            ReplyTx::Async(tx) => tx.send(reply).is_ok(),
            ReplyTx::Blocking(tx) => tx.send(reply).is_ok(),
        }
    }
}

struct Pending {
    kind: UpcallKind,
    reply: ReplyTx,
}

struct Inner {
    sink: Arc<dyn UpcallSink>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    policies: RwLock<HashMap<UpcallKind, UpcallPolicy>>,
    default_policy: RwLock<UpcallPolicy>,
    closed: AtomicBool,
}

/// The upcall table: issues upcalls through one [`UpcallSink`] and routes
/// [`respond`](Upcalls::respond)s back to their waiters. Clone-cheap; one
/// per binding runtime is enough (every adapter can share it).
#[derive(Clone)]
pub struct Upcalls {
    inner: Arc<Inner>,
}

impl fmt::Debug for Upcalls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upcalls")
            .field("pending", &self.pending_count())
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// Removes a pending entry if its waiter goes away first, and tells the
/// binding.
struct PendingGuard<'a> {
    upcalls: &'a Upcalls,
    id: u64,
    reason: CancelReason,
    armed: bool,
}

impl PendingGuard<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.upcalls.cancel(self.id, self.reason);
        }
    }
}

fn unix_ms_after(timeout: Duration) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now + timeout).as_millis().min(u64::MAX as u128) as u64
}

impl Upcalls {
    /// A table delivering to `sink`, every kind on [`UpcallPolicy::default`].
    pub fn new(sink: Arc<dyn UpcallSink>) -> Self {
        Self {
            inner: Arc::new(Inner {
                sink,
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                policies: RwLock::new(HashMap::new()),
                default_policy: RwLock::new(UpcallPolicy::default()),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// Set the policy for kinds without their own.
    pub fn set_default_policy(&self, policy: UpcallPolicy) {
        *self.inner.default_policy.write().unwrap() = policy;
    }

    /// Set `kind`'s policy. Takes effect for upcalls issued afterwards.
    pub fn set_policy(&self, kind: UpcallKind, policy: UpcallPolicy) {
        self.inner.policies.write().unwrap().insert(kind, policy);
    }

    /// The policy `kind` runs under.
    pub fn policy(&self, kind: UpcallKind) -> UpcallPolicy {
        self.inner
            .policies
            .read()
            .unwrap()
            .get(&kind)
            .copied()
            .unwrap_or_else(|| *self.inner.default_policy.read().unwrap())
    }

    /// Upcalls waiting for an answer.
    pub fn pending_count(&self) -> usize {
        self.inner.pending.lock().unwrap().len()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Answer invocation `invocation_id`. Callable from any thread, never
    /// blocks. `payload` is the kind's UCv1 response for
    /// [`UpcallStatus::Ok`], a UTF-8 message for [`UpcallStatus::Error`].
    pub fn respond(
        &self,
        invocation_id: u64,
        status: UpcallStatus,
        payload: Vec<u8>,
    ) -> Result<(), RespondError> {
        let pending = self.inner.pending.lock().unwrap().remove(&invocation_id);
        let Some(pending) = pending else {
            tracing::debug!(
                target: UPCALL,
                invocation_id,
                "respond for an unknown or finished upcall"
            );
            return Err(RespondError::UnknownInvocation(invocation_id));
        };
        tracing::trace!(target: UPCALL, invocation_id, kind = %pending.kind, ?status, "upcall answered");
        if pending.reply.send((status, payload)) {
            Ok(())
        } else {
            // The waiter vanished between our remove and its own cleanup.
            Err(RespondError::UnknownInvocation(invocation_id))
        }
    }

    /// Stop issuing upcalls and fail every pending one with
    /// [`UpcallError::Closed`] (a `Cancel{Shutdown}` event goes to the sink
    /// for each). Idempotent. Later upcalls fail at once (decision kinds
    /// fall back to their failure mode).
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        let drained: Vec<(u64, Pending)> = self.inner.pending.lock().unwrap().drain().collect();
        for (id, pending) in drained {
            drop(pending.reply);
            self.inner
                .sink
                .deliver(cancel_event(id, CancelReason::Shutdown));
        }
    }

    fn register(&self, kind: UpcallKind, reply: ReplyTx) -> Result<u64, UpcallError> {
        if self.is_closed() {
            return Err(UpcallError::Closed);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending { kind, reply });
        // `close` may have drained between the check and the insert.
        if self.is_closed() {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(UpcallError::Closed);
        }
        Ok(id)
    }

    fn cancel(&self, id: u64, reason: CancelReason) {
        let removed = self.inner.pending.lock().unwrap().remove(&id);
        if let Some(pending) = removed {
            tracing::debug!(target: UPCALL, invocation_id = id, kind = %pending.kind, ?reason, "upcall cancelled");
            self.inner.sink.deliver(cancel_event(id, reason));
        }
    }

    fn deliver(&self, id: u64, kind: UpcallKind, timeout: Duration, payload: Vec<u8>) -> bool {
        tracing::trace!(target: UPCALL, invocation_id = id, %kind, "upcall issued");
        self.inner.sink.deliver(UpcallEvent {
            invocation_id: id,
            kind,
            deadline_unix_ms: unix_ms_after(timeout),
            payload,
        })
    }

    /// Issue a `kind` upcall and wait (asynchronously) for the answer,
    /// bounded by the kind's timeout. Dropping the returned future cancels
    /// the upcall.
    pub async fn call(&self, kind: UpcallKind, payload: Vec<u8>) -> Result<Vec<u8>, UpcallError> {
        let timeout = self.policy(kind).timeout;
        let (tx, rx) = oneshot::channel();
        let id = self.register(kind, ReplyTx::Async(tx))?;
        let mut guard = PendingGuard {
            upcalls: self,
            id,
            reason: CancelReason::Abandoned,
            armed: true,
        };
        if !self.deliver(id, kind, timeout, payload) {
            guard.disarm();
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(UpcallError::Overloaded);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => {
                guard.disarm();
                reply_result(kind, reply)
            }
            Ok(Err(_)) => {
                guard.disarm();
                Err(UpcallError::Closed)
            }
            Err(_) => {
                guard.reason = CancelReason::Timeout;
                drop(guard);
                Err(UpcallError::Timeout)
            }
        }
    }

    /// As [`call`](Self::call), blocking the calling thread -- for the
    /// synchronous callback types (`LeaseAuthorizer`,
    /// `PlacementPolicySource`). On a multi-thread tokio worker it runs under
    /// `block_in_place`, so other tasks keep running; on a current-thread
    /// runtime it blocks that runtime for up to the timeout (the
    /// [`UpcallQueue`] needs no runtime to deliver, so it cannot deadlock).
    pub fn call_blocking(
        &self,
        kind: UpcallKind,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, UpcallError> {
        let timeout = self.policy(kind).timeout;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let id = self.register(kind, ReplyTx::Blocking(tx))?;
        let mut guard = PendingGuard {
            upcalls: self,
            id,
            reason: CancelReason::Abandoned,
            armed: true,
        };
        if !self.deliver(id, kind, timeout, payload) {
            guard.disarm();
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(UpcallError::Overloaded);
        }
        let wait = || rx.recv_timeout(timeout);
        let got = match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(wait)
            }
            _ => wait(),
        };
        match got {
            Ok(reply) => {
                guard.disarm();
                reply_result(kind, reply)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                guard.disarm();
                Err(UpcallError::Closed)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                guard.reason = CancelReason::Timeout;
                drop(guard);
                Err(UpcallError::Timeout)
            }
        }
    }

    /// Resolve a decision kind: decode an answer, or apply the kind's
    /// failure mode (logged at warn) when there is none.
    pub fn decide(
        &self,
        kind: UpcallKind,
        answer: Result<Vec<u8>, UpcallError>,
        decode: impl FnOnce(&[u8]) -> anyhow::Result<bool>,
    ) -> bool {
        let err = match answer {
            Ok(bytes) => match decode(&bytes) {
                Ok(v) => return v,
                Err(e) => UpcallError::Decode(e.to_string()),
            },
            Err(e) => e,
        };
        let policy = self.policy(kind);
        let fallback = policy.fallback();
        tracing::warn!(
            target: UPCALL,
            %kind,
            error = %err,
            fallback,
            "upcall decision unavailable; applying failure mode"
        );
        fallback
    }
}

fn cancel_event(id: u64, reason: CancelReason) -> UpcallEvent {
    UpcallEvent {
        invocation_id: id,
        kind: UpcallKind::Cancel,
        deadline_unix_ms: 0,
        payload: vec![reason as u8],
    }
}

fn reply_result(kind: UpcallKind, (status, payload): Reply) -> Result<Vec<u8>, UpcallError> {
    match status {
        UpcallStatus::Ok => Ok(payload),
        UpcallStatus::Error => Err(UpcallError::Foreign(
            String::from_utf8_lossy(&payload).into_owned(),
        )),
        UpcallStatus::Unimplemented => Err(UpcallError::Unimplemented(kind)),
    }
}

// ── the shared queue delivery ──────────────────────────────────────────────

/// A bounded [`UpcallSink`] the binding drains in batches -- the delivery
/// PyO3 (await [`ready`](Self::ready), then [`drain`](Self::drain)) and the
/// C ABI (poll thread on [`wait_blocking`](Self::wait_blocking)) share.
/// NAPI may use it too, or forward events straight into a
/// `ThreadsafeFunction` from its own sink.
///
/// Unlike the dispatch `RingSink` there is no pump task: `deliver` pushes
/// under a mutex (upcalls are low-rate), so delivery works with no runtime
/// and a blocked [`Upcalls::call_blocking`] cannot starve it. A full queue
/// refuses new upcalls (they fail with [`UpcallError::Overloaded`]) but
/// always accepts `Cancel` events; a `Cancel` whose upcall is still queued
/// withdraws that upcall instead of being queued itself.
pub struct UpcallQueue {
    state: Mutex<QueueState>,
    capacity: usize,
    notify: Notify,
    cvar: Condvar,
}

struct QueueState {
    events: VecDeque<UpcallEvent>,
    closed: bool,
}

impl UpcallQueue {
    /// A queue holding at most `capacity` (>= 1) upcalls at a time.
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QueueState {
                events: VecDeque::new(),
                closed: false,
            }),
            capacity: capacity.max(1),
            notify: Notify::new(),
            cvar: Condvar::new(),
        })
    }

    fn signal(&self) {
        self.notify.notify_waiters();
        self.notify.notify_one();
        self.cvar.notify_all();
    }

    /// Take up to `max` events in delivery order, calling `f` for each.
    /// Returns how many. `f` runs outside the queue lock, so it may call
    /// [`Upcalls::respond`].
    pub fn drain(&self, max: usize, mut f: impl FnMut(UpcallEvent)) -> usize {
        let batch: Vec<UpcallEvent> = {
            let mut st = self.state.lock().unwrap();
            let n = max.min(st.events.len());
            st.events.drain(..n).collect()
        };
        let n = batch.len();
        batch.into_iter().for_each(&mut f);
        n
    }

    /// Events ready to drain.
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stop accepting events and wake every waiter. Queued events can still
    /// be drained.
    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.signal();
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// Resolve once there is something to drain, or the queue is closed.
    pub async fn ready(&self) {
        loop {
            let notified = self.notify.notified();
            {
                let st = self.state.lock().unwrap();
                if !st.events.is_empty() || st.closed {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Block until there is something to drain, the queue is closed, or
    /// `timeout` passes. Returns whether events are available. For poll
    /// threads; never call it on a runtime worker.
    pub fn wait_blocking(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut st = self.state.lock().unwrap();
        loop {
            if !st.events.is_empty() || st.closed {
                return !st.events.is_empty();
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            st = self.cvar.wait_timeout(st, deadline - now).unwrap().0;
        }
    }
}

impl UpcallSink for UpcallQueue {
    fn deliver(&self, event: UpcallEvent) -> bool {
        let mut st = self.state.lock().unwrap();
        if event.kind == UpcallKind::Cancel {
            // Withdraw the upcall if the binding has not taken it yet.
            if let Some(pos) = st.events.iter().position(|e| {
                e.invocation_id == event.invocation_id && e.kind != UpcallKind::Cancel
            }) {
                st.events.remove(pos);
                return true;
            }
            if st.closed {
                return false;
            }
            st.events.push_back(event);
        } else {
            if st.closed {
                return false;
            }
            let queued_upcalls = st
                .events
                .iter()
                .filter(|e| e.kind != UpcallKind::Cancel)
                .count();
            if queued_upcalls >= self.capacity {
                return false;
            }
            st.events.push_back(event);
        }
        drop(st);
        self.signal();
        true
    }
}

impl<F> UpcallSink for F
where
    F: Fn(UpcallEvent) -> bool + Send + Sync + 'static,
{
    fn deliver(&self, event: UpcallEvent) -> bool {
        self(event)
    }
}
