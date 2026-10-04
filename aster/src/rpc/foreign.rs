//! Foreign services: services a language binding implements, served by
//! [`Server`](super::Server) (design `docs/_internal/bindings/dynamic-dispatch-design.md`
//! §2, slice R3c).
//!
//! A binding registers a [`DynamicService`] and a [`DispatchSink`]. The
//! [`Dispatcher`](super::Dispatcher) runs every pre-dispatch step in Rust --
//! header decode, lookup, `iroh_only`, method, serialization mode, the §7.5
//! session rules, Gate 3, the deadline -- and hands the binding a
//! [`ForeignCall`] carrying the decompressed first request. The binding
//! answers through the shared [`CallTable`] (or the [`CallHandle`] in the
//! event) with payloads and statuses; Rust does the framing, zstd and
//! `RpcStatus` encoding.
//!
//! The public surface is re-exported from `aster::binding_support::dispatch`
//! (feature `binding-support`); it is not a stable API.

// Without `binding-support` nothing outside the crate can register a foreign
// service, so the response operations are unreachable in that build.
#![cfg_attr(not(feature = "binding-support"), allow(dead_code))]

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc::Receiver, oneshot};
use tokio::time::Instant;

use aster_transport_core::framing::{
    decompress, encode_frame, encode_payload_frame, FLAG_COMPRESSED, FLAG_END_STREAM, FLAG_TRAILER,
    MAX_FRAME_SIZE,
};
use aster_transport_core::reactor::{
    CancelHook, OutgoingFrame, RequestFrame, ResponsePermit, ResponseReserveError, ResponseSender,
};
use aster_transport_core::CoreConnection;

use super::auth::CapabilityRequirement;
use super::codec::{encode_rpc_status, RpcStatus, SerializationMode, StreamHeader};
use super::contract::{MethodPattern, ScopeKind};
use super::deadline::{DeadlineGuard, DeadlineQueue};
use super::dynamic::DynamicService;
use super::observe::{CallInfo, CallObs};
use super::server::{
    corrupt_frame_status, next_request_frame, reject_now, warn_corrupt_frame, Call, CallParts,
    NextFrame, ServiceDispatch,
};
use super::sessions::{ServiceKey, SessionFactory, SessionInstance};
use super::status::StatusCode;
use crate::log_targets::FOREIGN;

// ── flags ───────────────────────────────────────────────────────────────────

/// [`ForeignCall::flags`]: the first request frame ended the request stream
/// (unary and server-stream calls, and one-message client streams). There is
/// nothing more to [`recv`](CallRef::recv).
pub const FLAG_FIRST_IS_END: u32 = 0x1;
/// [`ForeignCall::flags`]: first call on a new session. The binding creates
/// its per-session instance for [`ForeignCall::session_key`]. Set on exactly
/// one call per session; a binding should still treat any unknown
/// `session_key` as new.
pub const FLAG_NEW_SESSION: u32 = 0x2;
/// [`ForeignCall::flags`]: the peer id was authenticated by the iroh
/// handshake ([`PeerProvenance::IrohAuthenticated`](super::PeerProvenance)).
pub const FLAG_IROH_AUTHENTICATED: u32 = 0x4;

// ── events ──────────────────────────────────────────────────────────────────

/// Why a call was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CancelReason {
    /// The peer sent `FLAG_CANCEL`, or the stream or connection failed under
    /// the call.
    /// No trailer is sent (spec §6.0).
    Peer,
    /// The call's deadline passed. Rust has already sent `DEADLINE_EXCEEDED`.
    Deadline,
    /// A request frame after the first was corrupt (it did not inflate).
    /// Rust has already sent `INVALID_ARGUMENT`; [`CallRef::recv`] returned
    /// `None`.
    InvalidRequest,
}

/// Everything a [`DispatchSink`] receives.
#[non_exhaustive]
pub enum DispatchEvent {
    /// A call that passed every gate. Answer it through
    /// [`ForeignCall::handle`] or the [`CallTable`].
    Call(ForeignCall),
    /// The call ended without the binding finishing it: on peer cancel (or
    /// the stream or connection failing under the call) and on deadline, for
    /// every pattern. Over a transport without a cancel signal (HTTP), peer
    /// cancels are only seen for streaming patterns.
    /// Later response operations return [`SubmitError::NotFound`].
    Cancelled { call_id: u64, reason: CancelReason },
    /// A [`reserve`](CallTable::reserve) completed: retry the exact operation
    /// that returned [`SubmitError::QueueFull`]; it will not be full again.
    ResponseWritable { call_id: u64 },
    /// A pending or held reservation ended because the call did. Stop
    /// producing for it.
    ResponseClosed { call_id: u64 },
    /// A foreign session was reaped (its connection closed, or an HTTP
    /// session ended). Drop the instance for `session_key` and run its close
    /// hooks.
    SessionClosed { session_key: u64 },
    /// An `aster/1` connection closed. Its `SessionClosed` events precede
    /// this one.
    ConnectionClosed {
        connection_id: u64,
        peer_id: Arc<str>,
    },
    /// A connection on a non-RPC ALPN that no Rust handler claimed (e.g. the
    /// frozen consumer-admission ALPNs, D3).
    NonRpc {
        alpn: Vec<u8>,
        connection: CoreConnection,
    },
}

