#![cfg(feature = "otel")]
//! The `otel` feature end to end (design
//! `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md`, phase O2): the
//! built-in instruments and the server / client spans against aster-otel's
//! in-memory exporters, Rust↔Rust trace continuity, pass-through with no
//! pipeline, init after the server started, shutdown then re-init, and (with
//! `binding-support`) the foreign-call span lifecycle.
//!
//! The OTel pipeline and the `tracing` subscriber are process-global, so each
//! case runs in a fresh copy of this test binary.

use std::future::Future;
use std::time::Duration;

use aster::rpc::{
    Call, CallOptions, RpcConnection, RpcStatus, Server, ServerHandle, ServiceDispatch, StatusCode,
    StreamHeader, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_otel::opentelemetry::trace::{SpanId, SpanKind, Status, TraceId};
use aster_otel::testing::{MetricPoint, SpanData, TestExporters};
use aster_otel::ObservabilityConfig;
use tokio::time::timeout;

// ── child-process harness ───────────────────────────────────────────────────

const CASE: &str = "ASTER_OTEL_IT_CASE";

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
    // Lease cases run on a paused clock: lateness and expiry are driven by
    // `advance`, never by a real stall.
    if let Some(lease_case) = case.strip_prefix("lease_") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(async {
            match lease_case {
                "renew_late" => lease::case_renew_late().await,
                "lost_reasons" => lease::case_lost_reasons().await,
                other => panic!("unknown lease case {other}"),
            }
        });
        println!("case-ok");
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        match case.as_str() {
            "names_and_attributes" => case_names_and_attributes().await,
            "trace_continuity" => case_trace_continuity().await,
            "pass_through_then_hidden" => case_pass_through_then_hidden().await,
            "init_after_server_start" => case_init_after_server_start().await,
            "shutdown_then_reinit" => case_shutdown_then_reinit().await,
            "client_metrics" => case_client_metrics(false).await,
            "client_metrics_peer_labels" => case_client_metrics(true).await,
            "log_bridge_trace_ids" => case_log_bridge_trace_ids().await,
            #[cfg(feature = "binding-support")]
            "foreign_spans" => foreign::case_foreign_spans().await,
            #[cfg(feature = "binding-support")]
            "foreign_inbound_baggage" => foreign::case_inbound_baggage().await,
            #[cfg(feature = "binding-support")]
            "passthrough_none" => foreign::case_passthrough("none").await,
            #[cfg(feature = "binding-support")]
            "passthrough_traces_off" => foreign::case_passthrough("traces_off").await,
            #[cfg(feature = "binding-support")]
            "passthrough_span_filtered" => foreign::case_passthrough("span_filtered").await,
            other => panic!("unknown case {other}"),
        }
    });
    println!("case-ok");
}

// ── plumbing ────────────────────────────────────────────────────────────────

fn node_cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

/// Wait until `n` has a direct address to hand out: a condition wait
/// on the endpoint's address watcher, failing loudly after 30 s rather
/// than letting a later connect fail for an unrelated-looking reason.
async fn wait_for_addr(n: &Node) {
    use aster::native::iroh::Watcher;
    let mut addrs = n.native_endpoint().watch_addr();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while n.addr().direct_addresses.is_empty() {
            addrs.updated().await.expect("endpoint closed");
        }
    })
    .await
    .expect("the node had no direct address after 30 s");
}

async fn within<F: Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(15), f)
        .await
        .expect("operation timed out")
}

/// A server node and a client node that know each other.
async fn nodes() -> (Node, Node) {
    nodes_with(node_cfg()).await
}

