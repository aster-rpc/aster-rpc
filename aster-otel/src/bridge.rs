//! The binding log bridge (design §3, phase O4): Rust `tracing` events reach
//! each binding's own logger through a bounded, drop-counting queue.
//!
//! ```text
//! tracing event ─► BindingLogLayer (bridge slot, filter F_bridge, reloadable)
//!                    ├─ target aster::audit ─► audit channel   (default 1024)
//!                    └─ everything else     ─► general channel (default 8192)
//!                                                  │
//!                       LogDrain::poll_into ◄──────┘ (audit preferred, batches)
//! ```
//!
//! * **Cost when filtered out.** The bridge's per-layer filter is an
//!   `EnvFilter` without span-field directives, so a disabled callsite's
//!   interest is cached as `never`: no record is built, nothing allocates.
//! * **The emitter never blocks** (the queue push is `try_send`), except
//!   in the opt-in audit mode behind the Q8 flag
//!   ([`crate::ObservabilityConfig::log_audit_block`], off by default).
//! * **Drops are counted**: [`dropped_total`] / [`audit_dropped_total`], the
//!   `aster.log.records_dropped` / `aster.log.audit_dropped` counters (§4.1),
//!   and one synthetic WARN record (target [`DROP_REPORT_TARGET`]) at most once
//!   per [`DROP_REPORT_INTERVAL`] from the drain. An audit drop also marks the
//!   audit path [`audit_degraded`] for [`AUDIT_DEGRADED_WINDOW`] (§3.5;
//!   `aster::ops` fails readiness on it).
//! * **No feedback.** Events emitted on a thread inside [`suppress`] (every
//!   [`LogDrain`] poll is) or on a thread marked with [`mark_drain_thread`] never
//!   reach the queue, nor do events under an OTel telemetry-suppressed context
//!   (the SDK's export threads) or on [`crate::EXPORTER_INTERNAL_TARGETS`]
//!   (filtered by target).
//! * **Context.** Inside an `rpc` span (O2's server span) a record carries the
//!   call id and `rpc.service` / `rpc.method` / `aster.peer`, and, when the
//!   span is exported, the OTel trace id and span id.
//!
//! The drain API is what the bindings wrap: the C ABI's `aster_log_poll` (O5),
//! the PyO3 drain thread (O6), the NAPI TSFN pump and `pollLogs` (O7).

use std::borrow::Cow;
use std::cell::Cell;
use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Select, Sender, TrySendError};
use opentelemetry::trace::TraceContextExt;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::config::{with_exporter_targets_off, ResolvedConfig};
use crate::error::{Error, Result};
use crate::subscriber::{env_filter, SlotFilter, Slots};

/// The reserved target of audit events (AUTH notes item 7, design §3.5).
/// Records on it, or below it, go to the audit channel.
pub const AUDIT_TARGET: &str = "aster::audit";
/// The target of the drain's synthetic drop report.
pub const DROP_REPORT_TARGET: &str = "aster::log";
/// Records one poll takes at most when the caller does not say (design §3.1).
pub const DEFAULT_BATCH: usize = 256;
/// The drop report is emitted at most this often.
pub const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// How long after the last audit drop [`audit_degraded`] stays true (§3.5).
pub const AUDIT_DEGRADED_WINDOW: Duration = Duration::from_secs(30);

// ── records ─────────────────────────────────────────────────────────────────

/// Severity, numbered as the C ABI's `aster_log_level_t` (§7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl LogLevel {
    fn from_tracing(l: &Level) -> Self {
        match *l {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            Level::INFO => LogLevel::Info,
            Level::DEBUG => LogLevel::Debug,
            Level::TRACE => LogLevel::Trace,
        }
    }

    /// `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE`.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }
}

