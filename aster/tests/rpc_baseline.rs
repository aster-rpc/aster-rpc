#![cfg(feature = "rpc")]
//! Baseline `aster.ops.NodeInfo` service: registered by default on an
//! `AsterServer`, serves the configured `NodeIdentity` with the node_id
//! stamped from the node key; can be gated (Gate 3) or disabled.

use std::time::Duration;

use aster::rpc::baseline::{Attr, NodeIdentity, NodeInfoClient, Value};
use aster::rpc::{require_role, AsterServer, AttributeStore};
use aster::{AsterConfig, Node, RelayMode};
use tokio::time::timeout;

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn wait_for_addr(n: &Node) {
    for _ in 0..50 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(15), f)
        .await
        .expect("rpc operation timed out")
}

async fn connect(client: &Node, srv: &AsterServer) -> aster::rpc::RpcConnection {
    wait_for_addr(srv.node()).await;
    wait_for_addr(client).await;
    client.add_peer(srv.node()).unwrap();
    srv.node().add_peer(client).unwrap();
    within(client.rpc_connect(&srv.id())).await.unwrap()
}

#[tokio::test]
async fn node_info_served_by_default() {
    // No .service(...) at all — the baseline service alone makes the server
    // startable and describable.
    let srv = AsterServer::builder()
        .config(cfg())
        .node_identity(NodeIdentity {
            node_name: "test-node".into(),
            tags: vec!["blue".into()],
            roles: vec!["worker".into()],
            attributes: vec![
                Attr::new("region", "eu-west"),
                Attr::new("capacity", 512i64),
                Attr::new("beta", true),
                Attr::null("draining"),
            ],
            ..Default::default()
        })
        .start()
        .await
        .unwrap();

    let client = Node::start(cfg()).await.unwrap();
    let conn = connect(&client, &srv).await;
    let info = NodeInfoClient::new(conn);
    let identity = within(info.describe()).await.unwrap();

    assert_eq!(identity.node_id, srv.id().to_string(), "node_id stamped");
    assert_eq!(identity.node_name, "test-node");
    assert_eq!(identity.tags, vec!["blue".to_string()]);
    assert_eq!(identity.roles, vec!["worker".to_string()]);
    let attr = |n: &str| {
        identity
            .attributes
            .iter()
            .find(|a| a.name == n)
            .unwrap()
            .clone()
    };
    assert_eq!(attr("region").value.unwrap().as_text(), Some("eu-west"));
    assert_eq!(attr("capacity").value.unwrap().as_int(), Some(512));
    assert_eq!(attr("beta").value.unwrap().as_bool(), Some(true));
    assert_eq!(attr("draining").value, None, "declared-null attr");

    // Live update: mutate the served identity while the server runs.
    let handle = srv.node_identity().expect("baseline enabled");
    handle.update(|id| {
        id.tags.push("green".into());
        id.attributes = vec![Attr::new("draining", true)];
        id.node_id = "forged".into(); // must be ignored
    });
    let updated = within(info.describe()).await.unwrap();
    assert_eq!(updated.tags, vec!["blue".to_string(), "green".to_string()]);
    assert_eq!(
        updated.attributes[0].value.clone().unwrap().as_bool(),
        Some(true)
    );
    assert_eq!(
        updated.node_id,
        srv.id().to_string(),
        "node_id stays pinned through updates"
    );

    // set() replaces wholesale, also preserving node_id.
    handle.set(NodeIdentity {
        node_name: "renamed".into(),
        ..Default::default()
    });
    let replaced = within(info.describe()).await.unwrap();
    assert_eq!(replaced.node_name, "renamed");
    assert!(replaced.tags.is_empty());
    assert_eq!(replaced.node_id, srv.id().to_string());

    srv.shutdown().await;
}

#[tokio::test]
async fn node_info_can_be_gated() {
    let attrs = AttributeStore::new();
    let srv = AsterServer::builder()
        .config(cfg())
        .attributes(attrs.clone())
        .node_info_requires(require_role("operator"))
        .start()
        .await
        .unwrap();

    let client = Node::start(cfg()).await.unwrap();
    let conn = connect(&client, &srv).await;
    let info = NodeInfoClient::new(conn);

    // No role injected → PERMISSION_DENIED.
    let err = within(info.describe()).await;
    assert!(err.is_err(), "must be denied without the operator role");

    // Inject the role → allowed.
    attrs.set_role(client.id().to_string(), "operator");
    let identity = within(info.describe()).await.unwrap();
    assert_eq!(identity.node_id, srv.id().to_string());

    srv.shutdown().await;
}

#[tokio::test]
async fn node_info_can_be_disabled() {
    // Disabled + no services → refuse to start.
    let err = AsterServer::builder()
        .config(cfg())
        .builtin_node_info(false)
        .start()
        .await;
    assert!(err.is_err(), "no services at all must not start");
}

#[test]
fn value_conversions() {
    assert_eq!(Value::from(7i64).as_int(), Some(7));
    assert_eq!(Value::from(1.5f64).as_float(), Some(1.5));
    assert_eq!(Value::from(true).as_bool(), Some(true));
    assert_eq!(Value::from("x").as_text(), Some("x"));
    // Cross-kind accessors are None, not coerced.
    assert_eq!(Value::from(7i64).as_text(), None);
    assert_eq!(Value::from("7").as_int(), None);
}
