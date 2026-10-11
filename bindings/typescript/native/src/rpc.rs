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
//! | `"tsfn-batched"` (default) | Events enter a bounded ring; one unit TSFN notification drains the ready batch |
//! | `"tsfn"` | compatibility alias for the same bounded, coalesced TSFN delivery |
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
    CallTable, CancelReason, DispatchAdmission, DispatchEvent, DispatchHandoff, DispatchPermit,
    DispatchSendError, DispatchSink, RingConsumer, RingNotifier, RingSink, SubmitError,
};
use aster::rpc::{
    AsterServer, AttributeStore, AuthContext, AuthOutcome, Authenticator, CapabilityRequirement,
    Dispatcher, DynamicService, RpcStatus, Server, ServerHandle,
};
use aster_transport_core::CoreConnection;

use crate::admission::GatePolicy;
use crate::config::{aster_err, JsAsterConfig};
use crate::net::{IrohConnection, TransportSnapshot};
use crate::node::Node;

/// Version of the native topology baseline policy option. Host loaders require this before
/// forwarding role policies, so older addons cannot silently ignore a security-sensitive option.
#[napi]
pub fn rpc_topology_policy_version() -> u32 {
    1
}

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
/// Admission permits stay alive until N-API has converted the batch.
pub struct EvBatch {
    events: Vec<Ev>,
    handoffs: Vec<DispatchHandoff>,
    table: Option<CallTable>,
    shared: Option<Arc<Shared>>,
}

impl EvBatch {
    fn empty() -> Self {
        Self {
            events: Vec::new(),
            handoffs: Vec::new(),
            table: None,
            shared: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    fn with_cleanup(mut self, table: CallTable, shared: Arc<Shared>) -> Self {
        self.table = Some(table);
        self.shared = Some(shared);
        self
    }
}

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
        let EvBatch {
            events,
            handoffs,
            table,
            shared,
        } = val;
        let lost_calls: Vec<_> = events
            .iter()
            .filter(|ev| ev.kind == EVENT_CALL)
            .map(|ev| ev.call_id)
            .collect();
        let lost_non_rpc: Vec<_> = events
            .iter()
            .filter(|ev| ev.kind == EVENT_NON_RPC)
            .map(|ev| ev.connection_id)
            .collect();
        let js: Vec<JsDispatchEvent> = events.into_iter().map(Ev::into_js).collect();
        let result = unsafe { Vec::<JsDispatchEvent>::to_napi_value(env, js) };
        if result.is_err() {
            if let Some(table) = table {
                for call_id in lost_calls {
                    let _ = table.abandon(call_id);
                }
            }
            if let Some(shared) = shared {
                let mut non_rpc = shared.non_rpc.lock().unwrap();
                for connection_id in lost_non_rpc {
                    non_rpc.remove(&connection_id);
                }
            }
        }
        drop(handoffs);
        result
    }
}

// ── TSFN sink (tsfn, tsfn-batched) ──────────────────────────────────────────

/// A TSFN call is only a bounded notification; events stay in the ring.
/// Two waiting slots let an event arriving during a callback queue its
/// successor without carrying any event payload.
type Tsfn = ThreadsafeFunction<(), Unknown<'static>, EvBatch, Status, false, true, 2>;

struct TsfnQueue {
    sink: Arc<RingSink>,
    consumer: Mutex<RingConsumer>,
}

struct TsfnSink {
    tsfn: Mutex<Option<Arc<Tsfn>>>,
    shared: Arc<Shared>,
    /// Initialized on the runtime thread before the sink is registered.
    queue: Arc<OnceLock<TsfnQueue>>,
    /// A drain notification is queued or being handled.
    scheduled: Arc<AtomicBool>,
    failed: AtomicBool,
    /// For abandoning calls whose event cannot reach JS any more.
    table: Arc<OnceLock<CallTable>>,
    /// Diagnostics: TSFN calls queued, callbacks run, failed calls, and the
    /// last failure's `napi_status`.
    stats: Arc<TsfnStats>,
    /// Batched: signalled once per scheduled drain call; wakes the watchdog.
    scheduled_wake: Arc<tokio::sync::Notify>,
    /// Cancels the pump-readiness forwarder when the server closes.
    close_wake: Arc<tokio::sync::Notify>,
}

