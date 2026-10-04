//! Health, readiness and metrics over plain HTTP — the Rust home of the
//! Python/TypeScript `HealthServer`.
//!
//! A small HTTP/1.1 listener (hyper) on its own TCP port, separate from the
//! QUIC endpoint, for Kubernetes probes, load balancers and Prometheus:
//!
//! | Route | Response |
//! |---|---|
//! | `GET /healthz` | `200 {"status":"ok","uptime_s":…}`, `503 {"status":"unhealthy",…}` once [`set_healthy(false)`](HealthState::set_healthy) |
//! | `GET /readyz` | `200 {"status":"ready","services":n,"registry":b}` when [`set_ready(true)`](HealthState::set_ready), else `503 {"status":"not_ready",…}` |
//! | `GET /metrics` | JSON snapshot (`health`, `ready`, `connections`, `admission`, `rpc`) — the Python/TS shape. Prometheus text instead when the `Accept` header names `text/plain` or `application/openmetrics-text` (what a Prometheus scraper sends) or the query is `?format=prometheus` |
//! | `GET /metrics/prometheus` | Prometheus text exposition (`text/plain; version=0.0.4`) |
//! | anything else | `404 {"error":"not found"}`; non-`GET`/`HEAD` → `405` |
//!
//! The Prometheus text carries the `aster_*` series the Python binding emits
//! (uptime, connections, streams, admission, RPC, healthy/ready) followed, with
//! the `metrics` feature, by the transport series from
//! [`Node::metrics_prometheus`].
//!
//! State lives in a [`HealthState`] (clone-cheap, all setters `&self`). The
//! counters in [`HealthMetrics`] are recorded by whoever observes the events —
//! a binding's server loop, an interceptor, admission logic — and read back with
//! [`HealthState::snapshot`] or the rendered [`prometheus_text`](HealthState::prometheus_text)
//! / [`metrics_json`](HealthState::metrics_json), so a binding can scrape the
//! same state into its own stack without the HTTP listener.
//!
//! ```no_run
//! # async fn run(node: aster::Node) -> aster::Result<()> {
//! use aster::health::{HealthConfig, HealthServer};
//!
//! let health = HealthServer::start(&node, HealthConfig::new("0.0.0.0", 8080)).await?;
//! health.state().set_services(3);
//! health.state().set_ready(true);
//! println!("health on http://{}", health.addr());
//! // ...
//! health.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! [`AsterServer`](crate::rpc::AsterServer) starts one automatically when the
//! config enables it (`ASTER_HEALTH_PORT`, `[health] port`, see
//! [`AsterConfig::health_config`](crate::AsterConfig::health_config)).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderValue, ACCEPT, ALLOW, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

use crate::error::{Error, Result};
use crate::node::Node;

/// Default bind host: localhost only. Use `0.0.0.0` for pod probes.
pub const DEFAULT_HEALTH_HOST: &str = "127.0.0.1";

/// `Content-Type` of the Prometheus text exposition.
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// How long a client may take to send its request headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Concurrent connections served at once; further connections are closed
/// immediately until one finishes. Probes are one request per connection.
const MAX_CONNECTIONS: usize = 256;

/// Where the health server listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthConfig {
    /// Bind host (IP literal or resolvable name). Default `127.0.0.1`.
    pub host: String,
    /// TCP port. `0` binds an ephemeral port (read it back with
    /// [`HealthServer::addr`]). Note that in [`AsterConfig`](crate::AsterConfig)
    /// a `health_port` of `0` means *disabled* unless `health_enabled` is set.
    pub port: u16,
}

impl HealthConfig {
    /// A config for `host:port`.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self::new(DEFAULT_HEALTH_HOST, 0)
    }
}

// ── Counters ─────────────────────────────────────────────────────────────────

