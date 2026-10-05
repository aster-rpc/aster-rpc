//! Observability (ABI 4.6): the Rust-owned OpenTelemetry pipeline, the log
//! bridge, binding metrics and spans, and trace context.
//!
//! Design: `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md` §7.1-§7.4.
//! Everything here is process-global (no `runtime` argument), like
//! `aster_abi_version` and `aster_last_error_message`.
//!
//! ```text
//!   aster_observability_init(cfg) / _flush(ms) / _shutdown(ms)   refcounted
//!   aster_log_poll(out[], sizeof, max, ms, &n) → status                   one poller
//!   aster_log_close() / aster_log_dropped(&n) / aster_log_set_filter(..)
//!   aster_log_emit(rec)                                           path B (OTLP logs)
//!   aster_instrument_new(desc) → h,  aster_attrset_new(kv) → set
//!   aster_metric_{add_u64,add_i64,record_f64}(h, v, set, kv, call_id)
//!   aster_metrics_prometheus(&text)
//!   aster_span_start(name, kind, parent, kv) → s,  aster_span_*(s, ..), aster_span_end(s)
//!   aster_span_record(rec)                                        finished span, own ids
//!   aster_trace_context_format / _parse                           traceparent codec
//! ```
//!
//! Strings are `aster_bytes_t` / pointer + length, UTF-8, not NUL-terminated.
//! k/v lists use the metadata encoding (`u32 LE count`, then per entry
//! `u32 LE len` + key, `u32 LE len` + value). Attribute values are strings in
//! 4.6 (design Q13).

use std::sync::{Mutex, TryLockError};
use std::time::Duration;

use aster_otel::bridge::{self, AppLogRecord, LogDrain, Poll};
use aster_otel::instruments::{self, InstrumentDesc, InstrumentKind, Measurement};
use aster_otel::spans::{self, SpanEventRecord, SpanKind, SpanRecord};
use aster_otel::{
    Binding, ExporterKind, LogLevel, LogRecord, ObservabilityConfig, ObservabilityHandle,
    OtlpHeaders, OtlpProtocol, SamplerConfig, TraceContext, TRACEPARENT_LEN,
};

use crate::server::{decode_kv, encode_kv};
use crate::{alloc_string, aster_bytes_t, aster_status_t, set_last_error};

// ============================================================================
// Shared helpers
// ============================================================================

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;

fn fail(s: aster_status_t, msg: impl ToString) -> i32 {
    set_last_error(msg);
    s as i32
}

fn invalid(msg: impl ToString) -> i32 {
    fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, msg)
}

/// The status for an `aster_otel` error; its message becomes the last error.
pub(crate) fn otel_status(e: &aster_otel::Error) -> i32 {
    use aster_otel::Error as E;
    let s = match e {
        E::Config(_) | E::InvalidArgument(_) => aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
        E::AlreadyInitialized(_) | E::LogDrainBusy => aster_status_t::ASTER_STATUS_ALREADY_EXISTS,
        E::Timeout(_) => aster_status_t::ASTER_STATUS_TIMEOUT,
        E::LogBridgeNotOpen | E::NotFound(_) => aster_status_t::ASTER_STATUS_NOT_FOUND,
        E::QueueFull(_) => aster_status_t::ASTER_STATUS_QUEUE_FULL,
        E::Unsupported(_) => aster_status_t::ASTER_STATUS_UNSUPPORTED,
        E::Exporter(_) | E::Sdk(_) => aster_status_t::ASTER_STATUS_INTERNAL,
    };
    fail(s, e)
}

/// `p` / `len` as a slice (`NULL` or 0 = empty).
///
/// # Safety
/// A non-NULL `p` must point to `len` readable bytes for `'a`.
unsafe fn raw<'a>(p: *const u8, len: usize) -> &'a [u8] {
    if p.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: the caller's contract.
        unsafe { std::slice::from_raw_parts(p, len) }
    }
}

/// # Safety
/// As [`raw`], for the bytes `b` describes.
unsafe fn bytes<'a>(b: &aster_bytes_t) -> &'a [u8] {
    // SAFETY: forwarded.
    unsafe { raw(b.ptr, b.len) }
}

/// # Safety
/// As [`raw`].
unsafe fn text<'a>(b: &aster_bytes_t, what: &str) -> Result<&'a str, i32> {
    // SAFETY: forwarded.
    std::str::from_utf8(unsafe { bytes(b) }).map_err(|_| invalid(format!("{what} is not UTF-8")))
}

/// # Safety
/// As [`raw`].
unsafe fn kv_list(p: *const u8, len: usize, what: &str) -> Result<Vec<(String, String)>, i32> {
    // SAFETY: forwarded.
    decode_kv(unsafe { raw(p, len) }).map_err(|e| invalid(format!("{what}: {e}")))
}

/// Call `f` off any tokio runtime: building or dropping the OTLP exporter's
/// blocking HTTP client inside a runtime panics (design §9.1), and a binding
/// may call init from a runtime thread. Panics become `INTERNAL`.
fn off_runtime<T: Send>(f: impl FnOnce() -> T + Send) -> Result<T, i32> {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("aster-otel-ffi".into())
            .spawn_scoped(s, f)
            .map_err(|e| fail(aster_status_t::ASTER_STATUS_INTERNAL, e))?
            .join()
            .map_err(|_| {
                fail(
                    aster_status_t::ASTER_STATUS_INTERNAL,
                    "observability call panicked",
                )
            })
    })
}

fn on_tokio_thread(what: &str) -> bool {
    if tokio::runtime::Handle::try_current().is_ok() {
        set_last_error(format!(
            "{what} blocks; call it from a dedicated thread, not a \
             tokio runtime thread"
        ));
        return true;
    }
    false
}

// ============================================================================
// 7.1 Pipeline
// ============================================================================

/// `aster_observability_config_t.binding`: the `aster.binding` resource
/// attribute.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_binding_kind_t {
    ASTER_BINDING_UNKNOWN = 0,
    ASTER_BINDING_RUST = 1,
    ASTER_BINDING_PYTHON = 2,
    ASTER_BINDING_TYPESCRIPT = 3,
    ASTER_BINDING_JAVA = 4,
    ASTER_BINDING_DOTNET = 5,
    ASTER_BINDING_GO = 6,
}

/// `aster_observability_config_t.protocol`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_otel_protocol_t {
    /// `OTEL_EXPORTER_OTLP_PROTOCOL`, else `http/protobuf`.
    ASTER_OTEL_PROTOCOL_DEFAULT = 0,
    ASTER_OTEL_PROTOCOL_HTTP_PROTOBUF = 1,
    /// Refused at init (this build exports over http/protobuf only).
    ASTER_OTEL_PROTOCOL_GRPC = 2,
}

/// `flags`: apply `OTEL_*` / `ASTER_*` env under the explicit fields.
pub const ASTER_OBSERVABILITY_FROM_ENV: u32 = 1;
/// `flags`: export traces over OTLP. Clear: env decides (with `FROM_ENV`),
/// else no trace export.
pub const ASTER_OBSERVABILITY_TRACES: u32 = 2;
/// `flags`: export metrics over OTLP. Clear: env decides (with `FROM_ENV`),
/// else no metric push export (a health listener still adds the Prometheus
/// pull reader).
pub const ASTER_OBSERVABILITY_METRICS: u32 = 4;
/// `flags`: export Rust log records over OTLP (path A). Clear: env decides
/// (with `FROM_ENV`), else off.
pub const ASTER_OBSERVABILITY_OTLP_LOGS: u32 = 8;
/// `flags`: open the log bridge queue for `aster_log_poll`.
pub const ASTER_OBSERVABILITY_LOG_BRIDGE: u32 = 16;