impl fmt::Debug for DispatchEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DispatchEvent::Call(c) => fmt::Debug::fmt(c, f),
            DispatchEvent::Cancelled { call_id, reason } => f
                .debug_struct("Cancelled")
                .field("call_id", call_id)
                .field("reason", reason)
                .finish(),
            DispatchEvent::ResponseWritable { call_id } => f
                .debug_struct("ResponseWritable")
                .field("call_id", call_id)
                .finish(),
            DispatchEvent::ResponseClosed { call_id } => f
                .debug_struct("ResponseClosed")
                .field("call_id", call_id)
                .finish(),
            DispatchEvent::SessionClosed { session_key } => f
                .debug_struct("SessionClosed")
                .field("session_key", session_key)
                .finish(),
            DispatchEvent::ConnectionClosed {
                connection_id,
                peer_id,
            } => f
                .debug_struct("ConnectionClosed")
                .field("connection_id", connection_id)
                .field("peer_id", peer_id)
                .finish(),
            DispatchEvent::NonRpc { alpn, .. } => f
                .debug_struct("NonRpc")
                .field("alpn", &String::from_utf8_lossy(alpn))
                .finish(),
        }
    }
}

/// Receives [`DispatchEvent`]s. Called from the server's serve loop, its
/// deadline timer and per-call tasks, so `deliver` **must not block**: an
/// implementation owns its own queueing ([`RingSink`] is the one the PyO3
/// and C ABI bindings use).
///
/// [`RingSink`]: crate::binding_support::dispatch::RingSink
pub trait DispatchSink: Send + Sync + 'static {
    fn deliver(&self, event: DispatchEvent);
}

/// Discards every event. Used when no sink is configured, so a server
/// without foreign services pays nothing.
pub(crate) struct NullSink;

impl DispatchSink for NullSink {
    fn deliver(&self, _event: DispatchEvent) {}
}

/// The caller's resolved authorization attributes, as Gate 3 saw them: the
/// server's `AttributeStore` entry for the peer merged with the
/// `Authenticator` outcome. An opaque, read-only snapshot (design D3): R4
/// replaces how it is produced, not its shape. Entries are sorted by key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerAttributes {
    entries: Vec<(String, String)>,
}

impl PeerAttributes {
    pub(crate) fn from_map(map: HashMap<String, String>) -> Self {
        let mut entries: Vec<_> = map.into_iter().collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        PeerAttributes { entries }
    }

    /// The value of `key`, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .binary_search_by(|(k, _)| k.as_str().cmp(key))
            .ok()
            .map(|i| self.entries[i].1.as_str())
    }

    /// Every `(key, value)`, sorted by key.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A mutable copy.
    pub fn to_map(&self) -> HashMap<String, String> {
        self.entries.iter().cloned().collect()
    }
}

fn empty_attributes() -> Arc<PeerAttributes> {
    static EMPTY: OnceLock<Arc<PeerAttributes>> = OnceLock::new();
    EMPTY.get_or_init(Default::default).clone()
}

/// One call delivered to the binding.
pub struct ForeignCall {
    /// The [`CallTable`] key for every response operation. Unique per server.
    pub call_id: u64,
    /// The service's registration index (order of `dynamic_service` calls).
    pub service_idx: u32,
    /// The index into [`DynamicService::methods`].
    pub method_idx: u32,
    pub pattern: MethodPattern,
    /// `FLAG_*` bits.
    pub flags: u32,
    /// `0` for SHARED services; otherwise the Rust-assigned key of the
    /// session (per `(connection, sessionId, service)`).
    pub session_key: u64,
    /// The reactor's connection id; `0` for transports without one (HTTP).
    pub connection_id: u64,
    /// `StreamHeader.call_id` (logging / request id).
    pub wire_call_id: i32,
    /// When Rust will cancel the call with `DEADLINE_EXCEEDED`, in Unix
    /// milliseconds; `0` = no deadline.
    pub deadline_unix_ms: u64,
    /// The peer id, or the `Authenticator` principal when one resolved it.
    pub peer_id: Arc<str>,
    /// The first request payload, already decompressed.
    pub request: Vec<u8>,
    /// The decoded header (metadata, deadline, session id).
    pub header: Arc<StreamHeader>,
    pub attributes: Arc<PeerAttributes>,
    /// The connection the call arrived on (tunnels, remote address); `None`
    /// for transports without one.
    pub connection: Option<CoreConnection>,
    /// Owning handle: dropping it unanswered sends `INTERNAL "handler dropped
    /// call"`. Bindings that answer by id call [`CallHandle::detach`].
    pub handle: CallHandle,
}

impl fmt::Debug for ForeignCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Call")
            .field("call_id", &self.call_id)
            .field("service_idx", &self.service_idx)
            .field("method_idx", &self.method_idx)
            .field("pattern", &self.pattern)
            .field("flags", &self.flags)
            .field("session_key", &self.session_key)
            .field("connection_id", &self.connection_id)
            .field("request_len", &self.request.len())
            .finish()
    }
}

// ── registration ────────────────────────────────────────────────────────────

/// A registered [`DynamicService`] as the dispatcher sees it. Implements
/// [`ServiceDispatch`] so the shared gates (Gate 3, `iroh_only`, frame
/// limits, timeouts) consult it exactly as they do a native service; it is
/// never dispatched to natively.
pub(crate) struct ForeignService {
    pub(crate) idx: u32,
    pub(crate) def: DynamicService,
    method_index: HashMap<String, u32>,
}

