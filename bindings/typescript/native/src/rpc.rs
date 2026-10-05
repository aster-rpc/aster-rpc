//! `DispatchServer` — JS-implemented services served by
//! `aster::rpc::Server` (design `docs/_internal/bindings/dynamic-dispatch-design.md`
//! §4.3; plan T4).
//!
//! JS registers [`DynamicService`]s in their JSON form (design §1.5). Rust
//! runs every gate (routing, sessions, Gate 3, deadlines, frame limits) and
//! delivers one event per call; JS answers **by call id** through the shared
//! [`CallTable`] (`respond` / `send` / `finish` / `recv` / `waitWritable`),
//! passing payloads and status, never frames.
//!
//! Two ways to start one:
//! - `DispatchServer.serve(node, …)` serves on an existing [`Node`] (started
//!   with `aster/1` among its ALPNs). Gate 0 stays with the node's gate. This
//!   is what `AsterServer` and `RpcServer` use.
//! - `DispatchServer.start(config, …)` starts its own node (`AsterServer`).
//!   Used by the T4.0 delivery benchmark.
//!
//! Event delivery (`delivery`, decided by T4.0,
//! `docs/_internal/bindings/t4-dispatch-benchmark.md`):
//!
//! | `delivery` | How events reach JS |
//! |---|---|
//! | `"tsfn-batched"` (default) | Rust queues events and schedules one TSFN call while none is pending, so events that arrive while JS is busy are coalesced into one batch |
//! | `"tsfn"` | one `napi_call_threadsafe_function` per event |
//! | `"ring"` | `RingSink` (SPSC ring + pump); JS pulls with `await nextBatch()` or sync `pollBatch()` |
//!
//! Call handles are detached (`CallHandle::detach`) as the event is handed
//! to JS: the call then lives in the table until a terminal operation,
//! cancellation, or deadline.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

use aster::binding_support::dispatch::{
    CallTable, CancelReason, DispatchEvent, DispatchSink, RingConsumer, RingSink, SubmitError,
};
use aster::rpc::{
    AsterServer, AttributeStore, AuthContext, AuthOutcome, Authenticator, DynamicService,
    RpcStatus, Server, ServerHandle,
};
use aster_transport_core::CoreConnection;

use crate::admission::GatePolicy;
use crate::config::{aster_err, JsAsterConfig};
use crate::net::{IrohConnection, TransportSnapshot};
use crate::node::Node;

// ── events ──────────────────────────────────────────────────────────────────

/// `DispatchEvent.kind`: a call that passed every gate. Answer it by `callId`.
pub const EVENT_CALL: u32 = 1;
/// `DispatchEvent.kind`: the call ended without the handler finishing it
/// (`reason`: 1 = peer cancel / stream failure, 2 = deadline, 3 = corrupt
/// request frame).
pub const EVENT_CANCELLED: u32 = 2;
/// `DispatchEvent.kind`: reserved capacity (event-driven reserve) is ready.
pub const EVENT_RESPONSE_WRITABLE: u32 = 3;
/// `DispatchEvent.kind`: a reservation ended because the call did.
pub const EVENT_RESPONSE_CLOSED: u32 = 4;
/// `DispatchEvent.kind`: a session was reaped (`sessionKey`).
pub const EVENT_SESSION_CLOSED: u32 = 5;
/// `DispatchEvent.kind`: an `aster/1` connection closed (`connectionId`, `peer`).
pub const EVENT_CONNECTION_CLOSED: u32 = 6;
/// `DispatchEvent.kind`: a connection on a non-RPC ALPN (`alpn`) no Rust
/// handler claimed (the frozen admission ALPNs). Take it with
/// `takeConnection(connectionId)`; an untaken one is closed when the server
/// closes.
pub const EVENT_NON_RPC: u32 = 7;

/// `flags` bit (on top of the `FLAG_*` call bits): the event carries
/// `metadata`.
pub const FLAG_HAS_METADATA: u32 = 0x100;
/// `flags` bit: the event carries `attributes`.
pub const FLAG_HAS_ATTRIBUTES: u32 = 0x200;
/// `flags` bit: the event carries `traceparent` (the call's server span is
/// exported; design §5.2).
pub const FLAG_HAS_TRACE: u32 = 0x400;
/// `flags` bit: the event carries `baggage` and/or `tracestate` (the caller's
/// W3C headers, hidden from `metadata`), whether or not the span is exported.
pub const FLAG_HAS_INBOUND_TRACE: u32 = 0x800;

