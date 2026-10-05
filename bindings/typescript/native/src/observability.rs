//! Observability (design `OBSERVABILITY_HEALTH_DESIGN.md` §8 O7): the NAPI
//! side of `aster-otel`.
//!
//! * pipeline: `observabilityInit` / `observabilityFlush` /
//!   `observabilityShutdown` (one `ObservabilityHandle` per successful init,
//!   as the C ABI does);
//! * the Rust log bridge (§3.2): a drain thread delivers batches of
//!   [`LogRecord`]s to JavaScript over a **weak** `ThreadsafeFunction` with an
//!   explicit queue bound ([`LOG_TSFN_QUEUE`] batches). A refused batch (queue
//!   full) is counted, never retried. The Bun TSFN stall is handled twice: the
//!   drain thread nudges a call that sat unrun for [`TSFN_WATCHDOG`] (as
//!   `upcalls.rs` does), and `logPoll` drains the queue synchronously for a
//!   JavaScript timer (poll mode, which also parks the drain thread);
//! * thin wrappers over `aster_otel::{instruments, spans}` (§4.2, §5.3);
//! * `prometheusText()`: the OTel pull reader's scrape (what `/metrics`
//!   serves).
//!
//! Handles are `f64` (they count up from 1 and never reach 2^53).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Error, Status};
use napi_derive::napi;

use aster_otel::bridge::{self, LogDrain, LogRecord, Poll};
use aster_otel::instruments::{self, InstrumentDesc, InstrumentKind, Measurement};
use aster_otel::spans::{self, SpanKind};
use aster_otel::{
    Binding, ExporterKind, ObservabilityConfig, ObservabilityHandle, OtlpHeaders, SamplerConfig,
};

/// Batches the log TSFN holds before a call returns `QueueFull` (design §3.2:
/// an explicit bound, unlike the upcall TSFN).
const LOG_TSFN_QUEUE: usize = 64;

/// How long a scheduled TSFN call may sit unrun before the drain thread
/// queues a nudge (the Bun workaround of `upcalls.rs`).
const TSFN_WATCHDOG: Duration = Duration::from_millis(2);

/// How long the drain thread waits for records when nothing is outstanding.
const IDLE_POLL: Duration = Duration::from_millis(25);

/// Records per batch unless `maxBatch` says otherwise (`bridge::DEFAULT_BATCH`).
const DEFAULT_MAX_BATCH: usize = bridge::DEFAULT_BATCH;

/// Queued TSFN calls above which the drain thread stops sending small
/// batches and coalesces (design §3.1 "batching"): a burst of records then
/// costs a few full batches instead of one call each, which the 64-call
/// bound could not hold. `holdAbove` overrides it.
const DEFAULT_HOLD_ABOVE: i64 = 32;

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(Status::InvalidArg, msg.into())
}

fn otel_err(e: aster_otel::Error) -> Error {
    use aster_otel::Error as E;
    match e {
        E::InvalidArgument(m) => invalid(m),
        E::Config(_) | E::Unsupported(_) => Error::new(Status::InvalidArg, e.to_string()),
        other => Error::from_reason(other.to_string()),
    }
}

fn handle_in(what: &str, v: f64) -> Result<u64> {
    if !v.is_finite() || v < 0.0 || v.fract() != 0.0 || v > ((1u64 << 53) - 1) as f64 {
        return Err(invalid(format!("{what} is not a valid handle: {v}")));
    }
    Ok(v as u64)
}

fn pairs(map: Option<HashMap<String, String>>) -> Vec<(String, String)> {
    map.map(|m| m.into_iter().collect()).unwrap_or_default()
}

// ── pipeline ────────────────────────────────────────────────────────────────

/// Options for `observabilityInit`. Explicit fields beat `OTEL_*` / `ASTER_*`
/// env (read unless `fromEnv` is `false`), which beats `extraToml`, which
/// beats the defaults.
#[napi(object)]
#[derive(Default)]
pub struct ObservabilityOptions {
    /// Apply `OTEL_*` / `ASTER_*` env under the explicit fields. Default true.
    pub from_env: Option<bool>,
    /// `service.name`.
    pub service_name: Option<String>,
    /// OTLP base URL (`/v1/<signal>` is appended).
    pub otlp_endpoint: Option<String>,
    /// OTLP request headers. Secret: never logged or echoed.
    pub otlp_headers: Option<HashMap<String, String>>,
    /// Extra resource attributes.
    pub resource_attributes: Option<HashMap<String, String>>,
    /// `EnvFilter` directives for Rust events (`ASTER_LOG`).
    pub log_filter: Option<String>,
    /// An `[observability]` TOML table (the header line may be omitted).
    pub extra_toml: Option<String>,
    /// Parent-based trace-id-ratio sampler in `[0, 1]`.
    pub trace_sample_ratio: Option<f64>,
    /// Export traces over OTLP. Unset: env decides (with `fromEnv`), else off.
    pub export_traces: Option<bool>,
    /// Export metrics over OTLP. Unset: env decides (with `fromEnv`), else off.
    pub export_metrics: Option<bool>,
    /// Export Rust log records over OTLP. Unset: env decides, else off.
    pub export_otlp_logs: Option<bool>,
    /// Open the binding log queue for `logSinkStart` / `logPoll` (default true).
    pub log_bridge: Option<bool>,
    /// Bound of the Rust-side log queue in records (`ASTER_LOG_QUEUE`).
    pub log_queue: Option<u32>,
}