impl ForeignService {
    pub(crate) fn new(idx: u32, def: DynamicService) -> Self {
        let method_index = def
            .methods
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name.clone(), i as u32))
            .collect();
        ForeignService {
            idx,
            def,
            method_index,
        }
    }

    pub(crate) fn method_index(&self, name: &str) -> Option<u32> {
        self.method_index.get(name).copied()
    }

    pub(crate) fn accepts_wire_mode(&self, mode: i8) -> bool {
        let mode = match mode {
            0 => SerializationMode::Xlang,
            1 => SerializationMode::Native,
            2 => SerializationMode::Row,
            _ => return false,
        };
        self.def.accepts_mode(mode)
    }

    fn method(&self, name: &str) -> Option<&super::dynamic::MethodDesc> {
        self.method_index(name).and_then(|i| self.def.method(i))
    }
}

#[async_trait::async_trait]
impl ServiceDispatch for ForeignService {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn version(&self) -> i32 {
        self.def.version
    }
    fn service_requires(&self) -> Option<CapabilityRequirement> {
        self.def.requires.clone()
    }
    fn method_requires(&self, method: &str) -> Option<CapabilityRequirement> {
        self.method(method).and_then(|m| m.requires.clone())
    }
    fn max_frame_size(&self, method: &str) -> Option<u32> {
        self.method(method).and_then(|m| m.limits.max_frame_size)
    }
    fn iroh_only(&self) -> bool {
        self.def.iroh_only
    }
    fn method_timeout(&self, method: &str) -> Option<Duration> {
        self.method(method).and_then(|m| m.timeout())
    }
    async fn dispatch(&self, _method: &str, call: Call) {
        // Unreachable: foreign targets never reach `ServiceDispatch::dispatch`.
        let _ = call
            .finish(&RpcStatus::error(
                StatusCode::Internal,
                "foreign service dispatched natively",
            ))
            .await;
    }
}

/// The Rust-side record of one foreign session. Its state lives in the
/// binding under `key`; dropping the record (connection reaped, HTTP session
/// ended) tells the binding with [`DispatchEvent::SessionClosed`].
pub(crate) struct ForeignSession {
    key: u64,
    announced: AtomicBool,
    sink: Arc<dyn DispatchSink>,
}

impl ForeignSession {
    pub(crate) fn key(&self) -> u64 {
        self.key
    }

    /// True for exactly one caller: the first call on this session.
    fn announce(&self) -> bool {
        !self.announced.swap(true, Ordering::AcqRel)
    }
}

impl Drop for ForeignSession {
    fn drop(&mut self) {
        self.sink.deliver(DispatchEvent::SessionClosed {
            session_key: self.key,
        });
    }
}

/// Every foreign service of one server, the sink and the shared call table.
/// Built once and shared by every [`Dispatcher`](super::Dispatcher) clone.
pub(crate) struct Foreign {
    services: HashMap<ServiceKey, Arc<ForeignService>>,
    factories: HashMap<ServiceKey, SessionFactory>,
    pub(crate) sink: Arc<dyn DispatchSink>,
    pub(crate) table: CallTable,
    /// Whether a real sink was configured (connection and non-RPC events are
    /// only worth producing then).
    pub(crate) has_sink: bool,
}

impl Default for Foreign {
    fn default() -> Self {
        Foreign::new(Vec::new(), None)
    }
}

impl Foreign {
    /// `specs` are validated services in registration order.
    pub(crate) fn new(specs: Vec<DynamicService>, sink: Option<Arc<dyn DispatchSink>>) -> Self {
        let has_sink = sink.is_some();
        let sink: Arc<dyn DispatchSink> = sink.unwrap_or_else(|| Arc::new(NullSink));
        let next_session = Arc::new(AtomicU64::new(1));
        let mut services = HashMap::new();
        let mut factories = HashMap::new();
        for (idx, def) in specs.into_iter().enumerate() {
            let key = def.key();
            if def.scope == ScopeKind::Session {
                let sink = sink.clone();
                let next = next_session.clone();
                let factory: SessionFactory = Arc::new(move || {
                    SessionInstance::Foreign(Arc::new(ForeignSession {
                        key: next.fetch_add(1, Ordering::Relaxed),
                        announced: AtomicBool::new(false),
                        sink: sink.clone(),
                    }))
                });
                factories.insert(key.clone(), factory);
            }
            services.insert(key, Arc::new(ForeignService::new(idx as u32, def)));
        }
        Foreign {
            services,
            factories,
            table: CallTable::new(sink.clone()),
            sink,
            has_sink,
        }
    }

    pub(crate) fn service(&self, key: &ServiceKey) -> Option<&Arc<ForeignService>> {
        self.services.get(key)
    }

    pub(crate) fn factory(&self, key: &ServiceKey) -> Option<&SessionFactory> {
        self.factories.get(key)
    }

