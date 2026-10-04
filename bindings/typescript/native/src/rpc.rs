//! `DispatchServer` — foreign (JS-implemented) services served by
//! `aster::rpc::Server` (design `docs/_internal/bindings/dynamic-dispatch-design.md`
//! §4.3; plan T4).
//!
//! JS registers [`DynamicService`]s in their JSON form (design §1.5). Rust
//! runs every gate (routing, sessions, Gate 3, deadlines, frame limits) and
//! delivers one event per call; JS answers **by call id** through the shared
//! [`CallTable`] (`respond` / `send` / `finish` / `recv` / `waitWritable`),
//! passing payloads and status, never frames.
//!
//! **T4.0 prototype.** Every delivery variant from design §6 is here, so they
//! can be benchmarked against each other on Node and Bun
//! (`docs/_internal/bindings/t4-dispatch-benchmark.md`):
//!
//! | `delivery` | How events reach JS |
//! |---|---|
//! | `"tsfn"` (T-a) | one `napi_call_threadsafe_function` per event |
//! | `"tsfn-batched"` (T-b) | Rust queues events and schedules one TSFN call while none is pending, so events that arrive while JS is busy are coalesced into one batch |
//! | `"ring"` (R1 / R2) | `RingSink` (SPSC ring + pump); JS pulls with `await nextBatch()` (R1) or with sync `pollBatch()` from `setImmediate`, parking on `nextBatch()` when empty (R2) |
//!
//! Call handles are detached (`CallHandle::detach`) as the event is handed
//! to JS: the call then lives in the table until a terminal operation,
//! cancellation, or deadline.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

use aster::binding_support::dispatch::{
    CallTable, CancelReason, DispatchEvent, DispatchSink, RingConsumer, RingSink, SubmitError,
};
use aster::rpc::{AsterServer, DynamicService, RpcStatus};

use crate::config::{aster_err, JsAsterConfig};

// ── events ──────────────────────────────────────────────────────────────────

/// `DispatchEvent.kind`: a call that passed every gate. Answer it by `callId`.
pub const EVENT_CALL: u32 = 1;
/// `DispatchEvent.kind`: the call ended without the handler finishing it
/// (`reason`: 1 = peer cancel / stream failure, 2 = deadline).
pub const EVENT_CANCELLED: u32 = 2;
/// `DispatchEvent.kind`: reserved capacity (event-driven reserve) is ready.
pub const EVENT_RESPONSE_WRITABLE: u32 = 3;
/// `DispatchEvent.kind`: a reservation ended because the call did.
pub const EVENT_RESPONSE_CLOSED: u32 = 4;
/// `DispatchEvent.kind`: a session was reaped (`sessionKey`).
pub const EVENT_SESSION_CLOSED: u32 = 5;
/// `DispatchEvent.kind`: an `aster/1` connection closed (`connectionId`, `peer`).
pub const EVENT_CONNECTION_CLOSED: u32 = 6;