/// One Rust event, owned, ready for a binding logger. Field-for-field what the
/// C ABI's `aster_log_record_t` (§7.2) points into.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    pub level: LogLevel,
    /// [`LogRecord::HAS_TRACE`] | [`LogRecord::IN_CALL`] |
    /// [`LogRecord::SYNTHETIC`] | [`LogRecord::AUDIT`].
    pub flags: u8,
    /// Wall clock when the event was recorded.
    pub timestamp_unix_ns: i64,
    /// Valid when [`LogRecord::HAS_TRACE`] is set, else zero.
    pub trace_id: [u8; 16],
    /// Valid when [`LogRecord::HAS_TRACE`] is set, else zero.
    pub span_id: [u8; 8],
    /// The `rpc` span's call id when [`LogRecord::IN_CALL`] is set, else 0.
    pub call_id: u64,
    /// The `tracing` target (`aster::rpc::dispatch`); see
    /// [`LogRecord::logger_name`].
    pub target: Cow<'static, str>,
    /// The event's `message` field, formatted with `Display`.
    pub message: String,
    /// The other event fields (values as strings), then `rpc.service`,
    /// `rpc.method`, `aster.peer` from the enclosing `rpc` span and the
    /// emitting thread's `thread.name`.
    pub fields: Vec<(Cow<'static, str>, String)>,
}

impl LogRecord {
    /// `trace_id` / `span_id` are valid.
    pub const HAS_TRACE: u8 = 1;
    /// `call_id` is valid.
    pub const IN_CALL: u8 = 2;
    /// Made by the bridge itself (the drop report).
    pub const SYNTHETIC: u8 = 4;
    /// Target [`AUDIT_TARGET`].
    pub const AUDIT: u8 = 8;

    pub fn has_trace(&self) -> bool {
        self.flags & Self::HAS_TRACE != 0
    }
    pub fn in_call(&self) -> bool {
        self.flags & Self::IN_CALL != 0
    }
    pub fn is_synthetic(&self) -> bool {
        self.flags & Self::SYNTHETIC != 0
    }
    pub fn is_audit(&self) -> bool {
        self.flags & Self::AUDIT != 0
    }

    /// The value of field `key`, if present.
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The target as a logger name (`a::b::c` → `a.b.c`, design §3.1).
    pub fn logger_name(&self) -> String {
        self.target.replace("::", ".")
    }

    fn drop_report(dropped: u64) -> Self {
        LogRecord {
            level: LogLevel::Warn,
            flags: Self::SYNTHETIC,
            timestamp_unix_ns: now_unix_ns(),
            trace_id: [0; 16],
            span_id: [0; 8],
            call_id: 0,
            target: Cow::Borrowed(DROP_REPORT_TARGET),
            message: format!("dropped {dropped} Rust log records: the log queue was full"),
            fields: vec![(Cow::Borrowed("dropped"), dropped.to_string())],
        }
    }
}

fn now_unix_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn is_audit_target(target: &str) -> bool {
    target
        .strip_prefix(AUDIT_TARGET)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
}

// ── reentrancy guard ────────────────────────────────────────────────────────