const ALL_FLAGS: u32 = ASTER_OBSERVABILITY_FROM_ENV
    | ASTER_OBSERVABILITY_TRACES
    | ASTER_OBSERVABILITY_METRICS
    | ASTER_OBSERVABILITY_OTLP_LOGS
    | ASTER_OBSERVABILITY_LOG_BRIDGE;

/// Pipeline configuration for `aster_observability_init`. 128 bytes (LP64).
/// Explicit fields beat `OTEL_*` / `ASTER_*` env (applied only with
/// `ASTER_OBSERVABILITY_FROM_ENV`), which beats `extra_toml`, which beats the
/// defaults. Empty strings and lists = not set.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct aster_observability_config_t {
    /// `sizeof(aster_observability_config_t)`.
    pub struct_size: u32,
    /// `ASTER_OBSERVABILITY_*`.
    pub flags: u32,
    /// `service.name`.
    pub service_name: aster_bytes_t,
    /// OTLP base URL (`/v1/<signal>` is appended).
    pub otlp_endpoint: aster_bytes_t,
    /// OTLP request headers as a k/v list. Secret: never logged or echoed.
    pub otlp_headers: aster_bytes_t,
    /// Extra resource attributes as a k/v list.
    pub resource_attrs: aster_bytes_t,
    /// `EnvFilter` directives for Rust events; empty = `ASTER_LOG` / default.
    pub log_filter: aster_bytes_t,
    /// An `[observability]` TOML table (the header line may be omitted):
    /// extractors, baggage, limits, exporters.
    pub extra_toml: aster_bytes_t,
    /// Parent-based trace-id-ratio sampler in `[0, 1]`; `< 0` = the sampler
    /// from env / default.
    pub trace_sample_ratio: f64,
    /// `aster_otel_protocol_t`.
    pub protocol: u32,
    /// Bound of the log bridge queue; 0 = default (8192).
    pub log_queue_capacity: u32,
    /// `aster_binding_kind_t`.
    pub binding: u32,
    pub _reserved: u32,
}

/// The C ABI's references to the pipeline, one per successful init.
static HANDLES: Mutex<Vec<ObservabilityHandle>> = Mutex::new(Vec::new());

fn lower_config(c: &aster_observability_config_t) -> Result<ObservabilityConfig, i32> {
    let size = std::mem::size_of::<aster_observability_config_t>();
    if c.struct_size as usize != size {
        return Err(invalid(format!(
            "aster_observability_config_t.struct_size {} != {size}",
            c.struct_size
        )));
    }
    if c.flags & !ALL_FLAGS != 0 {
        return Err(invalid(format!(
            "unknown observability flags {:#x}",
            c.flags
        )));
    }
    let from_env = c.flags & ASTER_OBSERVABILITY_FROM_ENV != 0;
    let signal = |flag: u32| -> Option<Vec<ExporterKind>> {
        if c.flags & flag != 0 {
            Some(vec![ExporterKind::Otlp])
        } else if from_env {
            None
        } else {
            Some(Vec::new())
        }
    };
    let mut cfg = ObservabilityConfig {
        from_env,
        traces_exporter: signal(ASTER_OBSERVABILITY_TRACES),
        metrics_exporter: signal(ASTER_OBSERVABILITY_METRICS),
        logs_exporter: signal(ASTER_OBSERVABILITY_OTLP_LOGS),
        ..ObservabilityConfig::default()
    };
    if c.flags & ASTER_OBSERVABILITY_LOG_BRIDGE != 0 {
        cfg.log_bridge = Some(true);
    }
    // SAFETY (all reads below): the caller passes valid `aster_bytes_t`s.
    let opt = |b: &aster_bytes_t, what: &str| -> Result<Option<String>, i32> {
        let s = unsafe { text(b, what) }?;
        Ok((!s.is_empty()).then(|| s.to_owned()))
    };
    cfg.service_name = opt(&c.service_name, "service_name")?;
    cfg.otlp.endpoint = opt(&c.otlp_endpoint, "otlp_endpoint")?;
    let headers = unsafe { kv_list(c.otlp_headers.ptr, c.otlp_headers.len, "otlp_headers") }?;
    if !headers.is_empty() {
        cfg.otlp.headers = Some(OtlpHeaders::new(headers));
    }
    cfg.resource_attributes =
        unsafe { kv_list(c.resource_attrs.ptr, c.resource_attrs.len, "resource_attrs") }?;
    cfg.log_filter = opt(&c.log_filter, "log_filter")?;
    if let Some(toml) = opt(&c.extra_toml, "extra_toml")? {
        let doc = if toml.contains("[observability]") {
            toml
        } else {
            format!("[observability]\n{toml}")
        };
        let file = ObservabilityConfig::from_toml_str(&doc).map_err(|e| otel_status(&e))?;
        cfg = cfg.with_file_layer(file);
    }
    if c.trace_sample_ratio >= 0.0 {
        if c.trace_sample_ratio > 1.0 {
            return Err(invalid("trace_sample_ratio must be in [0, 1] (or < 0)"));
        }
        cfg.sampler = Some(SamplerConfig::ParentBasedTraceIdRatio(c.trace_sample_ratio));
    } else if c.trace_sample_ratio.is_nan() {
        return Err(invalid("trace_sample_ratio is NaN"));
    }
    cfg.otlp.protocol = match c.protocol {
        0 => None,
        1 => Some(OtlpProtocol::HttpProtobuf),
        2 => Some(OtlpProtocol::Grpc),
        p => return Err(invalid(format!("unknown protocol {p}"))),
    };
    if c.log_queue_capacity > 0 {
        cfg.log_queue = Some(c.log_queue_capacity as usize);
    }
    cfg.binding = match c.binding {
        0 => None,
        1 => Some(Binding::Rust),
        2 => Some(Binding::Python),
        3 => Some(Binding::TypeScript),
        4 => Some(Binding::Java),
        5 => Some(Binding::DotNet),
        6 => Some(Binding::Go),
        b => return Err(invalid(format!("unknown binding kind {b}"))),
    };
    Ok(cfg)
}

/// Start the observability pipeline, or join the running one (refcount + 1).
/// `cfg` NULL = env only (`OTEL_*` / `ASTER_*`). A different configuration
/// than the running pipeline's returns `ASTER_STATUS_ALREADY_EXISTS` (the
/// message names the differing settings); `ASTER_OBSERVABILITY_LOG_BRIDGE`
/// is additive and opens the bridge on a running pipeline. Each `OK` must be
/// matched by one `aster_observability_shutdown`.
///
/// # Safety
/// `cfg` is NULL or points to a valid `aster_observability_config_t` whose
/// byte fields are readable for the call.
#[no_mangle]
pub unsafe extern "C" fn aster_observability_init(cfg: *const aster_observability_config_t) -> i32 {
    let cfg = match cfg.as_ref() {
        None => ObservabilityConfig::default(),
        Some(c) => match lower_config(c) {
            Ok(cfg) => cfg,
            Err(e) => return e,
        },
    };
    match off_runtime(move || aster_otel::init(cfg)) {
        Err(e) => e,
        Ok(Err(e)) => otel_status(&e),
        Ok(Ok(handle)) => {
            HANDLES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(handle);
            OK
        }
    }
}