/// One dispatch event, flattened for JS. Fields that do not apply to the
/// event's `kind` are `0` (numbers) or absent.
#[napi(object, object_from_js = false, js_name = "DispatchEvent")]
pub struct JsDispatchEvent {
    /// 1 call, 2 cancelled, 3 response-writable, 4 response-closed,
    /// 5 session-closed, 6 connection-closed.
    pub kind: u32,
    /// Correlation key for every response operation (per server, from 1).
    pub call_id: i64,
    /// Index of the service in the `services` array passed to `start`.
    pub service_idx: u32,
    /// Index of the method in the service's `methods`.
    pub method_idx: u32,
    /// `FIRST_IS_END` (1) | `NEW_SESSION` (2) | `IROH_AUTHENTICATED` (4).
    pub flags: u32,
    /// 0 = shared service; otherwise the Rust-assigned session key.
    pub session_key: i64,
    /// Connection id (0 without one, e.g. HTTP).
    pub connection_id: i64,
    /// Absolute deadline, Unix ms; 0 = none.
    pub deadline_ms: i64,
    /// For `cancelled`: 1 peer, 2 deadline.
    pub reason: u32,
    /// Peer endpoint id (or authenticator principal); calls and
    /// connection-closed only.
    pub peer: Option<String>,
    /// The first request payload, already decompressed; calls only.
    pub request: Option<Buffer>,
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
    peer: Option<Arc<str>>,
    request: Option<Vec<u8>>,
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
            peer: None,
            request: None,
        }
    }

    /// `None` for events JS never sees (`NonRpc`: the connection is dropped).
    fn from_event(ev: DispatchEvent) -> Option<Ev> {
        Some(match ev {
            DispatchEvent::Call(call) => Ev {
                kind: EVENT_CALL,
                call_id: call.handle.detach(),
                service_idx: call.service_idx,
                method_idx: call.method_idx,
                flags: call.flags,
                session_key: call.session_key,
                connection_id: call.connection_id,
                deadline_ms: call.deadline_unix_ms,
                reason: 0,
                peer: Some(call.peer_id),
                request: Some(call.request),
            },
            DispatchEvent::Cancelled { call_id, reason } => Ev {
                call_id,
                reason: match reason {
                    CancelReason::Deadline => 2,
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
            } => Ev {
                connection_id,
                peer: Some(peer_id),
                ..Ev::simple(EVENT_CONNECTION_CLOSED)
            },
            _ => return None,
        })
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
            peer: self.peer.map(|p| p.to_string()),
            // External buffer over the Vec: no copy.
            request: self.request.map(Buffer::from),
        }
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

// ── TSFN sinks (T-a, T-b) ───────────────────────────────────────────────────

/// `Some(ev)`: deliver exactly that event (T-a). `None`: drain the shared
/// queue (T-b).
type Tsfn = ThreadsafeFunction<Option<Ev>, Unknown<'static>, EvBatch, Status, false>;

struct TsfnSink {
    tsfn: Mutex<Option<Arc<Tsfn>>>,
    batched: bool,
    /// T-b: events waiting for the scheduled TSFN call.
    queue: Arc<Mutex<Vec<Ev>>>,
    /// T-b: a TSFN call is queued and has not started draining yet.
    scheduled: Arc<AtomicBool>,
    /// For abandoning calls whose event cannot reach JS any more.
    table: OnceLock<CallTable>,
    /// Diagnostics: TSFN calls queued, callbacks run, failed calls, and the
    /// last failure's `napi_status`.
    stats: Arc<TsfnStats>,
    /// T-b: signalled once per scheduled drain call; wakes the watchdog.
    scheduled_wake: Arc<tokio::sync::Notify>,
}

/// How long a scheduled T-b drain call may sit unrun before the watchdog
/// queues another one.
const TSFN_WATCHDOG: std::time::Duration = std::time::Duration::from_millis(2);

/// T-b watchdog. Bun (1.2.15) sometimes leaves a queued TSFN call unrun
/// until the *next* call on the same TSFN arrives (a lost wakeup when a call
/// is queued while the JS thread is inside that TSFN's callback). T-a never
/// notices -- the next event is that next call -- but T-b makes exactly one
/// call per batch, so the stranded call stalls every later event for good.
/// The watchdog re-queues a drain call when one has been outstanding for
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
    /// Drain calls the T-b watchdog re-queued.
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
        if ev.kind == EVENT_CALL {
            if let Some(t) = self.table.get() {
                let _ = t.abandon(ev.call_id);
            }
        }
    }
}

