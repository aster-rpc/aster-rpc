//! Ops endpoints: liveness, readiness and Prometheus metrics over plain HTTP
//! (feature `health`, design `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md`
//! §6). The Rust home of the bindings' `HealthServer`.
//!
//! A small HTTP/1.1 listener (hyper) on its own TCP port, separate from the
//! QUIC endpoint, for Kubernetes probes, load balancers and Prometheus:
//!
//! | Route | Response |
//! |---|---|
//! | `GET /healthz` | `200 {"status":"ok","uptime_s":…}`, `503 {"status":"unhealthy",…}` once [`set_healthy(false)`](HealthState::set_healthy) |
//! | `GET /readyz` | `200 {"status":"ready","services":n,"registry":b,"checks":{name:bool}}` when ready, else `503` with `"status":"not_ready"`. Ready = healthy, [`set_ready(true)`](HealthState::set_ready), every named [`check`](HealthState::check) passing, and (once a binding log bridge is open) the built-in [`audit`](AUDIT_CHECK) check: no `aster::audit` record dropped in the last 30 s |
//! | `GET /metrics` | Prometheus text exposition ([`PROMETHEUS_CONTENT_TYPE`]) from the OpenTelemetry pull reader: the built-in `aster.*` instruments, any custom ones, `process.uptime` and `target_info` |
//! | anything else | `404 {"error":"not found"}`; non-`GET`/`HEAD` → `405` |
//!
//! `health` implies `otel`. [`HealthServer::start`] holds a refcounted
//! [`ObservabilityHandle`]: it joins the running `aster_otel` pipeline, or
//! starts one from the environment (`OTEL_*` / `ASTER_*`), and makes sure that
//! pipeline has the Prometheus pull reader whatever `OTEL_METRICS_EXPORTER`
//! says ([`aster_otel::init_with_prometheus`]), so a running listener always
//! has metrics to serve. `OTEL_SDK_DISABLED=true` is the exception: `/metrics`
//! then answers 503.
//!
//! The metrics are recorded where the events happen (RPC dispatch, the serve
//! loop, Gate 0, upcalls; `rpc::observe`), not by this module. What it adds
//! are observable instruments read at each scrape: `aster.health.ready` and
//! `aster.health.healthy` (0/1, over every live [`HealthState`]),
//! `process.uptime`, and the iroh transport counters of each state's node
//! (`aster.transport.bytes` by `aster.path` and `aster.direction`,
//! `aster.transport.connections`).
//!
//! ```no_run
//! # async fn run(node: aster::Node) -> aster::Result<()> {
//! use aster::ops::{HealthConfig, HealthServer};
//!
//! let health = HealthServer::start(&node, HealthConfig::new("0.0.0.0", 8080)).await?;
//! let db = health.state().check("db");
//! health.state().set_services(3);
//! health.state().set_ready(true);
//! db.set(true); // ready once the database is reachable too
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
//!
//! [`route`] is public so another HTTP stack (a Salvo handler on the HTTP/3
//! transport, later) can serve the same routing table.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use aster_otel::opentelemetry::metrics::Meter;
use aster_otel::opentelemetry::KeyValue;
use aster_otel::{ObservabilityConfig, ObservabilityHandle};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderValue, ALLOW, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

use crate::error::{Error, Result};
use crate::log_targets::OPS;
use crate::node::Node;

/// Default bind host: localhost only. Use `0.0.0.0` for pod probes.
pub const DEFAULT_HEALTH_HOST: &str = "127.0.0.1";

/// `Content-Type` of `/metrics`: the Prometheus text exposition format 0.0.4.
/// OpenMetrics is not offered (the exporter's encoder does not write it), so
/// the `Accept` header is not consulted.
pub const PROMETHEUS_CONTENT_TYPE: &str = aster_otel::PROMETHEUS_TEXT_CONTENT_TYPE;

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

// ── Readiness checks ─────────────────────────────────────────────────────────

/// A named readiness check from [`HealthState::check`]. Clone-cheap; every
/// clone (and every later `check` with the same name) sets the same flag.
#[derive(Clone, Debug)]
pub struct ReadinessCheck {
    name: Arc<str>,
    ok: Arc<AtomicBool>,
}

impl ReadinessCheck {
    /// Pass (`true`) or fail (`false`) the check.
    pub fn set(&self, ok: bool) {
        self.ok.store(ok, Ordering::Relaxed);
    }