/// Flush every signal of the running pipeline, waiting at most `timeout_ms`
/// (`ASTER_STATUS_TIMEOUT` otherwise). No pipeline: `OK`.
#[no_mangle]
pub extern "C" fn aster_observability_flush(timeout_ms: u32) -> i32 {
    match aster_otel::flush(Duration::from_millis(timeout_ms as u64)) {
        Ok(()) => OK,
        Err(e) => otel_status(&e),
    }
}

/// Release one reference taken by `aster_observability_init`. The last
/// reference (counting other holders, such as a health listener) shuts the
/// pipeline down: providers flush and stop, the log bridge closes, every live
/// `aster_span_t` ends and every instrument / attribute set handle is freed.
/// Waits at most `timeout_ms` (`ASTER_STATUS_TIMEOUT`; the shutdown then
/// completes in the background). No reference left: `ALREADY_CLOSED`.
#[no_mangle]
pub extern "C" fn aster_observability_shutdown(timeout_ms: u32) -> i32 {
    let Some(handle) = HANDLES.lock().unwrap_or_else(|e| e.into_inner()).pop() else {
        return fail(
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
            "no aster_observability_init reference to release",
        );
    };
    // Dropped on its own thread: the last drop flushes and joins exporter
    // threads, and must not run on a tokio thread (§9.1).
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("aster-otel-shutdown".into())
        .spawn(move || {
            drop(handle);
            let _ = tx.send(());
        });
    if let Err(e) = spawned {
        return fail(aster_status_t::ASTER_STATUS_INTERNAL, e);
    }
    match rx.recv_timeout(Duration::from_millis(timeout_ms as u64)) {
        Ok(()) => OK,
        Err(_) => fail(
            aster_status_t::ASTER_STATUS_TIMEOUT,
            "observability shutdown did not finish in time; it continues in the background",
        ),
    }
}

// ============================================================================
// 7.2 Logs
// ============================================================================

/// `aster_log_record_t.level`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_log_level_t {
    ASTER_LOG_ERROR = 1,
    ASTER_LOG_WARN = 2,
    ASTER_LOG_INFO = 3,
    ASTER_LOG_DEBUG = 4,
    ASTER_LOG_TRACE = 5,
}

/// `aster_log_record_t.flags`: `trace_id` / `span_id` are valid.
pub const ASTER_LOG_FLAG_HAS_TRACE: u8 = 1;
/// `call_id` is valid (the record was emitted inside a call's `rpc` span).
pub const ASTER_LOG_FLAG_IN_CALL: u8 = 2;
/// Made by the bridge itself (the drop report, target `aster::log`).
pub const ASTER_LOG_FLAG_SYNTHETIC: u8 = 4;
/// Target `aster::audit` (an audit record).
pub const ASTER_LOG_FLAG_AUDIT: u8 = 8;

/// One Rust log record, filled by `aster_log_poll`. 96 bytes (LP64). No
/// `struct_size`: the caller states the array stride to `aster_log_poll`.
/// The pointers stay valid until the next `aster_log_poll` or
/// `aster_log_close` (one arena per batch).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct aster_log_record_t {
    /// `aster_log_level_t`.
    pub level: u8,
    /// `ASTER_LOG_FLAG_*`.
    pub flags: u8,
    pub _p0: u16,
    pub _p1: u32,
    /// Wall clock when the event was recorded.
    pub timestamp_unix_ns: i64,
    /// Valid with `ASTER_LOG_FLAG_HAS_TRACE`, else zero.
    pub trace_id: [u8; 16],
    /// Valid with `ASTER_LOG_FLAG_HAS_TRACE`, else zero.
    pub span_id: [u8; 8],
    /// The call's id (`aster_dispatch_event_t.call_id` for a dispatched
    /// call) with `ASTER_LOG_FLAG_IN_CALL`, else 0.
    pub call_id: u64,
    /// The `tracing` target (`aster::rpc::dispatch`; logger name `a.b.c`).
    pub target_ptr: *const u8,
    pub target_len: u32,
    pub _p2: u32,
    /// The message.
    pub message_ptr: *const u8,
    pub message_len: u32,
    pub _p3: u32,
    /// k/v list: the event's fields, then `rpc.service`, `rpc.method`,
    /// `aster.peer` of the enclosing call and `thread.name`.
    pub fields_ptr: *const u8,
    pub fields_len: u32,
    pub _p4: u32,
}

/// The single poller's state: its drain and the batch arena the last poll's
/// pointers point into.
#[derive(Default)]
struct Poller {
    drain: Option<LogDrain>,
    records: Vec<LogRecord>,
    fields: Vec<Vec<u8>>,
}

impl Poller {
    fn clear(&mut self) {
        self.records.clear();
        self.fields.clear();
    }
}

static POLLER: Mutex<Option<Poller>> = Mutex::new(None);

fn ptr_len(b: &[u8]) -> (*const u8, u32) {
    if b.is_empty() {
        (std::ptr::null(), 0)
    } else {
        (b.as_ptr(), b.len() as u32)
    }
}

/// Lower `records` into `out[..records.len()]`, encoding each record's fields
/// into `fields` (cleared first). The written pointers borrow `records` and
/// `fields`.
///
/// # Safety
/// `out` must have room for `records.len()` records.
unsafe fn lower_records(
    records: &[LogRecord],
    fields: &mut Vec<Vec<u8>>,
    out: *mut aster_log_record_t,
) {
    fields.clear();
    fields.extend(records.iter().map(|r| {
        if r.fields.is_empty() {
            Vec::new()
        } else {
            encode_kv(r.fields.iter().map(|(k, v)| (k.as_ref(), v.as_str())))
        }
    }));
    for (i, (r, f)) in records.iter().zip(fields.iter()).enumerate() {
        let (target_ptr, target_len) = ptr_len(r.target.as_bytes());
        let (message_ptr, message_len) = ptr_len(r.message.as_bytes());
        let (fields_ptr, fields_len) = ptr_len(f);
        let rec = aster_log_record_t {
            level: r.level as u8,
            flags: r.flags,
            _p0: 0,
            _p1: 0,
            timestamp_unix_ns: r.timestamp_unix_ns,
            trace_id: r.trace_id,
            span_id: r.span_id,
            call_id: r.call_id,
            target_ptr,
            target_len,
            _p2: 0,
            message_ptr,
            message_len,
            _p3: 0,
            fields_ptr,
            fields_len,
            _p4: 0,
        };
        // SAFETY: the caller provides room for `records.len()` records.
        unsafe { std::ptr::write(out.add(i), rec) };
    }
}

/// Take up to `max` Rust log records into `out`, waiting up to `timeout_ms`
/// for the first (0 = return at once). The count goes to `*out_count`
/// (always written, 0 on any non-OK status): 0 with `OK` on timeout.
/// Audit records come first, then the rest in FIFO order; a synthetic drop
/// report (`ASTER_LOG_FLAG_SYNTHETIC`) leads a batch at most once a second.
///
/// Statuses: `NOT_FOUND` once the bridge is closed or not open;
/// `INVALID_ARGUMENT` for a NULL `out` / `out_count`, `max` 0 or a wrong
/// `record_size` (must equal `sizeof(aster_log_record_t)`, the array stride);
/// `ALREADY_EXISTS` for a concurrent second call (**one poller per process**:
/// it would free the batch the first is reading); `UNSUPPORTED` on a tokio
/// runtime thread: run it on a dedicated thread, which this call marks as a
/// log drain (events it emits never reach the bridge, so a logger that calls
/// back into Aster cannot feed its own queue). The returned pointers stay
/// valid until the next `aster_log_poll` or `aster_log_close`.
///
/// # Safety
/// `out` must have room for `max` records of `record_size` bytes;
/// `out_count` must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_log_poll(
    out: *mut aster_log_record_t,
    record_size: u32,
    max: u32,
    timeout_ms: u32,
    out_count: *mut u32,
) -> i32 {
    if out_count.is_null() {
        return invalid("out_count is NULL");
    }
    // SAFETY: checked non-null.
    unsafe { std::ptr::write(out_count, 0) };
    // SAFETY: forwarded caller contract.
    match unsafe { log_poll_inner(out, record_size, max, timeout_ms) } {
        Ok(n) => {
            // SAFETY: checked non-null.
            unsafe { std::ptr::write(out_count, n) };
            OK
        }
        Err(status) => status,
    }
}

