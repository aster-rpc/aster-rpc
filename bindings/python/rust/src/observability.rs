//! Observability for Python (design `OBSERVABILITY_HEALTH_DESIGN.md`, phase
//! O6): thin PyO3 wrappers over `aster-otel` and `aster::ops`.
//!
//! - pipeline: `observability_init` / `_flush` / `_shutdown` (a refcount held
//!   here, like the C ABI's `aster_observability_init`);
//! - logs: `log_drain_start` runs the `aster-log-drain` thread, which blocks on
//!   the bridge queue (no GIL held) and hands each batch to the Python
//!   callable `aster._logbridge._emit` under one attach; `log_drain_stop`
//!   stops and joins it with a bounded timeout (the `atexit` handler);
//! - metrics: instrument handles (`instrument_new`, `metric_add_u64`, ...) and
//!   `metrics_prometheus` (the `/metrics` text, rendered by the pull reader);
//! - spans: handle-based `span_*` calls with `traceparent` strings as the
//!   context interchange (Python parses and formats them itself);
//! - `HealthServer`: the `aster::ops` listener (`/healthz`, `/readyz`,
//!   `/metrics`).
//!
//! Nothing here parses OTel configuration: the Rust crate resolves env, TOML
//! and explicit fields (design 2.3).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pyo3::exceptions::{PyLookupError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyList;
use pyo3_async_runtimes::tokio::future_into_py;

use aster::ops::{HealthConfig, ReadinessCheck};
use aster_otel::bridge::{self, LogLevel, LogRecord, Poll};
use aster_otel::instruments::{self, InstrumentDesc, InstrumentKind, Measurement};
use aster_otel::spans::{self, SpanKind};
use aster_otel::{
    Binding, ExporterKind, ObservabilityConfig, ObservabilityHandle, OtlpHeaders, OtlpProtocol,
    SamplerConfig, TraceContext,
};

use crate::ensure_tokio_runtime;
use crate::error::aster_err_to_py;
use crate::node::Node;

fn otel_err(e: aster_otel::Error) -> PyErr {
    use aster_otel::Error as E;
    match e {
        E::Config(_) | E::InvalidArgument(_) | E::AlreadyInitialized(_) => {
            PyValueError::new_err(e.to_string())
        }
        E::NotFound(_) | E::LogBridgeNotOpen => PyLookupError::new_err(e.to_string()),
        other => PyRuntimeError::new_err(other.to_string()),
    }
}

// ── the pipeline ────────────────────────────────────────────────────────────

/// One reference per successful `observability_init`.
static HANDLES: Mutex<Vec<ObservabilityHandle>> = Mutex::new(Vec::new());

fn signal(v: Option<bool>) -> Option<Vec<ExporterKind>> {
    v.map(|on| {
        if on {
            vec![ExporterKind::Otlp]
        } else {
            Vec::new()
        }
    })
}