thread_local! {
    static SUPPRESS: Cell<u32> = const { Cell::new(0) };
    static DRAIN_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// While alive, events emitted on this thread do not reach the bridge. Nests.
#[must_use = "suppression ends when the guard is dropped"]
pub struct SuppressGuard {
    _not_send: std::marker::PhantomData<*const ()>,
}

/// Suppress the bridge on this thread until the guard drops. A binding wraps
/// its delivery to the native logger in this when the delivering thread is not
/// a dedicated drain thread (the NAPI `pollLogs` fallback on the JS thread).
pub fn suppress() -> SuppressGuard {
    SUPPRESS.with(|c| c.set(c.get() + 1));
    SuppressGuard {
        _not_send: std::marker::PhantomData,
    }
}

impl Drop for SuppressGuard {
    fn drop(&mut self) {
        SUPPRESS.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Mark the calling thread as a log drain for the rest of its life: nothing it
/// emits reaches the bridge, so a native log handler that calls back into Aster
/// cannot feed its own queue. For dedicated drain threads (PyO3's
/// `aster-log-drain`, the C ABI poll thread each FFI binding runs).
pub fn mark_drain_thread() {
    DRAIN_THREAD.with(|c| c.set(true));
}

/// True when events emitted on this thread are kept out of the bridge.
pub fn is_suppressed() -> bool {
    DRAIN_THREAD.with(Cell::get) || SUPPRESS.with(|c| c.get() > 0)
}

// ── counters ────────────────────────────────────────────────────────────────

static DROPPED: AtomicU64 = AtomicU64::new(0);
static AUDIT_DROPPED: AtomicU64 = AtomicU64::new(0);
/// Milliseconds since [`anchor`] of the last audit drop, plus one (0: never).
static LAST_AUDIT_DROP: AtomicU64 = AtomicU64::new(0);
static EVER_OPENED: AtomicBool = AtomicBool::new(false);

fn anchor() -> Instant {
    static ANCHOR: OnceLock<Instant> = OnceLock::new();
    *ANCHOR.get_or_init(Instant::now)
}

/// General records dropped because the queue was full, since the process
/// started (`aster.log.records_dropped`; the C ABI's `aster_log_dropped`).
pub fn dropped_total() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Audit records dropped because the audit channel was full, since the
/// process started (`aster.log.audit_dropped`).
pub fn audit_dropped_total() -> u64 {
    AUDIT_DROPPED.load(Ordering::Relaxed)
}

/// Whether a bridge was ever opened in this process. The `aster.log.*`
/// counters report only then (so pipelines without a bridge do not grow two
/// always-zero series), and so does the ops `audit` readiness check.
pub fn ever_opened() -> bool {
    EVER_OPENED.load(Ordering::Relaxed)
}

/// True for [`AUDIT_DEGRADED_WINDOW`] after an audit record was dropped: the
/// readiness signal of design §3.5 (`aster::ops` fails `/readyz` through its
/// `audit` check until a drop-free window passes).
pub fn audit_degraded() -> bool {
    let last = LAST_AUDIT_DROP.load(Ordering::Relaxed);
    if last == 0 {
        return false;
    }
    let now = anchor().elapsed().as_millis() as u64 + 1;
    now.saturating_sub(last) < AUDIT_DEGRADED_WINDOW.as_millis() as u64
}

fn note_audit_drop() {
    AUDIT_DROPPED.fetch_add(1, Ordering::Relaxed);
    let now = anchor().elapsed().as_millis() as u64 + 1;
    LAST_AUDIT_DROP.store(now, Ordering::Relaxed);
}

/// Register the `aster.log.*` counters (§4.1) on a freshly built meter
/// provider. Observable: the callbacks read the process-wide totals at each
/// collection, so they live and die with their provider.
pub(crate) fn register_instruments(meter: &opentelemetry::metrics::Meter) {
    meter
        .u64_observable_counter("aster.log.records_dropped")
        .with_description("Rust log records the binding log queue dropped because it was full")
        .with_callback(|o| {
            if ever_opened() {
                o.observe(dropped_total(), &[]);
            }
        })
        .build();
    meter
        .u64_observable_counter("aster.log.audit_dropped")
        .with_description("aster::audit records the audit channel dropped because it was full")
        .with_callback(|o| {
            if ever_opened() {
                o.observe(audit_dropped_total(), &[]);
            }
        })
        .build();
}

// ── the queue ───────────────────────────────────────────────────────────────

/// Two bounded channels (general and audit) plus a wake channel for close.
pub(crate) struct LogQueue {
    general_tx: Sender<LogRecord>,
    general_rx: Receiver<LogRecord>,
    audit_tx: Sender<LogRecord>,
    audit_rx: Receiver<LogRecord>,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
    audit_block: Option<Duration>,
    closed: AtomicBool,
    drain_taken: AtomicBool,
    /// General drops not yet reported by the drain.
    unreported: AtomicU64,
}

impl fmt::Debug for LogQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogQueue")
            .field("general", &self.general_rx.len())
            .field("audit", &self.audit_rx.len())
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}

impl LogQueue {
    fn new(general: usize, audit: usize, audit_block: Option<Duration>) -> Self {
        let (general_tx, general_rx) = crossbeam_channel::bounded(general.max(1));
        let (audit_tx, audit_rx) = crossbeam_channel::bounded(audit.max(1));
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        LogQueue {
            general_tx,
            general_rx,
            audit_tx,
            audit_rx,
            wake_tx,
            wake_rx,
            audit_block,
            closed: AtomicBool::new(false),
            drain_taken: AtomicBool::new(false),
            unreported: AtomicU64::new(0),
        }
    }

    /// Queue `rec`. Never blocks, except an audit record in the opt-in Q8
    /// mode (at most the configured `log_audit_block`). A full channel drops
    /// the record and counts it. After close, records are discarded.
    pub(crate) fn push(&self, rec: LogRecord) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        if rec.is_audit() {
            let sent = match self.audit_block {
                Some(d) => self.audit_tx.send_timeout(rec, d).is_ok(),
                None => self.audit_tx.try_send(rec).is_ok(),
            };
            if !sent {
                note_audit_drop();
            }
            return;
        }
        if let Err(TrySendError::Full(_)) = self.general_tx.try_send(rec) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            self.unreported.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.wake_tx.try_send(());
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Records waiting (general + audit).
    fn len(&self) -> usize {
        self.general_rx.len() + self.audit_rx.len()
    }
}

// ── the layer ───────────────────────────────────────────────────────────────

/// The `tracing` layer that turns enabled events into [`LogRecord`]s on the
/// bridge's queue. Lives in the global subscriber's bridge slot; its filter is
/// the slot's (see [`set_filter`]).
pub struct BindingLogLayer {
    queue: Arc<LogQueue>,
}

impl BindingLogLayer {
    fn new(queue: Arc<LogQueue>) -> Self {
        BindingLogLayer { queue }
    }
}

/// What a record takes from the enclosing `rpc` span. O2's INFO span names its
/// fields `rpc.service`, `rpc.method`, `aster.call_id`, `aster.peer`; the debug
/// span used when tracing is off names them `service`, `method`, `call_id`,
/// `peer`. Both are normalised to the INFO names.
#[derive(Default)]
struct RpcFields {
    call_id: Option<u64>,
    service: Option<String>,
    method: Option<String>,
    peer: Option<String>,
}

impl Visit for RpcFields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if matches!(field.name(), "aster.call_id" | "call_id") {
            self.call_id = Some(value);
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if let Ok(v) = u64::try_from(value) {
            self.record_u64(field, v);
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        let slot = match field.name() {
            "rpc.service" | "service" => &mut self.service,
            "rpc.method" | "method" => &mut self.method,
            "aster.peer" | "peer" => &mut self.peer,
            _ => return,
        };
        *slot = Some(value.to_owned());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if matches!(
            field.name(),
            "rpc.service" | "service" | "rpc.method" | "method" | "aster.peer" | "peer"
        ) {
            self.record_str(field, &format!("{value:?}"));
        }
    }
}

struct EventVisitor<'a> {
    rec: &'a mut LogRecord,
}

impl EventVisitor<'_> {
    fn push(&mut self, field: &Field, value: String) {
        self.rec.fields.push((Cow::Borrowed(field.name()), value));
    }
}