unsafe fn log_poll_inner(
    out: *mut aster_log_record_t,
    record_size: u32,
    max: u32,
    timeout_ms: u32,
) -> Result<u32, i32> {
    if on_tokio_thread("aster_log_poll") {
        return Err(aster_status_t::ASTER_STATUS_UNSUPPORTED as i32);
    }
    if record_size as usize != std::mem::size_of::<aster_log_record_t>() {
        return Err(invalid(format!(
            "record_size {record_size} != sizeof(aster_log_record_t) {}",
            std::mem::size_of::<aster_log_record_t>()
        )));
    }
    if out.is_null() || max == 0 {
        return Err(invalid("out is NULL or max is 0"));
    }
    let mut guard = match POLLER.try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => {
            return Err(fail(
                aster_status_t::ASTER_STATUS_ALREADY_EXISTS,
                "another aster_log_poll is running (one poller per process)",
            ));
        }
    };
    bridge::mark_drain_thread();
    let poller = guard.get_or_insert_with(Poller::default);
    poller.clear();
    if poller.drain.is_none() {
        match bridge::drain() {
            Ok(d) => poller.drain = Some(d),
            Err(e) => {
                let status = match e {
                    aster_otel::Error::LogDrainBusy => aster_status_t::ASTER_STATUS_ALREADY_EXISTS,
                    _ => aster_status_t::ASTER_STATUS_NOT_FOUND,
                };
                return Err(fail(status, e));
            }
        }
    }
    let Some(drain) = poller.drain.as_mut() else {
        return Ok(0);
    };
    let timeout = Duration::from_millis(timeout_ms as u64);
    match drain.poll_into(&mut poller.records, max as usize, timeout) {
        Poll::Records(_) => {}
        Poll::Timeout => return Ok(0),
        Poll::Closed => {
            // Everything was delivered; a re-init opens a new queue, which
            // the next poll takes.
            poller.drain = None;
            return Err(fail(
                aster_status_t::ASTER_STATUS_NOT_FOUND,
                "the log bridge was closed",
            ));
        }
    }
    let Poller {
        records, fields, ..
    } = poller;
    // SAFETY: `poll_into` appended at most `max` records; `out` has room.
    unsafe { lower_records(records, fields, out) };
    Ok(records.len() as u32)
}

/// Close the log bridge: Rust events stop being queued, a waiting
/// `aster_log_poll` wakes (and gets what was still queued, then `NOT_FOUND`), and the
/// last batch's arena is freed when no poll is running. A later
/// `aster_observability_init` with `ASTER_OBSERVABILITY_LOG_BRIDGE` opens a
/// new bridge. `OK` also when no bridge is open.
#[no_mangle]
pub extern "C" fn aster_log_close() -> i32 {
    bridge::close();
    if let Ok(mut g) = POLLER.try_lock() {
        *g = None;
    }
    OK
}

/// General (non-audit) Rust log records dropped because the bridge queue
/// was full, since the process started (also metric
/// `aster.log.records_dropped`).
///
/// # Safety
/// `out_total` is NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn aster_log_dropped(out_total: *mut u64) -> i32 {
    if out_total.is_null() {
        return invalid("out_total is NULL");
    }
    // SAFETY: checked non-null.
    unsafe { std::ptr::write(out_total, bridge::dropped_total()) };
    OK
}

/// Replace the bridge's filter (`EnvFilter` directives, as `ASTER_LOG`; the
/// binding's level sync). `INVALID_ARGUMENT` on a parse error, `NOT_FOUND`
/// when no bridge is open.
///
/// # Safety
/// `directives` points to `len` readable bytes (or is NULL with `len` 0).
#[no_mangle]
pub unsafe extern "C" fn aster_log_set_filter(directives: *const u8, len: u32) -> i32 {
    // SAFETY: caller contract.
    let Ok(d) = std::str::from_utf8(unsafe { raw(directives, len as usize) }) else {
        return invalid("directives are not UTF-8");
    };
    match bridge::set_filter(d) {
        Ok(()) => OK,
        Err(e) => otel_status(&e),
    }
}

/// An application log record for `aster_log_emit`. 72 bytes (LP64).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct aster_log_emit_t {
    /// `sizeof(aster_log_emit_t)`.
    pub struct_size: u32,
    /// `aster_log_level_t`; 0 = INFO.
    pub level: u8,
    pub _p: [u8; 3],
    /// 0 = now.
    pub timestamp_unix_ns: i64,
    /// The binding's logger name.
    pub logger: aster_bytes_t,
    pub message: aster_bytes_t,
    /// k/v list of attributes.
    pub fields: aster_bytes_t,
    /// The trace context to attach; NULL = none.
    pub context: *const aster_span_context_t,
}

/// Path B (design §3.4): write an application log record into the Rust OTel
/// logs SDK (exported over OTLP when log export is on; `OK` and nothing
/// exported otherwise). Never blocks; emits no Rust event, so it cannot
/// reach the log bridge.
///
/// # Safety
/// `rec` points to a valid `aster_log_emit_t` whose byte fields and
/// `context` are readable for the call.
#[no_mangle]
pub unsafe extern "C" fn aster_log_emit(rec: *const aster_log_emit_t) -> i32 {
    // SAFETY: caller contract.
    let Some(r) = (unsafe { rec.as_ref() }) else {
        return invalid("rec is NULL");
    };
    if r.struct_size as usize != std::mem::size_of::<aster_log_emit_t>() {
        return invalid(format!(
            "aster_log_emit_t.struct_size {} != {}",
            r.struct_size,
            std::mem::size_of::<aster_log_emit_t>()
        ));
    }
    let level = match r.level {
        0 | 3 => LogLevel::Info,
        1 => LogLevel::Error,
        2 => LogLevel::Warn,
        4 => LogLevel::Debug,
        5 => LogLevel::Trace,
        l => return invalid(format!("unknown log level {l}")),
    };
    // SAFETY: caller contract for every pointer below.
    let (logger, message, fields) = unsafe {
        let logger = match text(&r.logger, "logger") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let message = match text(&r.message, "message") {
            Ok(s) => s,
            Err(e) => return e,
        };
        let fields = match kv_list(r.fields.ptr, r.fields.len, "fields") {
            Ok(f) => f,
            Err(e) => return e,
        };
        (logger, message, fields)
    };
    // SAFETY: NULL or readable.
    let context = unsafe { r.context.as_ref() }
        .map(lift_context)
        .filter(TraceContext::is_valid)
        .map(|c| (c.trace_id, c.span_id, c.trace_flags));
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let app = AppLogRecord {
        level: Some(level),
        timestamp_unix_ns: r.timestamp_unix_ns,
        logger,
        message,
        fields: &pairs,
        context,
    };
    match bridge::emit_app_log(&app) {
        Ok(()) => OK,
        Err(e) => otel_status(&e),
    }
}

// ============================================================================
// 7.3 Metrics
// ============================================================================