    /// Whether the check currently passes.
    pub fn is_ok(&self) -> bool {
        self.ok.load(Ordering::Relaxed)
    }

    /// The check's name, as `/readyz` reports it.
    pub fn name(&self) -> &str {
        &self.name
    }
}

// ── Shared state ─────────────────────────────────────────────────────────────

/// A point-in-time copy of what `/healthz` and `/readyz` report. Plain fields
/// so a binding can convert it field by field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HealthSnapshot {
    /// Seconds since the [`HealthState`] was created.
    pub uptime_s: f64,
    /// Liveness (`/healthz`).
    pub healthy: bool,
    /// Readiness (`/readyz`): healthy, the ready flag, and every check.
    pub ready: bool,
    /// Services registered, reported by `/readyz`.
    pub services: u32,
    /// Whether contracts were published to a registry, reported by `/readyz`.
    pub registry: bool,
    /// The named readiness checks, sorted by name.
    pub checks: Vec<(String, bool)>,
}

struct StateInner {
    /// Its transport counters feed `aster.transport.*`.
    node: Option<Node>,
    started: Instant,
    healthy: AtomicBool,
    ready: AtomicBool,
    services: AtomicU32,
    registry: AtomicBool,
    checks: Mutex<BTreeMap<String, ReadinessCheck>>,
}