/// One dispatch event, flattened for JS. Fields that do not apply to the
/// event's `kind` are `0` (numbers) or absent.
#[napi(object, object_from_js = false, js_name = "DispatchEvent")]
pub struct JsDispatchEvent {
    /// 1 call, 2 cancelled, 3 response-writable, 4 response-closed,
    /// 5 session-closed, 6 connection-closed, 7 non-RPC connection.
    pub kind: u32,
    /// Correlation key for every response operation (per server, from 1).
    pub call_id: i64,
    /// Index of the service in the `services` array passed to `start`.
    pub service_idx: u32,
    /// Index of the method in the service's `methods`.
    pub method_idx: u32,
    /// `FIRST_IS_END` (1) | `NEW_SESSION` (2) | `IROH_AUTHENTICATED` (4) |
    /// `HAS_METADATA` (0x100) | `HAS_ATTRIBUTES` (0x200) | `HAS_TRACE` (0x400) |
    /// `HAS_INBOUND_TRACE` (0x800).
    pub flags: u32,
    /// 0 = shared service; otherwise the Rust-assigned session key.
    pub session_key: i64,
    /// Connection id (0 without one, e.g. HTTP). For `kind` 7, the handle
    /// for `takeConnection`.
    pub connection_id: i64,
    /// Absolute deadline, Unix ms; 0 = none.
    pub deadline_ms: i64,
    /// For `cancelled`: 1 peer, 2 deadline, 3 corrupt request frame.
    pub reason: u32,
    /// `StreamHeader.call_id` (request id for logs); calls only.
    pub wire_call_id: i32,
    /// `StreamHeader.session_id` (the client's session id); calls only.
    pub session_id: i32,
    /// Peer endpoint id (or authenticator principal); calls and
    /// connection-closed only.
    pub peer: Option<String>,
    /// The first request payload, already decompressed; calls only.
    pub request: Option<Buffer>,
    /// Call metadata (header), when non-empty.
    pub metadata: Option<HashMap<String, String>>,
    /// The caller's Gate-3 attributes as Rust saw them, when non-empty.
    pub attributes: Option<HashMap<String, String>>,
    /// The ALPN of a non-RPC connection (`kind` 7).
    pub alpn: Option<String>,
    /// W3C `traceparent` of the call's server span, when it is exported
    /// (`flags & HAS_TRACE`). Binding spans and logs parent on it.
    pub traceparent: Option<String>,
    /// The caller's raw W3C `baggage` header (within the 8192-byte / 64-member
    /// limits), when sent (`flags & HAS_INBOUND_TRACE`). Present whether or
    /// not the call's span is exported; it is not in `metadata`.
    pub baggage: Option<String>,
    /// The caller's raw W3C `tracestate` header, when sent.
    pub tracestate: Option<String>,
}

/// An event with every JS-irrelevant part dropped (call handle detached).
/// Built off the JS thread; turned into a [`JsDispatchEvent`] on it.
struct Ev {
    kind: u32,
    call_id: u64,
    service_idx: u32,
    method_idx: u32,
    flags: u32,
    session_key: u64,
    connection_id: u64,
    deadline_ms: u64,
    reason: u32,
    wire_call_id: i32,
    session_id: i32,
    peer: Option<Arc<str>>,
    request: Option<Vec<u8>>,
    metadata: Option<Vec<(String, String)>>,
    attributes: Option<Vec<(String, String)>>,
    alpn: Option<String>,
    traceparent: Option<String>,
    baggage: Option<String>,
    tracestate: Option<String>,
}

impl Ev {
    fn simple(kind: u32) -> Ev {
        Ev {
            kind,
            call_id: 0,
            service_idx: 0,
            method_idx: 0,
            flags: 0,
            session_key: 0,
            connection_id: 0,
            deadline_ms: 0,
            reason: 0,
            wire_call_id: 0,
            session_id: 0,
            peer: None,
            request: None,
            metadata: None,
            attributes: None,
            alpn: None,
            traceparent: None,
            baggage: None,
            tracestate: None,
        }
    }

    fn into_js(self) -> JsDispatchEvent {
        JsDispatchEvent {
            kind: self.kind,
            call_id: self.call_id as i64,
            service_idx: self.service_idx,
            method_idx: self.method_idx,
            flags: self.flags,
            session_key: self.session_key as i64,
            connection_id: self.connection_id as i64,
            deadline_ms: self.deadline_ms as i64,
            reason: self.reason,
            wire_call_id: self.wire_call_id,
            session_id: self.session_id,
            peer: self.peer.map(|p| p.to_string()),
            // External buffer over the Vec: no copy.
            request: self.request.map(Buffer::from),
            metadata: self.metadata.map(|m| m.into_iter().collect()),
            attributes: self.attributes.map(|m| m.into_iter().collect()),
            alpn: self.alpn,
            traceparent: self.traceparent,
            baggage: self.baggage,
            tracestate: self.tracestate,
        }
    }
}

/// State the sinks share with the server object: connection snapshots for
/// `CallContext` and the non-RPC connections waiting for JS.
#[derive(Default)]
struct Shared {
    /// `connection_id` → connection, for `transportSnapshot` /
    /// `connection`. Added on a connection's first call, dropped on its
    /// `ConnectionClosed`.
    connections: Mutex<HashMap<u64, CoreConnection>>,
    /// Non-RPC connections handed to JS by handle (`takeConnection`).
    non_rpc: Mutex<HashMap<u64, (String, CoreConnection)>>,
    next_non_rpc: AtomicU64,
}