/// How long a scheduled drain call may sit unrun before the
/// watchdog queues another one.
const TSFN_WATCHDOG: Duration = Duration::from_millis(2);
const TSFN_WATCHDOG_MAX: Duration = Duration::from_millis(250);
const TSFN_WATCHDOG_RESCUE_LIMIT: u32 = 128;

/// TSFN watchdog. Bun (1.2.15) sometimes leaves a queued TSFN
/// call unrun until the *next* call on the same TSFN arrives (a lost wakeup
/// when a call is queued while the JS thread is inside that TSFN's
/// callback). The watchdog re-queues a drain notification when one remains
/// outstanding without a callback run. Retries back off from 2 ms to 250 ms
/// and stop on sink close or after 128 failed rescues (about 30 seconds at
/// the maximum delay). The task parks on a Notify while idle.
async fn tsfn_watchdog(sink: std::sync::Weak<TsfnSink>, wake: Arc<tokio::sync::Notify>) {
    loop {
        wake.notified().await;
        let mut delay = TSFN_WATCHDOG;
        let mut rescues = 0;
        loop {
            let Some(s) = sink.upgrade() else { return };
            if s.failed.load(Ordering::Acquire) || s.tsfn.lock().unwrap().is_none() {
                return;
            }
            if !s.scheduled.load(Ordering::Acquire) {
                break;
            }
            let before = s.stats.callbacks.load(Ordering::Acquire);
            drop(s);
            tokio::time::sleep(delay).await;
            let Some(s) = sink.upgrade() else { return };
            if s.failed.load(Ordering::Acquire) || s.tsfn.lock().unwrap().is_none() {
                return;
            }
            if !s.scheduled.load(Ordering::Acquire) {
                break;
            }
            if s.stats.callbacks.load(Ordering::Acquire) != before {
                delay = TSFN_WATCHDOG;
                rescues = 0;
                continue;
            }
            let Some(tsfn) = s.tsfn.lock().unwrap().clone() else {
                return;
            };
            s.stats.calls.fetch_add(1, Ordering::Relaxed);
            s.stats.rescues.fetch_add(1, Ordering::Relaxed);
            let status = tsfn.call((), ThreadsafeFunctionCallMode::NonBlocking);
            if status != Status::Ok && status != Status::QueueFull {
                s.fail(status);
                return;
            }
            rescues += 1;
            if rescues >= TSFN_WATCHDOG_RESCUE_LIMIT {
                s.fail(Status::GenericFailure);
                return;
            }
            delay = (delay * 2).min(TSFN_WATCHDOG_MAX);
        }
    }
}