/// Operational counters served by `/metrics`. Mirrors the Python binding's
/// `ConnectionMetrics` + `AdmissionMetrics` + `MetricsInterceptor` snapshot.
/// All methods take `&self` and are lock-free.
#[derive(Debug, Default)]
pub struct HealthMetrics {
    active_connections: AtomicU64,
    total_connections: AtomicU64,
    active_streams: AtomicU64,
    total_streams: AtomicU64,
    consumer_admitted: AtomicU64,
    consumer_denied: AtomicU64,
    consumer_errors: AtomicU64,
    producer_admitted: AtomicU64,
    producer_denied: AtomicU64,
    producer_errors: AtomicU64,
    /// Microseconds, so the value stays an integer atomic.
    last_admission_us: AtomicU64,
    rpc_started: AtomicU64,
    rpc_succeeded: AtomicU64,
    rpc_failed: AtomicU64,
}

fn saturating_dec(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
}

fn ms_to_us(duration_ms: f64) -> u64 {
    if duration_ms.is_finite() && duration_ms > 0.0 {
        (duration_ms * 1000.0).round() as u64
    } else {
        0
    }
}

impl HealthMetrics {
    /// A peer connection was accepted.
    pub fn connection_opened(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
        self.total_connections.fetch_add(1, Ordering::Relaxed);
    }

    /// A peer connection closed.
    pub fn connection_closed(&self) {
        saturating_dec(&self.active_connections);
    }

    /// An RPC stream started.
    pub fn stream_opened(&self) {
        self.active_streams.fetch_add(1, Ordering::Relaxed);
        self.total_streams.fetch_add(1, Ordering::Relaxed);
    }

    /// An RPC stream ended.
    pub fn stream_closed(&self) {
        saturating_dec(&self.active_streams);
    }

    /// A consumer was admitted; `duration_ms` is the handshake time.
    pub fn record_consumer_admit(&self, duration_ms: f64) {
        self.consumer_admitted.fetch_add(1, Ordering::Relaxed);
        self.last_admission_us
            .store(ms_to_us(duration_ms), Ordering::Relaxed);
    }

    /// A consumer admission was denied.
    pub fn record_consumer_deny(&self) {
        self.consumer_denied.fetch_add(1, Ordering::Relaxed);
    }