impl Shared {
    /// Flatten `ev` for JS; `None` for events JS never sees.
    fn convert(&self, ev: DispatchEvent) -> Option<Ev> {
        Some(match ev {
            DispatchEvent::Call(call) => {
                if let Some(conn) = call.connection {
                    self.connections
                        .lock()
                        .unwrap()
                        .entry(call.connection_id)
                        .or_insert(conn);
                }
                let mut flags = call.flags;
                let header = &call.header;
                let metadata = (!header.metadata_keys.is_empty()).then(|| {
                    flags |= FLAG_HAS_METADATA;
                    header
                        .metadata_keys
                        .iter()
                        .cloned()
                        .zip(header.metadata_values.iter().cloned())
                        .collect()
                });
                let attributes = (!call.attributes.is_empty()).then(|| {
                    flags |= FLAG_HAS_ATTRIBUTES;
                    call.attributes
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect()
                });
                let traceparent = call.trace_context.map(|t| {
                    flags |= FLAG_HAS_TRACE;
                    aster_otel::TraceContext {
                        trace_id: t.trace_id,
                        span_id: t.span_id,
                        trace_flags: t.trace_flags,
                        is_remote: false,
                    }
                    .traceparent()
                });
                let (baggage, tracestate) = match &call.inbound_trace {
                    Some(t) => {
                        flags |= FLAG_HAS_INBOUND_TRACE;
                        (
                            t.baggage.as_deref().map(str::to_owned),
                            t.tracestate.as_deref().map(str::to_owned),
                        )
                    }
                    None => (None, None),
                };
                Ev {
                    kind: EVENT_CALL,
                    call_id: call.handle.detach(),
                    service_idx: call.service_idx,
                    method_idx: call.method_idx,
                    flags,
                    session_key: call.session_key,
                    connection_id: call.connection_id,
                    deadline_ms: call.deadline_unix_ms,
                    reason: 0,
                    wire_call_id: call.wire_call_id,
                    session_id: header.session_id,
                    peer: Some(call.peer_id),
                    request: Some(call.request),
                    metadata,
                    attributes,
                    alpn: None,
                    traceparent,
                    baggage,
                    tracestate,
                }
            }
            DispatchEvent::Cancelled { call_id, reason } => Ev {
                call_id,
                reason: match reason {
                    CancelReason::Deadline => 2,
                    CancelReason::InvalidRequest => 3,
                    _ => 1,
                },
                ..Ev::simple(EVENT_CANCELLED)
            },
            DispatchEvent::ResponseWritable { call_id } => Ev {
                call_id,
                ..Ev::simple(EVENT_RESPONSE_WRITABLE)
            },
            DispatchEvent::ResponseClosed { call_id } => Ev {
                call_id,
                ..Ev::simple(EVENT_RESPONSE_CLOSED)
            },
            DispatchEvent::SessionClosed { session_key } => Ev {
                session_key,
                ..Ev::simple(EVENT_SESSION_CLOSED)
            },
            DispatchEvent::ConnectionClosed {
                connection_id,
                peer_id,
            } => {
                self.connections.lock().unwrap().remove(&connection_id);
                Ev {
                    connection_id,
                    peer: Some(peer_id),
                    ..Ev::simple(EVENT_CONNECTION_CLOSED)
                }
            }
            DispatchEvent::NonRpc { alpn, connection } => {
                let id = self.next_non_rpc.fetch_add(1, Ordering::Relaxed) + 1;
                let alpn = String::from_utf8_lossy(&alpn).into_owned();
                let peer: Arc<str> = Arc::from(connection.remote_id());
                self.non_rpc
                    .lock()
                    .unwrap()
                    .insert(id, (alpn.clone(), connection));
                Ev {
                    connection_id: id,
                    peer: Some(peer),
                    alpn: Some(alpn),
                    ..Ev::simple(EVENT_NON_RPC)
                }
            }
            _ => return None,
        })
    }
}

/// A batch of events, converted to a `DispatchEvent[]` on the JS thread.
pub struct EvBatch(Vec<Ev>);

impl TypeName for EvBatch {
    fn type_name() -> &'static str {
        "DispatchEvent[]"
    }
    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for EvBatch {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        let js: Vec<JsDispatchEvent> = val.0.into_iter().map(Ev::into_js).collect();
        unsafe { Vec::<JsDispatchEvent>::to_napi_value(env, js) }
    }
}

// ── TSFN sink (tsfn, tsfn-batched) ──────────────────────────────────────────

/// `Some(ev)`: deliver exactly that event (`tsfn`). `None`: drain the shared
/// queue (`tsfn-batched`).
type Tsfn = ThreadsafeFunction<Option<Ev>, Unknown<'static>, EvBatch, Status, false>;

struct TsfnSink {
    tsfn: Mutex<Option<Arc<Tsfn>>>,
    batched: bool,
    shared: Arc<Shared>,
    /// Batched: events waiting for the scheduled TSFN call.
    queue: Arc<Mutex<Vec<Ev>>>,
    /// Batched: a TSFN call is queued and has not started draining yet.
    scheduled: Arc<AtomicBool>,
    /// For abandoning calls whose event cannot reach JS any more.
    table: OnceLock<CallTable>,
    /// Diagnostics: TSFN calls queued, callbacks run, failed calls, and the
    /// last failure's `napi_status`.
    stats: Arc<TsfnStats>,
    /// Batched: signalled once per scheduled drain call; wakes the watchdog.
    scheduled_wake: Arc<tokio::sync::Notify>,
}

/// How long a scheduled batched drain call may sit unrun before the
/// watchdog queues another one.
const TSFN_WATCHDOG: Duration = Duration::from_millis(2);

