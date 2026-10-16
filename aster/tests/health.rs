#![cfg(feature = "health")]
//! `aster::ops`: the HTTP health/readiness/metrics listener (design
//! `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md` §6, phase O3), hit
//! with a plain TCP client (no HTTP client dependency), plus the
//! `AsterServer` auto-start path.
//!
//! `/metrics` is the OTel Prometheus pull reader, and the pipeline is
//! process-global, so every case that asserts on metrics or on how the
//! listener joins a pipeline runs in a fresh copy of this test binary.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use aster::ops::{HealthConfig, HealthServer, PROMETHEUS_CONTENT_TYPE};
use aster::rpc::{AsterServer, Call, RpcStatus, ServiceDispatch, StatusCode, StreamHeader};
use aster::{AsterConfig, BlobFormat, FetchStrategy, Node, RelayMode};
use aster_otel::testing::TestExporters;
use aster_otel::ObservabilityConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

// ── child-process harness ───────────────────────────────────────────────────

const CASE: &str = "ASTER_HEALTH_IT_CASE";

fn run(case: &str) {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CASE, case);
    for (k, _) in std::env::vars() {
        if k.starts_with("OTEL_") || k.starts_with("ASTER_") || k == "RUST_LOG" {
            cmd.env_remove(k);
        }
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("case-ok"),
        "case {case} failed: {:?}\n--- stdout\n{stdout}\n--- stderr\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn child() {
    let Ok(case) = std::env::var(CASE) else {
        return;
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        match case.as_str() {
            "endpoints" => case_endpoints().await,
            "implicit_init" => case_implicit_init().await,
            "after_init_without_reader" => case_after_init_without_reader().await,
            "after_init_with_reader" => case_after_init_with_reader().await,
            "gate0" => case_gate0().await,
            "audit_readiness" => case_audit_readiness(),
            "blob_peer_metrics" => case_blob_peer_metrics().await,
            "peer_label_cap" => case_peer_label_cap().await,
            other => panic!("unknown case {other}"),
        }
    });
    println!("case-ok");
}

// ── plumbing ────────────────────────────────────────────────────────────────

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

struct Resp {
    status: u16,
    headers: String,
    body: String,
}

/// One HTTP/1.1 request over a fresh TCP connection; reads to EOF (the server
/// closes after each response).
async fn http(addr: SocketAddr, method: &str, path: &str, extra: &str) -> Resp {
    timeout(Duration::from_secs(10), async {
        let mut s = TcpStream::connect(addr).await.expect("connect");
        let req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n{extra}\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).await.unwrap();
        let raw = String::from_utf8(raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").expect("header/body split");
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .expect("status code");
        Resp {
            status,
            headers: head.to_ascii_lowercase(),
            body: body.to_string(),
        }
    })
    .await
    .expect("request timed out")
}

async fn get(addr: SocketAddr, path: &str) -> Resp {
    http(addr, "GET", path, "").await
}

fn json(r: &Resp) -> serde_json::Value {
    serde_json::from_str(&r.body).unwrap_or_else(|e| panic!("bad JSON {:?}: {e}", r.body))
}

// ── a minimal Prometheus text-format parser ─────────────────────────────────

#[derive(Debug)]
struct Sample {
    name: String,
    labels: BTreeMap<String, String>,
    value: f64,
}

/// Parse the text exposition format 0.0.4, panicking on any line that is not
/// a valid `# HELP`, `# TYPE` or sample line.
fn parse_prometheus(text: &str) -> Vec<Sample> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# HELP ") {
            assert!(!rest.is_empty(), "empty HELP: {line}");
            continue;
        }
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest.split_once(' ').expect("TYPE name kind");
            assert!(valid_name(name), "bad metric name in {line}");
            assert!(
                ["counter", "gauge", "histogram", "summary", "untyped"].contains(&kind),
                "bad TYPE: {line}"
            );
            continue;
        }
        assert!(!line.starts_with('#'), "unknown comment: {line}");
        let (series, value) = line.rsplit_once(' ').expect("sample value");
        let value: f64 = match value {
            "+Inf" => f64::INFINITY,
            "-Inf" => f64::NEG_INFINITY,
            v => v.parse().unwrap_or_else(|_| panic!("bad value: {line}")),
        };
        let (name, labels) = match series.split_once('{') {
            None => (series, BTreeMap::new()),
            Some((name, rest)) => {
                let body = rest.strip_suffix('}').expect("closing brace");
                (name, parse_labels(body, line))
            }
        };
        assert!(valid_name(name), "bad metric name in {line}");
        out.push(Sample {
            name: name.to_string(),
            labels,
            value,
        });
    }
    out
}