/// Liveness/readiness flags behind a [`HealthServer`].
///
/// Clone-cheap; every clone sees the same state. Starts **healthy** and **not
/// ready** — call [`set_ready(true)`](Self::set_ready) once the process can
/// take traffic (an [`AsterServer`](crate::rpc::AsterServer) does this after
/// its services are being served), and pass any named [`check`](Self::check).
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
    /// Fresh state. `node`, when given, contributes its transport counters to
    /// `/metrics`. Every live state also feeds `aster.health.*` and
    /// `process.uptime`.
    pub fn new(node: Option<&Node>) -> Self {
        let inner = Arc::new(StateInner {
            node: node.cloned(),
            started: Instant::now(),
            healthy: AtomicBool::new(true),
            ready: AtomicBool::new(false),
            services: AtomicU32::new(0),
            registry: AtomicBool::new(false),
            checks: Mutex::new(BTreeMap::new()),
        });
        let mut live = sources().lock().unwrap_or_else(|e| e.into_inner());
        live.retain(|w| w.strong_count() > 0);
        live.push(Arc::downgrade(&inner));
        drop(live);
        Self { inner }
    }

    /// Set liveness. `false` makes `/healthz` answer 503 (and `/readyz`).
    pub fn set_healthy(&self, healthy: bool) {
        self.inner.healthy.store(healthy, Ordering::Relaxed);
    }

    /// Current liveness.
    pub fn is_healthy(&self) -> bool {
        self.inner.healthy.load(Ordering::Relaxed)
    }

    /// Set the ready flag. `/readyz` answers 200 only while it is set, the
    /// state is healthy, and every [`check`](Self::check) passes.
    pub fn set_ready(&self, ready: bool) {
        self.inner.ready.store(ready, Ordering::Relaxed);
    }

    /// Current readiness: healthy, the ready flag, every check, and the
    /// built-in `audit` check (see [`snapshot`](Self::snapshot)).
    pub fn is_ready(&self) -> bool {
        self.is_healthy()
            && self.inner.ready.load(Ordering::Relaxed)
            && self.lock_checks().values().all(ReadinessCheck::is_ok)
            && audit_check().unwrap_or(true)
    }

    /// The named readiness check `name`, created **failing** on first use (a
    /// declared dependency is not ready until it says so). Readiness requires
    /// every check to pass; `/readyz` lists them under `checks`.
    pub fn check(&self, name: &str) -> ReadinessCheck {
        self.lock_checks()
            .entry(name.to_owned())
            .or_insert_with(|| ReadinessCheck {
                name: Arc::from(name),
                ok: Arc::new(AtomicBool::new(false)),
            })
            .clone()
    }

    /// Forget the check `name` (it no longer gates readiness). Returns whether
    /// it existed.
    pub fn remove_check(&self, name: &str) -> bool {
        self.lock_checks().remove(name).is_some()
    }

    fn lock_checks(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ReadinessCheck>> {
        self.inner.checks.lock().unwrap_or_else(|e| e.into_inner())
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

    /// Time since this state was created.
    pub fn uptime(&self) -> Duration {
        self.inner.started.elapsed()
    }

    /// Copy every flag and check. Once a binding log bridge has been opened
    /// in this process, the checks include the built-in `audit` check (design
    /// §3.5): it fails for 30 s after an `aster::audit` record was dropped
    /// (`aster_otel::bridge::audit_degraded`), so `/readyz` answers 503 and
    /// operators can alert on it. A check the application named `audit` is
    /// combined with it (both must pass).
    pub fn snapshot(&self) -> HealthSnapshot {
        let mut checks: BTreeMap<String, bool> = self
            .lock_checks()
            .iter()
            .map(|(k, c)| (k.clone(), c.is_ok()))
            .collect();
        if let Some(ok) = audit_check() {
            checks
                .entry(AUDIT_CHECK.to_owned())
                .and_modify(|v| *v &= ok)
                .or_insert(ok);
        }
        let checks: Vec<(String, bool)> = checks.into_iter().collect();
        let healthy = self.is_healthy();
        HealthSnapshot {
            uptime_s: self.uptime().as_secs_f64(),
            healthy,
            ready: healthy
                && self.inner.ready.load(Ordering::Relaxed)
                && checks.iter().all(|(_, ok)| *ok),
            services: self.services(),
            registry: self.inner.registry.load(Ordering::Relaxed),
            checks,
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
}

/// The name of the built-in audit readiness check (design §3.5).
pub const AUDIT_CHECK: &str = "audit";

/// The audit check: `None` until a log bridge has been opened in this
/// process, then whether no audit record was dropped in the last 30 s.
fn audit_check() -> Option<bool> {
    aster_otel::bridge::ever_opened().then(|| !aster_otel::bridge::audit_degraded())
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
    let checks: serde_json::Map<String, serde_json::Value> = s
        .checks
        .iter()
        .map(|(k, ok)| (k.clone(), serde_json::Value::Bool(*ok)))
        .collect();
    json!({
        "status": if s.ready { "ready" } else { "not_ready" },
        "services": s.services,
        "registry": s.registry,
        "checks": checks,
    })
}

// ── Observable instruments (design §4.1) ─────────────────────────────────────

/// Every live [`HealthState`], read by the observable callbacks.
fn sources() -> &'static Mutex<Vec<Weak<StateInner>>> {
    static SOURCES: OnceLock<Mutex<Vec<Weak<StateInner>>>> = OnceLock::new();
    SOURCES.get_or_init(|| Mutex::new(Vec::new()))
}

fn live_states() -> Vec<Arc<StateInner>> {
    sources()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(Weak::upgrade)
        .collect()
}

fn all_flag(states: &[Arc<StateInner>], flag: impl Fn(&HealthState) -> bool) -> u64 {
    u64::from(states.iter().all(|s| {
        flag(&HealthState {
            inner: Arc::clone(s),
        })
    }))
}

/// The transport counters summed over the distinct nodes of `states`.
fn transport(states: &[Arc<StateInner>]) -> aster_transport_core::TransportCounters {
    let mut seen = Vec::new();
    let mut sum = aster_transport_core::TransportCounters::default();
    for node in states.iter().filter_map(|s| s.node.as_ref()) {
        let id = node.id();
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        let c = node.core().transport_counters();
        sum.send_ipv4 += c.send_ipv4;
        sum.send_ipv6 += c.send_ipv6;
        sum.send_relay += c.send_relay;
        sum.recv_ipv4 += c.recv_ipv4;
        sum.recv_ipv6 += c.recv_ipv6;
        sum.recv_relay += c.recv_relay;
        sum.conns_opened += c.conns_opened;
        sum.conns_closed += c.conns_closed;
    }
    sum
}

/// The generation whose meter provider has the callbacks below.
static OBSERVED_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Register this module's observable instruments on `meter`, the meter of
/// provider generation `generation`, once per generation (callbacks live as
/// long as their provider; registering twice would report twice).
pub(crate) fn register_observables(generation: u64, meter: &Meter) {
    if OBSERVED_GENERATION.fetch_max(generation, Ordering::Relaxed) >= generation {
        return;
    }
    meter
        .u64_observable_gauge("aster.health.ready")
        .with_description("1 while every live health state is ready, else 0")
        .with_callback(|o| {
            let states = live_states();
            if !states.is_empty() {
                o.observe(all_flag(&states, HealthState::is_ready), &[]);
            }
        })
        .build();
    meter
        .u64_observable_gauge("aster.health.healthy")
        .with_description("1 while every live health state is healthy, else 0")
        .with_callback(|o| {
            let states = live_states();
            if !states.is_empty() {
                o.observe(all_flag(&states, HealthState::is_healthy), &[]);
            }
        })
        .build();
    meter
        .f64_observable_gauge("process.uptime")
        .with_description("Time since the oldest live health state was created")
        .with_unit("s")
        .with_callback(|o| {
            let up = live_states()
                .iter()
                .map(|s| s.started.elapsed().as_secs_f64())
                .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
            if let Some(up) = up {
                o.observe(up, &[]);
            }
        })
        .build();
    meter
        .u64_observable_counter("aster.transport.bytes")
        .with_description("Bytes the iroh endpoint sent and received, by path")
        .with_callback(|o| {
            let states = live_states();
            if !states.iter().any(|s| s.node.is_some()) {
                return;
            }
            let c = transport(&states);
            for (path, tx, rx) in [
                ("ipv4", c.send_ipv4, c.recv_ipv4),
                ("ipv6", c.send_ipv6, c.recv_ipv6),
                ("relay", c.send_relay, c.recv_relay),
            ] {
                let p = KeyValue::new("aster.path", path);
                o.observe(
                    tx,
                    &[p.clone(), KeyValue::new("aster.direction", "transmit")],
                );
                o.observe(rx, &[p, KeyValue::new("aster.direction", "receive")]);
            }
        })
        .build();
    meter
        .u64_observable_gauge("aster.transport.connections")
        .with_description("Handshaked iroh connections currently open")
        .with_callback(|o| {
            let states = live_states();
            if !states.iter().any(|s| s.node.is_some()) {
                return;
            }
            let c = transport(&states);
            o.observe(c.conns_opened.saturating_sub(c.conns_closed), &[]);
        })
        .build();
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

/// The response type of [`route`].
pub type OpsResponse = Response<Full<Bytes>>;

fn respond(status: StatusCode, content_type: &'static str, body: String) -> OpsResponse {
    let mut resp = Response::new(Full::new(Bytes::from(body)));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

fn json_response(status: StatusCode, body: String) -> OpsResponse {
    respond(status, "application/json", body)
}

/// The `/metrics` body: the running pipeline's Prometheus pull reader as text
/// exposition, or `None` without one (no pipeline, or `OTEL_SDK_DISABLED`).
/// For a binding that serves metrics from its own HTTP stack.
pub fn metrics_text() -> Option<String> {
    // Registers the observables on a provider generation no call has
    // recorded on yet, so they are in this scrape.
    let _ = crate::rpc::observe::otel::builtins();
    aster_otel::render_prometheus()
}

/// Route one ops request: `/healthz`, `/readyz`, `/metrics` (a trailing `/` is
/// ignored), `404` otherwise, `405` for methods other than `GET` / `HEAD`.
/// The body is ignored. Public so another HTTP stack can serve the same
/// routes; [`HealthServer`] is the built-in listener.
pub fn route<B>(state: &HealthState, req: &Request<B>) -> OpsResponse {
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
            let snapshot = state.snapshot();
            let status = if snapshot.ready {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            json_response(status, readyz_value(&snapshot).to_string())
        }
        "/metrics" => match metrics_text() {
            Some(text) => respond(StatusCode::OK, PROMETHEUS_CONTENT_TYPE, text),
            None => respond(
                StatusCode::SERVICE_UNAVAILABLE,
                "text/plain; charset=utf-8",
                "# no metrics pipeline (OpenTelemetry not initialised, or OTEL_SDK_DISABLED)\n"
                    .into(),
            ),
        },
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
                let (stream, _peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        // Transient accept errors (EMFILE, ECONNABORTED): back
                        // off briefly rather than spinning.
                        tracing::debug!(target: OPS, error = %e, "health listener accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                if conns.len() >= MAX_CONNECTIONS {
                    tracing::debug!(target: OPS, "health listener at its connection limit; closing");
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
                    if let Err(e) = builder.serve_connection(TokioIo::new(stream), service).await {
                        tracing::debug!(target: OPS, error = %e, "health connection failed");
                    }
                });
            }
        }
    }
    // Dropping the listener frees the port; dropping the set aborts any
    // connection still being served.
    drop(listener);
    conns.shutdown().await;
}

/// Join the running observability pipeline, or start one from the
/// environment, with the Prometheus pull reader (design §6). Off the async
/// worker: a re-init shuts the replaced meter provider down, which may flush
/// to an OTLP collector.
async fn acquire_observability() -> Result<ObservabilityHandle> {
    let handle = tokio::task::spawn_blocking(|| {
        aster_otel::init_with_prometheus(ObservabilityConfig::default())
    })
    .await
    .map_err(|e| Error::Transport(anyhow::anyhow!("observability init: {e}")))?
    .map_err(|e| Error::Transport(anyhow::anyhow!("observability init: {e}")))?;
    // Register the observables for this generation now, so the first scrape
    // has them even before any call records.
    let _ = crate::rpc::observe::otel::builtins();
    Ok(handle)
}

/// A running health/readiness/metrics HTTP listener.
///
/// Start with [`start`](Self::start); stop with [`shutdown`](Self::shutdown)
/// (or drop it, which aborts the listener without waiting). All methods take
/// `&self`, so a binding can hold it behind a shared handle. It holds a
/// reference to the observability pipeline for as long as it lives.
pub struct HealthServer {
    state: HealthState,
    addr: SocketAddr,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    task: Mutex<Option<JoinHandle<()>>>,
    observability: ObservabilityHandle,
}

impl std::fmt::Debug for HealthServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthServer")
            .field("addr", &self.addr)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl HealthServer {
    /// Bind `config.host:config.port` and serve the ops endpoints for `node`.
    /// Port `0` binds an ephemeral port. Joins (or starts, from the
    /// environment) the observability pipeline and gives it the Prometheus
    /// pull reader; see the [module docs](self). Must run inside a Tokio
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
        let observability = acquire_observability().await?;
        let (stop_tx, stop_rx) = oneshot::channel();
        let task = tokio::spawn(accept_loop(listener, state.clone(), stop_rx));
        tracing::info!(target: OPS, %addr, "health listener started");
        Ok(HealthServer {
            state,
            addr,
            stop: Mutex::new(Some(stop_tx)),
            task: Mutex::new(Some(task)),
            observability,
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

    /// The observability pipeline reference this server holds.
    pub fn observability(&self) -> &ObservabilityHandle {
        &self.observability
    }

    /// Shorthand for [`HealthState::set_ready`].
    pub fn set_ready(&self, ready: bool) {
        self.state.set_ready(ready);
    }

    /// Shorthand for [`HealthState::set_healthy`].
    pub fn set_healthy(&self, healthy: bool) {
        self.state.set_healthy(healthy);
    }

    /// Shorthand for [`HealthState::check`].
    pub fn check(&self, name: &str) -> ReadinessCheck {
        self.state.check(name)
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
            tracing::info!(target: OPS, addr = %self.addr, "health listener stopped");
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
        assert_eq!(
            route(&state, &get("/metrics/prometheus")).status(),
            StatusCode::NOT_FOUND,
            "the R7 alias is gone"
        );
        let post = Request::post("/healthz").body(()).unwrap();
        assert_eq!(
            route(&state, &post).status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[test]
    fn checks_gate_readiness() {
        let state = HealthState::new(None);
        state.set_ready(true);
        assert!(state.is_ready());
        let db = state.check("db");
        assert!(!db.is_ok(), "a new check starts failing");
        assert!(!state.is_ready());
        db.set(true);
        assert!(state.is_ready());
        // Same name, same flag.
        state.check("db").set(false);
        assert!(!db.is_ok() && !state.is_ready());
        let cache = state.check("cache");
        cache.set(true);
        db.set(true);
        let s = state.snapshot();
        assert!(s.ready);
        assert_eq!(
            s.checks,
            vec![("cache".to_string(), true), ("db".to_string(), true)]
        );
        let v: serde_json::Value = serde_json::from_str(&state.readyz_json()).unwrap();
        assert_eq!(v["checks"]["db"], true);
        assert_eq!(v["status"], "ready");
        cache.set(false);
        assert!(state.remove_check("cache"));
        assert!(!state.remove_check("cache"));
        assert!(state.is_ready(), "a removed check no longer gates");
    }
}