/// [`nodes`] with `client_cfg` for the client node.
async fn nodes_with(client_cfg: AsterConfig) -> (Node, Node) {
    let server = Node::start_with_alpns(node_cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(client_cfg).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();
    (server, client)
}

async fn connect(client: &Node, server: &Node) -> RpcConnection {
    within(client.rpc_connect(&server.id())).await.unwrap()
}

fn test_cfg(t: &TestExporters) -> ObservabilityConfig {
    let mut cfg = ObservabilityConfig {
        from_env: false,
        test_exporters: Some(t.clone()),
        metric_export_interval: Some(Duration::from_secs(3600)),
        ..Default::default()
    };
    cfg.span_batch.scheduled_delay = Some(Duration::from_secs(3600));
    cfg
}

fn header(service: &str, method: &str) -> StreamHeader {
    StreamHeader {
        service: service.into(),
        method: method.into(),
        version: 1,
        ..Default::default()
    }
}

/// Flush until `n` spans named `name` have been exported (a server span ends
/// when its call is torn down, a moment after the client has its answer).
async fn spans_named(t: &TestExporters, name: &str, n: usize) -> Vec<SpanData> {
    for _ in 0..100 {
        aster_otel::flush(Duration::from_secs(10)).unwrap();
        let got: Vec<SpanData> = t
            .spans
            .finished_spans()
            .into_iter()
            .filter(|s| s.name == name)
            .collect();
        if got.len() >= n {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "expected {n} spans named {name:?}; exported: {:?}",
        t.span_names()
    );
}

fn of_kind(spans: &[SpanData], kind: SpanKind) -> &SpanData {
    spans
        .iter()
        .find(|s| s.span_kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} span among {} spans", spans.len()))
}

fn attr(s: &SpanData, key: &str) -> Option<String> {
    s.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

#[cfg(feature = "binding-support")]
fn has_event(s: &SpanData, name: &str) -> bool {
    s.events.events.iter().any(|e| e.name == name)
}

/// The value of the point of `metric` whose attributes include `want`.
fn point(t: &TestExporters, metric: &str, want: &[(&str, &str)]) -> Option<MetricPoint> {
    t.metric_points(metric)
        .into_iter()
        .find(|p| want.iter().all(|(k, v)| p.attr(k) == Some(*v)))
}

fn value(t: &TestExporters, metric: &str, want: &[(&str, &str)]) -> f64 {
    point(t, metric, want).map_or(0.0, |p| p.value)
}

// ── services ────────────────────────────────────────────────────────────────

/// `test.Echo`: `unary` echoes, `fails` answers NOT_FOUND, `log` emits one
/// `tracing` event from the handler and echoes, `meta` answers with the
/// metadata the handler sees (`k=v` lines).
struct Echo;

#[async_trait::async_trait]
impl ServiceDispatch for Echo {
    fn name(&self) -> &str {
        "test.Echo"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["unary", "fails", "log", "meta", "hold"]
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        let req = call.recv_request().await.unwrap_or_default();
        match method {
            "unary" => {
                let _ = call.respond(req, &RpcStatus::ok()).await;
            }
            "fails" => {
                let _ = call
                    .finish(&RpcStatus::error(StatusCode::NotFound, "nope"))
                    .await;
            }
            // Never answers: the caller cancels it.
            "hold" => std::future::pending::<()>().await,
            "log" => {
                tracing::info!(target: "aster::test::handler", order = "example-1", "handler-log");
                let _ = call.respond(req, &RpcStatus::ok()).await;
            }
            _ => {
                let h = call.header();
                let lines: Vec<String> = h
                    .metadata_keys
                    .iter()
                    .zip(&h.metadata_values)
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                let _ = call
                    .respond(lines.join("\n").into_bytes(), &RpcStatus::ok())
                    .await;
            }
        }
    }
}

/// `test.Relay`: `relay` calls `test.Echo/unary` on another node and returns
/// its answer.
struct Relay(RpcConnection);

#[async_trait::async_trait]
impl ServiceDispatch for Relay {
    fn name(&self) -> &str {
        "test.Relay"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["relay"]
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let req = call.recv_request().await.unwrap_or_default();
        match self.0.unary(&header("test.Echo", "unary"), req).await {
            Ok(resp) => {
                let _ = call.respond(resp, &RpcStatus::ok()).await;
            }
            Err(e) => {
                let _ = call
                    .finish(&RpcStatus::error(StatusCode::Internal, e.to_string()))
                    .await;
            }
        }
    }
}

async fn echo_server() -> (Node, Node, ServerHandle, RpcConnection) {
    let (server, client) = nodes().await;
    let handle = Server::new(&server).register(Echo).serve();
    let conn = connect(&client, &server).await;
    (server, client, handle, conn)
}

fn meta_lines(resp: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(resp)
        .lines()
        .map(str::to_owned)
        .collect()
}

// ── the cases ───────────────────────────────────────────────────────────────

const ECHO_UNARY: &str = "test.Echo/unary";

/// Instrument names and attributes (§4.1), span names, kinds, attributes and
/// status (§5.2, §5.4), a declarative extractor (§4.4).
async fn case_names_and_attributes() {
    let t = TestExporters::new();
    let mut cfg = test_cfg(&t);
    cfg.attribute_extractors = Some(vec![("app.tenant".into(), "meta:x-tenant".into())]);
    let _h = aster_otel::init(cfg).unwrap();
    let (_server, _client, _handle, conn) = echo_server().await;

    let opts = CallOptions::new().metadata("x-tenant", "acme");
    let resp = within(conn.unary_with(&header("test.Echo", "unary"), vec![1, 2], &opts))
        .await
        .unwrap();
    assert_eq!(resp, [1, 2]);
    let err = within(conn.unary(&header("test.Echo", "fails"), vec![0]))
        .await
        .unwrap_err();
    assert!(matches!(err, aster::Error::Rpc { code, .. } if code == StatusCode::NotFound.as_i32()));
    // Unknown method: refused in routing, with a caller-chosen name.
    within(conn.unary(&header("test.Echo", "no-such-method"), vec![0]))
        .await
        .unwrap_err();

    let spans = spans_named(&t, ECHO_UNARY, 2).await;
    let server = of_kind(&spans, SpanKind::Server);
    let client = of_kind(&spans, SpanKind::Client);
    assert_eq!(attr(server, "rpc.system").as_deref(), Some("aster"));
    assert_eq!(attr(server, "rpc.service").as_deref(), Some("test.Echo"));
    assert_eq!(attr(server, "rpc.method").as_deref(), Some("unary"));
    assert_eq!(attr(server, "aster.status").as_deref(), Some("OK"));
    assert_eq!(attr(server, "app.tenant").as_deref(), Some("acme"));
    assert_eq!(server.status, Status::Ok);
    assert_eq!(attr(client, "rpc.method").as_deref(), Some("unary"));
    assert_eq!(client.status, Status::Ok);
    assert_eq!(server.instrumentation_scope.name(), aster_otel::SCOPE_NAME);

    let fails = spans_named(&t, "test.Echo/fails", 2).await;
    let fails = of_kind(&fails, SpanKind::Server);
    assert_eq!(attr(fails, "aster.status").as_deref(), Some("NOT_FOUND"));
    assert!(matches!(fails.status, Status::Error { .. }));
    let refused = spans_named(&t, "test.Echo/no-such-method", 2).await;
    assert!(matches!(
        of_kind(&refused, SpanKind::Server).status,
        Status::Error { .. }
    ));

    const SVC: (&str, &str) = ("rpc.service", "test.Echo");
    let unary = ("rpc.method", "unary");
    let started = "aster.rpc.server.requests_started";
    let completed = "aster.rpc.server.requests_completed";
    assert_eq!(value(&t, started, &[SVC, unary]), 1.0);
    assert_eq!(
        value(&t, completed, &[SVC, unary, ("aster.status", "OK")]),
        1.0
    );
    assert_eq!(
        value(
            &t,
            completed,
            &[SVC, ("rpc.method", "fails"), ("aster.status", "NOT_FOUND")]
        ),
        1.0
    );
    // The unknown method is labelled `_OTHER`, never by its name.
    assert_eq!(
        value(
            &t,
            completed,
            &[
                ("rpc.service", "_OTHER"),
                ("rpc.method", "_OTHER"),
                ("aster.status", "UNIMPLEMENTED")
            ]
        ),
        1.0
    );
    assert!(t
        .metric_points(completed)
        .iter()
        .all(|p| p.attr("rpc.method") != Some("no-such-method")));
    let d = point(&t, "aster.rpc.server.duration", &[SVC, unary]).unwrap();
    assert_eq!(d.count, 1);
    assert!(d.value > 0.0);
    for p in t.metric_points("aster.rpc.server.in_flight") {
        assert_eq!(p.value, 0.0, "{p:?}");
    }
    assert_eq!(value(&t, "aster.rpc.streams.active", &[]), 0.0);
    assert_eq!(value(&t, "aster.server.connections.opened", &[]), 1.0);
    assert_eq!(value(&t, "aster.server.connections.active", &[]), 1.0);
    assert_eq!(
        value(
            &t,
            "aster.rpc.client.requests_completed",
            &[SVC, unary, ("aster.status", "OK")]
        ),
        1.0
    );
    assert_eq!(
        value(
            &t,
            "aster.rpc.client.requests_completed",
            &[
                SVC,
                ("rpc.method", "no-such-method"),
                ("aster.status", "UNIMPLEMENTED")
            ]
        ),
        1.0
    );
    assert_eq!(
        point(&t, "aster.rpc.client.duration", &[SVC, unary])
            .unwrap()
            .count,
        1
    );
    aster_otel::shutdown().unwrap();
}

/// The client call metrics: `rpc.service` / `rpc.method` (the server's label
/// names), `aster.rpc.client.in_flight` back to zero after success, error and
/// cancellation, and `aster.peer` only when the node opts in (bounded by the
/// peer-label cap, the rest under `_OTHER`).
async fn case_client_metrics(peer_labels: bool) {
    const COMPLETED: &str = "aster.rpc.client.requests_completed";
    const IN_FLIGHT: &str = "aster.rpc.client.in_flight";
    let t = TestExporters::new();
    let _h = aster_otel::init(test_cfg(&t)).unwrap();
    let mut client_cfg = node_cfg();
    client_cfg.metrics_client_peer_labels = peer_labels;
    client_cfg.metrics_peer_label_cap = 1;
    let (server, client) = nodes_with(client_cfg).await;
    let _handle = Server::new(&server).register(Echo).serve();
    let conn = connect(&client, &server).await;
    let svc = ("rpc.service", "test.Echo");
    let peer = server.id();
    let peer_attr = peer.as_str();
    // The label rides on every client series when on, on none when off.
    let peer_want = |m: &'static str| -> Vec<(&'static str, &str)> {
        let mut w = vec![svc, ("rpc.method", m)];
        if peer_labels {
            w.push(("aster.peer", peer_attr));
        }
        w
    };

    within(conn.unary(&header("test.Echo", "unary"), vec![1]))
        .await
        .unwrap();
    within(conn.unary(&header("test.Echo", "fails"), vec![0]))
        .await
        .unwrap_err();
    // A cancelled call: in flight while held, then dropped.
    let held = {
        let conn = conn.clone();
        tokio::spawn(async move { conn.unary(&header("test.Echo", "hold"), vec![]).await })
    };
    let hold = peer_want("hold");
    let mut in_flight = 0.0;
    for _ in 0..100 {
        aster_otel::flush(Duration::from_secs(10)).unwrap();
        in_flight = value(&t, IN_FLIGHT, &hold);
        if in_flight == 1.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(in_flight, 1.0, "a held call counts as in flight");
    held.abort();
    let _ = held.await;
    aster_otel::flush(Duration::from_secs(10)).unwrap();

    for m in ["unary", "fails", "hold"] {
        let p = point(&t, IN_FLIGHT, &peer_want(m)).unwrap_or_else(|| panic!("{m}: no in_flight"));
        assert_eq!(p.value, 0.0, "in_flight returns to zero for {m}: {p:?}");
    }
    for (m, status) in [
        ("unary", "OK"),
        ("fails", "NOT_FOUND"),
        ("hold", "CANCELLED"),
    ] {
        let mut want = peer_want(m);
        want.push(("aster.status", status));
        assert_eq!(value(&t, COMPLETED, &want), 1.0, "{m} {status}");
    }
    let mut want = peer_want("unary");
    want.push(("aster.status", "OK"));
    assert!(point(&t, "aster.rpc.client.duration", &want).is_some());

    let all: Vec<MetricPoint> = [COMPLETED, IN_FLIGHT, "aster.rpc.client.duration"]
        .into_iter()
        .flat_map(|m| t.metric_points(m))
        .collect();
    assert!(!all.is_empty());
    for p in &all {
        assert_eq!(
            p.attr("aster.peer").is_some(),
            peer_labels,
            "peer label presence on {p:?}"
        );
    }

    if peer_labels {
        // Cap 1: a second peer is counted under `_OTHER`, not by its id.
        let second = Node::start_with_alpns(node_cfg(), vec![RPC_ALPN.to_vec()])
            .await
            .unwrap();
        wait_for_addr(&second).await;
        client.add_peer(&second).unwrap();
        let _second_handle = Server::new(&second).register(Echo).serve();
        let conn2 = connect(&client, &second).await;
        within(conn2.unary(&header("test.Echo", "unary"), vec![1]))
            .await
            .unwrap();
        aster_otel::flush(Duration::from_secs(10)).unwrap();
        assert_eq!(
            value(
                &t,
                COMPLETED,
                &[svc, ("aster.peer", "_OTHER"), ("aster.status", "OK")]
            ),
            1.0
        );
        assert!(t
            .metric_points(COMPLETED)
            .iter()
            .all(|p| p.attr("aster.peer") != Some(second.id().as_str())));
    }
    aster_otel::shutdown().unwrap();
}

/// O4: a `tracing` event a handler emits inside its call's `rpc` span reaches
/// the binding log bridge with that server span's trace id and span id, the
/// call id, and the call's service and method (design §3.1).
async fn case_log_bridge_trace_ids() {
    let t = TestExporters::new();
    let mut cfg = test_cfg(&t);
    cfg.log_bridge = Some(true);
    cfg.log_filter = Some("warn,aster=info".into());
    let _h = aster_otel::init(cfg).unwrap();
    let mut drain = aster_otel::bridge::drain().unwrap();
    let (_server, _client, _handle, conn) = echo_server().await;

    let resp = within(conn.unary(&header("test.Echo", "log"), vec![7]))
        .await
        .unwrap();
    assert_eq!(resp, [7]);

    let mut rec = None;
    for _ in 0..100 {
        rec = drain
            .try_drain(256)
            .into_iter()
            .find(|r| r.message == "handler-log");
        if rec.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let rec = rec.expect("the handler's record reached the bridge");
    let spans = spans_named(&t, "test.Echo/log", 2).await;
    let server = of_kind(&spans, SpanKind::Server);
    assert!(rec.has_trace() && rec.in_call(), "{rec:?}");
    assert_eq!(
        TraceId::from_bytes(rec.trace_id),
        server.span_context.trace_id()
    );
    assert_eq!(
        SpanId::from_bytes(rec.span_id),
        server.span_context.span_id()
    );
    assert_eq!(attr(server, "aster.call_id"), Some(rec.call_id.to_string()));
    assert_eq!(rec.field("rpc.service"), Some("test.Echo"));
    assert_eq!(rec.field("rpc.method"), Some("log"));
    assert_eq!(rec.field("order"), Some("example-1"));
    assert_eq!(rec.target, "aster::test::handler");
}

/// Client → Relay (node B) → Echo (node A): one trace, each span the parent
/// of the next, across two hops (§5.6, Rust↔Rust).
async fn case_trace_continuity() {
    let t = TestExporters::new();
    let _h = aster_otel::init(test_cfg(&t)).unwrap();
    // A serves Echo; B serves Relay and calls A; C calls B.
    let (a, c) = nodes().await;
    let _ha = Server::new(&a).register(Echo).serve();
    let b = Node::start_with_alpns(node_cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    wait_for_addr(&b).await;
    for (x, y) in [(&a, &b), (&b, &c)] {
        x.add_peer(y).unwrap();
        y.add_peer(x).unwrap();
    }
    let _hb = Server::new(&b)
        .register(Relay(connect(&b, &a).await))
        .serve();
    let to_b = connect(&c, &b).await;

    let resp = within(to_b.unary(&header("test.Relay", "relay"), vec![7]))
        .await
        .unwrap();
    assert_eq!(resp, [7]);

    let relay = spans_named(&t, "test.Relay/relay", 2).await;
    let echo = spans_named(&t, ECHO_UNARY, 2).await;
    let c1 = of_kind(&relay, SpanKind::Client);
    let s1 = of_kind(&relay, SpanKind::Server);
    let c2 = of_kind(&echo, SpanKind::Client);
    let s2 = of_kind(&echo, SpanKind::Server);
    let trace = c1.span_context.trace_id();
    assert_ne!(trace, TraceId::INVALID);
    for s in [s1, c2, s2] {
        assert_eq!(s.span_context.trace_id(), trace, "{}", s.name);
    }
    assert_eq!(
        c1.parent_span_id,
        SpanId::INVALID,
        "the first client span is a root"
    );
    assert_eq!(s1.parent_span_id, c1.span_context.span_id());
    assert!(s1.parent_span_is_remote);
    assert_eq!(c2.parent_span_id, s1.span_context.span_id());
    assert!(!c2.parent_span_is_remote);
    assert_eq!(s2.parent_span_id, c2.span_context.span_id());
    assert!(s2.parent_span_is_remote);
    aster_otel::shutdown().unwrap();
}

const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT: &str = "00f067aa0ba902b7";

/// No pipeline: the handler does not see the reserved trace keys either (Q7;
/// the context passes through, see `trace_context_passes_through_*`). Then
/// with tracing on, a caller-supplied `traceparent` parents the client span
/// (§5.4) and the handler still does not see them.
async fn case_pass_through_then_hidden() {
    let (_server, _client, _handle, conn) = echo_server().await;
    let traceparent = format!("00-{TRACE}-{PARENT}-01");
    let opts = CallOptions::new()
        .metadata("traceparent", traceparent.clone())
        .metadata("baggage", "k=v")
        .metadata("x-other", "1");
    let seen = meta_lines(
        &within(conn.unary_with(&header("test.Echo", "meta"), vec![0], &opts))
            .await
            .unwrap(),
    );
    assert_eq!(seen, ["x-other=1"]);

    let t = TestExporters::new();
    let _h = aster_otel::init(test_cfg(&t)).unwrap();
    let seen = meta_lines(
        &within(conn.unary_with(&header("test.Echo", "meta"), vec![0], &opts))
            .await
            .unwrap(),
    );
    assert_eq!(seen, ["x-other=1"]);
    let spans = spans_named(&t, "test.Echo/meta", 2).await;
    let client = of_kind(&spans, SpanKind::Client);
    let server = of_kind(&spans, SpanKind::Server);
    assert_eq!(client.span_context.trace_id().to_string(), TRACE);
    assert_eq!(client.parent_span_id.to_string(), PARENT);
    assert_eq!(server.span_context.trace_id().to_string(), TRACE);
    assert_eq!(server.parent_span_id, client.span_context.span_id());
    aster_otel::shutdown().unwrap();
}

/// A server built before `init` exports once a pipeline exists (§4.1a).
async fn case_init_after_server_start() {
    let (_server, _client, _handle, conn) = echo_server().await;
    within(conn.unary(&header("test.Echo", "unary"), vec![1]))
        .await
        .unwrap();
    let t = TestExporters::new();
    let _h = aster_otel::init(test_cfg(&t)).unwrap();
    within(conn.unary(&header("test.Echo", "unary"), vec![1]))
        .await
        .unwrap();
    let spans = spans_named(&t, ECHO_UNARY, 2).await;
    assert_eq!(spans.len(), 2, "only the call after init is traced");
    assert_eq!(
        value(
            &t,
            "aster.rpc.server.requests_completed",
            &[("rpc.method", "unary"), ("aster.status", "OK")]
        ),
        1.0
    );
    aster_otel::shutdown().unwrap();
}

/// Shutdown, then a new pipeline: the same server and client export to the
/// new provider only (§4.1a).
async fn case_shutdown_then_reinit() {
    let (_server, _client, _handle, conn) = echo_server().await;
    let first = TestExporters::new();
    let h = aster_otel::init(test_cfg(&first)).unwrap();
    within(conn.unary(&header("test.Echo", "unary"), vec![1]))
        .await
        .unwrap();
    spans_named(&first, ECHO_UNARY, 2).await;
    drop(h);
    aster_otel::shutdown().unwrap();

    let second = TestExporters::new();
    let mut cfg = test_cfg(&second);
    cfg.service_name = Some("second".into());
    let _h = aster_otel::init(cfg).unwrap();
    within(conn.unary(&header("test.Echo", "unary"), vec![1]))
        .await
        .unwrap();
    within(conn.unary(&header("test.Echo", "unary"), vec![1]))
        .await
        .unwrap();
    assert_eq!(spans_named(&second, ECHO_UNARY, 4).await.len(), 4);
    let ok = [("rpc.method", "unary"), ("aster.status", "OK")];
    assert_eq!(
        value(&second, "aster.rpc.server.requests_completed", &ok),
        2.0
    );
    assert_eq!(
        value(&second, "aster.rpc.client.requests_completed", &ok),
        2.0
    );
    assert_eq!(
        first.span_names().len(),
        2,
        "nothing more reached the old one"
    );
    aster_otel::shutdown().unwrap();
}

#[cfg(feature = "binding-support")]
mod foreign {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use aster::binding_support::dispatch::{
        CallRef, CallTraceContext, DispatchEvent, DispatchSendError, DispatchSink, InboundTrace,
    };
    use aster::rpc::{DynamicService, MethodDesc, MethodPattern, RpcStatus, Server, TypeRef};
    use aster_otel::opentelemetry::trace::{SpanKind, Status};
    use aster_otel::testing::TestExporters;

    use super::*;

    /// Method indices: `slow` is 0; `hang` (1) and `expire` (2) are held.
    const SLOW: u32 = 0;

    fn method(name: &str) -> MethodDesc {
        let hash = |b: u8| TypeRef::new(format!("{b:02x}").repeat(32));
        MethodDesc::new(name, MethodPattern::Unary, hash(0xa1), hash(0xb2))
    }

    /// The "binding": `slow` answers from a thread after 200 ms; `hang` and
    /// `expire` are never answered (kept, so they are not abandoned).
    struct Binding {
        held: Mutex<Vec<CallRef>>,
    }

    impl DispatchSink for Binding {
        fn deliver(&self, event: DispatchEvent) -> Result<(), DispatchSendError<DispatchEvent>> {
            let DispatchEvent::Call(call) = event else {
                return Ok(());
            };
            let r = call.handle.to_ref();
            let _ = call.handle.detach();
            match call.method_idx {
                SLOW => {
                    let request = call.request;
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(200));
                        r.try_respond(&request, &RpcStatus::ok()).unwrap();
                    });
                }
                _ => self.held.lock().unwrap().push(r),
            }
            Ok(())
        }
    }

    /// What the binding saw of each call, in arrival order.
    /// (inbound trace, span exported, metadata keys)
    type SeenCall = (Option<InboundTrace>, bool, Vec<String>);

    #[derive(Default)]
    struct Seen {
        calls: Mutex<Vec<SeenCall>>,
    }

    impl DispatchSink for Seen {
        fn deliver(&self, event: DispatchEvent) -> Result<(), DispatchSendError<DispatchEvent>> {
            let DispatchEvent::Call(call) = event else {
                return Ok(());
            };
            self.calls.lock().unwrap().push((
                call.inbound_trace.clone(),
                call.trace_context.is_some(),
                call.header.metadata_keys.clone(),
            ));
            let r = call.handle.to_ref();
            call.handle.detach();
            r.try_respond(&[], &RpcStatus::ok()).unwrap();
            Ok(())
        }
    }

    /// Inbound `baggage` / `tracestate` reach the binding on the call, not in
    /// its metadata (Q7); a baggage over the W3C limits is dropped.
    /// Records the trace context each call arrived with.
    #[derive(Default)]
    struct Ctx {
        calls: Mutex<Vec<(Option<CallTraceContext>, Vec<String>)>>,
    }

    impl DispatchSink for Ctx {
        fn deliver(&self, event: DispatchEvent) -> Result<(), DispatchSendError<DispatchEvent>> {
            let DispatchEvent::Call(call) = event else {
                return Ok(());
            };
            self.calls
                .lock()
                .unwrap()
                .push((call.trace_context, call.header.metadata_keys.clone()));
            let r = call.handle.to_ref();
            call.handle.detach();
            r.try_respond(&[], &RpcStatus::ok()).unwrap();
            Ok(())
        }
    }

    /// A Rust-native handler that calls `test.Inbound/m` on another node.
    struct Hop(RpcConnection);

    #[async_trait::async_trait]
    impl ServiceDispatch for Hop {
        fn name(&self) -> &str {
            "test.Hop"
        }
        fn version(&self) -> i32 {
            1
        }
        fn methods(&self) -> &[&str] {
            &["hop"]
        }
        async fn dispatch(&self, _method: &str, mut call: Call) {
            let req = call.recv_request().await.unwrap_or_default();
            let status = match self.0.unary(&header("test.Inbound", "m"), req).await {
                Ok(_) => RpcStatus::ok(),
                Err(e) => RpcStatus::error(StatusCode::Internal, e.to_string()),
            };
            let _ = call.respond(vec![], &status).await;
        }
    }

    /// Client → Hop (Rust-native) → Inbound (binding): the caller's trace
    /// context reaches the binding on both hops, in every pipeline mode, and
    /// the Rust-native hop's outbound call carries it on.
    pub(super) async fn case_passthrough(mode: &str) {
        let t = TestExporters::new();
        let _h = match mode {
            "none" => None,
            "traces_off" => Some(
                aster_otel::init(ObservabilityConfig {
                    from_env: false,
                    traces_exporter: Some(vec![]),
                    metrics_exporter: Some(vec![aster_otel::ExporterKind::Otlp]),
                    otlp_metrics: aster_otel::OtlpSettings {
                        endpoint: Some("http://127.0.0.1:9/v1/metrics".into()),
                        ..Default::default()
                    },
                    metric_export_interval: Some(Duration::from_secs(3600)),
                    ..Default::default()
                })
                .unwrap(),
            ),
            "span_filtered" => {
                let mut cfg = test_cfg(&t);
                cfg.span_filter = Some("warn".into());
                Some(aster_otel::init(cfg).unwrap())
            }
            other => panic!("unknown mode {other}"),
        };
        let (a, c) = nodes().await;
        let mut svc = DynamicService::new("test.Inbound", 1);
        svc.methods = vec![method("m")];
        let sink = Arc::new(Ctx::default());
        let _ha = Server::new(&a)
            .dispatch_sink(sink.clone())
            .dynamic_service(svc)
            .unwrap()
            .serve();
        let b = Node::start_with_alpns(node_cfg(), vec![RPC_ALPN.to_vec()])
            .await
            .unwrap();
        wait_for_addr(&b).await;
        for (x, y) in [(&a, &b), (&b, &c)] {
            x.add_peer(y).unwrap();
            y.add_peer(x).unwrap();
        }
        let _hb = Server::new(&b).register(Hop(connect(&b, &a).await)).serve();
        let to_b = connect(&c, &b).await;

        let opts = CallOptions::new()
            .metadata("traceparent", format!("00-{TRACE}-{PARENT}-01"))
            .metadata("x-user", "1");
        within(to_b.unary_with(&header("test.Hop", "hop"), vec![1], &opts))
            .await
            .unwrap();
        let calls = sink.calls.lock().unwrap();
        let (tc, keys) = &calls[0];
        let tc = tc.expect("the binding gets the trace context");
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(hex(&tc.trace_id), TRACE);
        assert_eq!(tc.trace_flags, 1);
        assert!(keys.is_empty(), "reserved keys hidden: {keys:?}");
        // The hop is a pass-through: the same trace, the inbound span id.
        assert_eq!(hex(&tc.span_id), PARENT);
        drop(calls);
        if mode != "traces_off" {
            aster_otel::shutdown().unwrap();
        } else {
            std::mem::forget(_h);
        }
    }

    pub(super) async fn case_inbound_baggage() {
        let t = TestExporters::new();
        let _h = aster_otel::init(test_cfg(&t)).unwrap();
        let (server, client) = nodes().await;
        let mut svc = DynamicService::new("test.Inbound", 1);
        svc.methods = vec![method("m")];
        let seen = Arc::new(Seen::default());
        let _handle = Server::new(&server)
            .dispatch_sink(seen.clone())
            .dynamic_service(svc)
            .unwrap()
            .serve();
        let conn = connect(&client, &server).await;
        let call = |opts: CallOptions| {
            let conn = &conn;
            async move {
                within(conn.unary_with(&header("test.Inbound", "m"), vec![1], &opts))
                    .await
                    .unwrap();
            }
        };
        let traceparent = format!("00-{TRACE}-{PARENT}-01");
        call(
            CallOptions::new()
                .metadata("traceparent", traceparent.clone())
                .metadata("baggage", "tenant=acme,flag=1;prop=x")
                .metadata("tracestate", "vendor=a")
                .metadata("x-user", "1"),
        )
        .await;
        // Baggage alone: still delivered, whatever the span's export state.
        call(CallOptions::new().metadata("baggage", "tenant=beta")).await;
        // Over 64 members: dropped.
        let many = (0..65)
            .map(|i| format!("k{i}=v"))
            .collect::<Vec<_>>()
            .join(",");
        call(CallOptions::new().metadata("baggage", many)).await;
        // Over 8192 bytes: dropped.
        call(CallOptions::new().metadata("baggage", format!("k={}", "v".repeat(8192)))).await;
        call(CallOptions::new().metadata("x-user", "2")).await;

        let calls = seen.calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        let (inbound, traced, keys) = &calls[0];
        let inbound = inbound.as_ref().unwrap();
        // The client's propagator re-serialises the members in map order.
        let mut members: Vec<&str> = inbound.baggage.as_deref().unwrap().split(',').collect();
        members.sort_unstable();
        assert_eq!(members, ["flag=1;prop=x", "tenant=acme"]);
        assert_eq!(inbound.tracestate.as_deref(), Some("vendor=a"));
        assert!(*traced);
        assert_eq!(keys, &["x-user"], "reserved keys hidden from metadata");
        let (inbound, _, keys) = &calls[1];
        assert_eq!(
            inbound.as_ref().unwrap().baggage.as_deref(),
            Some("tenant=beta")
        );
        assert!(keys.is_empty());
        assert!(calls[2].0.is_none(), "65 members dropped");
        assert!(calls[3].0.is_none(), "8193 bytes dropped");
        assert!(calls[4].0.is_none());
        drop(calls);
        aster_otel::shutdown().unwrap();
    }

    /// The server span of a foreign call lasts as long as the binding takes
    /// (§5.2): 200 ms ± 50 ms. A peer cancel and an expiry end it with an
    /// error status and a `cancelled` / `deadline` event.
    pub(super) async fn case_foreign_spans() {
        let t = TestExporters::new();
        let _h = aster_otel::init(test_cfg(&t)).unwrap();
        let (server, client) = nodes().await;
        let mut svc = DynamicService::new("test.Foreign", 1);
        let mut expire = method("expire");
        expire.default_timeout = Some(0.3);
        svc.methods = vec![method("slow"), method("hang"), expire];
        let _handle = Server::new(&server)
            .dispatch_sink(Arc::new(Binding {
                held: Mutex::new(Vec::new()),
            }))
            .dynamic_service(svc)
            .unwrap()
            .serve();
        let conn = connect(&client, &server).await;

        let at = Instant::now();
        let resp = within(conn.unary(&header("test.Foreign", "slow"), vec![5]))
            .await
            .unwrap();
        assert_eq!(resp, [5]);
        assert!(at.elapsed() >= Duration::from_millis(200));
        let spans = spans_named(&t, "test.Foreign/slow", 2).await;
        let s = of_kind(&spans, SpanKind::Server);
        let took = s.end_time.duration_since(s.start_time).unwrap();
        assert!(
            took >= Duration::from_millis(150) && took <= Duration::from_millis(250),
            "foreign server span lasted {took:?}, expected 200 ms ± 50 ms"
        );
        assert_eq!(s.status, Status::Ok);

        // Peer cancel: the client gives up and its pooled stream is cancelled.
        // short-wait-ok: dropping the call on expiry is the cancel under test
        let gave_up = tokio::time::timeout(
            Duration::from_millis(150),
            conn.unary(&header("test.Foreign", "hang"), vec![1]),
        )
        .await;
        assert!(gave_up.is_err());
        let spans = spans_named(&t, "test.Foreign/hang", 2).await;
        let s = of_kind(&spans, SpanKind::Server);
        assert_eq!(
            attr(of_kind(&spans, SpanKind::Client), "aster.status").as_deref(),
            Some("CANCELLED")
        );
        assert!(matches!(s.status, Status::Error { .. }), "{:?}", s.status);
        assert!(has_event(s, "cancelled"), "{:?}", s.events.events);
        assert_eq!(attr(s, "aster.status").as_deref(), Some("CANCELLED"));

        // Expiry: the method's 300 ms default timeout passes first.
        let err = within(conn.unary(&header("test.Foreign", "expire"), vec![1]))
            .await
            .unwrap_err();
        assert!(
            matches!(err, aster::Error::Rpc { code, .. } if code == StatusCode::DeadlineExceeded.as_i32())
        );
        let spans = spans_named(&t, "test.Foreign/expire", 2).await;
        let s = of_kind(&spans, SpanKind::Server);
        assert!(matches!(s.status, Status::Error { .. }));
        assert!(has_event(s, "deadline"), "{:?}", s.events.events);
        assert_eq!(
            attr(s, "aster.status").as_deref(),
            Some("DEADLINE_EXCEEDED")
        );
        aster_otel::shutdown().unwrap();
    }
}

// ── the tests ───────────────────────────────────────────────────────────────

#[test]
fn instrument_and_span_names_and_attributes() {
    run("names_and_attributes");
}

#[test]
fn client_metrics_carry_labels_and_in_flight_returns_to_zero() {
    run("client_metrics");
}

#[test]
fn client_metrics_peer_label_is_opt_in_and_bounded() {
    run("client_metrics_peer_labels");
}

#[test]
fn handler_logs_reach_the_bridge_with_the_server_span_ids() {
    run("log_bridge_trace_ids");
}

#[test]
fn rust_to_rust_trace_continuity_across_two_hops() {
    run("trace_continuity");
}

#[test]
fn trace_keys_are_hidden_from_handlers_with_and_without_a_pipeline() {
    run("pass_through_then_hidden");
}

#[test]
fn init_after_server_start_still_exports() {
    run("init_after_server_start");
}

#[test]
fn shutdown_then_reinit_exports_to_the_new_provider() {
    run("shutdown_then_reinit");
}

#[cfg(feature = "binding-support")]
#[test]
fn foreign_calls_carry_inbound_baggage_and_tracestate_outside_metadata() {
    run("foreign_inbound_baggage");
}

#[cfg(feature = "binding-support")]
#[test]
fn foreign_span_lasts_the_binding_handling_time_and_records_cancel_and_expiry() {
    run("foreign_spans");
}

#[cfg(feature = "binding-support")]
#[test]
fn trace_context_passes_through_two_hops_with_no_pipeline() {
    run("passthrough_none");
}

#[cfg(feature = "binding-support")]
#[test]
fn trace_context_passes_through_two_hops_with_traces_off() {
    run("passthrough_traces_off");
}

#[cfg(feature = "binding-support")]
#[test]
fn trace_context_passes_through_two_hops_with_the_rpc_span_filtered_out() {
    run("passthrough_span_filtered");
}

// ── lease metrics ───────────────────────────────────────────────────────────

mod lease {
    use super::*;
    use aster::lease::{
        revoke, AcquireOutcome, LeaseHandle, LeaseOp, LeaseOptions, LeaseSerializer, LeaseSnapshot,
        LeaseStatus, MemorySerializer, TransitionOutcome,
    };
    use std::sync::Arc;

    fn opts() -> LeaseOptions {
        LeaseOptions {
            ttl: Duration::from_secs(30),
            renew_interval: Duration::from_secs(10),
            holder_margin: Duration::from_secs(1),
        }
    }

    /// A serializer whose renewals fail (the backend is unreachable).
    struct RenewFails(MemorySerializer);

    #[async_trait::async_trait]
    impl LeaseSerializer for RenewFails {
        async fn transition(
            &self,
            resource: &str,
            op: LeaseOp,
        ) -> anyhow::Result<TransitionOutcome> {
            if matches!(op, LeaseOp::Renew { .. }) {
                anyhow::bail!("backend unreachable");
            }
            self.0.transition(resource, op).await
        }
        async fn snapshot(&self, resource: &str) -> anyhow::Result<LeaseSnapshot> {
            self.0.snapshot(resource).await
        }
    }

    async fn held(s: Arc<dyn LeaseSerializer>, resource: &str) -> LeaseHandle {
        match LeaseHandle::acquire(s, resource, "holder", opts())
            .await
            .unwrap()
        {
            AcquireOutcome::Granted(h) => h,
            AcquireOutcome::Denied(reason, _) => panic!("expected a grant, got {reason:?}"),
        }
    }

    /// Let the spawned renew loop run to its next await.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    pub async fn case_renew_late() {
        let t = TestExporters::new();
        let _h = aster_otel::init(test_cfg(&t)).unwrap();
        let lease = held(Arc::new(MemorySerializer::new()), "r").await;
        settle().await;
        // The renew timer is due at +10 s; the clock jumps 400 ms past it,
        // as a throttled or frozen process would observe.
        tokio::time::advance(Duration::from_millis(10_400)).await;
        settle().await;
        aster_otel::flush(Duration::from_secs(10)).unwrap();
        let p = point(&t, "aster.lease.renew_late", &[]).expect("renew_late recorded");
        assert_eq!(p.count, 1, "{p:?}");
        assert_eq!(p.value, 400.0, "the renewal was 400 ms late: {p:?}");
        assert_eq!(lease.status(), LeaseStatus::Held);
        assert!(
            t.metric_points("aster.lease.lost").is_empty(),
            "a late but successful renewal loses nothing"
        );
    }

    pub async fn case_lost_reasons() {
        let t = TestExporters::new();
        let _h = aster_otel::init(test_cfg(&t)).unwrap();
        let mem = Arc::new(MemorySerializer::new());

        // fenced: the authority revoked the lease; the next renewal is refused.
        let fenced = held(mem.clone(), "fenced").await;
        settle().await;
        revoke(&*mem, "fenced", "authority").await.unwrap();
        // expired: one renewal arrives after the serializer's TTL ran out.
        let expired = held(mem.clone(), "expired").await;
        // self_limit: renewals fail until the holder's own limit passes.
        let limit = held(Arc::new(RenewFails(MemorySerializer::new())), "limit").await;
        settle().await;

        // Expiry first, in one jump past the TTL: both the expired and the
        // revoked lease see their (late) renewal refused.
        tokio::time::advance(Duration::from_secs(31)).await;
        settle().await;
        assert_eq!(fenced.status(), LeaseStatus::Fenced);
        assert_eq!(expired.status(), LeaseStatus::Fenced);
        assert_eq!(limit.status(), LeaseStatus::Lost);

        aster_otel::flush(Duration::from_secs(10)).unwrap();
        for (reason, n) in [("expired", 1.0), ("fenced", 1.0), ("self_limit", 1.0)] {
            assert_eq!(
                value(&t, "aster.lease.lost", &[("reason", reason)]),
                n,
                "reason {reason}: {:?}",
                t.metric_points("aster.lease.lost")
            );
        }
    }
}

#[test]
fn lease_renew_late_is_recorded() {
    run("lease_renew_late");
}

#[test]
fn lease_lost_counts_by_reason() {
    run("lease_lost_reasons");
}