fn valid_name(n: &str) -> bool {
    let mut chars = n.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == ':')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

fn parse_labels(body: &str, line: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    let mut rest = body;
    while !rest.is_empty() {
        let (key, after) = rest
            .split_once("=\"")
            .unwrap_or_else(|| panic!("label: {line}"));
        assert!(valid_name(key), "bad label name in {line}");
        let mut value = String::new();
        let mut chars = after.char_indices();
        let end = loop {
            match chars.next() {
                Some((_, '\\')) => match chars.next() {
                    Some((_, 'n')) => value.push('\n'),
                    Some((_, c)) => value.push(c),
                    None => panic!("dangling escape: {line}"),
                },
                Some((i, '"')) => break i,
                Some((_, c)) => value.push(c),
                None => panic!("unterminated label value: {line}"),
            }
        };
        labels.insert(key.to_string(), value);
        rest = after[end + 1..].trim_start_matches(',');
    }
    labels
}

/// The value of the sample `name` whose labels include all of `want`.
fn value(samples: &[Sample], name: &str, want: &[(&str, &str)]) -> Option<f64> {
    samples
        .iter()
        .find(|s| {
            s.name == name
                && want
                    .iter()
                    .all(|(k, v)| s.labels.get(*k).map(String::as_str) == Some(*v))
        })
        .map(|s| s.value)
}

async fn scrape(addr: SocketAddr) -> Vec<Sample> {
    let r = get(addr, "/metrics").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        r.headers
            .contains(&format!("content-type: {PROMETHEUS_CONTENT_TYPE}")),
        "{}",
        r.headers
    );
    parse_prometheus(&r.body)
}