/// The pipeline references this addon holds, one per successful init.
static HANDLES: Mutex<Vec<ObservabilityHandle>> = Mutex::new(Vec::new());

fn lower_options(o: ObservabilityOptions) -> Result<ObservabilityConfig> {
    let from_env = o.from_env.unwrap_or(true);
    let signal = |v: Option<bool>| -> Option<Vec<ExporterKind>> {
        match v {
            Some(true) => Some(vec![ExporterKind::Otlp]),
            Some(false) => Some(Vec::new()),
            None if from_env => None,
            None => Some(Vec::new()),
        }
    };
    let mut cfg = ObservabilityConfig {
        from_env,
        binding: Some(Binding::TypeScript),
        traces_exporter: signal(o.export_traces),
        metrics_exporter: signal(o.export_metrics),
        logs_exporter: signal(o.export_otlp_logs),
        ..ObservabilityConfig::default()
    };
    if o.log_bridge.unwrap_or(true) {
        cfg.log_bridge = Some(true);
    }
    cfg.service_name = o.service_name.filter(|s| !s.is_empty());
    cfg.otlp.endpoint = o.otlp_endpoint.filter(|s| !s.is_empty());
    let headers = pairs(o.otlp_headers);
    if !headers.is_empty() {
        cfg.otlp.headers = Some(OtlpHeaders::new(headers));
    }
    cfg.resource_attributes = pairs(o.resource_attributes);
    cfg.log_filter = o.log_filter.filter(|s| !s.is_empty());
    if let Some(toml) = o.extra_toml.filter(|s| !s.trim().is_empty()) {
        let doc = if toml.contains("[observability]") {
            toml
        } else {
            format!("[observability]\n{toml}")
        };
        let file = ObservabilityConfig::from_toml_str(&doc).map_err(otel_err)?;
        cfg = cfg.with_file_layer(file);
    }
    if let Some(r) = o.trace_sample_ratio {
        if !(0.0..=1.0).contains(&r) {
            return Err(invalid("traceSampleRatio must be in [0, 1]"));
        }
        cfg.sampler = Some(SamplerConfig::ParentBasedTraceIdRatio(r));
    }
    if let Some(q) = o.log_queue.filter(|q| *q > 0) {
        cfg.log_queue = Some(q as usize);
    }
    Ok(cfg)
}

/// Start the observability pipeline, or join the running one (reference
/// count + 1). A different configuration than the running pipeline's fails;
/// `logBridge` is additive. Each success must be matched by one
/// `observabilityShutdown`. Runs on a helper thread (an init can flush the
/// provider it replaces).
#[napi]
pub async fn observability_init(options: Option<ObservabilityOptions>) -> Result<()> {
    let cfg = lower_options(options.unwrap_or_default())?;
    let handle = tokio::task::spawn_blocking(move || aster_otel::init(cfg))
        .await
        .map_err(|e| Error::from_reason(format!("observability init panicked: {e}")))?
        .map_err(otel_err)?;
    HANDLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(handle);
    Ok(())
}

/// Flush every signal of the running pipeline, waiting at most `timeoutMs`.
/// No pipeline: resolves at once.
#[napi]
pub async fn observability_flush(timeout_ms: Option<u32>) -> Result<()> {
    let timeout = Duration::from_millis(u64::from(timeout_ms.unwrap_or(2000)));
    tokio::task::spawn_blocking(move || aster_otel::flush(timeout))
        .await
        .map_err(|e| Error::from_reason(format!("observability flush panicked: {e}")))?
        .map_err(otel_err)
}