impl Visit for EventVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.rec.message.push_str(value);
        } else {
            self.push(field, value.to_owned());
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, value.to_string());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.push(field, value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, value.to_string());
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.push(field, value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.rec.message, "{value:?}");
        } else if !field.name().starts_with("log.") {
            // `log.*`: the `tracing-log` bridge's metadata fields.
            self.push(field, format!("{value:?}"));
        }
    }
}

impl<S> Layer<S> for BindingLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        // The slot filter only lets `rpc` spans through; check anyway, the
        // layer could be composed elsewhere.
        if attrs.metadata().name() != "rpc" {
            return;
        }
        let mut f = RpcFields::default();
        attrs.record(&mut f);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(f);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if is_suppressed() || opentelemetry::Context::is_current_telemetry_suppressed() {
            return;
        }
        let meta = event.metadata();
        let target = meta.target();
        let mut rec = LogRecord {
            level: LogLevel::from_tracing(meta.level()),
            flags: if is_audit_target(target) {
                LogRecord::AUDIT
            } else {
                0
            },
            timestamp_unix_ns: now_unix_ns(),
            trace_id: [0; 16],
            span_id: [0; 8],
            call_id: 0,
            target: Cow::Borrowed(target),
            message: String::new(),
            fields: Vec::new(),
        };
        event.record(&mut EventVisitor { rec: &mut rec });

        if let Some(scope) = ctx.event_scope(event) {
            for span in scope {
                let ext = span.extensions();
                let Some(f) = ext.get::<RpcFields>() else {
                    continue;
                };
                if let Some(id) = f.call_id {
                    rec.call_id = id;
                    rec.flags |= LogRecord::IN_CALL;
                }
                for (k, v) in [
                    ("rpc.service", &f.service),
                    ("rpc.method", &f.method),
                    ("aster.peer", &f.peer),
                ] {
                    if let Some(v) = v {
                        rec.fields.push((Cow::Borrowed(k), v.clone()));
                    }
                }
                break;
            }
        }

        // tracing-opentelemetry attaches the span's OTel context on enter, so
        // inside an exported `rpc` span the current context is that span's.
        opentelemetry::Context::map_current(|cx| {
            let span = cx.span();
            let sc = span.span_context();
            if sc.is_valid() {
                rec.trace_id = sc.trace_id().to_bytes();
                rec.span_id = sc.span_id().to_bytes();
                rec.flags |= LogRecord::HAS_TRACE;
            }
        });

        if let Some(name) = std::thread::current().name() {
            rec.fields
                .push((Cow::Borrowed("thread.name"), name.to_owned()));
        }
        self.queue.push(rec);
    }
}