    /// A consumer admission failed with an error.
    pub fn record_consumer_error(&self) {
        self.consumer_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// A producer was admitted; `duration_ms` is the handshake time.
    pub fn record_producer_admit(&self, duration_ms: f64) {
        self.producer_admitted.fetch_add(1, Ordering::Relaxed);
        self.last_admission_us
            .store(ms_to_us(duration_ms), Ordering::Relaxed);
    }

    /// A producer admission was denied.
    pub fn record_producer_deny(&self) {
        self.producer_denied.fetch_add(1, Ordering::Relaxed);
    }

    /// A producer admission failed with an error.
    pub fn record_producer_error(&self) {
        self.producer_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// An RPC call started.
    pub fn rpc_started(&self) {
        self.rpc_started.fetch_add(1, Ordering::Relaxed);
    }

    /// An RPC call completed; `ok` selects the succeeded/failed counter.
    pub fn rpc_completed(&self, ok: bool) {
        if ok {
            self.rpc_succeeded.fetch_add(1, Ordering::Relaxed);
        } else {
            self.rpc_failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Zero every counter (tests; mirrors Python `reset_metrics`).
    pub fn reset(&self) {
        for counter in [
            &self.active_connections,
            &self.total_connections,
            &self.active_streams,
            &self.total_streams,
            &self.consumer_admitted,
            &self.consumer_denied,
            &self.consumer_errors,
            &self.producer_admitted,
            &self.producer_denied,
            &self.producer_errors,
            &self.last_admission_us,
            &self.rpc_started,
            &self.rpc_succeeded,
            &self.rpc_failed,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

/// A point-in-time copy of everything `/healthz`, `/readyz` and `/metrics`
/// report. Plain fields so a binding can convert it field by field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HealthSnapshot {
    /// Seconds since the [`HealthState`] was created.
    pub uptime_s: f64,
    /// Liveness (`/healthz`).
    pub healthy: bool,
    /// Readiness (`/readyz`).
    pub ready: bool,
    /// Services registered, reported by `/readyz`.
    pub services: u32,
    /// Whether contracts were published to a registry, reported by `/readyz`.
    pub registry: bool,
    /// Currently open peer connections.
    pub active_connections: u64,
    /// Peer connections accepted since start.
    pub total_connections: u64,
    /// Currently open RPC streams.
    pub active_streams: u64,
    /// RPC streams handled since start.
    pub total_streams: u64,
    /// Consumer admissions granted.
    pub consumer_admitted: u64,
    /// Consumer admissions denied.
    pub consumer_denied: u64,
    /// Consumer admission errors.
    pub consumer_errors: u64,
    /// Producer admissions granted.
    pub producer_admitted: u64,
    /// Producer admissions denied.
    pub producer_denied: u64,
    /// Producer admission errors.
    pub producer_errors: u64,
    /// Duration of the last admission handshake, milliseconds.
    pub last_admission_ms: f64,
    /// RPC calls started.
    pub rpc_started: u64,
    /// RPC calls that completed OK.
    pub rpc_succeeded: u64,
    /// RPC calls that completed with an error.
    pub rpc_failed: u64,
    /// `started - succeeded - failed`, floored at 0.
    pub rpc_in_flight: u64,
}

// ── Shared state ─────────────────────────────────────────────────────────────

struct StateInner {
    /// Read only for the transport series (`metrics` feature).
    #[cfg_attr(not(feature = "metrics"), allow(dead_code))]
    node: Option<Node>,
    started: Instant,
    healthy: AtomicBool,
    ready: AtomicBool,
    services: AtomicU32,
    registry: AtomicBool,
    metrics: HealthMetrics,
}

/// Liveness/readiness flags and counters behind a [`HealthServer`].
///
/// Clone-cheap; every clone sees the same state. Starts **healthy** and **not
/// ready** — call [`set_ready(true)`](Self::set_ready) once the process can
/// take traffic (an [`AsterServer`](crate::rpc::AsterServer) does this after
/// its services are being served).
#[derive(Clone)]
pub struct HealthState {
    inner: Arc<StateInner>,
}

impl std::fmt::Debug for HealthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthState")
            .field("healthy", &self.is_healthy())
            .field("ready", &self.is_ready())
            .field("services", &self.services())
            .finish_non_exhaustive()
    }
}

impl HealthState {
    /// Fresh state. `node`, when given, contributes its transport series to the
    /// Prometheus output (with the `metrics` feature).
    pub fn new(node: Option<&Node>) -> Self {
        Self {
            inner: Arc::new(StateInner {
                node: node.cloned(),
                started: Instant::now(),
                healthy: AtomicBool::new(true),
                ready: AtomicBool::new(false),
                services: AtomicU32::new(0),
                registry: AtomicBool::new(false),
                metrics: HealthMetrics::default(),
            }),
        }
    }

    /// Set liveness. `false` makes `/healthz` answer 503.
    pub fn set_healthy(&self, healthy: bool) {
        self.inner.healthy.store(healthy, Ordering::Relaxed);
    }

    /// Current liveness.
    pub fn is_healthy(&self) -> bool {
        self.inner.healthy.load(Ordering::Relaxed)
    }

    /// Set readiness. `/readyz` answers 200 only while ready (and healthy).
    pub fn set_ready(&self, ready: bool) {
        self.inner.ready.store(ready, Ordering::Relaxed);
    }

    /// Current readiness: the ready flag, and healthy.
    pub fn is_ready(&self) -> bool {
        self.is_healthy() && self.inner.ready.load(Ordering::Relaxed)
    }

    /// Number of served services, reported by `/readyz`.
    pub fn set_services(&self, services: u32) {
        self.inner.services.store(services, Ordering::Relaxed);
    }

    /// Reported service count.
    pub fn services(&self) -> u32 {
        self.inner.services.load(Ordering::Relaxed)
    }

    /// Whether contracts were published to a registry, reported by `/readyz`.
    pub fn set_registry(&self, published: bool) {
        self.inner.registry.store(published, Ordering::Relaxed);
    }

    /// The counters; record events on them directly.
    pub fn metrics(&self) -> &HealthMetrics {
        &self.inner.metrics
    }

    /// Time since this state was created.
    pub fn uptime(&self) -> Duration {
        self.inner.started.elapsed()
    }

    /// Copy every flag and counter.
    pub fn snapshot(&self) -> HealthSnapshot {
        let m = &self.inner.metrics;
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let rpc_started = load(&m.rpc_started);
        let rpc_succeeded = load(&m.rpc_succeeded);
        let rpc_failed = load(&m.rpc_failed);
        HealthSnapshot {
            uptime_s: self.uptime().as_secs_f64(),
            healthy: self.is_healthy(),
            ready: self.is_ready(),
            services: self.services(),
            registry: self.inner.registry.load(Ordering::Relaxed),
            active_connections: load(&m.active_connections),
            total_connections: load(&m.total_connections),
            active_streams: load(&m.active_streams),
            total_streams: load(&m.total_streams),
            consumer_admitted: load(&m.consumer_admitted),
            consumer_denied: load(&m.consumer_denied),
            consumer_errors: load(&m.consumer_errors),
            producer_admitted: load(&m.producer_admitted),
            producer_denied: load(&m.producer_denied),
            producer_errors: load(&m.producer_errors),
            last_admission_ms: load(&m.last_admission_us) as f64 / 1000.0,
            rpc_started,
            rpc_succeeded,
            rpc_failed,
            rpc_in_flight: rpc_started.saturating_sub(rpc_succeeded + rpc_failed),
        }
    }

    /// The `/healthz` body.
    pub fn healthz_json(&self) -> String {
        healthz_value(&self.snapshot()).to_string()
    }

    /// The `/readyz` body.
    pub fn readyz_json(&self) -> String {
        readyz_value(&self.snapshot()).to_string()
    }

    /// The JSON `/metrics` body (Python/TS shape).
    pub fn metrics_json(&self) -> String {
        let s = self.snapshot();
        json!({
            "health": healthz_value(&s),
            "ready": readyz_value(&s),
            "connections": {
                "active_connections": s.active_connections,
                "total_connections": s.total_connections,
                "active_streams": s.active_streams,
                "total_streams": s.total_streams,
            },
            "admission": {
                "consumer_admitted": s.consumer_admitted,
                "consumer_denied": s.consumer_denied,
                "consumer_errors": s.consumer_errors,
                "producer_admitted": s.producer_admitted,
                "producer_denied": s.producer_denied,
                "producer_errors": s.producer_errors,
                "last_admission_ms": round1(s.last_admission_ms),
            },
            "rpc": {
                "started": s.rpc_started,
                "succeeded": s.rpc_succeeded,
                "failed": s.rpc_failed,
                "in_flight": s.rpc_in_flight,
            },
        })
        .to_string()
    }

    /// The Prometheus text exposition: the `aster_*` series, then (with the
    /// `metrics` feature and a node) [`Node::metrics_prometheus`].
    pub fn prometheus_text(&self) -> String {
        use std::fmt::Write as _;

        let s = self.snapshot();
        let mut out = String::with_capacity(4096);
        let mut series = |name: &str, kind: &str, help: &str, samples: &[(&str, String)]| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} {kind}");
            for (labels, value) in samples {
                let _ = writeln!(out, "{name}{labels} {value}");
            }
        };
        let one = |v: u64| [("", v.to_string())];
        series(
            "aster_uptime_seconds",
            "gauge",
            "Server uptime in seconds",
            &[("", format!("{:.1}", s.uptime_s))],
        );
        series(
            "aster_connections_active",
            "gauge",
            "Current active connections",
            &one(s.active_connections),
        );
        series(
            "aster_connections_total",
            "counter",
            "Total connections since startup",
            &one(s.total_connections),
        );
        series(
            "aster_streams_active",
            "gauge",
            "Current active RPC streams",
            &one(s.active_streams),
        );
        series(
            "aster_streams_total",
            "counter",
            "Total streams since startup",
            &one(s.total_streams),
        );
        series(
            "aster_admission_consumer_admitted_total",
            "counter",
            "Consumer admissions granted",
            &one(s.consumer_admitted),
        );
        series(
            "aster_admission_consumer_denied_total",
            "counter",
            "Consumer admissions denied",
            &one(s.consumer_denied),
        );
        series(
            "aster_admission_consumer_errors_total",
            "counter",
            "Consumer admission errors",
            &one(s.consumer_errors),
        );
        series(
            "aster_admission_producer_admitted_total",
            "counter",
            "Producer admissions granted",
            &one(s.producer_admitted),
        );
        series(
            "aster_admission_producer_denied_total",
            "counter",
            "Producer admissions denied",
            &one(s.producer_denied),
        );
        series(
            "aster_admission_producer_errors_total",
            "counter",
            "Producer admission errors",
            &one(s.producer_errors),
        );
        series(
            "aster_rpc_started_total",
            "counter",
            "Total RPC calls started",
            &one(s.rpc_started),
        );
        series(
            "aster_rpc_completed_total",
            "counter",
            "Total RPC calls completed",
            &[
                ("{status=\"ok\"}", s.rpc_succeeded.to_string()),
                ("{status=\"error\"}", s.rpc_failed.to_string()),
            ],
        );
        series(
            "aster_rpc_in_flight",
            "gauge",
            "Current in-flight RPC calls",
            &one(s.rpc_in_flight),
        );
        series(
            "aster_healthy",
            "gauge",
            "Whether the server is healthy (1=yes, 0=no)",
            &one(u64::from(s.healthy)),
        );
        series(
            "aster_ready",
            "gauge",
            "Whether the server is ready (1=yes, 0=no)",
            &one(u64::from(s.ready)),
        );
        #[cfg(feature = "metrics")]
        if let Some(node) = &self.inner.node {
            let transport = node.metrics_prometheus();
            out.push_str(&transport);
            if !transport.is_empty() && !transport.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn healthz_value(s: &HealthSnapshot) -> serde_json::Value {
    json!({
        "status": if s.healthy { "ok" } else { "unhealthy" },
        "uptime_s": round1(s.uptime_s),
    })
}

fn readyz_value(s: &HealthSnapshot) -> serde_json::Value {
    json!({
        "status": if s.ready { "ready" } else { "not_ready" },
        "services": s.services,
        "registry": s.registry,
    })
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

fn respond(status: StatusCode, content_type: &'static str, body: String) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(body)));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

fn json_response(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    respond(status, "application/json", body)
}

fn wants_prometheus<B>(req: &Request<B>) -> bool {
    let query_says = req.uri().query().is_some_and(|q| {
        q.split('&')
            .any(|kv| kv.eq_ignore_ascii_case("format=prometheus"))
    });
    let accept_says = req
        .headers()
        .get_all(ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("application/openmetrics-text") || v.contains("text/plain")
        });
    query_says || accept_says
}

/// Route one request. Public within the crate for unit tests.
fn route<B>(state: &HealthState, req: &Request<B>) -> Response<Full<Bytes>> {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        let mut resp = json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error": "method not allowed"}).to_string(),
        );
        resp.headers_mut()
            .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
        return resp;
    }
    let path = req.uri().path();
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    match path {
        "/healthz" => {
            let status = if state.is_healthy() {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            json_response(status, state.healthz_json())
        }
        "/readyz" => {
            let status = if state.is_ready() {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            json_response(status, state.readyz_json())
        }
        "/metrics" if wants_prometheus(req) => respond(
            StatusCode::OK,
            PROMETHEUS_CONTENT_TYPE,
            state.prometheus_text(),
        ),
        "/metrics" => json_response(StatusCode::OK, state.metrics_json()),
        "/metrics/prometheus" => respond(
            StatusCode::OK,
            PROMETHEUS_CONTENT_TYPE,
            state.prometheus_text(),
        ),
        _ => json_response(
            StatusCode::NOT_FOUND,
            json!({"error": "not found"}).to_string(),
        ),
    }
}

async fn accept_loop(listener: TcpListener, state: HealthState, mut stop: oneshot::Receiver<()>) {
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else {
                    // Transient accept errors (EMFILE, ECONNABORTED): back off
                    // briefly rather than spinning.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                if conns.len() >= MAX_CONNECTIONS {
                    drop(stream);
                    continue;
                }
                let state = state.clone();
                conns.spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let resp = route(&state, &req);
                        async move { Ok::<_, Infallible>(resp) }
                    });
                    let mut builder = http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_READ_TIMEOUT)
                        .keep_alive(false);
                    let _ = builder.serve_connection(TokioIo::new(stream), service).await;
                });
            }
        }
    }
    // Dropping the listener frees the port; dropping the set aborts any
    // connection still being served.
    drop(listener);
    conns.shutdown().await;
}