/// Release one reference taken by `observabilityInit`. The last reference
/// (counting other holders, such as a health listener) shuts the pipeline
/// down: providers flush and stop, the log bridge closes, live spans end and
/// instrument handles are freed. Resolves `false` when the addon holds no
/// reference. Waits at most `timeoutMs`; the shutdown then finishes in the
/// background.
#[napi]
pub async fn observability_shutdown(timeout_ms: Option<u32>) -> Result<bool> {
    let Some(handle) = HANDLES.lock().unwrap_or_else(|e| e.into_inner()).pop() else {
        return Ok(false);
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("aster-otel-shutdown".into())
        .spawn(move || {
            drop(handle);
            let _ = tx.send(());
        })
        .map_err(|e| Error::from_reason(format!("cannot start the shutdown thread: {e}")))?;
    let wait = Duration::from_millis(u64::from(timeout_ms.unwrap_or(2000)));
    match tokio::time::timeout(wait, rx).await {
        Ok(_) => Ok(true),
        Err(_) => Err(Error::new(
            Status::GenericFailure,
            "observability shutdown did not finish in time; it continues in the background",
        )),
    }
}

/// The OTel Prometheus pull reader's scrape (the `/metrics` body), or `null`
/// without a pull reader (no health listener has started, or
/// `OTEL_SDK_DISABLED`).
#[napi]
pub fn prometheus_text() -> Option<String> {
    aster::ops::metrics_text()
}

// ── log bridge ──────────────────────────────────────────────────────────────

/// One Rust log record, as the sink receives it.
#[napi(object, object_from_js = false, js_name = "RustLogRecord")]
pub struct JsLogRecord {
    /// 1 error, 2 warn, 3 info, 4 debug, 5 trace.
    pub level: u32,
    /// `HAS_TRACE` 1 | `IN_CALL` 2 | `SYNTHETIC` 4 (a drop report) | `AUDIT` 8.
    pub flags: u32,
    /// Wall clock, milliseconds since the Unix epoch (fractional).
    pub timestamp_ms: f64,
    /// 32 lowercase hex characters when `flags & 1`.
    pub trace_id: Option<String>,
    /// 16 lowercase hex characters when `flags & 1`.
    pub span_id: Option<String>,
    /// The `rpc` span's call id when `flags & 2`, else 0.
    pub call_id: f64,
    /// The `tracing` target (`aster::rpc::dispatch`).
    pub target: String,
    /// The target as a logger name (`aster.rpc.dispatch`).
    pub logger: String,
    pub message: String,
    /// The event's fields, then `rpc.service`, `rpc.method`, `aster.peer` and
    /// `thread.name` where they apply.
    pub fields: HashMap<String, String>,
}

fn to_js(r: LogRecord) -> JsLogRecord {
    let has_trace = r.has_trace();
    JsLogRecord {
        level: u32::from(r.level as u8),
        flags: u32::from(r.flags),
        timestamp_ms: r.timestamp_unix_ns as f64 / 1_000_000.0,
        trace_id: has_trace.then(|| hex::encode(r.trace_id)),
        span_id: has_trace.then(|| hex::encode(r.span_id)),
        call_id: r.call_id as f64,
        logger: r.logger_name(),
        target: r.target.into_owned(),
        message: r.message,
        fields: r
            .fields
            .into_iter()
            .map(|(k, v)| (k.into_owned(), v))
            .collect(),
    }
}

/// A drained batch, converted to `RustLogRecord[]` on the JavaScript thread.
pub struct LogBatch(Vec<LogRecord>);

impl TypeName for LogBatch {
    fn type_name() -> &'static str {
        "RustLogRecord[]"
    }
    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for LogBatch {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        let js: Vec<JsLogRecord> = val.0.into_iter().map(to_js).collect();
        unsafe { Vec::<JsLogRecord>::to_napi_value(env, js) }
    }
}

/// Weak (does not keep the event loop alive), unhandled-errors-to-JS off,
/// bounded at [`LOG_TSFN_QUEUE`] pending batches.
type LogTsfn =
    ThreadsafeFunction<LogBatch, Unknown<'static>, LogBatch, Status, false, true, LOG_TSFN_QUEUE>;

#[derive(Default)]
struct SinkStats {
    /// Records handed to the TSFN and accepted.
    forwarded: AtomicU64,
    /// Records in batches the TSFN refused (queue full, or the sink stopped).
    sink_dropped: AtomicU64,
    /// Batches refused.
    dropped_batches: AtomicU64,
    /// Batches accepted (nudges not counted).
    batches: AtomicU64,
    /// Records converted for JavaScript by a TSFN callback.
    delivered: AtomicU64,
    /// TSFN callbacks run on the JavaScript thread (nudges included).
    callbacks: AtomicU64,
    /// Nudge calls the watchdog queued.
    rescues: AtomicU64,
    /// Records returned by `logPoll`.
    polled: AtomicU64,
    /// TSFN calls queued and not yet run (can dip below zero transiently).
    outstanding: AtomicI64,
}