/// A binding-declared instrument (`aster_instrument_new`).
pub type aster_instrument_t = u64;
/// An interned attribute set (`aster_attrset_new`).
pub type aster_attrset_t = u64;

/// `aster_instrument_desc_t.kind`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_instrument_kind_t {
    /// `aster_metric_add_u64`.
    ASTER_INSTRUMENT_COUNTER_U64 = 1,
    /// `aster_metric_add_i64`.
    ASTER_INSTRUMENT_UP_DOWN_I64 = 2,
    /// `aster_metric_record_f64`.
    ASTER_INSTRUMENT_HISTOGRAM_F64 = 3,
    /// `aster_metric_record_f64` (last value).
    ASTER_INSTRUMENT_GAUGE_F64 = 4,
}

/// An instrument to declare. 96 bytes (LP64).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct aster_instrument_desc_t {
    /// `sizeof(aster_instrument_desc_t)`.
    pub struct_size: u32,
    /// `aster_instrument_kind_t`.
    pub kind: u32,
    /// OTel instrument name (ASCII letter first, then letters, digits, `_`,
    /// `.`, `-`, `/`; at most 255).
    pub name: aster_bytes_t,
    pub description: aster_bytes_t,
    /// UCUM unit; empty = none.
    pub unit: aster_bytes_t,
    /// Attribute keys taken from the current call when a record passes its
    /// `call_id`, as a list (`u32 LE count`, then `u32 LE len` + key ...).
    /// Kept; merging lands with the per-call extractor cache (O6+).
    pub auto_attrs: aster_bytes_t,
    /// Series limit; 0 = default (2000). Kept; 4.6 applies the default.
    pub cardinality_limit: u32,
    pub _p: u32,
    /// Histogram bucket upper bounds (`boundaries_len` doubles): finite and
    /// strictly increasing. `NULL` / 0 = none given: unit `s` gets the RPC
    /// seconds buckets (0.005 .. 10), any other unit the SDK default.
    /// `INVALID_ARGUMENT` on a non-histogram or on bad values.
    pub boundaries: *const f64,
    pub boundaries_len: u32,
    pub _p1: u32,
}

fn decode_key_list(mut b: &[u8]) -> Result<Vec<String>, String> {
    if b.is_empty() {
        return Ok(Vec::new());
    }
    fn u32_at(b: &mut &[u8]) -> Result<usize, String> {
        if b.len() < 4 {
            return Err("key list truncated".into());
        }
        let (h, rest) = b.split_at(4);
        *b = rest;
        Ok(u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize)
    }
    let n = u32_at(&mut b)?;
    if n > b.len() / 4 {
        return Err("key list count exceeds its length".into());
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let len = u32_at(&mut b)?;
        if b.len() < len {
            return Err("key list truncated".into());
        }
        let (k, rest) = b.split_at(len);
        b = rest;
        out.push(String::from_utf8(k.to_vec()).map_err(|_| "key list: invalid UTF-8".to_string())?);
    }
    if !b.is_empty() {
        return Err("key list has trailing bytes".into());
    }
    Ok(out)
}

/// Declare an instrument (scope `aster.app`). It records into whichever
/// pipeline is running at record time (a no-op without one). A name already
/// declared with another kind, or one breaking the OTel naming rules, is
/// `INVALID_ARGUMENT`. Free with `aster_instrument_free`; a pipeline
/// shutdown frees every handle.
///
/// # Safety
/// `d` points to a valid `aster_instrument_desc_t` whose byte fields are
/// readable for the call; `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_instrument_new(
    d: *const aster_instrument_desc_t,
    out: *mut aster_instrument_t,
) -> i32 {
    // SAFETY: caller contract.
    let (Some(d), false) = (unsafe { d.as_ref() }, out.is_null()) else {
        return invalid("desc or out is NULL");
    };
    if d.struct_size as usize != std::mem::size_of::<aster_instrument_desc_t>() {
        return invalid(format!(
            "aster_instrument_desc_t.struct_size {} != {}",
            d.struct_size,
            std::mem::size_of::<aster_instrument_desc_t>()
        ));
    }
    let Some(kind) = InstrumentKind::from_u32(d.kind) else {
        return invalid(format!("unknown instrument kind {}", d.kind));
    };
    // SAFETY: caller contract.
    let desc = unsafe {
        let s = |b: &aster_bytes_t, w: &str| text(b, w).map(str::to_owned);
        let name = match s(&d.name, "name") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let description = match s(&d.description, "description") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let unit = match s(&d.unit, "unit") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let auto_attrs = match decode_key_list(bytes(&d.auto_attrs)) {
            Ok(v) => v,
            Err(e) => return invalid(format!("auto_attrs: {e}")),
        };
        let boundaries = if d.boundaries.is_null() || d.boundaries_len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(d.boundaries, d.boundaries_len as usize).to_vec()
        };
        InstrumentDesc {
            kind,
            name,
            description,
            unit,
            auto_attrs,
            cardinality_limit: d.cardinality_limit,
            boundaries,
        }
    };
    match instruments::instrument_new(desc) {
        Ok(h) => {
            // SAFETY: checked non-null.
            unsafe { std::ptr::write(out, h) };
            OK
        }
        Err(e) => otel_status(&e),
    }
}