// ── the open bridge ─────────────────────────────────────────────────────────

struct Open {
    queue: Arc<LogQueue>,
    /// The directives in effect, as the caller gave them.
    directives: String,
}

static OPEN: Mutex<Option<Open>> = Mutex::new(None);

fn open_state() -> MutexGuard<'static, Option<Open>> {
    OPEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// `directives` as the bridge filter: exporter-internal targets off, and audit
/// events admitted at INFO unless the directives name `aster::audit` (§3.5:
/// audit records are never dropped silently, a filter included).
fn bridge_directives(directives: &str) -> String {
    let mut d = with_exporter_targets_off(directives);
    let names_audit = directives.split(',').any(|x| {
        x.trim()
            .split(['=', '['])
            .next()
            .is_some_and(|t| is_audit_target(t.trim()))
    });
    if !names_audit {
        d.push(',');
        d.push_str(AUDIT_TARGET);
        d.push_str("=info");
    }
    d
}

/// Open the bridge into `slots` (called by `init` with `log_bridge`). A bridge
/// already open is kept.
pub(crate) fn open(cfg: &ResolvedConfig, slots: &Slots) {
    let mut g = open_state();
    if g.is_some() {
        return;
    }
    let queue = Arc::new(LogQueue::new(
        cfg.log_queue,
        cfg.log_audit_queue,
        cfg.log_audit_block,
    ));
    EVER_OPENED.store(true, Ordering::Relaxed);
    let layer: crate::subscriber::BoxedLayer = Box::new(BindingLogLayer::new(queue.clone()));
    let _ = slots.bridge.reload(Some(layer));
    let _ = slots
        .bridge_filter
        .reload(SlotFilter::bridge(env_filter(&bridge_directives(
            &cfg.log_filter,
        ))));
    *g = Some(Open {
        queue,
        directives: cfg.log_filter.clone(),
    });
}

/// Whether the bridge is open.
pub fn is_open() -> bool {
    open_state().is_some()
}

/// Close the bridge: the layer leaves the subscriber, the queue stops taking
/// records, and a drain gets what was still queued, then
/// [`Poll::Closed`]. Called by `shutdown` / the last handle drop; the C ABI's
/// `aster_log_close` maps here. No bridge: no-op.
pub fn close() {
    let open = open_state().take();
    let Some(open) = open else { return };
    if let Some(s) = crate::subscriber::slots() {
        let _ = s.bridge_filter.reload(SlotFilter::off());
        let _ = s.bridge.reload(None);
    }
    open.queue.close();
}

/// Replace the bridge's filter (`EnvFilter` directives, as `ASTER_LOG`): the
/// binding's `sync_level()` / `set_log_filter` hook (§3.1). Exporter-internal
/// targets stay off unless named. Rebuilds the callsite interest cache.
pub fn set_filter(directives: &str) -> Result<()> {
    tracing_subscriber::EnvFilter::try_new(directives)
        .map_err(|e| Error::Config(format!("log filter {directives:?}: {e}")))?;
    let mut g = open_state();
    let open = g.as_mut().ok_or(Error::LogBridgeNotOpen)?;
    let slots = crate::subscriber::slots().ok_or(Error::LogBridgeNotOpen)?;
    let _ = slots
        .bridge_filter
        .reload(SlotFilter::bridge(env_filter(&bridge_directives(
            directives,
        ))));
    open.directives = directives.to_owned();
    Ok(())
}