impl DispatchSink for TsfnSink {
    fn deliver(&self, event: DispatchEvent) {
        let Some(ev) = Ev::from_event(event) else {
            return;
        };
        let tsfn = self.tsfn.lock().unwrap().clone();
        let Some(tsfn) = tsfn else {
            self.lost(&ev);
            return;
        };
        if !self.batched {
            let call_id = (ev.kind == EVENT_CALL).then_some(ev.call_id);
            self.stats.calls.fetch_add(1, Ordering::Relaxed);
            let status = tsfn.call(Some(ev), ThreadsafeFunctionCallMode::NonBlocking);
            if status != Status::Ok {
                self.stats.failed(status);
                if let (Some(id), Some(t)) = (call_id, self.table.get()) {
                    let _ = t.abandon(id);
                }
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

// ── ring sink (R1, R2) ──────────────────────────────────────────────────────

/// The JS-facing end of a `RingSink`.
struct Ring {
    consumer: tokio::sync::Mutex<RingConsumer>,
    closed: AtomicBool,
    close_notify: tokio::sync::Notify,
}

impl Ring {
    fn drain_into(consumer: &mut RingConsumer, max: usize, out: &mut Vec<Ev>) {
        consumer.drain(max, |ev| {
            if let Some(ev) = Ev::from_event(ev) {
                out.push(ev);
            }
        });
    }
}

// ── DispatchServer ──────────────────────────────────────────────────────────

/// Options for [`DispatchServer::start`].
#[napi(object)]
#[derive(Default)]
pub struct DispatchServerOptions {
    /// `"tsfn"` (default; one TSFN call per event), `"tsfn-batched"` (TSFN,
    /// coalesced while JS is busy) or `"ring"` (pull with `nextBatch` /
    /// `pollBatch`; no `onEvents`).
    pub delivery: Option<String>,
    /// Ring capacity in events (`"ring"` only; default 4096).
    pub ring_capacity: Option<u32>,
    /// Serve the baseline `aster.ops.NodeInfo` service (default true).
    pub builtin_node_info: Option<bool>,
}

/// An RPC server whose services are implemented in JS (T4.0 prototype).
///
/// Start with `DispatchServer.start(config, servicesJson, options, onEvents)`.
/// Every response operation is keyed by the event's `callId` and is
/// non-blocking except `recv` and `waitWritable`.
///
/// Submit results (`respond` / `send` / `finish`): `0` sent; `> 0` the
/// response queue is full — nothing was sent, `await waitWritable(callId, n)`
/// and retry the same operation; `-1` closed (peer gone); `-2` no such call;
/// `-3` too large; `-4` invalid (empty stream payload, second reservation).
#[napi]
pub struct DispatchServer {
    server: Arc<tokio::sync::Mutex<Option<AsterServer>>>,
    table: CallTable,
    address: String,
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

fn status_of(code: Option<i32>, message: Option<String>) -> RpcStatus {
    RpcStatus {
        code: code.unwrap_or(0),
        message: message.unwrap_or_default(),
        detail_keys: Vec::new(),
        detail_values: Vec::new(),
    }
}

fn call_id(id: i64) -> u64 {
    id.max(0) as u64
}

#[napi]
impl DispatchServer {
    /// Start a node serving `servicesJson` (one `DynamicService` JSON
    /// document per service, design §1.5; array index = `serviceIdx`).
    ///
    /// `onEvents` receives every event for the TSFN deliveries and is
    /// required for them; it must not throw. With `delivery: "ring"` pass
    /// nothing and pull with `nextBatch` / `pollBatch`.
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
        let services = services_json
            .iter()
            .map(|j| DynamicService::from_json(j).map_err(aster_err))
            .collect::<Result<Vec<_>>>()?;
        let delivery = options.delivery.as_deref().unwrap_or("tsfn");

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
                    "unknown delivery \"{other}\" (tsfn | tsfn-batched | ring)"
                )))
            }
        };
        let ring_cap = options.ring_capacity.unwrap_or(4096).max(16) as usize;
        let node_info = options.builtin_node_info.unwrap_or(true);