/// Scrape until `done` holds (instruments settle a moment after the client
/// sees its answer), then return that scrape.
async fn scrape_until(
    addr: SocketAddr,
    what: &str,
    done: impl Fn(&[Sample]) -> bool,
) -> Vec<Sample> {
    let mut last = Vec::new();
    for _ in 0..200 {
        last = scrape(addr).await;
        if done(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}: {last:#?}");
}

// ── a service to call ───────────────────────────────────────────────────────

struct Probe;

#[aster::rpc::async_trait]
impl ServiceDispatch for Probe {
    fn name(&self) -> &str {
        "Probe"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["ok", "fail"]
    }
    async fn dispatch(&self, method: &str, call: Call) {
        let _ = match method {
            "ok" => call.respond(vec![1], &RpcStatus::ok()).await,
            _ => {
                call.finish(&RpcStatus::error(StatusCode::Aborted, "no"))
                    .await
            }
        };
    }
}

fn probe(service: &str, method: &str) -> StreamHeader {
    StreamHeader {
        service: service.into(),
        method: method.into(),
        version: 1,
        ..Default::default()
    }
}

async fn wait_for_addr(n: &Node) {
    for _ in 0..50 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn client_of(srv: &AsterServer) -> Node {
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(srv.node()).await;
    wait_for_addr(&client).await;
    client.add_peer(srv.node()).unwrap();
    srv.node().add_peer(&client).unwrap();
    client
}

async fn probe_server() -> AsterServer {
    AsterServer::builder()
        .config(cfg())
        .service(Probe)
        .health(HealthConfig::new("127.0.0.1", 0))
        .start()
        .await
        .unwrap()
}

const COMPLETED: &str = "aster_rpc_server_requests_completed_total";

fn completed(samples: &[Sample], method: &str, status: &str) -> Option<f64> {
    value(
        samples,
        COMPLETED,
        &[
            ("rpc_service", "Probe"),
            ("rpc_method", method),
            ("aster_status", status),
        ],
    )
}

/// Three OK calls, one handler error and one refusal (no such service), then
/// wait until `/metrics` shows them.
async fn traffic(srv: &AsterServer) -> (Node, Vec<Sample>) {
    let addr = srv.health().unwrap().addr();
    let client = client_of(srv).await;
    let conn = timeout(Duration::from_secs(15), client.rpc_connect(&srv.id()))
        .await
        .unwrap()
        .unwrap();
    for _ in 0..3 {
        assert_eq!(
            conn.unary(&probe("Probe", "ok"), vec![9]).await.unwrap(),
            vec![1]
        );
    }
    assert!(conn.unary(&probe("Probe", "fail"), vec![9]).await.is_err());
    assert!(conn.unary(&probe("Nope", "x"), vec![9]).await.is_err());
    let samples = scrape_until(addr, "five settled calls", |s| {
        completed(s, "ok", "OK") == Some(3.0) && completed(s, "fail", "ABORTED") == Some(1.0)
    })
    .await;
    (client, samples)
}

// ── cases (child side) ──────────────────────────────────────────────────────

/// Liveness, readiness with named checks, routing, HEAD/404/405, and a port
/// released on shutdown. No pipeline existed: the listener started one.
async fn case_endpoints() {
    assert!(aster_otel::state().is_none());
    let node = Node::start(cfg()).await.unwrap();
    let health = HealthServer::start(&node, HealthConfig::new("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = health.addr();
    assert_ne!(addr.port(), 0, "ephemeral port resolved");
    assert!(aster_otel::state().is_some(), "implicit env-only init");

    // Liveness.
    let r = get(addr, "/healthz").await;
    assert_eq!(r.status, 200);
    assert!(r.headers.contains("content-type: application/json"));
    assert_eq!(json(&r)["status"], "ok");
    assert!(json(&r)["uptime_s"].is_number());

    // Readiness starts false, then toggles.
    let r = get(addr, "/readyz").await;
    assert_eq!(r.status, 503);
    assert_eq!(json(&r)["status"], "not_ready");
    assert_eq!(json(&r)["checks"], serde_json::json!({}));
    health.state().set_services(2);
    health.set_ready(true);
    let r = get(addr, "/readyz").await;
    assert_eq!(r.status, 200);
    assert_eq!(json(&r)["status"], "ready");
    assert_eq!(json(&r)["services"], 2);
    health.set_ready(false);
    assert_eq!(get(addr, "/readyz").await.status, 503);
    health.set_ready(true);

    // Named checks: ready = ready flag AND every check.
    let db = health.check("db");
    let r = get(addr, "/readyz").await;
    assert_eq!(r.status, 503, "a declared check starts failing");
    assert_eq!(json(&r)["checks"]["db"], false);
    db.set(true);
    let queue = health.state().check("queue");
    queue.set(true);
    let r = get(addr, "/readyz").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        json(&r)["checks"],
        serde_json::json!({"db": true, "queue": true})
    );
    queue.set(false);
    assert_eq!(get(addr, "/readyz").await.status, 503);
    assert!(health.state().remove_check("queue"));
    assert_eq!(get(addr, "/readyz").await.status, 200);
    // The health gauges follow.
    let s = scrape(addr).await;
    assert_eq!(value(&s, "aster_health_ready", &[]), Some(1.0));
    assert_eq!(value(&s, "aster_health_healthy", &[]), Some(1.0));
    db.set(false);
    let s = scrape(addr).await;
    assert_eq!(value(&s, "aster_health_ready", &[]), Some(0.0));
    db.set(true);

    // Unhealthy fails both probes; liveness ignores checks.
    health.set_healthy(false);
    let r = get(addr, "/healthz").await;
    assert_eq!(r.status, 503);
    assert_eq!(json(&r)["status"], "unhealthy");
    assert_eq!(get(addr, "/readyz").await.status, 503);
    health.set_healthy(true);
    health.check("down").set(false);
    assert_eq!(get(addr, "/healthz").await.status, 200);
    health.state().remove_check("down");

    // `/metrics` is Prometheus text whatever the Accept header says; the
    // JSON shape and the `/metrics/prometheus` alias are gone.
    for extra in [
        "",
        "Accept: application/json\r\n",
        "Accept: application/openmetrics-text;version=1.0.0,text/plain;version=0.0.4;q=0.5\r\n",
    ] {
        let r = http(addr, "GET", "/metrics", extra).await;
        assert_eq!(r.status, 200);
        assert!(r
            .headers
            .contains(&format!("content-type: {PROMETHEUS_CONTENT_TYPE}")));
        let s = parse_prometheus(&r.body);
        assert!(
            value(&s, "process_uptime_seconds", &[]).is_some(),
            "{}",
            r.body
        );
        assert!(
            value(
                &s,
                "aster_transport_bytes_total",
                &[("aster_path", "ipv4"), ("aster_direction", "transmit")]
            )
            .is_some(),
            "{}",
            r.body
        );
        assert!(value(&s, "aster_transport_connections", &[]).is_some());
        assert!(s.iter().any(|x| x.name == "target_info"));
    }
    assert_eq!(get(addr, "/metrics/prometheus").await.status, 404);

    // HEAD, 404, 405.
    let r = http(addr, "HEAD", "/healthz", "").await;
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty());
    let r = get(addr, "/nope").await;
    assert_eq!(r.status, 404);
    assert_eq!(json(&r)["error"], "not found");
    assert_eq!(
        http(addr, "POST", "/healthz", "Content-Length: 0\r\n")
            .await
            .status,
        405
    );

    // Shutdown frees the port: it can be bound again immediately, and new
    // connections are refused.
    health.shutdown().await;
    assert!(health.is_shut_down());
    health.shutdown().await; // idempotent
    assert!(TcpStream::connect(addr).await.is_err());
    let rebound = std::net::TcpListener::bind(addr).expect("port released");
    drop(rebound);

    // The listener held the only reference to the implicit pipeline.
    drop(health);
    assert!(aster_otel::state().is_none());
    node.shutdown().await;
}

/// Health started first (no init anywhere): the scrape carries the built-in
/// `rpc` series from real traffic, connections, and uptime.
async fn case_implicit_init() {
    let srv = probe_server().await;
    let addr = srv.health().unwrap().addr();
    let (client, s) = traffic(&srv).await;
    let started = |svc: &str, m: &str| {
        value(
            &s,
            "aster_rpc_server_requests_started_total",
            &[("rpc_service", svc), ("rpc_method", m)],
        )
    };
    assert_eq!(started("Probe", "ok"), Some(3.0));
    assert_eq!(started("_OTHER", "_OTHER"), Some(1.0), "refusal counted");
    assert_eq!(
        value(
            &s,
            "aster_rpc_server_duration_seconds_count",
            &[("rpc_method", "ok")]
        ),
        Some(3.0)
    );
    assert_eq!(
        value(&s, "aster_server_connections_opened_total", &[]),
        Some(1.0)
    );
    assert_eq!(value(&s, "aster_health_ready", &[]), Some(1.0));
    assert!(value(&s, "process_uptime_seconds", &[]).unwrap() >= 0.0);
    let tx = value(
        &s,
        "aster_transport_bytes_total",
        &[("aster_path", "ipv4"), ("aster_direction", "transmit")],
    )
    .unwrap();
    assert!(tx > 0.0, "the calls moved bytes over IPv4");
    client.shutdown().await;
    let s = scrape_until(addr, "connection close", |s| {
        value(s, "aster_server_connections_active", &[]) == Some(0.0)
    })
    .await;
    assert_eq!(
        value(&s, "aster_server_connections_opened_total", &[]),
        Some(1.0)
    );
    srv.shutdown().await;
}

fn test_cfg(t: &TestExporters) -> ObservabilityConfig {
    ObservabilityConfig {
        from_env: false,
        test_exporters: Some(t.clone()),
        metric_export_interval: Some(Duration::from_secs(3600)),
        ..Default::default()
    }
}

/// An application initialised OTel first, with push exporters only (here:
/// in-memory). Its meter provider already carries the pull reader, so the
/// listener joins without a rebuild; both the pull reader and the push
/// exporter see the calls, and the application's handle still counts.
async fn case_after_init_without_reader() {
    let t = TestExporters::new();
    let app = aster_otel::init(test_cfg(&t)).unwrap();
    let g = aster_otel::generation();
    assert!(aster_otel::render_prometheus().is_some());

    let srv = probe_server().await;
    assert_eq!(aster_otel::generation(), g, "no provider rebuild");
    let (client, s) = traffic(&srv).await;
    assert_eq!(completed(&s, "ok", "OK"), Some(3.0));
    aster_otel::flush(Duration::from_secs(10)).unwrap();
    let pushed: f64 = t
        .metric_points("aster.rpc.server.requests_completed")
        .iter()
        .filter(|p| p.attr("rpc.method") == Some("ok"))
        .map(|p| p.value)
        .sum();
    assert_eq!(pushed, 3.0, "the push exporter still gets the calls");

    // The application lets go first; the listener keeps the pipeline.
    drop(app);
    assert!(aster_otel::state().is_some());
    assert_eq!(
        completed(&scrape(srv.health().unwrap().addr()).await, "ok", "OK"),
        Some(3.0)
    );
    client.shutdown().await;
    srv.shutdown().await;
}

/// An application initialised OTel with a Prometheus exporter: the listener
/// joins it as is.
async fn case_after_init_with_reader() {
    let app = aster_otel::init(ObservabilityConfig {
        from_env: false,
        traces_exporter: Some(vec![]),
        metrics_exporter: Some(vec![aster_otel::ExporterKind::Prometheus]),
        ..Default::default()
    })
    .unwrap();
    let g = aster_otel::generation();
    let srv = probe_server().await;
    assert_eq!(aster_otel::generation(), g, "joined, no re-init");
    let (client, s) = traffic(&srv).await;
    assert_eq!(completed(&s, "fail", "ABORTED"), Some(1.0));
    client.shutdown().await;
    srv.shutdown().await;
    drop(app);
}

/// Gate 0 decisions through `AsterServer::take_admission` show up as
/// `aster.admission.*`.
async fn case_gate0() {
    let srv = AsterServer::builder()
        .config(cfg())
        .hooks(true)
        .service(Probe)
        .health(HealthConfig::new("127.0.0.1", 0))
        .start()
        .await
        .unwrap();
    let addr = srv.health().unwrap().addr();

    let admitted = client_of(&srv).await;
    let refused = client_of(&srv).await;
    let refused_id = refused.id();
    let mut adm = srv.take_admission().expect("hooks enabled");
    tokio::spawn(async move {
        while let Some(req) = adm.next_handshake().await {
            if req.peer == refused_id {
                req.reject(403, b"denied".to_vec());
            } else {
                req.accept();
            }
        }
    });

    let conn = timeout(Duration::from_secs(15), admitted.rpc_connect(&srv.id()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        conn.unary(&probe("Probe", "ok"), vec![9]).await.unwrap(),
        vec![1]
    );
    let refused_call = timeout(Duration::from_secs(10), async {
        let conn = refused.rpc_connect(&srv.id()).await?;
        conn.unary(&probe("Probe", "ok"), vec![9]).await
    })
    .await;
    assert!(!matches!(refused_call, Ok(Ok(_))), "{refused_call:?}");

    let decisions = |s: &[Sample], result: &str| {
        value(
            s,
            "aster_admission_decisions_total",
            &[("aster_role", "consumer"), ("aster_result", result)],
        )
        .unwrap_or(0.0)
    };
    let s = scrape_until(addr, "one admit and one deny", |s| {
        decisions(s, "admit") >= 1.0 && decisions(s, "deny") >= 1.0
    })
    .await;
    assert_eq!(decisions(&s, "error"), 0.0);
    admitted.shutdown().await;
    refused.shutdown().await;
    srv.shutdown().await;
}

// ── blob and peer transfer instruments ──────────────────────────────────────

fn monitored() -> AsterConfig {
    cfg().into_builder().monitoring(true).build()
}

/// Two started, connected nodes with monitoring (the peer view feeds
/// `aster.peer.bytes`).
async fn monitored_node() -> Node {
    let n = Node::start(monitored()).await.unwrap();
    wait_for_addr(&n).await;
    n
}

const BLOB: usize = 256 * 1024;

/// `consumer` fetches a fresh compressible blob from `holder` over zstd.
async fn fetch_blob(consumer: &Node, holder: &Node, fill: u8) {
    consumer.add_peer(holder).unwrap();
    holder.add_peer(consumer).unwrap();
    let hash = holder.blobs().add_bytes(vec![fill; BLOB]).await.unwrap();
    timeout(
        Duration::from_secs(30),
        consumer.blobs().download_hash_to_store_from(
            &hash,
            &[holder.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

fn peer_bytes(s: &[Sample], group: (&str, &str), protocol: &str, direction: &str) -> f64 {
    value(
        s,
        "aster_peer_bytes_total",
        &[
            group,
            ("aster_protocol", protocol),
            ("aster_direction", direction),
        ],
    )
    .unwrap_or(0.0)
}

/// `aster.blobs.*` from a zstd fetch, and `aster.peer.bytes` grouped by
/// locality level.
async fn case_blob_peer_metrics() {
    let a = monitored_node().await;
    let b = monitored_node().await;
    let health = HealthServer::start(&a, HealthConfig::new("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = health.addr();
    fetch_blob(&a, &b, b'x').await;

    let blob = |s: &[Sample], encoding: &str, measure: &str| {
        value(
            s,
            "aster_blobs_bytes_total",
            &[
                ("aster_direction", "fetch"),
                ("aster_encoding", encoding),
                ("aster_measure", measure),
            ],
        )
    };
    let s = scrape_until(addr, "blob and peer bytes", |s| {
        blob(s, "zstd", "payload") == Some(BLOB as f64)
            && peer_bytes(
                s,
                ("aster_locality_level", "same_host"),
                "blobs-zstd",
                "received",
            ) > 0.0
    })
    .await;
    let wire = blob(&s, "zstd", "wire").unwrap();
    assert!(wire > 0.0 && wire < BLOB as f64 / 4.0, "wire {wire}");
    assert_eq!(blob(&s, "plain", "payload"), Some(0.0));
    assert_eq!(
        value(
            &s,
            "aster_blobs_transfers_total",
            &[
                ("aster_direction", "fetch"),
                ("aster_encoding", "zstd"),
                ("aster_result", "ok"),
            ],
        ),
        Some(1.0)
    );
    assert_eq!(
        value(
            &s,
            "aster_blobs_zstd_decode_errors_total",
            &[("aster_direction", "fetch")]
        ),
        Some(0.0)
    );
    assert!(
        s.iter().all(|x| !x.labels.contains_key("aster_peer")),
        "no per-peer labels by default"
    );
    health.shutdown().await;
    a.shutdown().await;
    b.shutdown().await;
}

/// Per-peer labels with a cap of 1: the first peer keeps its own series, the
/// second is counted under `_OTHER`.
async fn case_peer_label_cap() {
    let a = monitored_node().await;
    let b = monitored_node().await;
    let c = monitored_node().await;
    let health = HealthServer::start(&a, HealthConfig::new("127.0.0.1", 0).with_peer_labels(1))
        .await
        .unwrap();
    let addr = health.addr();
    let b_id = b.id().to_string();
    let c_id = c.id().to_string();

    fetch_blob(&a, &b, b'b').await;
    scrape_until(addr, "B's own series", |s| {
        peer_bytes(s, ("aster_peer", &b_id), "blobs-zstd", "received") > 0.0
    })
    .await;

    fetch_blob(&a, &c, b'c').await;
    let s = scrape_until(addr, "C under _OTHER", |s| {
        peer_bytes(s, ("aster_peer", "_OTHER"), "blobs-zstd", "received") > 0.0
    })
    .await;
    assert!(peer_bytes(&s, ("aster_peer", &b_id), "blobs-zstd", "received") > 0.0);
    assert!(
        s.iter()
            .all(|x| x.labels.get("aster_peer").map(String::as_str) != Some(c_id.as_str())),
        "C has no series of its own"
    );
    assert!(
        s.iter()
            .filter(|x| x.name == "aster_peer_bytes_total")
            .all(|x| !x.labels.contains_key("aster_locality_level")),
        "per-peer series are not also grouped by level"
    );
    health.shutdown().await;
    a.shutdown().await;
    b.shutdown().await;
    c.shutdown().await;
}

/// Design §3.5: once a log bridge is open, `/readyz` carries an `audit`
/// check, and a dropped `aster::audit` record fails readiness.
fn case_audit_readiness() {
    // The production window is 30 s. 5 s keeps the drop-then-503 assertions
    // well inside the window on a loaded runner while not waiting 30 s for the
    // recovery. This runs in its own child process, so nothing else sees it.
    aster_otel::bridge::set_audit_degraded_window_for_testing(Duration::from_secs(5));
    let state = aster::ops::HealthState::new(None);
    state.set_ready(true);
    assert!(state.is_ready());
    assert!(
        state.snapshot().checks.is_empty(),
        "no audit check before a bridge is opened"
    );
    let handle = aster_otel::init(ObservabilityConfig {
        from_env: false,
        traces_exporter: Some(vec![]),
        log_bridge: Some(true),
        log_filter: Some("warn".into()),
        log_audit_queue: Some(1),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(state.snapshot().checks, [("audit".to_string(), true)]);
    assert!(state.is_ready());
    // Nobody drains: the second record overflows the one-slot audit channel.
    for n in 0..2 {
        tracing::info!(target: "aster::audit", n, "synthetic audit record");
    }
    assert!(aster_otel::bridge::audit_dropped_total() >= 1);
    let snap = state.snapshot();
    assert!(!snap.ready);
    assert_eq!(snap.checks, [("audit".to_string(), false)]);
    assert!(!state.is_ready());
    let resp = aster::ops::route(&state, &hyper_get("/readyz"));
    assert_eq!(resp.status(), 503);

    // A continuously drop-free window restores readiness after the
    // degradation period (5 s here). Keep the bound loose enough for loaded CI.
    let recovered_by = std::time::Instant::now() + Duration::from_secs(20);
    while aster_otel::bridge::audit_degraded() && std::time::Instant::now() < recovered_by {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !aster_otel::bridge::audit_degraded(),
        "audit readiness did not recover within 20 seconds"
    );
    assert!(state.is_ready());
    assert_eq!(
        aster::ops::route(&state, &hyper_get("/readyz")).status(),
        200
    );
    drop(handle);
}

fn hyper_get(path: &str) -> hyper::Request<()> {
    hyper::Request::get(path).body(()).unwrap()
}

// ── tests (parent side) ─────────────────────────────────────────────────────

#[test]
fn audit_drop_fails_readiness() {
    run("audit_readiness");
}

#[test]
fn endpoints_readiness_checks_and_shutdown() {
    run("endpoints");
}

#[test]
fn health_before_any_init_serves_rpc_series() {
    run("implicit_init");
}

#[test]
fn health_after_an_init_with_push_exporters_joins_and_serves() {
    run("after_init_without_reader");
}

#[test]
fn health_after_an_init_with_prometheus_joins_it() {
    run("after_init_with_reader");
}

#[test]
fn blob_transfers_and_peer_bytes_reach_metrics() {
    run("blob_peer_metrics");
}

#[test]
fn peer_labels_fold_past_the_cap() {
    run("peer_label_cap");
}

#[test]
fn gate0_decisions_reach_metrics() {
    run("gate0");
}

// These share this process's pipeline (whichever listener starts it); they
// assert nothing about metrics.

#[tokio::test]
async fn bind_conflict_is_an_error() {
    let node = Node::start(cfg()).await.unwrap();
    let first = HealthServer::start(&node, HealthConfig::default())
        .await
        .unwrap();
    let err = HealthServer::start(&node, HealthConfig::new("127.0.0.1", first.addr().port()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("health server bind"), "{err}");
    first.shutdown().await;
    node.shutdown().await;
}

#[tokio::test]
async fn aster_server_starts_health_from_config() {
    let config = cfg()
        .into_builder()
        .health(true) // port 0 + enabled → ephemeral
        .health_host("127.0.0.1")
        .build();
    let srv = AsterServer::builder().config(config).start().await.unwrap();
    let health = srv.health().expect("health server started from config");
    let addr = health.addr();
    assert_eq!(get(addr, "/healthz").await.status, 200);
    // Ready once the server is serving.
    let r = get(addr, "/readyz").await;
    assert_eq!(r.status, 200, "{}", r.body);
    // A check added by the application gates it.
    let warm = health.check("cache-warm");
    assert_eq!(get(addr, "/readyz").await.status, 503);
    warm.set(true);
    assert_eq!(get(addr, "/readyz").await.status, 200);
    srv.shutdown().await;
    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn aster_server_builder_option_and_default_off() {
    let srv = AsterServer::builder().config(cfg()).start().await.unwrap();
    assert!(srv.health().is_none(), "disabled by default");
    srv.shutdown().await;

    let srv = AsterServer::builder()
        .config(cfg())
        .health(HealthConfig::new("127.0.0.1", 0))
        .start()
        .await
        .unwrap();
    let addr = srv.health().expect("builder option").addr();
    assert_eq!(get(addr, "/healthz").await.status, 200);
    srv.shutdown().await;
}

/// A deferred server is not ready until its RPC surface is active, counts
/// only host services before that, and is not ready again once the surface
/// is stopped (`docs/_internal/aster-server-late-activation.md`).
#[tokio::test]
async fn deferred_surface_readiness_follows_activation() {
    use aster::rpc::RpcSurface;
    let srv = AsterServer::builder()
        .config(cfg())
        .health(HealthConfig::new("127.0.0.1", 0))
        .deferred_surface()
        .start()
        .await
        .unwrap();
    let addr = srv.health().unwrap().addr();
    assert_eq!(get(addr, "/healthz").await.status, 200, "the node is live");
    let r = get(addr, "/readyz").await;
    assert_eq!(r.status, 503, "{}", r.body);
    assert_eq!(json(&r)["services"], 0);

    srv.activate(RpcSurface::builder().service(Probe).build())
        .await
        .unwrap();
    let r = get(addr, "/readyz").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(json(&r)["services"], 1);
    assert_eq!(json(&r)["registry"], false);

    srv.deactivate().await.unwrap();
    let r = get(addr, "/readyz").await;
    assert_eq!(r.status, 503, "{}", r.body);
    assert_eq!(json(&r)["services"], 0);
    assert_eq!(get(addr, "/healthz").await.status, 200);
    srv.shutdown().await;
}

#[test]
fn prometheus_parser_rejects_garbage() {
    let ok = parse_prometheus(
        "# HELP a_total x\n# TYPE a_total counter\na_total{k=\"v\\\"q\",z=\"1\"} 2\nb 1.5e3\n",
    );
    assert_eq!(ok.len(), 2);
    assert_eq!(ok[0].labels["k"], "v\"q");
    assert_eq!(ok[1].value, 1500.0);
    for bad in ["a_total{k=\"v} 1", "a b c", "1abc 2", "# TYPE x weird"] {
        assert!(
            std::panic::catch_unwind(|| parse_prometheus(bad)).is_err(),
            "{bad}"
        );
    }
}