/// The bridge's filter directives as last set (by `init` or [`set_filter`]),
/// for the drain's periodic level re-check.
pub fn filter() -> Option<String> {
    open_state().as_ref().map(|o| o.directives.clone())
}

// ── the drain ───────────────────────────────────────────────────────────────

/// What [`LogDrain::poll_into`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    /// This many records were appended.
    Records(usize),
    /// Nothing arrived before the timeout.
    Timeout,
    /// The bridge was closed and everything queued before has been delivered.
    /// Later polls return `Closed` at once; take a new drain after a re-init.
    Closed,
}

/// The single consumer of the open bridge's queue. One per process at a time
/// ([`Error::LogDrainBusy`] otherwise); dropping it lets another be taken.
///
/// Each poll runs inside [`suppress`]. The binding delivers each batch to its
/// native logger outside the poll, so the delivering thread should be marked
/// with [`mark_drain_thread`] (a dedicated drain thread) or deliver inside
/// [`suppress`].
#[derive(Debug)]
pub struct LogDrain {
    queue: Arc<LogQueue>,
    last_report: Option<Instant>,
}

/// Take the drain of the open bridge.
pub fn drain() -> Result<LogDrain> {
    let queue = open_state()
        .as_ref()
        .map(|o| o.queue.clone())
        .ok_or(Error::LogBridgeNotOpen)?;
    if queue.drain_taken.swap(true, Ordering::AcqRel) {
        return Err(Error::LogDrainBusy);
    }
    Ok(LogDrain {
        queue,
        last_report: None,
    })
}

impl Drop for LogDrain {
    fn drop(&mut self) {
        self.queue.drain_taken.store(false, Ordering::Release);
    }
}

impl LogDrain {
    /// Append up to `max` records to `out`, waiting up to `timeout` for the
    /// first one (zero: do not wait). Audit records come first, then general
    /// records in FIFO order; a pending drop report (at most one per
    /// [`DROP_REPORT_INTERVAL`]) leads the batch. `out` is not cleared, so a
    /// caller can reuse one buffer as its per-batch arena.
    pub fn poll_into(&mut self, out: &mut Vec<LogRecord>, max: usize, timeout: Duration) -> Poll {
        let _quiet = suppress();
        let start = out.len();
        if max == 0 {
            return Poll::Records(0);
        }
        self.fill(out, max);
        if out.len() == start && !timeout.is_zero() && !self.queue.is_closed() {
            let deadline = Instant::now() + timeout;
            loop {
                let mut sel = Select::new();
                sel.recv(&self.queue.audit_rx);
                sel.recv(&self.queue.general_rx);
                sel.recv(&self.queue.wake_rx);
                if sel.ready_deadline(deadline).is_err() {
                    break;
                }
                self.fill(out, max);
                if out.len() > start || self.queue.is_closed() {
                    break;
                }
            }
        }
        let n = out.len() - start;
        if n > 0 {
            Poll::Records(n)
        } else if self.queue.is_closed() {
            Poll::Closed
        } else {
            Poll::Timeout
        }
    }

    /// [`Self::poll_into`] into a new vector: `None` once closed and empty.
    pub fn poll(&mut self, max: usize, timeout: Duration) -> Option<Vec<LogRecord>> {
        let mut out = Vec::new();
        match self.poll_into(&mut out, max, timeout) {
            Poll::Closed => None,
            _ => Some(out),
        }
    }

    /// Everything queued now, up to `max`, without waiting.
    pub fn try_drain(&mut self, max: usize) -> Vec<LogRecord> {
        let mut out = Vec::new();
        let _ = self.poll_into(&mut out, max, Duration::ZERO);
        out
    }

    /// The bridge this drain reads was closed (records may still be queued).
    pub fn is_closed(&self) -> bool {
        self.queue.is_closed()
    }