/// Free an instrument handle (`NOT_FOUND` if unknown).
#[no_mangle]
pub extern "C" fn aster_instrument_free(h: aster_instrument_t) -> i32 {
    if instruments::instrument_free(h) {
        OK
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

/// Intern an attribute set from a k/v list, to pass to `aster_metric_*`
/// without re-encoding (build once, record many times).
///
/// # Safety
/// `kv` points to `kv_len` readable bytes; `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_attrset_new(
    kv: *const u8,
    kv_len: u32,
    out: *mut aster_attrset_t,
) -> i32 {
    if out.is_null() {
        return invalid("out is NULL");
    }
    // SAFETY: caller contract.
    let pairs = match unsafe { kv_list(kv, kv_len as usize, "kv") } {
        Ok(p) => p,
        Err(e) => return e,
    };
    match instruments::attrset_new(pairs) {
        Ok(h) => {
            // SAFETY: checked non-null.
            unsafe { std::ptr::write(out, h) };
            OK
        }
        Err(e) => otel_status(&e),
    }
}

/// Free an attribute set handle (`NOT_FOUND` if unknown).
#[no_mangle]
pub extern "C" fn aster_attrset_free(h: aster_attrset_t) -> i32 {
    if instruments::attrset_free(h) {
        OK
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

/// # Safety
/// `kv` points to `kv_len` readable bytes.
unsafe fn metric(
    h: u64,
    v: Measurement,
    set: u64,
    kv: *const u8,
    kv_len: u32,
    call_id: u64,
) -> i32 {
    // SAFETY: forwarded.
    let extra = match unsafe { kv_list(kv, kv_len as usize, "kv") } {
        Ok(p) => p,
        Err(e) => return e,
    };
    match instruments::record(h, v, set, &extra, call_id) {
        Ok(()) => OK,
        Err(e) => otel_status(&e),
    }
}

/// Add `v` to a `COUNTER_U64` instrument with the attributes of `set` (0 =
/// none) followed by the k/v list `kv`. Never blocks on I/O and never calls
/// into the runtime (a short SDK lock; a new series takes it for writing).
/// `call_id` is reserved for `auto_attrs` (0 = none).
///
/// # Safety
/// `kv` points to `kv_len` readable bytes (or is NULL with `kv_len` 0).
#[no_mangle]
pub unsafe extern "C" fn aster_metric_add_u64(
    h: aster_instrument_t,
    v: u64,
    set: aster_attrset_t,
    kv: *const u8,
    kv_len: u32,
    call_id: u64,
) -> i32 {
    // SAFETY: forwarded.
    unsafe { metric(h, Measurement::U64(v), set, kv, kv_len, call_id) }
}

/// Add `v` to an `UP_DOWN_I64` instrument (see `aster_metric_add_u64`).
///
/// # Safety
/// As `aster_metric_add_u64`.
#[no_mangle]
pub unsafe extern "C" fn aster_metric_add_i64(
    h: aster_instrument_t,
    v: i64,
    set: aster_attrset_t,
    kv: *const u8,
    kv_len: u32,
    call_id: u64,
) -> i32 {
    // SAFETY: forwarded.
    unsafe { metric(h, Measurement::I64(v), set, kv, kv_len, call_id) }
}

/// Record `v` on a `HISTOGRAM_F64` or `GAUGE_F64` instrument (see
/// `aster_metric_add_u64`).
///
/// # Safety
/// As `aster_metric_add_u64`.
#[no_mangle]
pub unsafe extern "C" fn aster_metric_record_f64(
    h: aster_instrument_t,
    v: f64,
    set: aster_attrset_t,
    kv: *const u8,
    kv_len: u32,
    call_id: u64,
) -> i32 {
    // SAFETY: forwarded.
    unsafe { metric(h, Measurement::F64(v), set, kv, kv_len, call_id) }
}

/// The running pipeline's metrics as Prometheus text (format 0.0.4): the
/// built-in `aster.*` instruments, binding instruments, `process.uptime`,
/// `target_info` -- what a health listener's `/metrics` serves, without
/// HTTP. Needs the Prometheus pull reader: a running health listener
/// (`aster_health_start`), or `OTEL_METRICS_EXPORTER=prometheus` /
/// `metrics_exporter = "prometheus"` in `extra_toml`; otherwise `NOT_FOUND`.
/// `*out` is library-owned; free with `aster_string_release(out->ptr,
/// out->len)`. Replaces `aster_node_metrics_prometheus` (removed in 4.6).
///
/// # Safety
/// `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_metrics_prometheus(out: *mut aster_bytes_t) -> i32 {
    if out.is_null() {
        return invalid("out is NULL");
    }
    match aster::ops::metrics_text() {
        Some(text) => {
            // SAFETY: checked non-null.
            unsafe { std::ptr::write(out, alloc_string(text)) };
            OK
        }
        None => fail(
            aster_status_t::ASTER_STATUS_NOT_FOUND,
            "no Prometheus pull reader: start a health listener or configure the prometheus \
             metrics exporter",
        ),
    }
}

// ============================================================================
// 7.4 Spans and trace context
// ============================================================================

/// A span context. 32 bytes. All-zero `trace_id` = none.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct aster_span_context_t {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    /// W3C trace flags; bit 0 = sampled.
    pub trace_flags: u8,
    /// 1 when the context came from another process.
    pub is_remote: u8,
    pub _p0: u16,
    pub _p1: u32,
}

/// A binding span (`aster_span_start`).
pub type aster_span_t = u64;

/// Span kind.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_span_kind_t {
    ASTER_SPAN_INTERNAL = 0,
    ASTER_SPAN_SERVER = 1,
    ASTER_SPAN_CLIENT = 2,
    ASTER_SPAN_PRODUCER = 3,
    ASTER_SPAN_CONSUMER = 4,
}

pub(crate) fn lift_context(c: &aster_span_context_t) -> TraceContext {
    TraceContext {
        trace_id: c.trace_id,
        span_id: c.span_id,
        trace_flags: c.trace_flags,
        is_remote: c.is_remote != 0,
    }
}

pub(crate) fn lower_context(c: &TraceContext) -> aster_span_context_t {
    aster_span_context_t {
        trace_id: c.trace_id,
        span_id: c.span_id,
        trace_flags: c.trace_flags,
        is_remote: c.is_remote as u8,
        _p0: 0,
        _p1: 0,
    }
}

fn span_kind(k: u32) -> Result<SpanKind, i32> {
    SpanKind::from_u32(k).ok_or_else(|| invalid(format!("unknown span kind {k}")))
}

fn span_result(r: aster_otel::Result<()>) -> i32 {
    match r {
        Ok(()) => OK,
        Err(e) => otel_status(&e),
    }
}

/// Start a span in the Rust tracer (scope `aster.app`), exported with
/// Aster's own spans and sharing their sampling. `parent` NULL (or an
/// all-zero context) = a root. `kv` = string attributes. Without a pipeline
/// the span is a no-op (its context is all-zero). At most 65 536 live spans
/// (`QUEUE_FULL` beyond); a pipeline shutdown ends the ones still open.
///
/// # Safety
/// `name` and `kv` are readable for the call; `parent` is NULL or readable;
/// `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_span_start(
    name: aster_bytes_t,
    kind: u32,
    parent: *const aster_span_context_t,
    kv: *const u8,
    kv_len: u32,
    out: *mut aster_span_t,
) -> i32 {
    if out.is_null() {
        return invalid("out is NULL");
    }
    let kind = match span_kind(kind) {
        Ok(k) => k,
        Err(e) => return e,
    };
    // SAFETY: caller contract.
    let (name, attrs, parent) = unsafe {
        let name = match text(&name, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        let attrs = match kv_list(kv, kv_len as usize, "kv") {
            Ok(a) => a,
            Err(e) => return e,
        };
        (name, attrs, parent.as_ref().map(lift_context))
    };
    match spans::span_start(name, kind, parent.as_ref(), &attrs) {
        Ok(h) => {
            // SAFETY: checked non-null.
            unsafe { std::ptr::write(out, h) };
            OK
        }
        Err(e) => otel_status(&e),
    }
}

/// Add string attributes to a live span.
///
/// # Safety
/// `kv` points to `kv_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_span_set_attributes(
    s: aster_span_t,
    kv: *const u8,
    kv_len: u32,
) -> i32 {
    // SAFETY: caller contract.
    match unsafe { kv_list(kv, kv_len as usize, "kv") } {
        Ok(a) => span_result(spans::span_set_attributes(s, &a)),
        Err(e) => e,
    }
}

/// Add an event (timestamped now) to a live span.
///
/// # Safety
/// `name` and `kv` are readable for the call.
#[no_mangle]
pub unsafe extern "C" fn aster_span_add_event(
    s: aster_span_t,
    name: aster_bytes_t,
    kv: *const u8,
    kv_len: u32,
) -> i32 {
    // SAFETY: caller contract.
    let (name, attrs) = unsafe {
        let name = match text(&name, "name") {
            Ok(n) => n,
            Err(e) => return e,
        };
        match kv_list(kv, kv_len as usize, "kv") {
            Ok(a) => (name, a),
            Err(e) => return e,
        }
    };
    span_result(spans::span_add_event(s, name, &attrs))
}

/// Set a live span's status: `code` 0 unset, 1 ok, 2 error (with `message`).
///
/// # Safety
/// `message` is readable for the call.
#[no_mangle]
pub unsafe extern "C" fn aster_span_set_status(
    s: aster_span_t,
    code: u32,
    message: aster_bytes_t,
) -> i32 {
    // SAFETY: caller contract.
    match unsafe { text(&message, "message") } {
        Ok(m) => span_result(spans::span_set_status(s, code, m)),
        Err(e) => e,
    }
}

