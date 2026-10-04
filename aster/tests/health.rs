#![cfg(feature = "health")]
//! `aster::health`: the HTTP health/readiness/metrics listener, hit with a
//! plain TCP client (no HTTP client dependency), plus the `AsterServer`
//! auto-start path.

use std::net::SocketAddr;
use std::time::Duration;

use aster::health::{HealthConfig, HealthServer, PROMETHEUS_CONTENT_TYPE};
use aster::{AsterConfig, Node, RelayMode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

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

#[tokio::test]
async fn endpoints_readiness_and_shutdown() {
    let node = Node::start(cfg()).await.unwrap();
    let health = HealthServer::start(&node, HealthConfig::new("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = health.addr();
    assert_ne!(addr.port(), 0, "ephemeral port resolved");

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
    health.state().set_services(2);
    health.set_ready(true);
    let r = get(addr, "/readyz").await;
    assert_eq!(r.status, 200);
    assert_eq!(json(&r)["status"], "ready");
    assert_eq!(json(&r)["services"], 2);
    health.set_ready(false);
    assert_eq!(get(addr, "/readyz").await.status, 503);
    health.set_ready(true);

    // Unhealthy fails both probes.
    health.set_healthy(false);
    let r = get(addr, "/healthz").await;
    assert_eq!(r.status, 503);
    assert_eq!(json(&r)["status"], "unhealthy");
    assert_eq!(get(addr, "/readyz").await.status, 503);
    health.set_healthy(true);

    // Counters show up in both renderings.
    let m = health.state();
    m.metrics().connection_opened();
    m.metrics().record_consumer_admit(3.0);
    m.metrics().rpc_started();
    m.metrics().rpc_completed(true);

    let r = get(addr, "/metrics").await;
    assert_eq!(r.status, 200);
    let j = json(&r);
    assert_eq!(j["connections"]["active_connections"], 1);
    assert_eq!(j["admission"]["consumer_admitted"], 1);
    assert_eq!(j["rpc"]["succeeded"], 1);
    assert_eq!(j["ready"]["status"], "ready");

    for (path, extra) in [
        ("/metrics/prometheus", ""),
        ("/metrics?format=prometheus", ""),
        ("/metrics", "Accept: text/plain;version=0.0.4\r\n"),
    ] {
        let r = http(addr, "GET", path, extra).await;
        assert_eq!(r.status, 200, "{path}");
        assert!(
            r.headers
                .contains(&format!("content-type: {PROMETHEUS_CONTENT_TYPE}")),
            "{path}: {}",
            r.headers
        );
        assert!(r.body.contains("aster_connections_total 1\n"), "{path}");
        assert!(r
            .body
            .contains("aster_admission_consumer_admitted_total 1\n"));
        assert!(r
            .body
            .contains("aster_rpc_completed_total{status=\"ok\"} 1\n"));
        assert!(r.body.contains("aster_ready 1\n"));
        #[cfg(feature = "metrics")]
        assert!(
            r.body.contains("# TYPE iroh_send_ipv4 counter"),
            "transport series from Node::metrics_prometheus"
        );
    }

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

    node.shutdown().await;
}

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

#[cfg(feature = "rpc")]
mod server {
    use super::*;
    use aster::rpc::{AsterServer, Call, RpcStatus, ServiceDispatch, StatusCode, StreamHeader};

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
        srv.shutdown().await;
        assert!(TcpStream::connect(addr).await.is_err());
    }

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

    /// Poll `/metrics` until `done` holds (counters settle a moment after the
    /// client sees its answer), then return the last snapshot.
    async fn metrics_until(
        addr: SocketAddr,
        what: &str,
        done: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let mut last = serde_json::Value::Null;
        for _ in 0..200 {
            last = json(&get(addr, "/metrics").await);
            if done(&last) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}: {last}");
    }

    /// R10: the counters behind `/metrics` reflect real traffic through the
    /// server -- calls by outcome (refusals included), streams in flight, and
    /// connections.
    #[tokio::test]
    async fn aster_server_feeds_metrics_from_traffic() {
        let srv = AsterServer::builder()
            .config(cfg())
            .service(Probe)
            .health(HealthConfig::new("127.0.0.1", 0))
            .start()
            .await
            .unwrap();
        let addr = srv.health().unwrap().addr();
        let j = json(&get(addr, "/metrics").await);
        assert_eq!(j["rpc"]["started"], 0);
        assert_eq!(j["connections"]["total_connections"], 0);

        let client = client_of(&srv).await;
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
        // A handler error and a refusal (no such service) both count as failed.
        assert!(conn.unary(&probe("Probe", "fail"), vec![9]).await.is_err());
        assert!(conn.unary(&probe("Nope", "x"), vec![9]).await.is_err());

        let j = metrics_until(addr, "five settled calls", |j| {
            j["rpc"]["succeeded"] == 3 && j["rpc"]["failed"] == 2
        })
        .await;
        assert_eq!(j["rpc"]["started"], 5);
        assert_eq!(j["rpc"]["in_flight"], 0);
        assert_eq!(j["connections"]["total_streams"], 5);
        assert_eq!(j["connections"]["active_streams"], 0);
        assert_eq!(j["connections"]["active_connections"], 1);
        assert_eq!(j["connections"]["total_connections"], 1);

        let prom = get(addr, "/metrics/prometheus").await.body;
        assert!(prom.contains("aster_rpc_started_total 5\n"), "{prom}");
        assert!(prom.contains("aster_rpc_completed_total{status=\"ok\"} 3\n"));
        assert!(prom.contains("aster_rpc_completed_total{status=\"error\"} 2\n"));

        // The connection closes: active drops, total stays.
        client.shutdown().await;
        let j = metrics_until(addr, "connection close", |j| {
            j["connections"]["active_connections"] == 0
        })
        .await;
        assert_eq!(j["connections"]["total_connections"], 1);
        srv.shutdown().await;
    }

    /// R10: Gate 0 decisions made through `AsterServer::take_admission` are
    /// counted as the consumer admission series.
    #[tokio::test]
    async fn aster_server_counts_gate0_decisions() {
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
        // The refused peer gets nowhere (the connect or the call fails).
        let refused_call = timeout(Duration::from_secs(10), async {
            let conn = refused.rpc_connect(&srv.id()).await?;
            conn.unary(&probe("Probe", "ok"), vec![9]).await
        })
        .await;
        assert!(!matches!(refused_call, Ok(Ok(_))), "{refused_call:?}");

        let j = metrics_until(addr, "one admit and one deny", |j| {
            j["admission"]["consumer_admitted"].as_u64() >= Some(1)
                && j["admission"]["consumer_denied"].as_u64() >= Some(1)
        })
        .await;
        assert_eq!(j["admission"]["consumer_errors"], 0);

        admitted.shutdown().await;
        refused.shutdown().await;
        srv.shutdown().await;
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
}