    /// Records waiting.
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    fn fill(&mut self, out: &mut Vec<LogRecord>, max: usize) {
        let start = out.len();
        let room = |out: &Vec<LogRecord>| out.len() - start < max;
        while self.queue.wake_rx.try_recv().is_ok() {}
        let due = self
            .last_report
            .is_none_or(|t| t.elapsed() >= DROP_REPORT_INTERVAL);
        if due && self.queue.unreported.load(Ordering::Relaxed) > 0 {
            let n = self.queue.unreported.swap(0, Ordering::Relaxed);
            out.push(LogRecord::drop_report(n));
            self.last_report = Some(Instant::now());
        }
        while room(out) {
            match self.queue.audit_rx.try_recv() {
                Ok(r) => out.push(r),
                Err(_) => break,
            }
        }
        while room(out) {
            match self.queue.general_rx.try_recv() {
                Ok(r) => out.push(r),
                Err(_) => break,
            }
        }
    }
}

// ── path B: application records into the Rust OTel logs SDK (§3.4) ─────────

/// An application log record a binding hands to the Rust OTel logs SDK
/// (design §3.4 path B; the C ABI's `aster_log_emit_t`).
#[derive(Debug, Clone, Default)]
pub struct AppLogRecord<'a> {
    pub level: Option<LogLevel>,
    /// Zero: now.
    pub timestamp_unix_ns: i64,
    /// The binding's logger name.
    pub logger: &'a str,
    pub message: &'a str,
    pub fields: &'a [(&'a str, &'a str)],
    /// The trace context to attach (`trace_id`, `span_id`, `trace_flags`).
    pub context: Option<([u8; 16], [u8; 8], u8)>,
}

/// Export `rec` through the running pipeline's OTel logs provider (path B).
/// `Ok` without exporting when no logs exporter is configured;
/// [`Error::Unsupported`] without feature `otlp-logs`. Never blocks (the SDK's
/// batch processor drops when its own queue is full), and emits no `tracing`
/// event, so it cannot feed the bridge.
#[cfg(feature = "otlp-logs")]
pub fn emit_app_log(rec: &AppLogRecord<'_>) -> Result<()> {
    use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _};
    use opentelemetry::trace::{SpanId, TraceFlags, TraceId};

    if crate::fork::forked() {
        return Ok(());
    }
    let Some(provider) = crate::pipeline::current_logger_provider() else {
        return Ok(());
    };
    let logger = provider.logger("aster.app");
    let mut r = logger.create_log_record();
    let level = rec.level.unwrap_or(LogLevel::Info);
    r.set_severity_number(severity(level));
    r.set_severity_text(level.as_str());
    let ts = if rec.timestamp_unix_ns > 0 {
        UNIX_EPOCH + Duration::from_nanos(rec.timestamp_unix_ns as u64)
    } else {
        SystemTime::now()
    };
    r.set_timestamp(ts);
    r.set_observed_timestamp(SystemTime::now());
    r.set_target(rec.logger.to_owned());
    r.set_body(AnyValue::from(rec.message.to_owned()));
    for (k, v) in rec.fields {
        r.add_attribute(k.to_string(), v.to_string());
    }
    if let Some((t, s, f)) = rec.context {
        r.set_trace_context(
            TraceId::from_bytes(t),
            SpanId::from_bytes(s),
            Some(TraceFlags::new(f)),
        );
    }
    logger.emit(r);
    Ok(())
}

/// Without feature `otlp-logs`: [`Error::Unsupported`].
#[cfg(not(feature = "otlp-logs"))]
pub fn emit_app_log(_rec: &AppLogRecord<'_>) -> Result<()> {
    Err(Error::Unsupported(
        "OTLP log export needs aster-otel feature `otlp-logs`",
    ))
}