/// Start the pipeline, or join the running one. `traces` / `metrics` /
/// `otlp_logs`: `True` = export that signal over OTLP, `False` = off, `None` =
/// the environment decides (`OTEL_*`). `extra_toml` is the `[observability]`
/// table (header optional) as the file layer, below env. A configuration that
/// differs from the running pipeline's raises `ValueError` naming the
/// differing settings; `log_bridge` alone is additive.
#[pyfunction]
#[pyo3(signature = (*, service_name=None, otlp_endpoint=None, otlp_headers=None, resource_attributes=None, log_filter=None, extra_toml=None, from_env=true, traces=None, metrics=None, otlp_logs=None, trace_sample_ratio=None, log_queue=None, log_bridge=true, otlp_protocol=None))]
#[allow(clippy::too_many_arguments)]
fn observability_init(
    py: Python<'_>,
    service_name: Option<String>,
    otlp_endpoint: Option<String>,
    otlp_headers: Option<Vec<(String, String)>>,
    resource_attributes: Option<Vec<(String, String)>>,
    log_filter: Option<String>,
    extra_toml: Option<String>,
    from_env: bool,
    traces: Option<bool>,
    metrics: Option<bool>,
    otlp_logs: Option<bool>,
    trace_sample_ratio: Option<f64>,
    log_queue: Option<usize>,
    log_bridge: bool,
    otlp_protocol: Option<String>,
) -> PyResult<()> {
    let mut cfg = ObservabilityConfig {
        from_env,
        traces_exporter: signal(traces),
        metrics_exporter: signal(metrics),
        logs_exporter: signal(otlp_logs),
        binding: Some(Binding::Python),
        service_name,
        log_filter,
        log_queue,
        ..ObservabilityConfig::default()
    };
    if log_bridge {
        cfg.log_bridge = Some(true);
    }
    cfg.otlp.endpoint = otlp_endpoint.filter(|s| !s.is_empty());
    if let Some(h) = otlp_headers.filter(|h| !h.is_empty()) {
        cfg.otlp.headers = Some(OtlpHeaders::new(h));
    }
    cfg.resource_attributes = resource_attributes.unwrap_or_default();
    cfg.otlp.protocol = match otlp_protocol.as_deref() {
        None | Some("") => None,
        Some("http/protobuf") => Some(OtlpProtocol::HttpProtobuf),
        Some("grpc") => Some(OtlpProtocol::Grpc),
        Some(other) => {
            return Err(PyValueError::new_err(format!(
                "otlp_protocol: expected 'http/protobuf' or 'grpc', got {other:?}"
            )))
        }
    };
    if let Some(r) = trace_sample_ratio {
        if !(0.0..=1.0).contains(&r) {
            return Err(PyValueError::new_err(
                "trace_sample_ratio must be in [0, 1]",
            ));
        }
        cfg.sampler = Some(SamplerConfig::ParentBasedTraceIdRatio(r));
    }
    if let Some(toml) = extra_toml.filter(|t| !t.trim().is_empty()) {
        let doc = if toml.contains("[observability]") {
            toml
        } else {
            format!("[observability]\n{toml}")
        };
        let file = ObservabilityConfig::from_toml_str(&doc).map_err(otel_err)?;
        cfg = cfg.with_file_layer(file);
    }
    // Init flushes a replaced provider and joins exporter threads: no GIL.
    let handle = py.detach(|| aster_otel::init(cfg)).map_err(otel_err)?;
    HANDLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(handle);
    Ok(())
}

/// Flush every signal, waiting at most `timeout_ms` (`RuntimeError` on a
/// timeout). No pipeline: a no-op.
#[pyfunction]
#[pyo3(signature = (timeout_ms=2000))]
fn observability_flush(py: Python<'_>, timeout_ms: u64) -> PyResult<()> {
    py.detach(|| aster_otel::flush(Duration::from_millis(timeout_ms)))
        .map_err(otel_err)
}