/// Record an error on a live span: an `exception` event with
/// `exception.type` / `exception.message`, and status error.
///
/// # Safety
/// `error_type` and `message` are readable for the call.
#[no_mangle]
pub unsafe extern "C" fn aster_span_record_error(
    s: aster_span_t,
    error_type: aster_bytes_t,
    message: aster_bytes_t,
) -> i32 {
    // SAFETY: caller contract.
    let (t, m) = unsafe {
        match (text(&error_type, "type"), text(&message, "message")) {
            (Ok(t), Ok(m)) => (t, m),
            (Err(e), _) | (_, Err(e)) => return e,
        }
    };
    span_result(spans::span_record_error(s, t, m))
}

/// A live span's context (all-zero for a no-op span), for the binding's
/// call-context carrier and for `aster_trace_context_format`.
///
/// # Safety
/// `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_span_context(
    s: aster_span_t,
    out: *mut aster_span_context_t,
) -> i32 {
    if out.is_null() {
        return invalid("out is NULL");
    }
    match spans::span_context(s) {
        Ok(c) => {
            // SAFETY: checked non-null.
            unsafe { std::ptr::write(out, lower_context(&c)) };
            OK
        }
        Err(e) => otel_status(&e),
    }
}

/// End a live span and free its handle.
#[no_mangle]
pub extern "C" fn aster_span_end(s: aster_span_t) -> i32 {
    span_result(spans::span_end(s))
}

/// A finished span whose ids, times and content the binding owns (the .NET
/// `Activity` bridge, design §5.3). 152 bytes (LP64).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct aster_span_record_t {
    /// Trace id, span id and flags of the finished span. A clear sampled
    /// flag means it is not exported.
    pub context: aster_span_context_t,
    /// All-zero `trace_id` = root; otherwise it must share `context.trace_id`.
    pub parent: aster_span_context_t,
    pub start_unix_nanos: u64,
    pub end_unix_nanos: u64,
    /// `aster_span_kind_t`.
    pub kind: u32,
    /// 0 unset, 1 ok, 2 error.
    pub status_code: u32,
    pub name: aster_bytes_t,
    pub status_message: aster_bytes_t,
    /// k/v list of attributes.
    pub kv: *const u8,
    pub kv_len: u32,
    pub _p0: u32,
    /// Events: `u32 LE count`, then per event `u32 LE len` + name,
    /// `u64 LE unix_nanos`, `u32 LE len` + a k/v list of its attributes.
    pub events: *const u8,
    pub events_len: u32,
    pub _p1: u32,
}

fn decode_events(mut b: &[u8]) -> Result<Vec<SpanEventRecord>, String> {
    if b.is_empty() {
        return Ok(Vec::new());
    }
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
        if b.len() < n {
            return Err("events truncated".into());
        }
        let (h, rest) = b.split_at(n);
        *b = rest;
        Ok(h)
    }
    fn u32_of(b: &mut &[u8]) -> Result<usize, String> {
        let h = take(b, 4)?;
        Ok(u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize)
    }
    let n = u32_of(&mut b)?;
    // Each event takes at least 16 bytes (two lengths and a timestamp).
    if n > b.len() / 16 {
        return Err("events count exceeds their length".into());
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let len = u32_of(&mut b)?;
        let name = std::str::from_utf8(take(&mut b, len)?)
            .map_err(|_| "event name: invalid UTF-8".to_string())?
            .to_owned();
        let t = take(&mut b, 8)?;
        let time_unix_nanos = u64::from_le_bytes([t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7]]);
        let kv_len = u32_of(&mut b)?;
        let attributes = decode_kv(take(&mut b, kv_len)?)?;
        out.push(SpanEventRecord {
            name,
            time_unix_nanos,
            attributes,
        });
    }
    if !b.is_empty() {
        return Err("events have trailing bytes".into());
    }
    Ok(out)
}

/// Export a finished span with the binding's own ids and times. Copies
/// everything; never blocks. The pipeline's sampler still runs (the default
/// parent-based one follows the parent's flag). `INVALID_ARGUMENT` for an
/// all-zero context, a parent from another trace, an end before the start,
/// or a malformed field.
///
/// # Safety
/// `rec` points to a valid `aster_span_record_t` whose pointers are
/// readable for the call.
#[no_mangle]
pub unsafe extern "C" fn aster_span_record(rec: *const aster_span_record_t) -> i32 {
    // SAFETY: caller contract.
    let Some(r) = (unsafe { rec.as_ref() }) else {
        return invalid("rec is NULL");
    };
    let kind = match span_kind(r.kind) {
        Ok(k) => k,
        Err(e) => return e,
    };
    // SAFETY: caller contract for every pointer below.
    let record = unsafe {
        let name = match text(&r.name, "name") {
            Ok(n) => n.to_owned(),
            Err(e) => return e,
        };
        let status_message = match text(&r.status_message, "status_message") {
            Ok(m) => m.to_owned(),
            Err(e) => return e,
        };
        let attributes = match kv_list(r.kv, r.kv_len as usize, "kv") {
            Ok(a) => a,
            Err(e) => return e,
        };
        let events = match decode_events(raw(r.events, r.events_len as usize)) {
            Ok(e) => e,
            Err(e) => return invalid(e),
        };
        SpanRecord {
            context: lift_context(&r.context),
            parent: lift_context(&r.parent),
            start_unix_nanos: r.start_unix_nanos,
            end_unix_nanos: r.end_unix_nanos,
            kind,
            status_code: r.status_code,
            status_message,
            name,
            attributes,
            events,
        }
    };
    span_result(spans::span_record(&record))
}

/// Write the W3C `traceparent` of `ctx` (55 bytes, version `00`, lowercase)
/// into `buf` for outbound call metadata. `cap < 55`: `BUFFER_TOO_SMALL`
/// with `*out_len` = 55. An all-zero id: `INVALID_ARGUMENT`.
///
/// # Safety
/// `ctx` is readable; `buf` has room for `cap` bytes; `out_len` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_trace_context_format(
    ctx: *const aster_span_context_t,
    buf: *mut u8,
    cap: u32,
    out_len: *mut u32,
) -> i32 {
    // SAFETY: caller contract.
    let Some(c) = (unsafe { ctx.as_ref() }) else {
        return invalid("ctx is NULL");
    };
    if out_len.is_null() {
        return invalid("out_len is NULL");
    }
    let c = lift_context(c);
    if !c.is_valid() {
        return invalid("span context has an all-zero trace or span id");
    }
    // SAFETY: checked non-null.
    unsafe { std::ptr::write(out_len, TRACEPARENT_LEN as u32) };
    if buf.is_null() || (cap as usize) < TRACEPARENT_LEN {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }
    let mut text = [0u8; TRACEPARENT_LEN];
    c.write_traceparent(&mut text);
    // SAFETY: `buf` has room for `cap >= 55` bytes.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), buf, TRACEPARENT_LEN) };
    OK
}

/// Parse a W3C `traceparent` (from inbound metadata) into `*out` (marked
/// remote). `INVALID_ARGUMENT` when malformed or an id is all-zero.
///
/// # Safety
/// `s` points to `len` readable bytes; `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_trace_context_parse(
    s: *const u8,
    len: u32,
    out: *mut aster_span_context_t,
) -> i32 {
    if out.is_null() {
        return invalid("out is NULL");
    }
    // SAFETY: caller contract.
    match TraceContext::parse_traceparent(unsafe { raw(s, len as usize) }) {
        Some(c) => {
            // SAFETY: checked non-null.
            unsafe { std::ptr::write(out, lower_context(&c)) };
            OK
        }
        None => invalid("malformed traceparent"),
    }
}