    /// Hand one routed call to the binding: inflate the first frame, enter
    /// the call in the table, deliver, then arm its deadline and its cancel
    /// hook. Synchronous: runs inline in the serve loop, so the foreign fast
    /// path spawns nothing per call.
    pub(crate) fn deliver(&self, d: Delivery, deadlines: &DeadlineQueue) {
        let Delivery {
            svc,
            method_idx,
            session,
            header,
            attributes,
            deadline,
            parts,
            obs,
        } = d;
        let CallParts {
            peer_id,
            provenance,
            mut request_payload,
            request_flags,
            response_sender,
            request_receiver,
            cancelled,
            connection_id,
            connection,
            cancel_notify,
            ..
        } = parts;

        // §5.6: inflate after the gates, so an unauthorised caller costs no
        // decompression.
        if request_flags & FLAG_COMPRESSED != 0 {
            match decompress(&request_payload) {
                Ok(p) => request_payload = p,
                Err(e) => {
                    let status = corrupt_frame_status(&e);
                    warn_corrupt_frame(&header, &peer_id, 0, &e);
                    let mut obs = obs;
                    obs.finish(
                        StatusCode::InvalidArgument,
                        CallInfo {
                            service: &header.service,
                            method: &header.method,
                            call_id: header.call_id,
                            peer: &peer_id,
                        },
                    );
                    reject_now(response_sender, &status);
                    return;
                }
            }
        }

        let pattern = svc
            .def
            .method(method_idx)
            .map_or(MethodPattern::Unary, |m| m.pattern);
        let first_is_end = request_flags & FLAG_END_STREAM != 0;
        let mut flags = 0;
        if first_is_end {
            flags |= FLAG_FIRST_IS_END;
        }
        if provenance.is_authenticated() {
            flags |= FLAG_IROH_AUTHENTICATED;
        }
        let session_key = match &session {
            Some(s) => {
                if s.announce() {
                    flags |= FLAG_NEW_SESSION;
                }
                s.key()
            }
            None => 0,
        };

        let peer_id: Arc<str> = Arc::from(peer_id);
        let header = Arc::new(header);
        let attributes = if attributes.is_empty() {
            empty_attributes()
        } else {
            Arc::new(PeerAttributes::from_map(attributes))
        };
        let state = self.table.insert(|call_id, table| CallState {
            call_id,
            sender: response_sender,
            cancelled,
            first_is_end,
            requests: tokio::sync::Mutex::new((!first_is_end).then_some(request_receiver)),
            frames_read: AtomicU64::new(0),
            inner: Mutex::new(CallInner {
                obs: Some(obs),
                ..CallInner::default()
            }),
            header: header.clone(),
            peer: peer_id.clone(),
            attributes: attributes.clone(),
            table,
        });
        let call_id = state.call_id;
        let deadline_unix_ms = deadline.map_or(0, unix_ms);
        // Kept to arm the deadline and the cancel watcher only after the call
        // is delivered, so a `Cancelled` can never overtake its `Call`.
        let armed = state.clone();

        tracing::trace!(
            target: FOREIGN,
            call_id,
            service = header.service.as_str(),
            method = header.method.as_str(),
            service_idx = svc.idx,
            method_idx,
            session_key,
            peer = &*peer_id,
            "foreign call delivered"
        );
        self.sink.deliver(DispatchEvent::Call(ForeignCall {
            call_id,
            service_idx: svc.idx,
            method_idx,
            pattern,
            flags,
            session_key,
            connection_id: connection_id.unwrap_or(0),
            wire_call_id: header.call_id,
            deadline_unix_ms,
            peer_id,
            request: request_payload,
            header,
            attributes,
            connection,
            handle: CallHandle {
                call: Some(CallRef(state)),
            },
        }));

        if let Some(at) = deadline {
            let mut inner = armed.inner.lock().unwrap();
            // A call the binding already finished needs no timer.
            if !inner.terminal {
                let weak = Arc::downgrade(&armed);
                inner.deadline = Some(deadlines.register(at, move || {
                    if let Some(state) = weak.upgrade() {
                        state.expire();
                    }
                }));
            }
        }
        match cancel_notify {
            // The reactor fires the hook on the task that saw the cancel, so
            // every pattern -- unary included -- is covered without a task
            // per call. A call already over ignores it (`end` is a no-op).
            Some(signal) => {
                let hook: Weak<dyn CancelHook> = Arc::downgrade(&armed) as Weak<CallState>;
                signal.on_cancel(hook);
            }
            // A transport without a signal (HTTP): watch the response channel
            // for streaming calls, where the handler outlives the request.
            None => {
                if pattern != MethodPattern::Unary && !armed.inner.lock().unwrap().terminal {
                    self.table.runtime().spawn(watch_closed(armed));
                }
            }
        }
    }
}

/// Inputs to [`Foreign::deliver`], assembled by the dispatcher.
pub(crate) struct Delivery {
    pub(crate) svc: Arc<ForeignService>,
    pub(crate) method_idx: u32,
    pub(crate) session: Option<Arc<ForeignSession>>,
    pub(crate) header: StreamHeader,
    pub(crate) attributes: HashMap<String, String>,
    pub(crate) deadline: Option<Instant>,
    pub(crate) parts: CallParts,
    pub(crate) obs: CallObs,
}

fn unix_ms(at: Instant) -> u64 {
    let remaining = at.saturating_duration_since(Instant::now());
    (SystemTime::now() + remaining)
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
        .max(1)
}

/// Streaming calls on a transport without a cancel signal: turn the response
/// channel closing under an unfinished call into [`DispatchEvent::Cancelled`].
async fn watch_closed(state: Arc<CallState>) {
    state.sender.closed().await;
    // A finished call also closes the channel (the writer drops it after the
    // trailer); `end` ignores calls that are already terminal.
    state.end(Some(CancelReason::Peer), None);
}

/// A peer cancel, a failed stream or a lost connection (the reactor's cancel
/// signal) ends the call with [`DispatchEvent::Cancelled`] `{Peer}`.
impl CancelHook for CallState {
    fn cancelled(&self) {
        self.end(Some(CancelReason::Peer), None);
    }
}

// ── call table ──────────────────────────────────────────────────────────────