/// Forward pump readiness to the JS callback scheduler. The ring's staging
/// channel is intentionally invisible here: `ready()` resolves only after
/// the pump has moved an event into the consumer ring.
async fn tsfn_ready_forwarder(
    sink: std::sync::Weak<TsfnSink>,
    notifier: RingNotifier,
    close: Arc<tokio::sync::Notify>,
) {
    loop {
        if sink
            .upgrade()
            .is_none_or(|s| s.failed.load(Ordering::Acquire))
        {
            return;
        }
        tokio::select! {
            ready = notifier.ready() => {
                if !ready {
                    return;
                }
                let Some(s) = sink.upgrade() else { return };
                if s.failed.load(Ordering::Acquire) {
                    return;
                }
                s.notify();
            }
            _ = close.notified() => return,
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
    fn shutdown(&self) {
        self.failed.store(true, Ordering::Release);
        self.scheduled.store(false, Ordering::Release);
        self.tsfn.lock().unwrap().take();
        self.close_wake.notify_one();
        if let Some(table) = self.table.get() {
            table.abandon_all();
        }
        self.discard_queued();
    }

    fn fail(&self, status: Status) {
        self.stats.failed(status);
        self.shutdown();
    }

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

    fn discard_queued(&self) {
        let Some(queue) = self.queue.get() else {
            return;
        };
        let mut consumer = queue.consumer.lock().unwrap();
        consumer.close();
        consumer.drain_admitted(usize::MAX, |event, handoff| {
            if let Some(ev) = self.shared.convert(event) {
                self.lost(&ev);
            }
            drop(handoff);
        });
    }

    fn notify(&self) {
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let tsfn = { self.tsfn.lock().unwrap().clone() };
        let Some(tsfn) = tsfn else {
            self.fail(Status::Closing);
            return;
        };
        self.stats.calls.fetch_add(1, Ordering::Relaxed);
        let status = tsfn.call((), ThreadsafeFunctionCallMode::NonBlocking);
        if status == Status::Ok || status == Status::QueueFull {
            // QueueFull means an earlier wake is already queued. It can still
            // be stranded by a runtime lost-wakeup, so arm the watchdog too.
            self.scheduled_wake.notify_one();
        } else {
            self.fail(status);
        }
    }
}

impl Drop for TsfnSink {
    fn drop(&mut self) {
        self.scheduled_wake.notify_one();
        self.close_wake.notify_one();
        if let Some(queue) = self.queue.get() {
            queue.consumer.lock().unwrap().close();
        }
    }
}

impl DispatchSink for TsfnSink {
    fn try_admit(&self, kind: DispatchAdmission) -> Option<DispatchPermit> {
        if self.failed.load(Ordering::Acquire) {
            return None;
        }
        self.queue.get()?.sink.try_admit(kind)
    }

    fn deliver(
        &self,
        event: DispatchEvent,
    ) -> std::result::Result<(), DispatchSendError<DispatchEvent>> {
        if self.failed.load(Ordering::Acquire) {
            if let Some(ev) = self.shared.convert(event) {
                self.lost(&ev);
            }
            return Ok(());
        }
        let Some(queue) = self.queue.get() else {
            if let Some(ev) = self.shared.convert(event) {
                self.lost(&ev);
            }
            return Ok(());
        };
        match queue.sink.deliver(event) {
            Ok(()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn deliver_for(
        &self,
        event: DispatchEvent,
        permit: &DispatchPermit,
    ) -> std::result::Result<(), DispatchSendError<DispatchEvent>> {
        if self.failed.load(Ordering::Acquire) {
            if let Some(ev) = self.shared.convert(event) {
                self.lost(&ev);
            }
            return Ok(());
        }
        let Some(queue) = self.queue.get() else {
            return Err(DispatchSendError::Closed(event));
        };
        match queue.sink.deliver_for(event, permit) {
            Ok(()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn deliver_admitted(
        &self,
        event: DispatchEvent,
        permit: DispatchPermit,
    ) -> std::result::Result<(), DispatchSendError<(DispatchEvent, DispatchPermit)>> {
        if self.failed.load(Ordering::Acquire) {
            if let Some(ev) = self.shared.convert(event) {
                self.lost(&ev);
            }
            drop(permit);
            return Ok(());
        }
        let Some(queue) = self.queue.get() else {
            return Err(DispatchSendError::Closed((event, permit)));
        };
        match queue.sink.deliver_admitted(event, permit) {
            Ok(()) => Ok(()),
            Err(error) => Err(error),
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
    fn drain_into(shared: &Shared, consumer: &mut RingConsumer, max: usize, out: &mut EvBatch) {
        consumer.drain_admitted(max, |ev, handoff| {
            if let Some(ev) = shared.convert(ev) {
                out.events.push(ev);
                out.handoffs.push(handoff);
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
    /// `"tsfn-batched"` (default; bounded ring with coalesced TSFN notification),
    /// `"tsfn"` (compatibility alias) or `"ring"` (pull with
    /// `nextBatch` / `pollBatch`; no `onEvents`).
    pub delivery: Option<String>,
    /// Dispatch ring capacity in events (default 4096; minimum 16). Applies
    /// to both TSFN and ring delivery.
    pub ring_capacity: Option<u32>,
    /// Per-kind maximum for admitted foreign calls, sessions, RPC
    /// connections, and non-RPC connections (default 4096; minimum 1).
    pub max_foreign_inflight: Option<u32>,
    /// Serve the baseline `aster.ops.NodeInfo` service (default true).
    pub builtin_node_info: Option<bool>,
    /// Serve the baseline `aster.net.Topology` service (default false).
    pub builtin_topology: Option<bool>,
    /// Optional native CapabilityRequirement JSON for every topology baseline method.
    pub topology_requires_json: Option<String>,
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
    runtime: tokio::runtime::Handle,
    dispatcher: Dispatcher,
    http_children: Arc<Mutex<Vec<HttpOwner>>>,
    running: Arc<tokio::sync::Mutex<Option<Running>>>,
    table: CallTable,
    attributes: AttributeStore,
    address: String,
    shared: Arc<Shared>,
    tsfn_sink: Option<Arc<TsfnSink>>,
    ring: Option<Arc<Ring>>,
}

type HttpOwner = Arc<tokio::sync::Mutex<Option<aster_transport_salvo::OwnedListener>>>;

/// Authenticated PEM listener options. Authentication is required for every RPC.
#[napi(object)]
pub struct HttpListenerOptions {
    pub bind_addr: Option<String>,
    pub certificate_pem: Buffer,
    pub private_key_pem: Buffer,
    pub web_transport: Option<bool>,
    pub max_pending_auth: Option<u32>,
    pub max_auth_payload_bytes: Option<u32>,
}

/// An owned HTTPS / optional WebTransport listener. Await close for socket release.
#[napi]
pub struct HttpListener {
    inner: HttpOwner,
    tcp: String,
    quic: Option<String>,
    runtime: tokio::runtime::Handle,
}

async fn stop_http_listener(inner: &HttpOwner, grace: Duration) -> Result<()> {
    let mut owner = inner.lock().await;
    if let Some(listener) = owner.as_mut() {
        listener.stop(grace).await.map_err(Error::from_reason)?;
        owner.take();
    }
    Ok(())
}

#[napi]
impl HttpListener {
    #[napi(getter)]
    pub fn tcp_addr(&self) -> String {
        self.tcp.clone()
    }
    #[napi(getter)]
    pub fn quic_addr(&self) -> Option<String> {
        self.quic.clone()
    }

    /// Grace bounds application drain; QUIC protocol/socket drain may take longer.
    #[napi]
    pub async fn stop(&self, grace_ms: Option<f64>) -> Result<()> {
        let ms = grace_ms.unwrap_or(0.0);
        if !ms.is_finite() || !(0.0..=u32::MAX as f64).contains(&ms) {
            return Err(Error::from_reason(
                "graceMs must be finite and between 0 and 4294967295",
            ));
        }
        stop_http_listener(&self.inner, Duration::from_secs_f64(ms / 1000.0)).await
    }

    #[napi]
    pub async fn close(&self) -> Result<()> {
        stop_http_listener(&self.inner, Duration::ZERO).await
    }
}

impl Drop for HttpListener {
    fn drop(&mut self) {
        let inner = self.inner.clone();
        self.runtime.spawn(async move {
            let _ = stop_http_listener(&inner, Duration::ZERO).await;
        });
    }
}

impl Drop for DispatchServer {
    fn drop(&mut self) {
        let children = self.http_children.clone();
        let running = self.running.clone();
        self.runtime.spawn(async move {
            let mut running = running.lock().await;
            let children = std::mem::take(&mut *children.lock().unwrap());
            for child in children {
                let _ = stop_http_listener(&child, Duration::ZERO).await;
            }
            match running.take() {
                Some(Running::Own(server)) => server.shutdown().await,
                Some(Running::Attached(handle)) => handle.abort(),
                None => {}
            }
        });
    }
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
    max_inflight: usize,
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
                let queue = Arc::new(OnceLock::<TsfnQueue>::new());
                let scheduled = Arc::new(AtomicBool::new(false));
                let stats = Arc::new(TsfnStats::default());
                let table = Arc::new(OnceLock::new());
                let (q, s, st, sh, t) = (
                    queue.clone(),
                    scheduled.clone(),
                    stats.clone(),
                    shared.clone(),
                    table.clone(),
                );
                let tsfn: Tsfn = f
                    .build_threadsafe_function::<()>()
                    .callee_handled::<false>()
                    .weak::<true>()
                    .max_queue_size::<2>()
                    .build_callback(move |_ctx| {
                        st.callbacks.fetch_add(1, Ordering::Relaxed);
                        // Clear before draining. An event arriving afterward
                        // either lands in this drain or schedules the next wake.
                        s.store(false, Ordering::Release);
                        let Some(queue) = q.get() else {
                            return Ok(EvBatch::empty());
                        };
                        let Some(table) = t.get().cloned() else {
                            return Ok(EvBatch::empty());
                        };
                        let mut batch = EvBatch::empty().with_cleanup(table, sh.clone());
                        queue.consumer.lock().unwrap().drain_admitted(
                            usize::MAX,
                            |event, handoff| {
                                if let Some(ev) = sh.convert(event) {
                                    batch.events.push(ev);
                                    batch.handoffs.push(handoff);
                                }
                            },
                        );
                        Ok(batch)
                    })?;
                Some(Arc::new(TsfnSink {
                    tsfn: Mutex::new(Some(Arc::new(tsfn))),
                    shared: shared.clone(),
                    queue,
                    scheduled,
                    failed: AtomicBool::new(false),
                    table,
                    stats,
                    scheduled_wake: Arc::new(tokio::sync::Notify::new()),
                    close_wake: Arc::new(tokio::sync::Notify::new()),
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
            max_inflight: options.max_foreign_inflight.unwrap_or(4096).max(1) as usize,
        })
    }

    /// The `DispatchSink` to register (call inside the tokio runtime).
    fn sink(&self) -> (Arc<dyn DispatchSink>, Option<Arc<Ring>>) {
        match &self.tsfn_sink {
            Some(s) => {
                let (sink, consumer) = RingSink::new_with_limit(self.ring_cap, self.max_inflight);
                let notifier = consumer.notifier();
                let _ = s.queue.set(TsfnQueue {
                    sink,
                    consumer: Mutex::new(consumer),
                });
                tokio::spawn(tsfn_watchdog(Arc::downgrade(s), s.scheduled_wake.clone()));
                tokio::spawn(tsfn_ready_forwarder(
                    Arc::downgrade(s),
                    notifier,
                    s.close_wake.clone(),
                ));
                (s.clone(), None)
            }
            None => {
                let (sink, consumer) = RingSink::new_with_limit(self.ring_cap, self.max_inflight);
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
        dispatcher: Dispatcher,
        table: CallTable,
        attributes: AttributeStore,
        address: String,
        ring: Option<Arc<Ring>>,
    ) -> DispatchServer {
        if let Some(s) = &self.tsfn_sink {
            let _ = s.table.set(table.clone());
            // A drain that ran before the table existed returned nothing and
            // left its events queued; schedule another for them.
            s.notify();
        }
        DispatchServer {
            runtime: tokio::runtime::Handle::current(),
            dispatcher,
            http_children: Arc::new(Mutex::new(Vec::new())),
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
        let topology_requires = options
            .topology_requires_json
            .as_deref()
            .map(serde_json::from_str::<CapabilityRequirement>)
            .transpose()
            .map_err(|e| Error::from_reason(format!("invalid topology requirement: {e}")))?;
        let max_sessions = options.max_sessions_per_connection;
        if topology && !node.topology().has_monitoring() {
            return Err(Error::from_reason("builtinTopology requires monitoring"));
        }

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
                server = server.with_builtin_topology(&node, topology_requires);
            }
            for svc in services {
                server = server.dynamic_service(svc).map_err(aster_err)?;
            }
            let dispatcher = server.dispatcher();
            let table = server.call_table();
            let handle = server.serve();
            #[cfg(feature = "test-hooks")]
            attach_gate(table.clone()).await;
            Ok(sinks.finish(
                Running::Attached(handle),
                dispatcher,
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
        let topology_requires = options
            .topology_requires_json
            .as_deref()
            .map(serde_json::from_str::<CapabilityRequirement>)
            .transpose()
            .map_err(|e| Error::from_reason(format!("invalid topology requirement: {e}")))?;
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
            if let Some(requirement) = topology_requires {
                builder = builder.topology_requires(requirement);
            }
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
            let dispatcher = server.dispatcher();
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
                dispatcher,
                table,
                attributes,
                address,
                ring,
            ))
        })
    }

    /// Bind a listener with bounded, fail-closed per-RPC foreign authentication.
    #[napi]
    pub fn start_http<'env>(
        &self,
        env: &'env Env,
        options: HttpListenerOptions,
        upcalls: &crate::upcalls::Upcalls,
    ) -> Result<PromiseRaw<'env, HttpListener>> {
        use aster::binding_support::upcall::auth::{
            ForeignAuthenticator, ForeignAuthenticatorOptions,
        };
        let upcalls = upcalls.table();
        if upcalls.is_closed() {
            return Err(Error::from_reason("authentication table is closed"));
        }
        let defaults = ForeignAuthenticatorOptions::default();
        let auth = ForeignAuthenticator::with_options(
            upcalls,
            ForeignAuthenticatorOptions {
                max_pending: options
                    .max_pending_auth
                    .filter(|n| *n != 0)
                    .map_or(defaults.max_pending, |n| n as usize),
                max_payload_bytes: options
                    .max_auth_payload_bytes
                    .filter(|n| *n != 0)
                    .map_or(defaults.max_payload_bytes, |n| n as usize),
            },
        )
        .map_err(|error| Error::from_reason(error.to_string()))?;
        let config = aster_transport_salvo::HttpConfig::new(
            options.bind_addr.unwrap_or_else(|| "127.0.0.1:0".into()),
            aster_transport_salvo::TlsMaterial::pem(
                options.certificate_pem.to_vec(),
                options.private_key_pem.to_vec(),
            ),
        )
        .webtransport(options.web_transport.unwrap_or(false));
        let dispatcher = self.dispatcher.clone();
        let running = self.running.clone();
        // Capture a registry independently of the borrowed NAPI receiver.
        let children = self.http_children.clone();
        env.spawn_future(async move {
            let guard = running.lock().await;
            if guard.is_none() {
                return Err(Error::from_reason("server is closed"));
            }
            let listener = config
                .start_authenticated(dispatcher, Arc::new(auth))
                .await
                .map_err(Error::from_reason)?;
            let tcp = listener.tcp_addr().to_string();
            let quic = listener.quic_addr().map(|a| a.to_string());
            let inner = Arc::new(tokio::sync::Mutex::new(Some(listener)));
            let mut children = children.lock().unwrap();
            children.retain(|c| c.try_lock().map_or(true, |v| v.is_some()));
            children.push(inner.clone());
            Ok(HttpListener {
                inner,
                tcp,
                quic,
                runtime: tokio::runtime::Handle::current(),
            })
        })
    }

    /// Bind an explicit anonymous listener. Session services are disabled by the dispatcher.
    #[napi]
    pub fn start_public_http<'env>(
        &self,
        env: &'env Env,
        options: HttpListenerOptions,
    ) -> Result<PromiseRaw<'env, HttpListener>> {
        if options.max_pending_auth.unwrap_or(0) != 0
            || options.max_auth_payload_bytes.unwrap_or(0) != 0
        {
            return Err(Error::from_reason(
                "public listeners do not accept authentication limits",
            ));
        }
        let config = aster_transport_salvo::HttpConfig::new(
            options.bind_addr.unwrap_or_else(|| "127.0.0.1:0".into()),
            aster_transport_salvo::TlsMaterial::pem(
                options.certificate_pem.to_vec(),
                options.private_key_pem.to_vec(),
            ),
        )
        .webtransport(options.web_transport.unwrap_or(false));
        let dispatcher = self.dispatcher.clone();
        let running = self.running.clone();
        let children = self.http_children.clone();
        env.spawn_future(async move {
            let guard = running.lock().await;
            if guard.is_none() {
                return Err(Error::from_reason("server is closed"));
            }
            let listener = config
                .start_public(dispatcher)
                .await
                .map_err(Error::from_reason)?;
            let tcp = listener.tcp_addr().to_string();
            let quic = listener.quic_addr().map(|a| a.to_string());
            let inner = Arc::new(tokio::sync::Mutex::new(Some(listener)));
            let mut children = children.lock().unwrap();
            children.retain(|c| c.try_lock().map_or(true, |v| v.is_some()));
            children.push(inner.clone());
            Ok(HttpListener {
                inner,
                tcp,
                quic,
                runtime: tokio::runtime::Handle::current(),
            })
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
    /// events waiting in the bounded TSFN ring, `flag` = a drain notification
    /// is outstanding. Ring: `queued` = events in the ring (-1 while a
    /// `nextBatch` holds it), `flag` = the pump stopped.
    #[napi]
    pub fn debug_state(&self) -> Vec<f64> {
        let mut out = vec![self.table.len() as f64];
        match (&self.tsfn_sink, &self.ring) {
            (Some(s), _) => {
                let st = &s.stats;
                out.extend([
                    s.queue
                        .get()
                        .and_then(|q| q.consumer.lock().ok().map(|c| c.len()))
                        .unwrap_or(0) as f64,
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

    /// Diagnostics: queue one extra drain notification (no-op for ring
    /// deliveries).
    #[napi]
    pub fn debug_nudge(&self) {
        if let Some(s) = &self.tsfn_sink {
            if let Some(tsfn) = s.tsfn.lock().unwrap().clone() {
                s.stats.calls.fetch_add(1, Ordering::Relaxed);
                let _ = tsfn.call((), ThreadsafeFunctionCallMode::NonBlocking);
                s.scheduled_wake.notify_one();
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
        let mut out = EvBatch::empty().with_cleanup(self.table.clone(), self.shared.clone());
        loop {
            // Register before checking `closed`: `close` must not slip
            // between the check and the wait, since it then needs this lock.
            let closed = ring.close_notify.notified();
            tokio::pin!(closed);
            closed.as_mut().enable();
            if ring.closed.load(Ordering::Acquire) {
                return Ok(out);
            }
            Ring::drain_into(&self.shared, &mut consumer, max, &mut out);
            if !out.is_empty() || consumer.is_closed() {
                return Ok(out);
            }
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
        let mut out = EvBatch::empty().with_cleanup(self.table.clone(), self.shared.clone());
        if let Ok(mut consumer) = ring.consumer.try_lock() {
            Ring::drain_into(
                &self.shared,
                &mut consumer,
                max.unwrap_or(256).max(1) as usize,
                &mut out,
            );
        }
        Ok(out)
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
        let mut running = self.running.lock().await;
        let children = self.http_children.lock().unwrap().clone();
        for child in children {
            stop_http_listener(&child, Duration::ZERO).await?;
        }
        self.http_children.lock().unwrap().clear();
        if let Some(s) = &self.tsfn_sink {
            s.shutdown();
        }
        if let Some(r) = &self.ring {
            r.closed.store(true, Ordering::Release);
            r.close_notify.notify_waiters();
        }
        match running.take() {
            Some(Running::Own(server)) => server.shutdown().await,
            Some(Running::Attached(handle)) => handle.abort(),
            None => {}
        }
        if let Some(r) = &self.ring {
            // As TSFN shutdown: release what JS never took and end live
            // calls (an attached node keeps their connections open).
            let mut consumer = r.consumer.lock().await;
            consumer.close();
            consumer.drain_admitted(usize::MAX, |event, handoff| {
                let _ = self.shared.convert(event);
                drop(handoff);
            });
            drop(consumer);
            self.table.abandon_all();
        }
        self.shared.non_rpc.lock().unwrap().clear();
        self.shared.connections.lock().unwrap().clear();
        Ok(())
    }
}

#[cfg(feature = "test-hooks")]
struct AttachGate {
    table: OnceLock<CallTable>,
    claimed: AtomicBool,
    release: tokio::sync::Semaphore,
}

#[cfg(feature = "test-hooks")]
fn attach_gate_slot() -> &'static Mutex<Option<Arc<AttachGate>>> {
    static GATE: OnceLock<Mutex<Option<Arc<AttachGate>>>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(None))
}

#[cfg(feature = "test-hooks")]
async fn attach_gate(table: CallTable) {
    let gate = attach_gate_slot()
        .lock()
        .unwrap()
        .as_ref()
        .filter(|gate| !gate.claimed.swap(true, Ordering::AcqRel))
        .cloned();
    if let Some(gate) = gate {
        let _ = gate.table.set(table);
        gate.release.acquire().await.unwrap().forget();
        attach_gate_slot().lock().unwrap().take();
    }
}

#[cfg(feature = "test-hooks")]
#[napi]
impl DispatchServer {
    /// Arm a one-shot attached-server gate before the delivery table attaches.
    #[napi]
    pub fn debug_arm_attach() -> Result<()> {
        let mut slot = attach_gate_slot().lock().unwrap();
        if slot.is_some() {
            return Err(Error::from_reason("attach gate already armed"));
        }
        *slot = Some(Arc::new(AttachGate {
            table: OnceLock::new(),
            claimed: AtomicBool::new(false),
            release: tokio::sync::Semaphore::new(0),
        }));
        Ok(())
    }

    /// -1 before entry; otherwise the number of calls registered while held.
    #[napi]
    pub fn debug_attach_state() -> i32 {
        attach_gate_slot()
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|g| g.table.get())
            .map(|t| t.len() as i32)
            .unwrap_or(-1)
    }

    #[napi]
    pub fn debug_release_attach() {
        let mut slot = attach_gate_slot().lock().unwrap();
        if let Some(gate) = slot.as_ref() {
            gate.release.add_permits(1);
            if !gate.claimed.load(Ordering::Acquire) {
                slot.take();
            }
        }
    }

    /// Diagnostics: force the same terminal cleanup used after TSFN delivery
    /// failure or watchdog exhaustion. Test-hook addons only.
    #[napi]
    pub fn debug_fail_delivery(&self) {
        if let Some(s) = &self.tsfn_sink {
            s.fail(Status::GenericFailure);
        }
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