// Pure parts (the arena lowering, the codecs). Also run under Miri
// (`rust-nightly.yml`, `cargo miri test -p aster_transport_ffi --lib`).
#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    fn record(target: &'static str, message: &str, fields: &[(&'static str, &str)]) -> LogRecord {
        LogRecord {
            level: LogLevel::Warn,
            flags: ASTER_LOG_FLAG_IN_CALL,
            timestamp_unix_ns: 7,
            trace_id: [1; 16],
            span_id: [2; 8],
            call_id: 42,
            target: Cow::Borrowed(target),
            message: message.to_owned(),
            fields: fields
                .iter()
                .map(|(k, v)| (Cow::Borrowed(*k), v.to_string()))
                .collect(),
        }
    }

    unsafe fn view<'a>(p: *const u8, len: u32) -> &'a [u8] {
        // SAFETY: the test's own arena.
        unsafe { raw(p, len as usize) }
    }

    #[test]
    fn arena_lowering_points_into_the_batch() {
        let records = vec![
            record("aster::rpc::dispatch", "hello", &[("rpc.method", "Echo")]),
            record("aster::log", "", &[]),
        ];
        let mut fields = Vec::new();
        let mut out = [std::mem::MaybeUninit::<aster_log_record_t>::uninit(); 2];
        // SAFETY: room for two records.
        unsafe { lower_records(&records, &mut fields, out.as_mut_ptr().cast()) };
        // SAFETY: both written above.
        let out: [aster_log_record_t; 2] = unsafe { std::mem::transmute(out) };
        assert_eq!(out[0].level, LogLevel::Warn as u8);
        assert_eq!(out[0].call_id, 42);
        assert_eq!(out[0].trace_id, [1; 16]);
        // SAFETY: pointers into `records` / `fields`, alive here.
        unsafe {
            assert_eq!(
                view(out[0].target_ptr, out[0].target_len),
                b"aster::rpc::dispatch"
            );
            assert_eq!(view(out[0].message_ptr, out[0].message_len), b"hello");
            let kv = decode_kv(view(out[0].fields_ptr, out[0].fields_len)).unwrap();
            assert_eq!(kv, [("rpc.method".to_string(), "Echo".to_string())]);
        }
        assert!(out[1].message_ptr.is_null() && out[1].message_len == 0);
        assert!(out[1].fields_ptr.is_null() && out[1].fields_len == 0);
    }

    #[test]
    fn traceparent_round_trip_through_the_abi() {
        let tp = b"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        let mut cx = aster_span_context_t::default();
        // SAFETY: valid pointers.
        unsafe {
            assert_eq!(
                aster_trace_context_parse(tp.as_ptr(), tp.len() as u32, &mut cx),
                OK
            );
        }
        assert_eq!(cx.is_remote, 1);
        assert_eq!(cx.trace_flags, 1);
        let mut buf = [0u8; 64];
        let mut len = 0u32;
        // SAFETY: valid pointers.
        unsafe {
            assert_eq!(
                aster_trace_context_format(&cx, buf.as_mut_ptr(), 10, &mut len),
                aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32
            );
            assert_eq!(len, 55);
            assert_eq!(
                aster_trace_context_format(&cx, buf.as_mut_ptr(), 64, &mut len),
                OK
            );
        }
        assert_eq!(&buf[..55], tp);
        // SAFETY: valid pointers.
        unsafe {
            let bad = b"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-0";
            assert_eq!(
                aster_trace_context_parse(bad.as_ptr(), bad.len() as u32, &mut cx),
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
            );
            assert_eq!(
                aster_trace_context_format(std::ptr::null(), buf.as_mut_ptr(), 64, &mut len),
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
            );
            let zero = aster_span_context_t::default();
            assert_eq!(
                aster_trace_context_format(&zero, buf.as_mut_ptr(), 64, &mut len),
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
            );
        }
    }

    fn event(name: &str, t: u64, kv: &[(&str, &str)]) -> Vec<u8> {
        let kvb = encode_kv(kv.iter().copied());
        let mut b = Vec::new();
        b.extend_from_slice(&(name.len() as u32).to_le_bytes());
        b.extend_from_slice(name.as_bytes());
        b.extend_from_slice(&t.to_le_bytes());
        b.extend_from_slice(&(kvb.len() as u32).to_le_bytes());
        b.extend_from_slice(&kvb);
        b
    }

    #[test]
    fn events_and_key_lists_decode_and_reject_garbage() {
        let mut b = 2u32.to_le_bytes().to_vec();
        b.extend(event("retry", 5, &[("attempt", "2")]));
        b.extend(event("done", 9, &[]));
        let ev = decode_events(&b).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].name, "retry");
        assert_eq!(ev[0].time_unix_nanos, 5);
        assert_eq!(ev[0].attributes, [("attempt".into(), "2".into())]);
        assert!(decode_events(&b[..b.len() - 1]).is_err());
        assert!(decode_events(&u32::MAX.to_le_bytes()).is_err());
        let mut extra = b.clone();
        extra.push(0);
        assert!(decode_events(&extra).is_err());

        let mut keys = 2u32.to_le_bytes().to_vec();
        for k in ["tenant.id", "region"] {
            keys.extend_from_slice(&(k.len() as u32).to_le_bytes());
            keys.extend_from_slice(k.as_bytes());
        }
        assert_eq!(decode_key_list(&keys).unwrap(), ["tenant.id", "region"]);
        assert!(decode_key_list(&keys[..keys.len() - 1]).is_err());
        assert!(decode_key_list(&[]).unwrap().is_empty());
    }

    #[test]
    fn config_lowering_validates() {
        let empty = aster_bytes_t {
            ptr: std::ptr::null(),
            len: 0,
        };
        let base = aster_observability_config_t {
            struct_size: std::mem::size_of::<aster_observability_config_t>() as u32,
            flags: ASTER_OBSERVABILITY_TRACES | ASTER_OBSERVABILITY_LOG_BRIDGE,
            service_name: aster_bytes_t {
                ptr: b"svc".as_ptr(),
                len: 3,
            },
            otlp_endpoint: empty,
            otlp_headers: empty,
            resource_attrs: empty,
            log_filter: empty,
            extra_toml: empty,
            trace_sample_ratio: -1.0,
            protocol: 0,
            log_queue_capacity: 16,
            binding: aster_binding_kind_t::ASTER_BINDING_GO as u32,
            _reserved: 0,
        };
        let cfg = lower_config(&base).unwrap();
        assert!(!cfg.from_env);
        assert_eq!(cfg.traces_exporter, Some(vec![ExporterKind::Otlp]));
        assert_eq!(cfg.metrics_exporter, Some(vec![]));
        assert_eq!(cfg.log_bridge, Some(true));
        assert_eq!(cfg.service_name.as_deref(), Some("svc"));
        assert_eq!(cfg.log_queue, Some(16));
        assert_eq!(cfg.binding, Some(Binding::Go));
        for bad in [
            aster_observability_config_t {
                struct_size: 8,
                ..base
            },
            aster_observability_config_t {
                flags: 1 << 9,
                ..base
            },
            aster_observability_config_t {
                trace_sample_ratio: 1.5,
                ..base
            },
            aster_observability_config_t {
                protocol: 9,
                ..base
            },
            aster_observability_config_t {
                binding: 99,
                ..base
            },
        ] {
            assert!(lower_config(&bad).is_err());
        }
        let with_env = aster_observability_config_t {
            flags: ASTER_OBSERVABILITY_FROM_ENV,
            ..base
        };
        let cfg = lower_config(&with_env).unwrap();
        assert!(cfg.from_env && cfg.traces_exporter.is_none());
    }
}