#[cfg(feature = "otlp-logs")]
fn severity(l: LogLevel) -> opentelemetry::logs::Severity {
    use opentelemetry::logs::Severity;
    match l {
        LogLevel::Error => Severity::Error,
        LogLevel::Warn => Severity::Warn,
        LogLevel::Info => Severity::Info,
        LogLevel::Debug => Severity::Debug,
        LogLevel::Trace => Severity::Trace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(target: &'static str, msg: &str) -> LogRecord {
        LogRecord {
            level: LogLevel::Info,
            flags: if is_audit_target(target) {
                LogRecord::AUDIT
            } else {
                0
            },
            timestamp_unix_ns: 0,
            trace_id: [0; 16],
            span_id: [0; 8],
            call_id: 0,
            target: Cow::Borrowed(target),
            message: msg.to_owned(),
            fields: Vec::new(),
        }
    }

    fn drain_of(q: &Arc<LogQueue>) -> LogDrain {
        q.drain_taken.store(true, Ordering::Relaxed);
        LogDrain {
            queue: q.clone(),
            last_report: None,
        }
    }

    #[test]
    fn audit_targets() {
        assert!(is_audit_target("aster::audit"));
        assert!(is_audit_target("aster::audit::gate0"));
        assert!(!is_audit_target("aster::auditor"));
        assert!(!is_audit_target("aster::rpc"));
    }

    #[test]
    fn bridge_directives_keep_exporters_off_and_admit_audit() {
        assert_eq!(
            bridge_directives("warn"),
            "warn,reqwest=off,hyper=off,opentelemetry=off,aster::audit=info"
        );
        assert_eq!(
            bridge_directives("warn,aster::audit=off"),
            "warn,aster::audit=off,reqwest=off,hyper=off,opentelemetry=off"
        );
    }

    #[test]
    fn queue_is_fifo_and_counts_drops_with_one_report() {
        let q = Arc::new(LogQueue::new(4, 2, None));
        let before = dropped_total();
        for i in 0..6 {
            q.push(rec("aster::probe", &format!("m{i}")));
        }
        // Process-wide: other tests may drop too.
        assert!(dropped_total() - before >= 2);
        let mut d = drain_of(&q);
        let got = d.try_drain(100);
        assert!(got[0].is_synthetic());
        assert_eq!(got[0].field("dropped"), Some("2"));
        let msgs: Vec<&str> = got[1..].iter().map(|r| r.message.as_str()).collect();
        assert_eq!(msgs, ["m0", "m1", "m2", "m3"]);
        // A second report waits for the interval.
        for i in 0..6 {
            q.push(rec("aster::probe", &format!("n{i}")));
        }
        let got = d.try_drain(100);
        assert!(!got.iter().any(LogRecord::is_synthetic));
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn audit_is_reserved_and_preferred() {
        let q = Arc::new(LogQueue::new(2, 2, None));
        for i in 0..10 {
            q.push(rec("aster::probe", &format!("g{i}")));
        }
        q.push(rec("aster::audit", "a0"));
        q.push(rec("aster::audit", "a1"));
        let mut d = drain_of(&q);
        let got = d.try_drain(3);
        let msgs: Vec<&str> = got.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(msgs[1..], ["a0", "a1"]);
        assert!(got[0].is_synthetic());
    }

    #[test]
    fn poll_times_out_wakes_on_push_and_reports_close() {
        let q = Arc::new(LogQueue::new(8, 2, None));
        let mut d = drain_of(&q);
        let mut out = Vec::new();
        assert_eq!(
            d.poll_into(&mut out, 8, Duration::from_millis(20)),
            Poll::Timeout
        );
        let q2 = q.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            q2.push(rec("aster::probe", "late"));
        });
        assert_eq!(
            d.poll_into(&mut out, 8, Duration::from_secs(10)),
            Poll::Records(1)
        );
        t.join().unwrap();
        q.push(rec("aster::probe", "before-close"));
        q.close();
        q.push(rec("aster::probe", "after-close"));
        out.clear();
        assert_eq!(
            d.poll_into(&mut out, 8, Duration::from_secs(10)),
            Poll::Records(1)
        );
        assert_eq!(out[0].message, "before-close");
        let t0 = Instant::now();
        assert_eq!(
            d.poll_into(&mut out, 8, Duration::from_secs(10)),
            Poll::Closed
        );
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn close_wakes_a_blocked_poll() {
        let q = Arc::new(LogQueue::new(8, 2, None));
        let mut d = drain_of(&q);
        let q2 = q.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            q2.close();
        });
        let t0 = Instant::now();
        let mut out = Vec::new();
        assert_eq!(
            d.poll_into(&mut out, 8, Duration::from_secs(10)),
            Poll::Closed
        );
        assert!(t0.elapsed() < Duration::from_secs(5));
        t.join().unwrap();
    }

    #[test]
    fn suppression_nests_and_unwinds() {
        assert!(!is_suppressed());
        {
            let _a = suppress();
            let _b = suppress();
            assert!(is_suppressed());
        }
        assert!(!is_suppressed());
    }
}