/// Why a response operation did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubmitError {
    /// The response queue is full. Nothing was sent; retry the exact same
    /// operation, or [`reserve`](CallTable::reserve) `required_len` bytes
    /// and retry on [`DispatchEvent::ResponseWritable`]. zstd is
    /// deterministic, so the retry needs exactly `required_len` again.
    QueueFull { required_len: usize },
    /// The call is over (peer cancel, connection gone). It has been removed
    /// from the table.
    Closed,
    /// No such call: never existed, already finished, or cancelled.
    NotFound,
    /// The encoded frame exceeds the frame or per-call byte limit.
    TooLarge,
    /// The request is malformed: an empty stream payload (a data frame cannot
    /// carry none), or a second reservation while one is pending.
    Invalid,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubmitError::QueueFull { required_len } => {
                write!(f, "response queue full ({required_len} bytes required)")
            }
            SubmitError::Closed => f.write_str("call closed"),
            SubmitError::NotFound => f.write_str("no such call"),
            SubmitError::TooLarge => f.write_str("response too large"),
            SubmitError::Invalid => f.write_str("invalid response operation"),
        }
    }
}

impl std::error::Error for SubmitError {}

const SHARDS: usize = 16;

/// The live foreign calls of one server, keyed by `call_id`. Cheap to
/// clone; every clone is the same table. Shared by all three bindings (it
/// replaces the per-call maps of `ffi/src/reactor.rs` and the per-call
/// pyclasses / napi classes).
#[derive(Clone)]
pub struct CallTable {
    inner: Arc<TableInner>,
}

struct TableInner {
    shards: [Mutex<HashMap<u64, Arc<CallState>>>; SHARDS],
    next_id: AtomicU64,
    sink: Arc<dyn DispatchSink>,
    /// Captured on the first insert (always on the runtime), so operations
    /// called from a binding's own threads can still spawn.
    runtime: OnceLock<tokio::runtime::Handle>,
}

impl TableInner {
    fn shard(&self, id: u64) -> &Mutex<HashMap<u64, Arc<CallState>>> {
        &self.shards[(id as usize) % SHARDS]
    }

    fn remove(&self, id: u64) {
        let removed = self.shard(id).lock().unwrap().remove(&id);
        drop(removed);
    }
}

impl CallTable {
    pub(crate) fn new(sink: Arc<dyn DispatchSink>) -> Self {
        CallTable {
            inner: Arc::new(TableInner {
                shards: std::array::from_fn(|_| Mutex::new(HashMap::new())),
                next_id: AtomicU64::new(1),
                sink,
                runtime: OnceLock::new(),
            }),
        }
    }

    fn insert(&self, make: impl FnOnce(u64, Weak<TableInner>) -> CallState) -> Arc<CallState> {
        self.inner
            .runtime
            .get_or_init(tokio::runtime::Handle::current);
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(make(id, Arc::downgrade(&self.inner)));
        self.inner
            .shard(id)
            .lock()
            .unwrap()
            .insert(id, state.clone());
        state
    }

    fn runtime(&self) -> tokio::runtime::Handle {
        self.inner
            .runtime
            .get_or_init(tokio::runtime::Handle::current)
            .clone()
    }