        env.spawn_future(async move {
            let (sink, ring): (Arc<dyn DispatchSink>, Option<Arc<Ring>>) = match &tsfn_sink {
                Some(s) => {
                    if s.batched {
                        tokio::spawn(tsfn_watchdog(Arc::downgrade(s), s.scheduled_wake.clone()));
                    }
                    (s.clone(), None)
                }
                None => {
                    let (sink, consumer) = RingSink::new(ring_cap);
                    let ring = Arc::new(Ring {
                        consumer: tokio::sync::Mutex::new(consumer),
                        closed: AtomicBool::new(false),
                        close_notify: tokio::sync::Notify::new(),
                    });
                    (sink, Some(ring))
                }
            };
            let mut builder = AsterServer::builder()
                .config(config)
                .builtin_node_info(node_info)
                .dispatch_sink(sink);
            for svc in services {
                builder = builder.dynamic_service(svc).map_err(aster_err)?;
            }
            let server = builder.start().await.map_err(aster_err)?;
            let table = server.call_table();
            if let Some(s) = &tsfn_sink {
                let _ = s.table.set(table.clone());
            }
            // A ticket without direct addresses is undialable offline; they
            // appear shortly after bind.
            for _ in 0..100 {
                if !server.node().addr().direct_addresses.is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let address = server.address().map_err(aster_err)?;
            Ok(DispatchServer {
                server: Arc::new(tokio::sync::Mutex::new(Some(server))),
                table,
                address,
                tsfn_sink,
                ring,
            })
        })
    }

    /// This server's `aster1…` ticket.
    #[napi(getter)]
    pub fn address(&self) -> String {
        self.address.clone()
    }

    /// Number of live calls in the call table (0 once every call ended).
    #[napi]
    pub fn live_calls(&self) -> u32 {
        self.table.len() as u32
    }

    /// Diagnostics: `[liveCalls, queued, flag, tsfnCalls, tsfnCallbacks,
    /// tsfnFailures, lastFailureStatus, watchdogRescues]`. TSFN: `queued` = events waiting in
    /// the T-b queue, `flag` = a T-b call is outstanding. Ring: `queued` =
    /// events in the ring (-1 while a `nextBatch` holds it), `flag` = the
    /// pump stopped.
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

    /// Diagnostics: queue one extra T-b drain call (no-op for other
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

    // ── ring pulls (R1 / R2) ────────────────────────────────────────────────

    /// R1: resolve with the next batch (at most `max`, default 256), waiting
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
            Ring::drain_into(&mut consumer, max, &mut out);
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

    /// R2: take whatever is ready now (at most `max`, default 256), without
    /// waiting. Returns `[]` while a `nextBatch` is pending.
    #[napi(ts_return_type = "DispatchEvent[]")]
    pub fn poll_batch(&self, max: Option<u32>) -> Result<EvBatch> {
        let ring = self.ring_ref()?;
        let mut out = Vec::new();
        if let Ok(mut consumer) = ring.consumer.try_lock() {
            Ring::drain_into(&mut consumer, max.unwrap_or(256).max(1) as usize, &mut out);
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
    ) -> i64 {
        submit_code(self.table.try_respond(
            self::call_id(call_id),
            &payload,
            &status_of(code, message),
        ))
    }

    /// Server-stream / bidi: one data frame (`payload` must not be empty).
    #[napi]
    pub fn send(&self, call_id: i64, payload: Uint8ArraySlice) -> i64 {
        submit_code(self.table.try_send(self::call_id(call_id), &payload))
    }

    /// End a stream with the trailer. `code` defaults to OK.
    #[napi]
    pub fn finish(&self, call_id: i64, code: Option<i32>, message: Option<String>) -> i64 {
        submit_code(
            self.table
                .try_finish(self::call_id(call_id), &status_of(code, message)),
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

    /// Stop serving and shut the node down. Releases the event callback and
    /// resolves any pending `nextBatch` with `[]`.
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
        let server = self.server.lock().await.take();
        if let Some(server) = server {
            server.shutdown().await;
        }
        Ok(())
    }
}
