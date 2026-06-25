//! Mission Control over HTTP with the **generated** JSON projection
//! (`codecs = ["json"]`). A browser-style client POSTs plain JSON and reads
//! JSON / NDJSON back — no Aster framing, no Fory on the client. Proves the
//! generated `MissionControlProjection` end-to-end.

use aster::rpc::{ProjectionRegistry, Server};
use aster::{AsterConfig, Node, RelayMode};
use mission_control::{MissionControlImpl, MissionControlProjection, MissionControlServer};
use salvo::http::StatusCode as HttpStatus;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};
use serde_json::Value;

async fn service() -> (Node, Service) {
    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let node = Node::start(cfg).await.unwrap();
    let dispatcher = Server::new(&node)
        .register(MissionControlServer::new(MissionControlImpl))
        .dispatcher();
    let projections = ProjectionRegistry::new().register(MissionControlProjection::new());
    let svc = Service::new(aster_transport_salvo::router_with(dispatcher, projections));
    (node, svc)
}

#[tokio::test]
async fn unary_json() {
    let (node, svc) = service().await;

    let mut res = TestClient::post("http://localhost/aster/MissionControl/getStatus")
        .add_header("content-type", "application/json", true)
        .add_header("accept", "application/json", true)
        .body(r#"{"agent_id":"agent-1"}"#)
        .send(&svc)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::OK));
    let v: Value = serde_json::from_slice(&res.take_bytes(None).await.unwrap()).unwrap();
    assert_eq!(v["agent_id"], "agent-1");
    assert_eq!(v["status"], "running");
    assert_eq!(v["uptime_secs"], 3600);

    node.shutdown().await;
}

#[tokio::test]
async fn server_stream_ndjson() {
    let (node, svc) = service().await;

    let mut res = TestClient::post("http://localhost/aster/MissionControl/tailLogs")
        .add_header("content-type", "application/json", true)
        .add_header("accept", "application/x-ndjson", true)
        .body(r#"{"agent_id":"agent-1","level":"info"}"#)
        .send(&svc)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::OK));
    let text = String::from_utf8(res.take_bytes(None).await.unwrap().to_vec()).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["message"], "log 0");
    assert_eq!(lines[2]["message"], "log 2");

    node.shutdown().await;
}

#[tokio::test]
async fn bidi_not_projected() {
    let (node, svc) = service().await;

    // runCommand is bidi → not available over plain HTTP JSON → 406.
    let res = TestClient::post("http://localhost/aster/MissionControl/runCommand")
        .add_header("content-type", "application/json", true)
        .body(r#"{"command":"echo hi"}"#)
        .send(&svc)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::NOT_ACCEPTABLE));

    node.shutdown().await;
}