    /// Number of live calls.
    pub fn len(&self) -> usize {
        self.inner
            .shards
            .iter()
            .map(|s| s.lock().unwrap().len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A non-owning reference to call `call_id`, for repeated operations
    /// without a table lookup each time.
    pub fn get(&self, call_id: u64) -> Option<CallRef> {
        self.inner
            .shard(call_id)
            .lock()
            .unwrap()
            .get(&call_id)
            .cloned()
            .map(CallRef)
    }

    fn with<T>(
        &self,
        call_id: u64,
        f: impl FnOnce(&CallRef) -> Result<T, SubmitError>,
    ) -> Result<T, SubmitError> {
        match self.get(call_id) {
            Some(call) => f(&call),
            None => Err(SubmitError::NotFound),
        }
    }

    /// See [`CallRef::try_respond`].
    pub fn try_respond(
        &self,
        call_id: u64,
        payload: &[u8],
        status: &RpcStatus,
    ) -> Result<(), SubmitError> {
        self.with(call_id, |c| c.try_respond(payload, status))
    }

    /// See [`CallRef::try_send`].
    pub fn try_send(&self, call_id: u64, payload: &[u8]) -> Result<(), SubmitError> {
        self.with(call_id, |c| c.try_send(payload))
    }

    /// See [`CallRef::try_finish`].
    pub fn try_finish(&self, call_id: u64, status: &RpcStatus) -> Result<(), SubmitError> {
        self.with(call_id, |c| c.try_finish(status))
    }

    /// See [`CallRef::reserve`].
    pub fn reserve(&self, call_id: u64, required_len: usize) -> Result<(), SubmitError> {
        self.with(call_id, |c| c.reserve(required_len))
    }

    /// See [`CallRef::cancel_reservation`].
    pub fn cancel_reservation(&self, call_id: u64) -> Result<(), SubmitError> {
        self.with(call_id, |c| c.cancel_reservation())
    }

    /// See [`CallRef::recv`]. `Err(NotFound)` for an unknown call.
    pub async fn recv(&self, call_id: u64) -> Result<Option<Vec<u8>>, SubmitError> {
        match self.get(call_id) {
            Some(c) => Ok(c.recv().await),
            None => Err(SubmitError::NotFound),
        }
    }

    /// Whether the call was cancelled. An unknown call reads as cancelled:
    /// there is nothing left to produce for it.
    pub fn is_cancelled(&self, call_id: u64) -> bool {
        self.get(call_id).is_none_or(|c| c.is_cancelled())
    }

    /// See [`CallRef::abandon`].
    pub fn abandon(&self, call_id: u64) -> Result<(), SubmitError> {
        self.with(call_id, |c| {
            c.abandon();
            Ok(())
        })
    }
}

/// Per-call state. Owned by the table until the call ends; [`CallRef`]s
/// keep it alive past that, answering `NotFound`.
pub(crate) struct CallState {
    call_id: u64,
    sender: ResponseSender,
    cancelled: Arc<AtomicBool>,
    first_is_end: bool,
    requests: tokio::sync::Mutex<Option<Receiver<RequestFrame>>>,
    /// Request frames read after the first (for the corrupt-frame log).
    frames_read: AtomicU64,
    inner: Mutex<CallInner>,
    /// The call's header and peer (shared with the [`ForeignCall`]), for logs.
    header: Arc<StreamHeader>,
    peer: Arc<str>,
    /// The caller's attributes (shared with the [`ForeignCall`]), so bindings
    /// that answer by id can read them later ([`CallRef::attributes`]).
    attributes: Arc<PeerAttributes>,
    table: Weak<TableInner>,
}

#[derive(Default)]
struct CallInner {
    /// Set once a terminal frame was queued or the call otherwise ended.
    /// Every operation after that is `NotFound`.
    terminal: bool,
    /// The call's telemetry; taken when it turns terminal.
    obs: Option<CallObs>,
    /// At most one capacity reservation (single producer per call).
    reservation: Option<Reservation>,
    deadline: Option<DeadlineGuard>,
    next_token: u64,
}

enum Reservation {
    Pending {
        token: u64,
        cancel: oneshot::Sender<()>,
    },
    Ready {
        permit: ResponsePermit,
    },
}

impl CallState {
    fn sink(&self) -> Option<Arc<dyn DispatchSink>> {
        self.table.upgrade().map(|t| t.sink.clone())
    }

    fn deliver(&self, event: DispatchEvent) {
        if let Some(sink) = self.sink() {
            sink.deliver(event);
        }
    }

    fn remove(&self) {
        if let Some(table) = self.table.upgrade() {
            table.remove(self.call_id);
        }
    }

    /// Record the call's final status (telemetry; the first one wins).
    fn observe_end(&self, obs: Option<CallObs>, code: StatusCode) {
        if let Some(mut obs) = obs {
            obs.finish(
                code,
                CallInfo {
                    service: &self.header.service,
                    method: &self.header.method,
                    call_id: self.header.call_id,
                    peer: &self.peer,
                },
            );
        }
    }

    /// Queue one frame under the call's single-producer reservation rules.
    /// `terminal` is the status of a terminal frame, `None` for a data frame.
    fn submit(
        &self,
        frame: OutgoingFrame,
        terminal: Option<StatusCode>,
    ) -> Result<(), SubmitError> {
        let len = frame.encoded_len();
        let mut inner = self.inner.lock().unwrap();
        if inner.terminal {
            return Err(SubmitError::NotFound);
        }
        let held = match inner.reservation.take() {
            Some(pending @ Reservation::Pending { .. }) => {
                // A reservation is in flight: keep it, and keep this producer
                // waiting for its `ResponseWritable`.
                inner.reservation = Some(pending);
                return Err(SubmitError::QueueFull { required_len: len });
            }
            Some(Reservation::Ready { permit }) if permit.encoded_len() == len => Some(permit),
            // A held permit for a different size: release it and try afresh.
            Some(Reservation::Ready { .. }) | None => None,
        };
        let permit = match held {
            Some(permit) => permit,
            None => match self.sender.try_reserve(len) {
                Ok(permit) => permit,
                Err(ResponseReserveError::Full) => {
                    return Err(SubmitError::QueueFull { required_len: len })
                }
                Err(ResponseReserveError::TooLarge) => return Err(SubmitError::TooLarge),
                Err(ResponseReserveError::Closed) => {
                    drop(inner);
                    self.end(None, None);
                    return Err(SubmitError::Closed);
                }
            },
        };
        if permit.send(frame).is_err() {
            return Err(SubmitError::Invalid);
        }
        if let Some(code) = terminal {
            inner.terminal = true;
            let guard = inner.deadline.take();
            let obs = inner.obs.take();
            drop(inner);
            drop(guard);
            self.remove();
            self.observe_end(obs, code);
        }
        Ok(())
    }

    /// End an unfinished call: mark it terminal, wake a parked reservation,
    /// optionally queue `trailer`, drop it from the table, and report
    /// `cancel`. A no-op for a call that already ended; returns whether this
    /// ended it.
    fn end(&self, cancel: Option<CancelReason>, trailer: Option<&RpcStatus>) -> bool {
        let (reservation, guard, obs) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.terminal {
                return false;
            }
            inner.terminal = true;
            (
                inner.reservation.take(),
                inner.deadline.take(),
                inner.obs.take(),
            )
        };
        drop(guard);
        if cancel.is_some() {
            self.cancelled.store(true, Ordering::Release);
        }
        if let Some(reason) = cancel {
            tracing::debug!(target: FOREIGN, call_id = self.call_id, ?reason, "foreign call cancelled");
        }
        let code = match (trailer, cancel) {
            (Some(status), _) => status.status_code(),
            (None, Some(CancelReason::Deadline)) => StatusCode::DeadlineExceeded,
            (None, Some(CancelReason::InvalidRequest)) => StatusCode::InvalidArgument,
            // A peer cancel, or the response channel closed under the call.
            (None, _) => StatusCode::Cancelled,
        };
        self.observe_end(obs, code);
        let had_reservation = match reservation {
            Some(Reservation::Pending { cancel, .. }) => {
                let _ = cancel.send(());
                true
            }
            // Released before the trailer so its capacity is available to it.
            Some(Reservation::Ready { permit }) => {
                drop(permit);
                true
            }
            None => false,
        };
        if let Some(status) = trailer {
            self.queue_trailer(status);
        }
        self.remove();
        if had_reservation {
            self.deliver(DispatchEvent::ResponseClosed {
                call_id: self.call_id,
            });
        }
        if let Some(reason) = cancel {
            self.deliver(DispatchEvent::Cancelled {
                call_id: self.call_id,
                reason,
            });
        }
        true
    }

    /// Best-effort trailer outside the producer's control: immediate when
    /// there is room, else from a task once there is.
    fn queue_trailer(&self, status: &RpcStatus) {
        let Ok(frame) = trailer_frame(status) else {
            return;
        };
        match self.sender.try_reserve(frame.encoded_len()) {
            Ok(permit) => {
                let _ = permit.send(frame);
            }
            Err(ResponseReserveError::Full) => {
                if let Some(rt) = self.table.upgrade().and_then(|t| t.runtime.get().cloned()) {
                    let sender = self.sender.clone();
                    rt.spawn(async move {
                        let _ = sender.send(frame).await;
                    });
                }
            }
            Err(_) => {}
        }
    }

    /// The deadline passed (called from the server's deadline timer).
    fn expire(&self) {
        self.end(
            Some(CancelReason::Deadline),
            Some(&RpcStatus::error(
                StatusCode::DeadlineExceeded,
                "deadline exceeded",
            )),
        );
    }

    /// Enter a pending reservation for `len` bytes. Synchronous, so a submit
    /// racing the caller already sees it.
    fn begin_reserve(&self, len: usize) -> Result<(u64, oneshot::Receiver<()>), SubmitError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.terminal {
            return Err(SubmitError::NotFound);
        }
        if len > self.sender.byte_limit() {
            return Err(SubmitError::TooLarge);
        }
        if inner.reservation.is_some() {
            return Err(SubmitError::Invalid);
        }
        inner.next_token += 1;
        let token = inner.next_token;
        let (cancel, cancel_rx) = oneshot::channel();
        inner.reservation = Some(Reservation::Pending { token, cancel });
        Ok((token, cancel_rx))
    }