/// A running health/readiness/metrics HTTP listener.
///
/// Start with [`start`](Self::start); stop with [`shutdown`](Self::shutdown)
/// (or drop it, which aborts the listener without waiting). All methods take
/// `&self`, so a binding can hold it behind a shared handle.
pub struct HealthServer {
    state: HealthState,
    addr: SocketAddr,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for HealthServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthServer")
            .field("addr", &self.addr)
            .field("state", &self.state)
            .finish()
    }
}

impl HealthServer {
    /// Bind `config.host:config.port` and serve the health endpoints for
    /// `node`. Port `0` binds an ephemeral port. Must run inside a Tokio
    /// runtime.
    pub async fn start(node: &Node, config: HealthConfig) -> Result<HealthServer> {
        Self::start_with_state(HealthState::new(Some(node)), config).await
    }

    /// Like [`start`](Self::start), serving an existing [`HealthState`] (e.g.
    /// one a binding created before the node, or shares with its own stack).
    pub async fn start_with_state(
        state: HealthState,
        config: HealthConfig,
    ) -> Result<HealthServer> {
        let host = config.host.trim();
        let host = if host.is_empty() {
            DEFAULT_HEALTH_HOST
        } else {
            host
        };
        // Bracket bare IPv6 literals so `host:port` parses.
        let target = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{}", config.port)
        } else {
            format!("{host}:{}", config.port)
        };
        let listener = TcpListener::bind(&target)
            .await
            .map_err(|e| Error::Connection(format!("health server bind {target}: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| Error::Connection(format!("health server local_addr: {e}")))?;
        let (stop_tx, stop_rx) = oneshot::channel();
        let task = tokio::spawn(accept_loop(listener, state.clone(), stop_rx));
        Ok(HealthServer {
            state,
            addr,
            stop: Mutex::new(Some(stop_tx)),
            task: Mutex::new(Some(task)),
        })
    }

    /// The bound address (the real port when started with port `0`).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The shared state this server reports.
    pub fn state(&self) -> HealthState {
        self.state.clone()
    }

    /// Shorthand for [`HealthState::set_ready`].
    pub fn set_ready(&self, ready: bool) {
        self.state.set_ready(ready);
    }

    /// Shorthand for [`HealthState::set_healthy`].
    pub fn set_healthy(&self, healthy: bool) {
        self.state.set_healthy(healthy);
    }

    /// Whether [`shutdown`](Self::shutdown) has completed (or started).
    pub fn is_shut_down(&self) -> bool {
        self.stop.lock().map(|s| s.is_none()).unwrap_or(true)
    }

    /// Stop listening and wait until the port is released. Idempotent.
    pub async fn shutdown(&self) {
        if let Some(stop) = self.stop.lock().ok().and_then(|mut s| s.take()) {
            let _ = stop.send(());
        }
        let task = self.task.lock().ok().and_then(|mut t| t.take());
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for HealthServer {
    fn drop(&mut self) {
        if let Ok(mut task) = self.task.lock() {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str) -> Request<()> {
        Request::get(path).body(()).unwrap()
    }

    #[test]
    fn routes_and_status_codes() {
        let state = HealthState::new(None);
        assert_eq!(route(&state, &get("/healthz")).status(), StatusCode::OK);
        assert_eq!(
            route(&state, &get("/readyz")).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        state.set_ready(true);
        assert_eq!(route(&state, &get("/readyz")).status(), StatusCode::OK);
        assert_eq!(route(&state, &get("/readyz/")).status(), StatusCode::OK);
        state.set_healthy(false);
        assert_eq!(
            route(&state, &get("/healthz")).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        // Unhealthy implies not ready.
        assert_eq!(
            route(&state, &get("/readyz")).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(route(&state, &get("/nope")).status(), StatusCode::NOT_FOUND);
        let post = Request::post("/healthz").body(()).unwrap();
        assert_eq!(
            route(&state, &post).status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[test]
    fn metrics_negotiation() {
        let state = HealthState::new(None);
        let ct = |r: &Response<Full<Bytes>>| {
            r.headers()
                .get(CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(ct(&route(&state, &get("/metrics"))), "application/json");
        assert_eq!(
            ct(&route(&state, &get("/metrics?format=prometheus"))),
            PROMETHEUS_CONTENT_TYPE
        );
        assert_eq!(
            ct(&route(&state, &get("/metrics/prometheus"))),
            PROMETHEUS_CONTENT_TYPE
        );
        let scrape = Request::get("/metrics")
            .header(
                ACCEPT,
                "application/openmetrics-text;version=1.0.0,text/plain;version=0.0.4;q=0.5",
            )
            .body(())
            .unwrap();
        assert_eq!(ct(&route(&state, &scrape)), PROMETHEUS_CONTENT_TYPE);
    }

    #[test]
    fn counters_snapshot_and_render() {
        let state = HealthState::new(None);
        let m = state.metrics();
        m.connection_opened();
        m.connection_opened();
        m.connection_closed();
        m.connection_closed();
        m.connection_closed(); // saturates at 0
        m.stream_opened();
        m.record_consumer_admit(12.34);
        m.record_consumer_deny();
        m.record_producer_admit(1.5);
        m.rpc_started();
        m.rpc_started();
        m.rpc_started();
        m.rpc_completed(true);
        m.rpc_completed(false);

        let s = state.snapshot();
        assert_eq!(s.active_connections, 0);
        assert_eq!(s.total_connections, 2);
        assert_eq!(s.active_streams, 1);
        assert_eq!(s.total_streams, 1);
        assert_eq!(s.consumer_admitted, 1);
        assert_eq!(s.consumer_denied, 1);
        assert_eq!(s.producer_admitted, 1);
        assert_eq!(s.last_admission_ms, 1.5);
        assert_eq!(s.rpc_in_flight, 1);

        let text = state.prometheus_text();
        assert!(text.contains("aster_connections_total 2\n"));
        assert!(text.contains("aster_rpc_completed_total{status=\"ok\"} 1\n"));
        assert!(text.contains("aster_rpc_completed_total{status=\"error\"} 1\n"));
        assert!(text.contains("aster_ready 0\n"));
        assert!(text.contains("# TYPE aster_uptime_seconds gauge\n"));

        let json: serde_json::Value = serde_json::from_str(&state.metrics_json()).unwrap();
        assert_eq!(json["connections"]["total_connections"], 2);
        assert_eq!(json["admission"]["consumer_admitted"], 1);
        assert_eq!(json["rpc"]["in_flight"], 1);
        assert_eq!(json["health"]["status"], "ok");
        assert_eq!(json["ready"]["status"], "not_ready");

        m.reset();
        assert_eq!(state.snapshot().total_connections, 0);
    }
}