/// Release one `observability_init` reference. The last reference (counting
/// other holders such as a health listener) shuts the pipeline down: providers
/// flush, the log bridge closes, spans end and instrument handles are freed.
/// Returns whether a reference was held. Waits at most `timeout_ms`; the
/// shutdown then continues in the background.
#[pyfunction]
#[pyo3(signature = (timeout_ms=2000))]
fn observability_shutdown(py: Python<'_>, timeout_ms: u64) -> PyResult<bool> {
    let Some(handle) = HANDLES.lock().unwrap_or_else(|e| e.into_inner()).pop() else {
        return Ok(false);
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("aster-otel-shutdown".into())
        .spawn(move || {
            drop(handle);
            let _ = tx.send(());
        })
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    py.detach(move || rx.recv_timeout(Duration::from_millis(timeout_ms)))
        .map_err(|_| {
            PyRuntimeError::new_err(
                "observability shutdown did not finish in time; it continues in the background",
            )
        })?;
    Ok(true)
}

/// References this module holds (diagnostics, tests).
#[pyfunction]
fn observability_refs() -> usize {
    HANDLES.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// `(traces, metrics, generation)` of the running pipeline, or `None`.
#[pyfunction]
fn observability_state() -> Option<(bool, bool, u64)> {
    aster_otel::state().map(|s| (s.traces, s.metrics, s.generation))
}

// ── the log bridge ──────────────────────────────────────────────────────────

/// `logging` level for a Rust level (TRACE = 5, the "TRACE" custom level).
fn py_level(l: LogLevel) -> u32 {
    match l {
        LogLevel::Error => 40,
        LogLevel::Warn => 30,
        LogLevel::Info => 20,
        LogLevel::Debug => 10,
        LogLevel::Trace => 5,
    }
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

/// A record as the tuple `aster._logbridge._emit` takes:
/// `(level, logger, message, timestamp_ns, flags, trace_id | None,
/// span_id | None, call_id, fields)`.
fn record_to_py<'py>(py: Python<'py>, r: &LogRecord) -> PyResult<Bound<'py, PyAny>> {
    let fields = PyList::empty(py);
    for (k, v) in &r.fields {
        fields.append((k.as_ref(), v.as_str()))?;
    }
    let (tid, sid) = if r.has_trace() {
        (Some(hex(&r.trace_id)), Some(hex(&r.span_id)))
    } else {
        (None, None)
    };
    let logger = r.logger_name();
    Ok((
        py_level(r.level),
        logger,
        r.message.as_str(),
        r.timestamp_unix_ns,
        r.flags,
        tid,
        sid,
        r.call_id,
        fields,
    )
        .into_pyobject(py)?
        .into_any())
}

struct Drain {
    stop: Arc<AtomicBool>,
    join: std::thread::JoinHandle<()>,
}

static DRAIN: Mutex<Option<Drain>> = Mutex::new(None);

/// The re-check cadence of the binding's logger level (design 3.1).
const RECHECK: Duration = Duration::from_secs(5);
/// Longest one poll waits, so a stop request is noticed promptly.
const POLL_SLICE: Duration = Duration::from_millis(250);

fn deliver(emit: &Py<PyAny>, batch: &[LogRecord]) {
    Python::attach(|py| {
        let list = PyList::empty(py);
        for r in batch {
            if let Ok(t) = record_to_py(py, r) {
                let _ = list.append(t);
            }
        }
        if let Err(e) = emit.call1(py, (list,)) {
            // `_emit` catches everything itself; this is a bug in it.
            e.write_unraisable(py, None);
        }
    });
}

fn drain_loop(emit: Py<PyAny>, recheck: Option<Py<PyAny>>, stop: Arc<AtomicBool>) {
    // Events this thread emits (and the handlers it runs) never re-enter the
    // bridge, so a handler that calls into Aster cannot feed back.
    bridge::mark_drain_thread();
    let mut drain = match bridge::drain() {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut buf: Vec<LogRecord> = Vec::with_capacity(bridge::DEFAULT_BATCH);
    let mut last_check = Instant::now();
    while !stop.load(Ordering::Acquire) {
        buf.clear();
        match drain.poll_into(&mut buf, bridge::DEFAULT_BATCH, POLL_SLICE) {
            // Atexit sets `stop` before finalisation starts: a batch that
            // arrives after it is dropped, never attached for.
            Poll::Records(_) if !stop.load(Ordering::Acquire) => deliver(&emit, &buf),
            Poll::Records(_) | Poll::Timeout => {}
            Poll::Closed => break,
        }
        if let Some(rc) = &recheck {
            if last_check.elapsed() >= RECHECK && !stop.load(Ordering::Acquire) {
                last_check = Instant::now();
                Python::attach(|py| {
                    if let Err(e) = rc.call0(py) {
                        e.write_unraisable(py, None);
                    }
                });
            }
        }
    }
}

/// Start the `aster-log-drain` thread delivering bridged Rust records to
/// `emit(batch)`; `recheck()` (optional) runs every 5 s to re-sync the Rust
/// filter with the Python logger level. Returns `False` when a drain already
/// runs or the bridge is not open (`init` with `log_bridge=True` first).
#[pyfunction]
#[pyo3(signature = (emit, recheck=None))]
fn log_drain_start(emit: Py<PyAny>, recheck: Option<Py<PyAny>>) -> PyResult<bool> {
    let mut g = DRAIN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = g.as_ref() {
        if !d.join.is_finished() {
            return Ok(false);
        }
    }
    if !bridge::is_open() {
        return Ok(false);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let join = std::thread::Builder::new()
        .name("aster-log-drain".into())
        .spawn(move || drain_loop(emit, recheck, s))
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    *g = Some(Drain { stop, join });
    Ok(true)
}

/// Stop the drain and join it, waiting at most `timeout_ms`. Records still
/// queued are left in the bridge. Returns whether the thread has ended.
#[pyfunction]
#[pyo3(signature = (timeout_ms=1000))]
fn log_drain_stop(py: Python<'_>, timeout_ms: u64) -> bool {
    let Some(d) = DRAIN.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return true;
    };
    d.stop.store(true, Ordering::Release);
    // The drain may be waiting for the GIL: release it while we wait.
    let ended = py.detach(|| {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while !d.join.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    });
    if ended {
        let _ = d.join.join();
    }
    ended
}

/// `os.register_at_fork(after_in_child=...)`: in a forked child the drain
/// thread does not exist and the parent's pipeline references are not ours to
/// drop (a lock they would take may have been held at fork time), so forget
/// both. Telemetry in a child forked after `init` is a no-op (design 2.4); a
/// child forked before it can `init` normally.
#[pyfunction]
fn observability_after_fork() {
    if let Some(d) = DRAIN.lock().unwrap_or_else(|e| e.into_inner()).take() {
        std::mem::forget(d);
    }
    let handles = std::mem::take(&mut *HANDLES.lock().unwrap_or_else(|e| e.into_inner()));
    std::mem::forget(handles);
}

/// Replace the bridge filter (`EnvFilter` directives, as `ASTER_LOG`).
#[pyfunction]
fn log_set_filter(directives: &str) -> PyResult<()> {
    bridge::set_filter(directives).map_err(otel_err)
}

/// The bridge's current filter directives, or `None` when it is not open.
#[pyfunction]
fn log_filter() -> Option<String> {
    bridge::filter()
}

/// `(general, audit)` records dropped by the bridge so far.
#[pyfunction]
fn log_dropped() -> (u64, u64) {
    (bridge::dropped_total(), bridge::audit_dropped_total())
}

/// Close the bridge (a later `observability_init(log_bridge=True)` reopens it).
#[pyfunction]
fn log_close() {
    bridge::close();
}

/// Export an application log record through the Rust OTel logs provider
/// (design 3.4 path B). A no-op without an OTLP logs exporter.
#[pyfunction]
#[pyo3(signature = (level, logger, message, fields=None, timestamp_ns=0, traceparent=None))]
fn log_emit(
    level: u8,
    logger: &str,
    message: &str,
    fields: Option<Vec<(String, String)>>,
    timestamp_ns: i64,
    traceparent: Option<&str>,
) -> PyResult<()> {
    let level = match level {
        1 => LogLevel::Error,
        2 => LogLevel::Warn,
        3 => LogLevel::Info,
        4 => LogLevel::Debug,
        5 => LogLevel::Trace,
        l => return Err(PyValueError::new_err(format!("unknown log level {l}"))),
    };
    let context = traceparent
        .and_then(|t| TraceContext::parse_traceparent(t.as_bytes()))
        .map(|c| (c.trace_id, c.span_id, c.trace_flags));
    let fields = fields.unwrap_or_default();
    let view: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    bridge::emit_app_log(&bridge::AppLogRecord {
        level: Some(level),
        timestamp_unix_ns: timestamp_ns,
        logger,
        message,
        fields: &view,
        context,
    })
    .map_err(otel_err)
}

// ── metrics ─────────────────────────────────────────────────────────────────

/// Declare an instrument (`kind`: 1 counter, 2 up-down counter, 3 histogram,
/// 4 gauge) and return its handle. A name already declared with a different
/// kind, or breaking the OTel naming rules, raises `ValueError`.
/// `boundaries` (histograms only): explicit bucket upper bounds, finite and
/// strictly increasing; none = the RPC seconds buckets for unit `s`, else the
/// SDK default.
#[pyfunction]
#[pyo3(signature = (kind, name, description="", unit="", auto_attrs=None, cardinality_limit=0, boundaries=None))]
fn instrument_new(
    kind: u32,
    name: String,
    description: &str,
    unit: &str,
    auto_attrs: Option<Vec<String>>,
    cardinality_limit: u32,
    boundaries: Option<Vec<f64>>,
) -> PyResult<u64> {
    let kind = InstrumentKind::from_u32(kind)
        .ok_or_else(|| PyValueError::new_err(format!("unknown instrument kind {kind}")))?;
    instruments::instrument_new(InstrumentDesc {
        kind,
        name,
        description: description.to_owned(),
        unit: unit.to_owned(),
        auto_attrs: auto_attrs.unwrap_or_default(),
        cardinality_limit,
        boundaries: boundaries.unwrap_or_default(),
    })
    .map_err(otel_err)
}

#[pyfunction]
fn instrument_free(handle: u64) -> bool {
    instruments::instrument_free(handle)
}

fn rec(handle: u64, v: Measurement, attrs: Option<Vec<(String, String)>>) -> PyResult<()> {
    instruments::record(handle, v, 0, &attrs.unwrap_or_default(), 0).map_err(otel_err)
}

/// Add to a counter. A freed handle (pipeline shut down) raises `LookupError`.
#[pyfunction]
#[pyo3(signature = (handle, value, attrs=None))]
fn metric_add_u64(handle: u64, value: u64, attrs: Option<Vec<(String, String)>>) -> PyResult<()> {
    rec(handle, Measurement::U64(value), attrs)
}

#[pyfunction]
#[pyo3(signature = (handle, value, attrs=None))]
fn metric_add_i64(handle: u64, value: i64, attrs: Option<Vec<(String, String)>>) -> PyResult<()> {
    rec(handle, Measurement::I64(value), attrs)
}

#[pyfunction]
#[pyo3(signature = (handle, value, attrs=None))]
fn metric_record_f64(
    handle: u64,
    value: f64,
    attrs: Option<Vec<(String, String)>>,
) -> PyResult<()> {
    rec(handle, Measurement::F64(value), attrs)
}

/// The `/metrics` text of the running pipeline's Prometheus pull reader, or
/// `None` when there is none (no `HealthServer` has joined the pipeline).
#[pyfunction]
fn metrics_prometheus() -> Option<String> {
    aster::ops::metrics_text()
}

// ── spans ───────────────────────────────────────────────────────────────────

fn parent_of(traceparent: Option<&str>) -> Option<TraceContext> {
    traceparent.and_then(|t| TraceContext::parse_traceparent(t.as_bytes()))
}

/// Start a span (`kind`: 0 internal, 1 server, 2 client, 3 producer, 4
/// consumer) under the context of `parent` (a `traceparent`; `None` or
/// invalid = a root) and return its handle. Without a tracer provider the span
/// is a no-op. A full live-span table raises `RuntimeError`.
#[pyfunction]
#[pyo3(signature = (name, kind=0, parent=None, attrs=None))]
fn span_start(
    name: &str,
    kind: u32,
    parent: Option<&str>,
    attrs: Option<Vec<(String, String)>>,
) -> PyResult<u64> {
    let kind = SpanKind::from_u32(kind)
        .ok_or_else(|| PyValueError::new_err(format!("unknown span kind {kind}")))?;
    spans::span_start(
        name,
        kind,
        parent_of(parent).as_ref(),
        &attrs.unwrap_or_default(),
    )
    .map_err(otel_err)
}

#[pyfunction]
fn span_set_attributes(handle: u64, attrs: Vec<(String, String)>) -> PyResult<()> {
    spans::span_set_attributes(handle, &attrs).map_err(otel_err)
}

#[pyfunction]
#[pyo3(signature = (handle, name, attrs=None))]
fn span_add_event(handle: u64, name: &str, attrs: Option<Vec<(String, String)>>) -> PyResult<()> {
    spans::span_add_event(handle, name, &attrs.unwrap_or_default()).map_err(otel_err)
}

/// `code`: 0 unset, 1 ok, 2 error (with `message`).
#[pyfunction]
#[pyo3(signature = (handle, code, message=""))]
fn span_set_status(handle: u64, code: u32, message: &str) -> PyResult<()> {
    spans::span_set_status(handle, code, message).map_err(otel_err)
}

#[pyfunction]
#[pyo3(signature = (handle, error_type, message))]
fn span_record_error(handle: u64, error_type: &str, message: &str) -> PyResult<()> {
    spans::span_record_error(handle, error_type, message).map_err(otel_err)
}

/// The span's context as a `traceparent`, or `None` for a no-op span.
#[pyfunction]
fn span_traceparent(handle: u64) -> PyResult<Option<String>> {
    let c = spans::span_context(handle).map_err(otel_err)?;
    Ok(c.is_valid().then(|| c.traceparent()))
}

/// End the span and free its handle.
#[pyfunction]
fn span_end(handle: u64) -> PyResult<()> {
    spans::span_end(handle).map_err(otel_err)
}

/// Validate a W3C `traceparent` and return its canonical form, or `None`.
#[pyfunction]
fn traceparent_normalize(value: &str) -> Option<String> {
    TraceContext::parse_traceparent(value.as_bytes()).map(|c| c.traceparent())
}

/// The `traceparent` of a dispatched call's server span (`ServerCall`).
pub(crate) fn call_traceparent(trace_id: [u8; 16], span_id: [u8; 8], trace_flags: u8) -> String {
    TraceContext {
        trace_id,
        span_id,
        trace_flags,
        is_remote: false,
    }
    .traceparent()
}

// ── health (ops endpoints) ──────────────────────────────────────────────────

/// A named readiness check (`HealthServer.check(name)`). Created failing:
/// a declared dependency is not ready until it says so.
#[pyclass(frozen, module = "aster._aster")]
pub struct ReadinessCheckHandle {
    inner: ReadinessCheck,
}

#[pymethods]
impl ReadinessCheckHandle {
    fn set(&self, ok: bool) {
        self.inner.set(ok);
    }

    fn is_ok(&self) -> bool {
        self.inner.is_ok()
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name().to_owned()
    }

    fn __repr__(&self) -> String {
        format!(
            "ReadinessCheck({:?}, ok={})",
            self.inner.name(),
            self.inner.is_ok()
        )
    }
}

/// The ops listener: `GET /healthz`, `/readyz`, `/metrics` (Prometheus text),
/// served by `aster::ops` and fed by the built-in instruments. Disabled by
/// default in `AsterServer` (`health_port = 0`); the default bind address is
/// `127.0.0.1`.
#[pyclass(frozen, module = "aster._aster", name = "HealthServer")]
pub struct HealthServer {
    inner: Arc<tokio::sync::Mutex<Option<aster::ops::HealthServer>>>,
    state: aster::ops::HealthState,
    addr: std::net::SocketAddr,
    stopped: Arc<AtomicBool>,
}

// Caller cancellation must not abandon the listener join or owner release.
async fn stop_owned_health(
    inner: Arc<tokio::sync::Mutex<Option<aster::ops::HealthServer>>>,
    stopped: Arc<AtomicBool>,
) -> PyResult<()> {
    tokio::spawn(async move {
        let mut owned = inner.lock().await;
        if let Some(server) = owned.take() {
            server.shutdown().await;
            drop(server);
        }
        stopped.store(true, Ordering::Release);
    })
    .await
    .map_err(|e| PyRuntimeError::new_err(format!("health shutdown: {e}")))
}

#[pymethods]
impl HealthServer {
    /// `await HealthServer.start(node, host="127.0.0.1", port=0)`. Port `0`
    /// binds an ephemeral port (see `port`). Joins, or starts from the
    /// environment, the observability pipeline and gives it the Prometheus
    /// pull reader.
    #[staticmethod]
    #[pyo3(signature = (node, host="127.0.0.1".to_string(), port=0))]
    fn start<'py>(
        py: Python<'py>,
        node: PyRef<'py, Node>,
        host: String,
        port: u16,
    ) -> PyResult<Bound<'py, PyAny>> {
        ensure_tokio_runtime();
        // Python's own start (log bridge on, exporters per OTEL_*) first, so a
        // listener never creates a pipeline with a different configuration
        // than the one a server or client would; a later `observability.init`
        // with other settings then reports the difference.
        py.import("aster.observability")?
            .call_method0("ensure_started")?;
        let node = node.inner.clone();
        future_into_py(py, async move {
            aster::ops::HealthServer::start(&node, HealthConfig::new(host, port))
                .await
                .map(|s| HealthServer {
                    state: s.state(),
                    addr: s.addr(),
                    inner: Arc::new(tokio::sync::Mutex::new(Some(s))),
                    stopped: Arc::new(AtomicBool::new(false)),
                })
                .map_err(aster_err_to_py)
        })
    }

    fn set_ready(&self, ready: bool) {
        self.state.set_ready(ready);
    }

    fn set_healthy(&self, healthy: bool) {
        self.state.set_healthy(healthy);
    }

    fn is_ready(&self) -> bool {
        self.state.is_ready()
    }

    fn is_healthy(&self) -> bool {
        self.state.is_healthy()
    }

    /// The number of registered services `/readyz` reports.
    fn set_services(&self, services: u32) {
        self.state.set_services(services);
    }

    /// Whether the registry / contracts are published (`/readyz`).
    fn set_registry(&self, published: bool) {
        self.state.set_registry(published);
    }

    /// A named readiness check; `/readyz` is ready only while all are true.
    fn check(&self, name: &str) -> ReadinessCheckHandle {
        ReadinessCheckHandle {
            inner: self.state.check(name),
        }
    }

    fn remove_check(&self, name: &str) -> bool {
        self.state.remove_check(name)
    }

    /// The bound `host:port`.
    #[getter]
    fn addr(&self) -> String {
        self.addr.to_string()
    }

    #[getter]
    fn port(&self) -> u16 {
        self.addr.port()
    }

    #[getter]
    fn is_shut_down(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Stop listening and release the owned observability reference. Concurrent
    /// calls join shutdown before returning; cached state/address remain usable.
    fn stop<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let stopped = self.stopped.clone();
        self.state.set_ready(false);
        future_into_py(py, async move {
            stop_owned_health(inner, stopped).await?;
            Ok(())
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let obj: Py<PyAny> = slf.into_any().unbind();
        future_into_py(py, async move { Ok(obj) })
    }

    #[pyo3(signature = (_exc_type=None, _exc=None, _tb=None))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Option<Bound<'py, PyAny>>,
        _exc: Option<Bound<'py, PyAny>>,
        _tb: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        let stopped = self.stopped.clone();
        self.state.set_ready(false);
        future_into_py(py, async move {
            stop_owned_health(inner, stopped).await?;
            Ok(false)
        })
    }

    fn __repr__(&self) -> String {
        format!("HealthServer(addr='{}')", self.addr)
    }
}

pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<HealthServer>()?;
    m.add_class::<ReadinessCheckHandle>()?;
    m.add_function(wrap_pyfunction!(observability_init, m)?)?;
    m.add_function(wrap_pyfunction!(observability_flush, m)?)?;
    m.add_function(wrap_pyfunction!(observability_shutdown, m)?)?;
    m.add_function(wrap_pyfunction!(observability_refs, m)?)?;
    m.add_function(wrap_pyfunction!(observability_state, m)?)?;
    m.add_function(wrap_pyfunction!(log_drain_start, m)?)?;
    m.add_function(wrap_pyfunction!(log_drain_stop, m)?)?;
    m.add_function(wrap_pyfunction!(observability_after_fork, m)?)?;
    m.add_function(wrap_pyfunction!(log_set_filter, m)?)?;
    m.add_function(wrap_pyfunction!(log_filter, m)?)?;
    m.add_function(wrap_pyfunction!(log_dropped, m)?)?;
    m.add_function(wrap_pyfunction!(log_close, m)?)?;
    m.add_function(wrap_pyfunction!(log_emit, m)?)?;
    m.add_function(wrap_pyfunction!(instrument_new, m)?)?;
    m.add_function(wrap_pyfunction!(instrument_free, m)?)?;
    m.add_function(wrap_pyfunction!(metric_add_u64, m)?)?;
    m.add_function(wrap_pyfunction!(metric_add_i64, m)?)?;
    m.add_function(wrap_pyfunction!(metric_record_f64, m)?)?;
    m.add_function(wrap_pyfunction!(metrics_prometheus, m)?)?;
    m.add_function(wrap_pyfunction!(span_start, m)?)?;
    m.add_function(wrap_pyfunction!(span_set_attributes, m)?)?;
    m.add_function(wrap_pyfunction!(span_add_event, m)?)?;
    m.add_function(wrap_pyfunction!(span_set_status, m)?)?;
    m.add_function(wrap_pyfunction!(span_record_error, m)?)?;
    m.add_function(wrap_pyfunction!(span_traceparent, m)?)?;
    m.add_function(wrap_pyfunction!(span_end, m)?)?;
    m.add_function(wrap_pyfunction!(traceparent_normalize, m)?)?;
    Ok(())
}