    /// Wait for the capacity of reservation `token` and hold it. Returns
    /// whether it is now held.
    async fn finish_reserve(
        &self,
        token: u64,
        cancel_rx: oneshot::Receiver<()>,
        len: usize,
    ) -> bool {
        let permit = tokio::select! {
            permit = self.sender.reserve(len) => permit.ok(),
            _ = cancel_rx => return false,
        };
        let mut inner = self.inner.lock().unwrap();
        let current = matches!(
            inner.reservation,
            Some(Reservation::Pending { token: t, .. }) if t == token
        );
        if !current {
            return false;
        }
        match permit {
            Some(permit) => {
                inner.reservation = Some(Reservation::Ready { permit });
                true
            }
            None => {
                // The channel closed under the wait: the call is over. `end`
                // reports the pending reservation as `ResponseClosed`.
                drop(inner);
                self.end(None, None);
                false
            }
        }
    }
}

/// A non-owning reference to one foreign call. Cheap to clone. Every
/// operation is non-blocking except [`recv`](CallRef::recv) and
/// [`wait_writable`](CallRef::wait_writable).
#[derive(Clone)]
pub struct CallRef(Arc<CallState>);

impl CallRef {
    pub fn call_id(&self) -> u64 {
        self.0.call_id
    }

    /// The call's decoded header (the same as [`ForeignCall::header`]), for
    /// bindings that answer by id and read metadata lazily.
    pub fn header(&self) -> &Arc<StreamHeader> {
        &self.0.header
    }

    /// The caller's peer id (the same as [`ForeignCall::peer_id`]).
    pub fn peer(&self) -> &Arc<str> {
        &self.0.peer
    }

    /// The caller's attributes (the same as [`ForeignCall::attributes`]).
    pub fn attributes(&self) -> &Arc<PeerAttributes> {
        &self.0.attributes
    }

    /// Single-shot terminal response: one data frame (omitted when `payload`
    /// is empty) plus the trailer, written together (the unary fast path).
    pub fn try_respond(&self, payload: &[u8], status: &RpcStatus) -> Result<(), SubmitError> {
        let trailer = encode_status_frame(status)?;
        let buf = if payload.is_empty() {
            trailer
        } else {
            let mut b = encode_data_frame(payload)?;
            b.extend_from_slice(&trailer);
            b
        };
        self.0.submit(
            OutgoingFrame::CompleteUnary(buf),
            Some(status.status_code()),
        )
    }

    /// One stream data frame (server-stream / bidi). `payload` must not be
    /// empty.
    pub fn try_send(&self, payload: &[u8]) -> Result<(), SubmitError> {
        if payload.is_empty() {
            return Err(SubmitError::Invalid);
        }
        let frame = encode_data_frame(payload)?;
        self.0.submit(OutgoingFrame::Frame(frame), None)
    }

    /// The terminal trailer.
    pub fn try_finish(&self, status: &RpcStatus) -> Result<(), SubmitError> {
        let frame = encode_status_frame(status)?;
        self.0
            .submit(OutgoingFrame::Trailer(frame), Some(status.status_code()))
    }