struct SinkShared {
    tsfn: Mutex<Option<Arc<LogTsfn>>>,
    drain: Mutex<Option<LogDrain>>,
    stop: AtomicBool,
    /// The drain thread has exited (stopped, or the bridge closed).
    ended: AtomicBool,
    /// JavaScript polls (`logPoll`); the drain thread parks.
    poll_mode: AtomicBool,
    /// `logPoll` calls waiting for the drain lock; the thread yields to them.
    poll_waiters: AtomicU32,
    max_batch: usize,
    /// Outstanding TSFN calls above which the thread coalesces.
    hold_above: i64,
    /// Records taken from the queue but not delivered when the thread parked
    /// for polling; `logPoll` returns them first.
    carry: Mutex<Vec<LogRecord>>,
    stats: Arc<SinkStats>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

static SINK: Mutex<Option<Arc<SinkShared>>> = Mutex::new(None);

fn current_sink() -> Option<Arc<SinkShared>> {
    SINK.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

impl SinkShared {
    fn tsfn(&self) -> Option<Arc<LogTsfn>> {
        self.tsfn.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn deliver(&self, records: Vec<LogRecord>) {
        let n = records.len() as u64;
        let Some(tsfn) = self.tsfn() else {
            self.stats.sink_dropped.fetch_add(n, Ordering::Relaxed);
            self.stats.dropped_batches.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // Counted before the call: the callback may run before it returns.
        self.stats.outstanding.fetch_add(1, Ordering::AcqRel);
        let status = tsfn.call(LogBatch(records), ThreadsafeFunctionCallMode::NonBlocking);
        if status == Status::Ok {
            self.stats.forwarded.fetch_add(n, Ordering::Relaxed);
            self.stats.batches.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats.outstanding.fetch_sub(1, Ordering::AcqRel);
            self.stats.sink_dropped.fetch_add(n, Ordering::Relaxed);
            self.stats.dropped_batches.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Re-kick a call that has sat unrun (Bun TSFN stall): an empty batch.
    fn nudge(&self) {
        let Some(tsfn) = self.tsfn() else { return };
        self.stats.outstanding.fetch_add(1, Ordering::AcqRel);
        self.stats.rescues.fetch_add(1, Ordering::Relaxed);
        let status = tsfn.call(
            LogBatch(Vec::new()),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
        if status != Status::Ok {
            self.stats.outstanding.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn drain_loop(sh: Arc<SinkShared>) {
    // Events this thread emits itself never reach the bridge (design §3.1).
    bridge::mark_drain_thread();
    let mut last_callbacks = 0u64;
    // Taken from the queue, not yet handed to the TSFN.
    let mut held: Vec<LogRecord> = Vec::new();
    while !sh.stop.load(Ordering::Acquire) {
        if sh.poll_mode.load(Ordering::Acquire) || sh.poll_waiters.load(Ordering::Acquire) > 0 {
            if !held.is_empty() {
                sh.carry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .append(&mut held);
            }
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let outstanding = sh.stats.outstanding.load(Ordering::Acquire);
        let room = sh.max_batch.saturating_sub(held.len());
        let polled = if room == 0 {
            // A full batch waits for the JavaScript thread to catch up.
            std::thread::sleep(TSFN_WATCHDOG);
            Poll::Timeout
        } else {
            let timeout = if outstanding > 0 || !held.is_empty() {
                TSFN_WATCHDOG
            } else {
                IDLE_POLL
            };
            let mut g = sh.drain.lock().unwrap_or_else(|e| e.into_inner());
            let Some(d) = g.as_mut() else { break };
            d.poll_into(&mut held, room, timeout)
        };
        if matches!(polled, Poll::Closed) {
            if held.is_empty() {
                break;
            }
            // Still holding a batch for a busy JavaScript thread.
            std::thread::sleep(TSFN_WATCHDOG);
        }
        let outstanding = sh.stats.outstanding.load(Ordering::Acquire);
        // Under the hold mark everything goes at once; over it only a full
        // batch goes, and only while the TSFN has room. Otherwise records
        // accumulate (here, then in the bounded Rust queue, where an
        // overflow is counted as a bridge drop).
        let send = !held.is_empty()
            && (outstanding <= sh.hold_above
                || (held.len() >= sh.max_batch && outstanding < LOG_TSFN_QUEUE as i64));
        if send {
            sh.deliver(std::mem::take(&mut held));
            last_callbacks = sh.stats.callbacks.load(Ordering::Acquire);
            continue;
        }
        if matches!(polled, Poll::Timeout) {
            let cb = sh.stats.callbacks.load(Ordering::Acquire);
            if outstanding > 0 && cb == last_callbacks {
                sh.nudge();
            }
            last_callbacks = cb;
        }
    }
    if !held.is_empty() {
        let n = held.len() as u64;
        sh.stats.sink_dropped.fetch_add(n, Ordering::Relaxed);
        sh.stats.dropped_batches.fetch_add(1, Ordering::Relaxed);
    }
    sh.ended.store(true, Ordering::Release);
}

/// Options for `logSinkStart`.
#[napi(object)]
#[derive(Default)]
pub struct LogSinkOptions {
    /// Most records per batch handed to JavaScript (default 256).
    pub max_batch: Option<u32>,
    /// Start in poll mode: no TSFN deliveries, JavaScript calls `logPoll`
    /// (Bun, where TSFN delivery can stall).
    pub poll: Option<bool>,
    /// Queued TSFN calls (of 64) above which small batches are coalesced
    /// into full ones instead of sent (default 32). 64 or more never holds:
    /// a refused batch is then dropped and counted.
    pub hold_above: Option<u32>,
}

/// Start delivering Rust log records: `onBatch` is called on the JavaScript
/// thread with each batch (an empty batch is a watchdog nudge; ignore it).
/// Needs the bridge open (`observabilityInit` with `logBridge`). The callback
/// does not keep the process alive. One sink at a time.
#[napi]
pub fn log_sink_start(
    #[napi(ts_arg_type = "(records: RustLogRecord[]) => void")] on_batch: Function<
        '_,
        LogBatch,
        Unknown<'static>,
    >,
    options: Option<LogSinkOptions>,
) -> Result<()> {
    let options = options.unwrap_or_default();
    let mut slot = SINK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = slot.as_ref() {
        if !old.ended.load(Ordering::Acquire) {
            return Err(Error::from_reason("a log sink is already running"));
        }
        // The bridge closed under it: reclaim the slot.
        stop_sink(old);
    }
    let drain = bridge::drain().map_err(otel_err)?;
    let stats = Arc::new(SinkStats::default());
    let cb_stats = stats.clone();
    let tsfn: LogTsfn = on_batch
        .build_threadsafe_function::<LogBatch>()
        .callee_handled::<false>()
        .weak::<true>()
        .max_queue_size::<LOG_TSFN_QUEUE>()
        .build_callback(move |ctx| {
            cb_stats.callbacks.fetch_add(1, Ordering::AcqRel);
            cb_stats.outstanding.fetch_sub(1, Ordering::AcqRel);
            cb_stats
                .delivered
                .fetch_add(ctx.value.0.len() as u64, Ordering::Relaxed);
            Ok(ctx.value)
        })?;
    let sh = Arc::new(SinkShared {
        tsfn: Mutex::new(Some(Arc::new(tsfn))),
        drain: Mutex::new(Some(drain)),
        stop: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        poll_mode: AtomicBool::new(options.poll.unwrap_or(false)),
        poll_waiters: AtomicU32::new(0),
        max_batch: options
            .max_batch
            .map_or(DEFAULT_MAX_BATCH, |n| (n as usize).max(1)),
        hold_above: options.hold_above.map_or(DEFAULT_HOLD_ABOVE, i64::from),
        carry: Mutex::new(Vec::new()),
        stats,
        thread: Mutex::new(None),
    });
    let t = {
        let sh = sh.clone();
        std::thread::Builder::new()
            .name("aster-log-drain".into())
            .spawn(move || drain_loop(sh))
            .map_err(|e| Error::from_reason(format!("cannot start the log drain thread: {e}")))?
    };
    *sh.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
    *slot = Some(sh);
    Ok(())
}

fn stop_sink(sh: &Arc<SinkShared>) {
    sh.stop.store(true, Ordering::Release);
    sh.tsfn.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(t) = sh.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = t.join();
    }
    // Releases the single drain slot; records still queued stay queued.
    sh.drain.lock().unwrap_or_else(|e| e.into_inner()).take();
}

/// Stop the sink. Records still queued in Rust stay queued for the next sink.
/// Idempotent.
#[napi]
pub fn log_sink_stop() {
    let sh = SINK.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(sh) = sh {
        stop_sink(&sh);
    }
}

/// Switch between TSFN delivery (`false`) and JavaScript polling (`true`).
/// In poll mode the drain thread parks and `logPoll` is the only reader.
#[napi]
pub fn log_set_poll_mode(enabled: bool) -> Result<()> {
    let sh = current_sink().ok_or_else(|| Error::from_reason("no log sink is running"))?;
    sh.poll_mode.store(enabled, Ordering::Release);
    Ok(())
}

/// Drain up to `max` queued records synchronously (default 256), without
/// waiting. Works in either mode (in TSFN mode the drain thread yields for
/// the call, up to one idle poll). The fallback for Bun and the final drain
/// at exit.
#[napi]
pub fn log_poll(max: Option<u32>) -> Result<Vec<JsLogRecord>> {
    let sh = current_sink().ok_or_else(|| Error::from_reason("no log sink is running"))?;
    let max = max.map_or(DEFAULT_MAX_BATCH, |n| (n as usize).max(1));
    sh.poll_waiters.fetch_add(1, Ordering::AcqRel);
    let mut g = sh.drain.lock().unwrap_or_else(|e| e.into_inner());
    sh.poll_waiters.fetch_sub(1, Ordering::AcqRel);
    let Some(d) = g.as_mut() else {
        return Ok(Vec::new());
    };
    // Delivery to JavaScript happens on this thread: events it emits must not
    // loop back into the bridge.
    let _quiet = bridge::suppress();
    let mut out = {
        let mut carry = sh.carry.lock().unwrap_or_else(|e| e.into_inner());
        if carry.len() > max {
            let rest = carry.split_off(max);
            std::mem::replace(&mut *carry, rest)
        } else {
            std::mem::take(&mut *carry)
        }
    };
    let room = max.saturating_sub(out.len());
    if room > 0 {
        let _ = d.poll_into(&mut out, room, Duration::ZERO);
    }
    sh.stats
        .polled
        .fetch_add(out.len() as u64, Ordering::Relaxed);
    Ok(out.into_iter().map(to_js).collect())
}

/// Replace the bridge's `EnvFilter` directives (the Rust side of the
/// binding's level sync). `aster::audit=info` is kept unless named.
#[napi]
pub fn log_set_filter(directives: String) -> Result<()> {
    bridge::set_filter(&directives).map_err(otel_err)
}

/// Export an application log record through the Rust OTel logs provider
/// (design 3.4 path B). `level`: 1 error, 2 warn, 3 info, 4 debug, 5 trace.
/// `timestampNs` 0 = now. A no-op without an OTLP logs exporter. The record
/// emits no Rust event, so it never reaches the log bridge.
#[napi]
pub fn log_emit(
    level: u32,
    logger: String,
    message: String,
    fields: Option<HashMap<String, String>>,
    timestamp_ns: Option<f64>,
    traceparent: Option<String>,
) -> Result<()> {
    let level = match level {
        1 => bridge::LogLevel::Error,
        2 => bridge::LogLevel::Warn,
        3 => bridge::LogLevel::Info,
        4 => bridge::LogLevel::Debug,
        5 => bridge::LogLevel::Trace,
        l => return Err(invalid(format!("unknown log level {l}"))),
    };
    let context = parent_in(traceparent).map(|c| (c.trace_id, c.span_id, c.trace_flags));
    let fields = pairs(fields);
    let view: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    bridge::emit_app_log(&bridge::AppLogRecord {
        level: Some(level),
        timestamp_unix_ns: timestamp_ns.map_or(0, |t| t as i64),
        logger: &logger,
        message: &message,
        fields: &view,
        context,
    })
    .map_err(otel_err)
}

/// The bridge's directives as last set, or `null` when it is not open.
#[napi]
pub fn log_filter() -> Option<String> {
    bridge::filter()
}

/// Log delivery counters.
#[napi(object)]
pub struct LogStats {
    /// A sink is running (its drain thread has not exited).
    pub running: bool,
    pub poll_mode: bool,
    /// Records the TSFN accepted.
    pub forwarded: f64,
    /// Records in batches the TSFN refused (queue full).
    pub sink_dropped: f64,
    pub dropped_batches: f64,
    pub batches: f64,
    /// Records the TSFN callback handed to JavaScript.
    pub delivered: f64,
    /// TSFN callbacks run on the JavaScript thread.
    pub callbacks: f64,
    /// Nudge calls queued by the Bun watchdog.
    pub rescues: f64,
    /// Records returned by `logPoll`.
    pub polled: f64,
    /// Records the Rust queue dropped when it was full (process-wide).
    pub bridge_dropped: f64,
    /// `aster::audit` records the Rust queue dropped.
    pub audit_dropped: f64,
    /// Records waiting in the Rust queue.
    pub pending: f64,
}

#[napi]
pub fn log_stats() -> LogStats {
    let n = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64;
    let sh = current_sink();
    let (s, running, poll_mode, pending) = match &sh {
        Some(sh) => (
            Some(&sh.stats),
            !sh.ended.load(Ordering::Acquire),
            sh.poll_mode.load(Ordering::Acquire),
            sh.drain
                .try_lock()
                .ok()
                .and_then(|g| g.as_ref().map(|d| d.pending()))
                .unwrap_or(0),
        ),
        None => (None, false, false, 0),
    };
    let z = |f: fn(&SinkStats) -> &AtomicU64| s.map_or(0.0, |s| n(f(s)));
    LogStats {
        running,
        poll_mode,
        forwarded: z(|s| &s.forwarded),
        sink_dropped: z(|s| &s.sink_dropped),
        dropped_batches: z(|s| &s.dropped_batches),
        batches: z(|s| &s.batches),
        delivered: z(|s| &s.delivered),
        callbacks: z(|s| &s.callbacks),
        rescues: z(|s| &s.rescues),
        polled: z(|s| &s.polled),
        bridge_dropped: bridge::dropped_total() as f64,
        audit_dropped: bridge::audit_dropped_total() as f64,
        pending: pending as f64,
    }
}

#[cfg(feature = "test-hooks")]
static TEST_EMITTED: AtomicU64 = AtomicU64::new(0);

/// Test hook: records `emitTestLogs` has emitted so far (process-wide).
#[cfg(feature = "test-hooks")]
#[napi]
pub fn test_emitted() -> f64 {
    TEST_EMITTED.load(Ordering::Relaxed) as f64
}

/// Test hook: emit `count` Rust events on target `aster::o7test` from a
/// background thread, `intervalUs` apart, at `level` (1 error .. 5 trace,
/// default 3). Messages are `<prefix> <i>`. Returns at once; watch
/// `testEmitted()`. Compiled only with the `test-hooks` feature, so the
/// published addon (built without it) does not export it.
#[cfg(feature = "test-hooks")]
#[napi]
pub fn emit_test_logs(
    count: u32,
    interval_us: Option<u32>,
    level: Option<u32>,
    prefix: Option<String>,
) {
    let prefix = prefix.unwrap_or_else(|| "test record".to_string());
    let pause = Duration::from_micros(u64::from(interval_us.unwrap_or(0)));
    let level = level.unwrap_or(3);
    let _ = std::thread::Builder::new()
        .name("aster-o7-test-emitter".into())
        .spawn(move || {
            for i in 0..count {
                let msg = format!("{prefix} {i}");
                match level {
                    1 => tracing::error!(target: "aster::o7test", idx = i, "{msg}"),
                    2 => tracing::warn!(target: "aster::o7test", idx = i, "{msg}"),
                    4 => tracing::debug!(target: "aster::o7test", idx = i, "{msg}"),
                    5 => tracing::trace!(target: "aster::o7test", idx = i, "{msg}"),
                    _ => tracing::info!(target: "aster::o7test", idx = i, "{msg}"),
                }
                TEST_EMITTED.fetch_add(1, Ordering::AcqRel);
                if !pause.is_zero() {
                    std::thread::sleep(pause);
                }
            }
        });
}

// ── instruments (design §4.2) ───────────────────────────────────────────────

/// Declare an instrument on the global meter (scope `aster.app`). `kind`: 1
/// counter (u64), 2 up-down counter (i64), 3 histogram (f64), 4 gauge (f64).
/// Returns its handle. A name that exists with another kind fails.
/// `boundaries` (histograms only): bucket upper bounds, finite and strictly
/// increasing; none = the RPC seconds buckets for unit `s`, else the SDK
/// default.
#[napi]
pub fn instrument_new(
    kind: u32,
    name: String,
    description: Option<String>,
    unit: Option<String>,
    auto_attrs: Option<Vec<String>>,
    cardinality_limit: Option<u32>,
    boundaries: Option<Vec<f64>>,
) -> Result<f64> {
    let kind = InstrumentKind::from_u32(kind).ok_or_else(|| invalid("unknown instrument kind"))?;
    let h = instruments::instrument_new(InstrumentDesc {
        kind,
        name,
        description: description.unwrap_or_default(),
        unit: unit.unwrap_or_default(),
        auto_attrs: auto_attrs.unwrap_or_default(),
        cardinality_limit: cardinality_limit.unwrap_or(0),
        boundaries: boundaries.unwrap_or_default(),
    })
    .map_err(otel_err)?;
    Ok(h as f64)
}

/// Free an instrument handle. `false` when it was already gone.
#[napi]
pub fn instrument_free(handle: f64) -> Result<bool> {
    Ok(instruments::instrument_free(handle_in(
        "instrument",
        handle,
    )?))
}

/// Intern an attribute set; pass its handle to the `metric*` calls to skip
/// building the list per record.
#[napi]
pub fn attrset_new(attributes: HashMap<String, String>) -> Result<f64> {
    instruments::attrset_new(pairs(Some(attributes)))
        .map(|h| h as f64)
        .map_err(otel_err)
}

#[napi]
pub fn attrset_free(handle: f64) -> Result<bool> {
    Ok(instruments::attrset_free(handle_in(
        "attribute set",
        handle,
    )?))
}

fn record(
    instrument: f64,
    value: Measurement,
    attrset: Option<f64>,
    attributes: Option<HashMap<String, String>>,
) -> Result<()> {
    let h = handle_in("instrument", instrument)?;
    let set = attrset.map(|a| handle_in("attribute set", a)).transpose()?;
    instruments::record(h, value, set.unwrap_or(0), &pairs(attributes), 0).map_err(otel_err)
}

/// Add to a counter (kind 1). `value` must be a non-negative integer.
#[napi]
pub fn metric_add_u64(
    instrument: f64,
    value: f64,
    attrset: Option<f64>,
    attributes: Option<HashMap<String, String>>,
) -> Result<()> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > u64::MAX as f64 {
        return Err(invalid("a counter takes a non-negative integer"));
    }
    record(
        instrument,
        Measurement::U64(value as u64),
        attrset,
        attributes,
    )
}

/// Add to an up-down counter (kind 2). `value` must be an integer.
#[napi]
pub fn metric_add_i64(
    instrument: f64,
    value: f64,
    attrset: Option<f64>,
    attributes: Option<HashMap<String, String>>,
) -> Result<()> {
    if !value.is_finite() || value.fract() != 0.0 || value.abs() > i64::MAX as f64 {
        return Err(invalid("an up-down counter takes an integer"));
    }
    record(
        instrument,
        Measurement::I64(value as i64),
        attrset,
        attributes,
    )
}

/// Record on a histogram (kind 3) or set a gauge (kind 4).
#[napi]
pub fn metric_record_f64(
    instrument: f64,
    value: f64,
    attrset: Option<f64>,
    attributes: Option<HashMap<String, String>>,
) -> Result<()> {
    if value.is_nan() {
        return Err(invalid("NaN is not a measurement"));
    }
    record(instrument, Measurement::F64(value), attrset, attributes)
}

// ── spans (design §5.3) ─────────────────────────────────────────────────────

fn parent_in(parent: Option<String>) -> Option<aster_otel::TraceContext> {
    // A malformed traceparent is ignored: the span becomes a root (W3C).
    parent.and_then(|p| aster_otel::TraceContext::parse_traceparent(p.as_bytes()))
}

/// Start a span. `kind`: 0 internal, 1 server, 2 client, 3 producer, 4
/// consumer. `parent` is a W3C `traceparent` (the carrier's current span), or
/// `null` for a root. Returns its handle; without a tracer pipeline the span
/// is a no-op. Fails with `QueueFull` at 65 536 live spans.
#[napi]
pub fn span_start(
    name: String,
    kind: u32,
    parent: Option<String>,
    attributes: Option<HashMap<String, String>>,
) -> Result<f64> {
    let kind = SpanKind::from_u32(kind).ok_or_else(|| invalid("unknown span kind"))?;
    spans::span_start(&name, kind, parent_in(parent).as_ref(), &pairs(attributes))
        .map(|h| h as f64)
        .map_err(otel_err)
}

#[napi]
pub fn span_set_attributes(handle: f64, attributes: HashMap<String, String>) -> Result<()> {
    spans::span_set_attributes(handle_in("span", handle)?, &pairs(Some(attributes)))
        .map_err(otel_err)
}

#[napi]
pub fn span_add_event(
    handle: f64,
    name: String,
    attributes: Option<HashMap<String, String>>,
) -> Result<()> {
    spans::span_add_event(handle_in("span", handle)?, &name, &pairs(attributes)).map_err(otel_err)
}

/// `code`: 0 unset, 1 ok, 2 error (with `message`).
#[napi]
pub fn span_set_status(handle: f64, code: u32, message: Option<String>) -> Result<()> {
    spans::span_set_status(
        handle_in("span", handle)?,
        code,
        message.as_deref().unwrap_or(""),
    )
    .map_err(otel_err)
}

/// An `exception` event plus status error.
#[napi]
pub fn span_record_error(handle: f64, error_type: Option<String>, message: String) -> Result<()> {
    spans::span_record_error(
        handle_in("span", handle)?,
        error_type.as_deref().unwrap_or(""),
        &message,
    )
    .map_err(otel_err)
}

/// The span's W3C `traceparent`, or `null` for a no-op span.
#[napi]
pub fn span_context(handle: f64) -> Result<Option<String>> {
    let c = spans::span_context(handle_in("span", handle)?).map_err(otel_err)?;
    Ok(c.is_valid().then(|| c.traceparent()))
}

/// End the span and free its handle.
#[napi]
pub fn span_end(handle: f64) -> Result<()> {
    spans::span_end(handle_in("span", handle)?).map_err(otel_err)
}

/// Spans started and not yet ended (diagnostics).
#[napi]
pub fn live_spans() -> u32 {
    spans::live_spans() as u32
}