/// Batched-delivery watchdog. Bun (1.2.15) sometimes leaves a queued TSFN
/// call unrun until the *next* call on the same TSFN arrives (a lost wakeup
/// when a call is queued while the JS thread is inside that TSFN's
/// callback). Per-event delivery never notices -- the next event is that
/// next call -- but batched delivery makes exactly one call per batch, so
/// the stranded call would stall every later event for good. The watchdog
/// re-queues a drain call when one has been outstanding for
/// [`TSFN_WATCHDOG`] with no callback run. Idle cost: none (it parks on a
/// `Notify`); under load at most one timer per watchdog period.
async fn tsfn_watchdog(sink: std::sync::Weak<TsfnSink>, wake: Arc<tokio::sync::Notify>) {
    loop {
        wake.notified().await;
        loop {
            let Some(s) = sink.upgrade() else { return };
            let before = s.stats.callbacks.load(Ordering::Acquire);
            drop(s);
            tokio::time::sleep(TSFN_WATCHDOG).await;
            let Some(s) = sink.upgrade() else { return };
            if s.tsfn.lock().unwrap().is_none() {
                return;
            }
            if !s.scheduled.load(Ordering::Acquire) {
                break;
            }
            if s.stats.callbacks.load(Ordering::Acquire) == before {
                let Some(tsfn) = s.tsfn.lock().unwrap().clone() else {
                    return;
                };
                s.stats.calls.fetch_add(1, Ordering::Relaxed);
                s.stats.rescues.fetch_add(1, Ordering::Relaxed);
                let _ = tsfn.call(None, ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }
}

#[derive(Default)]
struct TsfnStats {
    calls: AtomicU64,
    callbacks: AtomicU64,
    failures: AtomicU64,
    last_failure: AtomicU64,
    /// Drain calls the watchdog re-queued.
    rescues: AtomicU64,
}

impl TsfnStats {
    fn failed(&self, status: Status) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        self.last_failure.store(status as u64, Ordering::Relaxed);
    }
}

impl TsfnSink {
    fn lost(&self, ev: &Ev) {
        match ev.kind {
            EVENT_CALL => {
                if let Some(t) = self.table.get() {
                    let _ = t.abandon(ev.call_id);
                }
            }
            EVENT_NON_RPC => {
                self.shared
                    .non_rpc
                    .lock()
                    .unwrap()
                    .remove(&ev.connection_id);
            }
            _ => {}
        }
    }
}

impl DispatchSink for TsfnSink {
    fn deliver(&self, event: DispatchEvent) {
        let Some(ev) = self.shared.convert(event) else {
            return;
        };
        let tsfn = self.tsfn.lock().unwrap().clone();
        let Some(tsfn) = tsfn else {
            self.lost(&ev);
            return;
        };
        if !self.batched {
            let kind = ev.kind;
            let id = if kind == EVENT_NON_RPC {
                ev.connection_id
            } else {
                ev.call_id
            };
            self.stats.calls.fetch_add(1, Ordering::Relaxed);
            let status = tsfn.call(Some(ev), ThreadsafeFunctionCallMode::NonBlocking);
            if status != Status::Ok {
                self.stats.failed(status);
                let mut lost = Ev::simple(kind);
                lost.call_id = id;
                lost.connection_id = id;
                self.lost(&lost);
            }
            return;
        }
        self.queue.lock().unwrap().push(ev);
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        self.stats.calls.fetch_add(1, Ordering::Relaxed);
        let status = tsfn.call(None, ThreadsafeFunctionCallMode::NonBlocking);
        if status == Status::Ok {
            self.scheduled_wake.notify_one();
        } else {
            self.stats.failed(status);
            // Nothing will drain the queue: answer its calls, and clear the
            // flag so the next event schedules again.
            let lost: Vec<Ev> = std::mem::take(&mut *self.queue.lock().unwrap());
            self.scheduled.store(false, Ordering::Release);
            for ev in &lost {
                self.lost(ev);
            }
        }
    }
}

// ── ring sink ───────────────────────────────────────────────────────────────

/// The JS-facing end of a `RingSink`.
struct Ring {
    consumer: tokio::sync::Mutex<RingConsumer>,
    closed: AtomicBool,
    close_notify: tokio::sync::Notify,
}

impl Ring {
    fn drain_into(shared: &Shared, consumer: &mut RingConsumer, max: usize, out: &mut Vec<Ev>) {
        consumer.drain(max, |ev| {
            if let Some(ev) = shared.convert(ev) {
                out.push(ev);
            }
        });
    }
}

// ── attribute bridge ────────────────────────────────────────────────────────

/// Gate 3 reads the caller's Gate-0 admission attributes: the bridge from
/// the (frozen) TS credential admission, which records attributes in the
/// node's `GatePolicy` store, to Rust-enforced `requires` (design §2.6).
/// Live: expiry and revocation apply from the next call.
struct GateAttributes(aster::PeerAdmissionStore);

#[aster::rpc::async_trait]
impl Authenticator for GateAttributes {
    async fn authenticate(&self, ctx: &AuthContext) -> std::result::Result<AuthOutcome, RpcStatus> {
        Ok(AuthOutcome {
            principal: None,
            attributes: self.0.attributes(&ctx.peer_id),
        })
    }
}

// ── DispatchServer ──────────────────────────────────────────────────────────

/// Options for [`DispatchServer::serve`] / [`DispatchServer::start`].
#[napi(object)]
#[derive(Default)]
pub struct DispatchServerOptions {
    /// `"tsfn-batched"` (default; TSFN, coalesced while JS is busy),
    /// `"tsfn"` (one TSFN call per event) or `"ring"` (pull with
    /// `nextBatch` / `pollBatch`; no `onEvents`).
    pub delivery: Option<String>,
    /// Ring capacity in events (`"ring"` only; default 4096).
    pub ring_capacity: Option<u32>,
    /// Serve the baseline `aster.ops.NodeInfo` service (default true).
    pub builtin_node_info: Option<bool>,
    /// Serve the baseline `aster.net.Topology` service (default false).
    pub builtin_topology: Option<bool>,
    /// Cap on concurrently open sessions per connection (default 1024).
    pub max_sessions_per_connection: Option<u32>,
    /// Cap on any handler, ms (default 300 000; 0 = no cap). A call's
    /// deadline is `min(header deadline, method timeout, this)`.
    pub handler_timeout_ms: Option<f64>,
}

enum Running {
    /// Own node (`start`): shut down on close.
    Own(Box<AsterServer>),
    /// Serving on a caller's node (`serve`): stop serving on close; the node
    /// stays up.
    Attached(ServerHandle),
}

/// An RPC server whose services are implemented in JS.
///
/// Start with `DispatchServer.serve(node, servicesJson, options, onEvents)`
/// (or `start(config, …)` for a server with its own node). Every response
/// operation is keyed by the event's `callId` and is non-blocking except
/// `recv` and `waitWritable`.
///
/// Submit results (`respond` / `send` / `finish`): `0` sent; `> 0` the
/// response queue is full — nothing was sent, `await waitWritable(callId, n)`
/// and retry the same operation; `-1` closed (peer gone); `-2` no such call;
/// `-3` too large; `-4` invalid (empty stream payload, second reservation).
#[napi]
pub struct DispatchServer {
    running: Arc<tokio::sync::Mutex<Option<Running>>>,
    table: CallTable,
    attributes: AttributeStore,
    address: String,
    shared: Arc<Shared>,
    tsfn_sink: Option<Arc<TsfnSink>>,
    ring: Option<Arc<Ring>>,
}

fn submit_code(r: std::result::Result<(), SubmitError>) -> i64 {
    match r {
        Ok(()) => 0,
        Err(SubmitError::QueueFull { required_len }) => required_len as i64,
        Err(SubmitError::Closed) => -1,
        Err(SubmitError::NotFound) => -2,
        Err(SubmitError::TooLarge) => -3,
        Err(_) => -4,
    }
}

fn status_of(
    code: Option<i32>,
    message: Option<String>,
    details: Option<HashMap<String, String>>,
) -> RpcStatus {
    let (detail_keys, detail_values) = details.unwrap_or_default().into_iter().unzip();
    RpcStatus {
        code: code.unwrap_or(0),
        message: message.unwrap_or_default(),
        detail_keys,
        detail_values,
    }
}

fn call_id(id: i64) -> u64 {
    id.max(0) as u64
}

/// The sinks a server is started with, built on the JS thread.
struct Sinks {
    tsfn_sink: Option<Arc<TsfnSink>>,
    shared: Arc<Shared>,
    ring_cap: usize,
}

impl Sinks {
    fn new(
        options: &DispatchServerOptions,
        on_events: Option<Function<'_, EvBatch, Unknown<'static>>>,
    ) -> Result<Sinks> {
        let shared = Arc::new(Shared::default());
        let delivery = options.delivery.as_deref().unwrap_or("tsfn-batched");
        let tsfn_sink = match delivery {
            "tsfn" | "tsfn-batched" => {
                let f = on_events.ok_or_else(|| {
                    Error::from_reason(format!("delivery \"{delivery}\" needs onEvents"))
                })?;
                let queue: Arc<Mutex<Vec<Ev>>> = Arc::new(Mutex::new(Vec::new()));
                let scheduled = Arc::new(AtomicBool::new(false));
                let stats = Arc::new(TsfnStats::default());
                let (q, s, st) = (queue.clone(), scheduled.clone(), stats.clone());
                let tsfn: Tsfn = f
                    .build_threadsafe_function::<Option<Ev>>()
                    .callee_handled::<false>()
                    .build_callback(move |ctx| {
                        st.callbacks.fetch_add(1, Ordering::Relaxed);
                        Ok(match ctx.value {
                            Some(ev) => EvBatch(vec![ev]),
                            None => {
                                // Clear the flag before draining: an event
                                // pushed after this point schedules a new
                                // call instead of being stranded.
                                s.store(false, Ordering::Release);
                                EvBatch(std::mem::take(&mut *q.lock().unwrap()))
                            }
                        })
                    })?;
                Some(Arc::new(TsfnSink {
                    tsfn: Mutex::new(Some(Arc::new(tsfn))),
                    batched: delivery == "tsfn-batched",
                    shared: shared.clone(),
                    queue,
                    scheduled,
                    table: OnceLock::new(),
                    stats,
                    scheduled_wake: Arc::new(tokio::sync::Notify::new()),
                }))
            }
            "ring" => None,
            other => {
                return Err(Error::from_reason(format!(
                    "unknown delivery \"{other}\" (tsfn-batched | tsfn | ring)"
                )))
            }
        };
        Ok(Sinks {
            tsfn_sink,
            shared,
            ring_cap: options.ring_capacity.unwrap_or(4096).max(16) as usize,
        })
    }

    /// The `DispatchSink` to register (call inside the tokio runtime).
    fn sink(&self) -> (Arc<dyn DispatchSink>, Option<Arc<Ring>>) {
        match &self.tsfn_sink {
            Some(s) => {
                if s.batched {
                    tokio::spawn(tsfn_watchdog(Arc::downgrade(s), s.scheduled_wake.clone()));
                }
                (s.clone(), None)
            }
            None => {
                let (sink, consumer) = RingSink::new(self.ring_cap);
                let ring = Arc::new(Ring {
                    consumer: tokio::sync::Mutex::new(consumer),
                    closed: AtomicBool::new(false),
                    close_notify: tokio::sync::Notify::new(),
                });
                (sink, Some(ring))
            }
        }
    }

    fn finish(
        self,
        running: Running,
        table: CallTable,
        attributes: AttributeStore,
        address: String,
        ring: Option<Arc<Ring>>,
    ) -> DispatchServer {
        if let Some(s) = &self.tsfn_sink {
            let _ = s.table.set(table.clone());
        }
        DispatchServer {
            running: Arc::new(tokio::sync::Mutex::new(Some(running))),
            table,
            attributes,
            address,
            shared: self.shared,
            tsfn_sink: self.tsfn_sink,
            ring,
        }
    }
}

fn parse_services(services_json: &[String]) -> Result<Vec<DynamicService>> {
    services_json
        .iter()
        .map(|j| DynamicService::from_json(j).map_err(aster_err))
        .collect()
}

fn handler_timeout(ms: Option<f64>) -> Result<Option<Option<Duration>>> {
    match ms {
        None => Ok(None),
        Some(ms) if !ms.is_finite() || ms < 0.0 => Err(Error::new(
            Status::InvalidArg,
            "handlerTimeoutMs must be a finite number >= 0",
        )),
        Some(0.0) => Ok(Some(None)),
        Some(ms) => Ok(Some(Some(Duration::from_secs_f64(ms / 1000.0)))),
    }
}

#[napi]
impl DispatchServer {
    /// Serve `servicesJson` (one `DynamicService` JSON document per service,
    /// design §1.5; array index = `serviceIdx`) on `node`, which must have
    /// been started with `aster/1` among its ALPNs. Rust takes over the
    /// node's accept loop: non-RPC ALPNs no Rust handler claims arrive as
    /// `kind` 7 events. `close()` stops serving; the node stays up.
    ///
    /// `onEvents` receives every event for the TSFN deliveries and is
    /// required for them; it must not throw. With `delivery: "ring"` pass
    /// nothing and pull with `nextBatch` / `pollBatch`.
    ///
    /// `gateAttributes`: Gate 3 also reads each caller's attributes from
    /// this gate's admission store (live; merged under `setPeerAttributes`).
    /// Costs one task per call, so pass it only when a service declares
    /// `requires`.
    #[napi(
        ts_args_type = "node: Node, servicesJson: string[], options?: DispatchServerOptions | undefined | null, onEvents?: ((events: DispatchEvent[]) => void) | undefined | null, gateAttributes?: GatePolicy | undefined | null",
        ts_return_type = "Promise<DispatchServer>"
    )]
    pub fn serve<'env>(
        env: &'env Env,
        node: &Node,
        services_json: Vec<String>,
        options: Option<DispatchServerOptions>,
        on_events: Option<Function<'env, EvBatch, Unknown<'static>>>,
        gate_attributes: Option<&GatePolicy>,
    ) -> Result<PromiseRaw<'env, DispatchServer>> {
        let node = node.aster().clone();
        let gate_store =
            gate_attributes.map(|g| g.inner.read().unwrap_or_else(|e| e.into_inner()).store());
        let options = options.unwrap_or_default();
        let services = parse_services(&services_json)?;
        let timeout = handler_timeout(options.handler_timeout_ms)?;
        let sinks = Sinks::new(&options, on_events)?;
        let node_info = options.builtin_node_info.unwrap_or(true);
        let topology = options.builtin_topology.unwrap_or(false);
        let max_sessions = options.max_sessions_per_connection;

        env.spawn_future(async move {
            let (sink, ring) = sinks.sink();
            let attributes = AttributeStore::new();
            let mut server = Server::new(&node)
                .attributes(attributes.clone())
                .dispatch_sink(sink);
            if let Some(store) = gate_store {
                server = server.authenticator(GateAttributes(store));
            }
            if let Some(max) = max_sessions {
                server = server.max_sessions_per_connection(max.max(1) as usize);
            }
            if let Some(t) = timeout {
                server = server.handler_timeout(t);
            }
            if node_info {
                server = server
                    .with_builtin_node_info(&node, Default::default(), None)
                    .0;
            }
            if topology {
                server = server.with_builtin_topology(&node, None);
            }
            for svc in services {
                server = server.dynamic_service(svc).map_err(aster_err)?;
            }
            let table = server.call_table();
            let handle = server.serve();
            Ok(sinks.finish(
                Running::Attached(handle),
                table,
                attributes,
                String::new(),
                ring,
            ))
        })
    }

    /// Start a node from `config` and serve `servicesJson` on it (see
    /// `serve`). `close()` shuts the node down too.
    #[napi(
        ts_args_type = "config: AsterConfig | undefined | null, servicesJson: string[], options?: DispatchServerOptions | undefined | null, onEvents?: ((events: DispatchEvent[]) => void) | undefined | null",
        ts_return_type = "Promise<DispatchServer>"
    )]
    pub fn start<'env>(
        env: &'env Env,
        config: Option<JsAsterConfig>,
        services_json: Vec<String>,
        options: Option<DispatchServerOptions>,
        on_events: Option<Function<'env, EvBatch, Unknown<'static>>>,
    ) -> Result<PromiseRaw<'env, DispatchServer>> {
        let config = config.unwrap_or_default().to_rust()?;
        let options = options.unwrap_or_default();
        let services = parse_services(&services_json)?;
        let timeout = handler_timeout(options.handler_timeout_ms)?;
        let sinks = Sinks::new(&options, on_events)?;
        let node_info = options.builtin_node_info.unwrap_or(true);
        let topology = options.builtin_topology.unwrap_or(false);
        let max_sessions = options.max_sessions_per_connection;

        env.spawn_future(async move {
            let (sink, ring) = sinks.sink();
            let attributes = AttributeStore::new();
            let mut builder = AsterServer::builder()
                .config(config)
                .attributes(attributes.clone())
                .builtin_node_info(node_info)
                .builtin_topology(topology)
                .dispatch_sink(sink);
            if let Some(max) = max_sessions {
                builder = builder.max_sessions_per_connection(max.max(1) as usize);
            }
            if let Some(t) = timeout {
                builder = builder.handler_timeout(t);
            }
            for svc in services {
                builder = builder.dynamic_service(svc).map_err(aster_err)?;
            }
            let server = builder.start().await.map_err(aster_err)?;
            let table = server.call_table();
            // A ticket without direct addresses is undialable offline; they
            // appear shortly after bind.
            for _ in 0..100 {
                if !server.node().addr().direct_addresses.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let address = server.address().map_err(aster_err)?;
            Ok(sinks.finish(
                Running::Own(Box::new(server)),
                table,
                attributes,
                address,
                ring,
            ))
        })
    }

    /// This server's `aster1…` ticket (`start` only; `""` for `serve`).
    #[napi(getter)]
    pub fn address(&self) -> String {
        self.address.clone()
    }

    /// Number of live calls in the call table (0 once every call ended).
    #[napi]
    pub fn live_calls(&self) -> u32 {
        self.table.len() as u32
    }

    /// Attribute bridge (design §2.6): set the attributes Gate 3 checks
    /// `requires` against for `peer` (hex node id). `null` / empty clears
    /// the entry. Fed by the frozen credential admission.
    #[napi]
    pub fn set_peer_attributes(&self, peer: String, attributes: Option<HashMap<String, String>>) {
        match attributes {
            Some(a) if !a.is_empty() => self.attributes.set(peer, a),
            _ => self.attributes.remove(&peer),
        }
    }

    /// The attributes Gate 3 currently sees for `peer`.
    #[napi]
    pub fn peer_attributes(&self, peer: String) -> HashMap<String, String> {
        self.attributes.get(&peer)
    }

    /// Take a non-RPC connection (`kind` 7 event) by its `connectionId`.
    /// `null` if already taken or unknown.
    #[napi]
    pub fn take_connection(&self, connection_id: i64) -> Option<IrohConnection> {
        let (alpn, conn) = self
            .shared
            .non_rpc
            .lock()
            .unwrap()
            .remove(&call_id(connection_id))?;
        let mut c = IrohConnection::from(conn);
        c.alpn_tag = Some(alpn);
        Some(c)
    }

    /// The selected QUIC path of an `aster/1` connection (by the call
    /// event's `connectionId`); `null` once it closed.
    #[napi]
    pub fn transport_snapshot(&self, connection_id: i64) -> Option<TransportSnapshot> {
        let conns = self.shared.connections.lock().unwrap();
        conns
            .get(&call_id(connection_id))
            .map(|c| TransportSnapshot::from(c.transport_snapshot()))
    }

    /// The `aster/1` connection a call arrived on (tunnels, path info);
    /// `null` once it closed.
    #[napi]
    pub fn connection(&self, connection_id: i64) -> Option<IrohConnection> {
        let conns = self.shared.connections.lock().unwrap();
        conns
            .get(&call_id(connection_id))
            .map(|c| IrohConnection::from(c.clone()))
    }

    /// Diagnostics: `[liveCalls, queued, flag, tsfnCalls, tsfnCallbacks,
    /// tsfnFailures, lastFailureStatus, watchdogRescues]`. TSFN: `queued` =
    /// events waiting in the batched queue, `flag` = a batched call is
    /// outstanding. Ring: `queued` = events in the ring (-1 while a
    /// `nextBatch` holds it), `flag` = the pump stopped.
    #[napi]
    pub fn debug_state(&self) -> Vec<f64> {
        let mut out = vec![self.table.len() as f64];
        match (&self.tsfn_sink, &self.ring) {
            (Some(s), _) => {
                let st = &s.stats;
                out.extend([
                    s.queue.lock().unwrap().len() as f64,
                    s.scheduled.load(Ordering::Acquire) as u8 as f64,
                    st.calls.load(Ordering::Relaxed) as f64,
                    st.callbacks.load(Ordering::Relaxed) as f64,
                    st.failures.load(Ordering::Relaxed) as f64,
                    st.last_failure.load(Ordering::Relaxed) as f64,
                    st.rescues.load(Ordering::Relaxed) as f64,
                ]);
            }
            (None, Some(r)) => match r.consumer.try_lock() {
                Ok(c) => out.extend([c.len() as f64, c.is_closed() as u8 as f64]),
                Err(_) => out.extend([-1.0, 0.0]),
            },
            _ => {}
        }
        out
    }

    /// Diagnostics: queue one extra batched drain call (no-op for other
    /// deliveries).
    #[napi]
    pub fn debug_nudge(&self) {
        if let Some(s) = &self.tsfn_sink {
            if let Some(tsfn) = s.tsfn.lock().unwrap().clone() {
                s.stats.calls.fetch_add(1, Ordering::Relaxed);
                let _ = tsfn.call(None, ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }

    // ── ring pulls ──────────────────────────────────────────────────────────

    /// Resolve with the next batch (at most `max`, default 256), waiting
    /// until there is one. Resolves `[]` once the server is closed.
    #[napi(ts_return_type = "Promise<DispatchEvent[]>")]
    pub async fn next_batch(&self, max: Option<u32>) -> Result<EvBatch> {
        let ring = self.ring_ref()?;
        let max = max.unwrap_or(256).max(1) as usize;
        let mut consumer = ring.consumer.lock().await;
        let mut out = Vec::new();
        loop {
            if ring.closed.load(Ordering::Acquire) {
                return Ok(EvBatch(out));
            }
            Ring::drain_into(&self.shared, &mut consumer, max, &mut out);
            if !out.is_empty() || consumer.is_closed() {
                return Ok(EvBatch(out));
            }
            let closed = ring.close_notify.notified();
            tokio::select! {
                _ = consumer.ready() => {}
                _ = closed => {}
            }
        }
    }

    /// Take whatever is ready now (at most `max`, default 256), without
    /// waiting. Returns `[]` while a `nextBatch` is pending.
    #[napi(ts_return_type = "DispatchEvent[]")]
    pub fn poll_batch(&self, max: Option<u32>) -> Result<EvBatch> {
        let ring = self.ring_ref()?;
        let mut out = Vec::new();
        if let Ok(mut consumer) = ring.consumer.try_lock() {
            Ring::drain_into(
                &self.shared,
                &mut consumer,
                max.unwrap_or(256).max(1) as usize,
                &mut out,
            );
        }
        Ok(EvBatch(out))
    }

    fn ring_ref(&self) -> Result<&Arc<Ring>> {
        self.ring
            .as_ref()
            .ok_or_else(|| Error::from_reason("nextBatch/pollBatch need delivery \"ring\""))
    }

    // ── response operations, by call id ─────────────────────────────────────

    /// Unary / client-stream: one terminal response (data frame, omitted when
    /// `payload` is empty, plus trailer). `code` defaults to OK.
    #[napi]
    pub fn respond(
        &self,
        call_id: i64,
        payload: Uint8ArraySlice,
        code: Option<i32>,
        message: Option<String>,
        details: Option<HashMap<String, String>>,
    ) -> i64 {
        submit_code(self.table.try_respond(
            self::call_id(call_id),
            &payload,
            &status_of(code, message, details),
        ))
    }

    /// Server-stream / bidi: one data frame (`payload` must not be empty).
    #[napi]
    pub fn send(&self, call_id: i64, payload: Uint8ArraySlice) -> i64 {
        submit_code(self.table.try_send(self::call_id(call_id), &payload))
    }

    /// End a stream with the trailer. `code` defaults to OK.
    #[napi]
    pub fn finish(
        &self,
        call_id: i64,
        code: Option<i32>,
        message: Option<String>,
        details: Option<HashMap<String, String>>,
    ) -> i64 {
        submit_code(
            self.table
                .try_finish(self::call_id(call_id), &status_of(code, message, details)),
        )
    }

    /// After a submit returned `n > 0`: resolve `true` once `n` bytes of
    /// response capacity are held (the retry will not be full), `false` if
    /// the call ended first.
    #[napi]
    pub async fn wait_writable(&self, call_id: i64, required_len: u32) -> Result<bool> {
        let Some(call) = self.table.get(self::call_id(call_id)) else {
            return Ok(false);
        };
        Ok(call
            .wait_writable(required_len as usize)
            .await
            .unwrap_or(false))
    }

    /// Client-stream / bidi: the next request payload after the one in the
    /// call event; `null` at the end of the request stream (or for an
    /// unknown call).
    #[napi(ts_return_type = "Promise<Buffer | null>")]
    pub async fn recv(&self, call_id: i64) -> Result<Option<Buffer>> {
        let Some(call) = self.table.get(self::call_id(call_id)) else {
            return Ok(None);
        };
        Ok(call.recv().await.map(Buffer::from))
    }

    /// Client-stream / bidi: the next request payloads, waiting for the
    /// first and then taking (without waiting) any others already received,
    /// up to `max` (default 64). `[]` at the end of the request stream (or
    /// for an unknown call). One crossing for a burst of requests.
    #[napi(ts_return_type = "Promise<Buffer[]>")]
    pub async fn recv_batch(&self, call_id: i64, max: Option<u32>) -> Result<Vec<Buffer>> {
        let Some(call) = self.table.get(self::call_id(call_id)) else {
            return Ok(Vec::new());
        };
        let max = max.unwrap_or(64).max(1) as usize;
        let Some(first) = call.recv().await else {
            return Ok(Vec::new());
        };
        let mut out = vec![Buffer::from(first)];
        // `recv` is cancel-safe (a mutex lock, then an mpsc receive), so a
        // poll that finds nothing ready loses nothing.
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        while out.len() < max {
            let fut = std::pin::pin!(call.recv());
            match fut.poll(&mut cx) {
                std::task::Poll::Ready(Some(p)) => out.push(Buffer::from(p)),
                // End of stream: the next call returns `[]`.
                std::task::Poll::Ready(None) | std::task::Poll::Pending => break,
            }
        }
        Ok(out)
    }

    /// The call's header metadata (also on the event when non-empty).
    #[napi]
    pub fn metadata(&self, call_id: i64) -> HashMap<String, String> {
        let Some(call) = self.table.get(self::call_id(call_id)) else {
            return HashMap::new();
        };
        let h = call.header();
        h.metadata_keys
            .iter()
            .cloned()
            .zip(h.metadata_values.iter().cloned())
            .collect()
    }

    /// The caller's attributes as Gate 3 saw them (also on the event when
    /// non-empty).
    #[napi]
    pub fn attributes(&self, call_id: i64) -> HashMap<String, String> {
        let Some(call) = self.table.get(self::call_id(call_id)) else {
            return HashMap::new();
        };
        call.attributes()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Whether the call was cancelled (peer, stream failure, deadline). An
    /// unknown call reads as cancelled.
    #[napi]
    pub fn is_cancelled(&self, call_id: i64) -> bool {
        self.table.is_cancelled(self::call_id(call_id))
    }

    /// End the call with `INTERNAL "handler dropped call"` unless it already
    /// ended.
    #[napi]
    pub fn abandon(&self, call_id: i64) {
        let _ = self.table.abandon(self::call_id(call_id));
    }

    /// Stop serving (and, for `start`, shut the node down). Releases the
    /// event callback and resolves any pending `nextBatch` with `[]`.
    /// Untaken non-RPC connections are closed.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        if let Some(s) = &self.tsfn_sink {
            let tsfn = s.tsfn.lock().unwrap().take();
            drop(tsfn);
            // Let a parked watchdog see the closed sink and exit.
            s.scheduled_wake.notify_one();
        }
        if let Some(r) = &self.ring {
            r.closed.store(true, Ordering::Release);
            r.close_notify.notify_waiters();
        }
        let running = self.running.lock().await.take();
        match running {
            Some(Running::Own(server)) => server.shutdown().await,
            Some(Running::Attached(handle)) => handle.abort(),
            None => {}
        }
        self.shared.non_rpc.lock().unwrap().clear();
        self.shared.connections.lock().unwrap().clear();
        Ok(())
    }
}

/// The contract id Rust derives from a `DynamicService` JSON document
/// (validates it too). Equal to what the server publishes and what
/// `computeContractIdFromJson` gives for the equivalent `ServiceContract`.
#[napi]
pub fn dynamic_service_contract_id(json: String) -> Result<String> {
    DynamicService::from_json(&json)
        .and_then(|s| s.contract_id())
        .map_err(aster_err)
}