    /// After [`SubmitError::QueueFull`]: wait in the background for
    /// `required_len` bytes, then deliver [`DispatchEvent::ResponseWritable`]
    /// (or [`ResponseClosed`](DispatchEvent::ResponseClosed) if the call ends
    /// first). The next submit of exactly that size consumes the held
    /// capacity. One reservation per call.
    pub fn reserve(&self, required_len: usize) -> Result<(), SubmitError> {
        let rt = self
            .0
            .table
            .upgrade()
            .and_then(|t| t.runtime.get().cloned())
            .ok_or(SubmitError::NotFound)?;
        let (token, cancel_rx) = self.0.begin_reserve(required_len)?;
        let state = self.0.clone();
        rt.spawn(async move {
            if state.finish_reserve(token, cancel_rx, required_len).await {
                state.deliver(DispatchEvent::ResponseWritable {
                    call_id: state.call_id,
                });
            }
        });
        Ok(())
    }

    /// As [`reserve`](CallRef::reserve), but awaited: resolves `Ok(true)`
    /// once the capacity is held (no event is delivered), `Ok(false)` if the
    /// call ended or the reservation was cancelled.
    pub async fn wait_writable(&self, required_len: usize) -> Result<bool, SubmitError> {
        let (token, cancel_rx) = self.0.begin_reserve(required_len)?;
        Ok(self.0.finish_reserve(token, cancel_rx, required_len).await)
    }

    /// Drop a pending or held reservation (e.g. the producer was
    /// interrupted). `NotFound` when there is none.
    pub fn cancel_reservation(&self) -> Result<(), SubmitError> {
        let reservation = self.0.inner.lock().unwrap().reservation.take();
        match reservation {
            Some(Reservation::Pending { cancel, .. }) => {
                let _ = cancel.send(());
                Ok(())
            }
            Some(Reservation::Ready { .. }) => Ok(()),
            None => Err(SubmitError::NotFound),
        }
    }

    /// The next request payload after the first (client-stream / bidi),
    /// decompressed; `None` at the end of the request stream. Single
    /// consumer.
    ///
    /// A frame that does not inflate fails the call: Rust sends
    /// `INVALID_ARGUMENT`, logs a warning, delivers
    /// [`DispatchEvent::Cancelled`] with [`CancelReason::InvalidRequest`],
    /// and this returns `None`.
    pub async fn recv(&self) -> Option<Vec<u8>> {
        if self.0.first_is_end {
            return None;
        }
        let mut rx = self.0.requests.lock().await;
        let next = next_request_frame(rx.as_mut()?).await;
        let frame = self.0.frames_read.fetch_add(1, Ordering::Relaxed) + 1;
        match next {
            NextFrame::Frame(p) => Some(p),
            NextFrame::End => None,
            NextFrame::Corrupt(err) => {
                drop(rx);
                warn_corrupt_frame(&self.0.header, &self.0.peer, frame, &err);
                self.0.end(
                    Some(CancelReason::InvalidRequest),
                    Some(&corrupt_frame_status(&err)),
                );
                None
            }
        }
    }

    /// Whether the peer cancelled, the stream failed, or the deadline passed.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Whether the call has ended (answered, cancelled, or abandoned).
    pub fn is_finished(&self) -> bool {
        self.0.inner.lock().unwrap().terminal
    }

    /// End the call with `INTERNAL "handler dropped call"` unless it already
    /// ended. What dropping a [`CallHandle`] does.
    pub fn abandon(&self) {
        let ended = self.0.end(
            None,
            Some(&RpcStatus::error(
                StatusCode::Internal,
                "handler dropped call",
            )),
        );
        if ended {
            tracing::debug!(
                target: FOREIGN,
                call_id = self.0.call_id,
                "foreign call abandoned by its handler"
            );
        }
    }
}

/// The owning handle delivered in [`ForeignCall::handle`]. Dereferences to
/// [`CallRef`]. Dropping it while the call is unfinished abandons the call
/// (`INTERNAL "handler dropped call"`), so a binding that loses a call --
/// a dropped event, a handler object collected -- never leaves the peer
/// waiting or the table holding it.
pub struct CallHandle {
    call: Option<CallRef>,
}

impl CallHandle {
    /// Give up ownership without abandoning: the call stays in the
    /// [`CallTable`] until a terminal operation, cancellation or deadline.
    /// For bindings that answer by `call_id`.
    pub fn detach(mut self) -> u64 {
        self.call.take().map_or(0, |c| c.call_id())
    }

    /// A non-owning reference (dropping it does nothing).
    pub fn to_ref(&self) -> CallRef {
        self.call.clone().expect("live handle")
    }
}

impl std::ops::Deref for CallHandle {
    type Target = CallRef;
    fn deref(&self) -> &CallRef {
        self.call.as_ref().expect("live handle")
    }
}

impl Drop for CallHandle {
    fn drop(&mut self) {
        if let Some(call) = self.call.take() {
            call.abandon();
        }
    }
}

fn encode_data_frame(payload: &[u8]) -> Result<Vec<u8>, SubmitError> {
    encode_payload_frame(payload, 0).map_err(|_| too_large_or_invalid(payload.len()))
}

fn encode_status_frame(status: &RpcStatus) -> Result<Vec<u8>, SubmitError> {
    let bytes = encode_rpc_status(status).map_err(|_| SubmitError::Invalid)?;
    encode_frame(&bytes, FLAG_TRAILER).map_err(|_| too_large_or_invalid(bytes.len()))
}

fn trailer_frame(status: &RpcStatus) -> Result<OutgoingFrame, SubmitError> {
    encode_status_frame(status).map(OutgoingFrame::Trailer)
}

fn too_large_or_invalid(len: usize) -> SubmitError {
    if len >= MAX_FRAME_SIZE as usize {
        SubmitError::TooLarge
    } else {
        SubmitError::Invalid
    }
}
