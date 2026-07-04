#![cfg(feature = "rpc")]
//! Topology v1: the local per-peer proximity view (`Node::topology()`) and
//! the opt-in baseline `aster.net.Topology` RPC service.
//!
//! Both nodes run with monitoring enabled and talk over 127.0.0.1, so the
//! sampled view must classify the peer as L0 (same-host, loopback path).

use std::time::Duration;

use aster::rpc::baseline::{NodeInfoClient, PeerQuery, TopologyClient};
use aster::rpc::{require_role, AsterServer, AttributeStore};
use aster::topology::LadderLevel;
use aster::{AsterConfig, Node, RelayMode};
use tokio::time::timeout;

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .monitoring(true)
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

/// Poll `f` until it returns `Some` (sampler ticks once per second, so the
/// first views appear only after a tick or two).
async fn poll<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..100 {
        if let Some(v) = f() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn local_view_classifies_loopback_peer() {
    let srv = AsterServer::builder().config(cfg()).start().await.unwrap();
    let client = Node::start(cfg()).await.unwrap();

    let conn = connect(&client, &srv).await;
    // Some RPC traffic so paths select and counters move.
    let info = NodeInfoClient::new(conn);
    within(info.describe()).await.unwrap();

    let topo = srv.topology();
    assert!(topo.has_monitoring());

    let client_id = client.id();
    let view = poll("server-side peer view", || topo.peer(&client_id)).await;
    assert_eq!(view.level, LadderLevel::SameHost, "127.0.0.1 → L0");
    assert_eq!(view.level_reason, "loopback path");
    assert!(view.direct, "localhost path must be direct");
    assert!(view.is_connected);
    assert!(view.samples > 0);
    assert!(view.confidence_ppm > 0);

    // RTT appears once the path has a measurement (may lag the first sample).
    let view = poll("measured rtt", || {
        topo.peer(&client_id).filter(|v| v.rtt_us.is_some())
    })
    .await;
    assert!(view.rtt_us.unwrap() < 5_000_000, "sane localhost rtt");

    // The client side observes the server symmetrically (hook fires on the
    // connect path too), and peers() lists it.
    let client_topo = client.topology();
    let srv_id = srv.id();
    let peer_view = poll("client-side peer view", || client_topo.peer(&srv_id)).await;
    assert_eq!(peer_view.level, LadderLevel::SameHost);
    assert!(client_topo
        .peers()
        .iter()
        .any(|p| p.node_id == srv_id.to_string()));

    srv.shutdown().await;
}

#[tokio::test]
async fn topology_rpc_served_when_opted_in() {
    let srv = AsterServer::builder()
        .config(cfg())
        .builtin_topology(true)
        .start()
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();

    let conn = connect(&client, &srv).await;
    let topo = TopologyClient::new(conn);

    // peers(): the client itself must appear once sampled.
    let client_hex = client.id().to_string();
    let list = poll_rpc(&topo, &client_hex).await;
    let me = list.peers.iter().find(|p| p.node_id == client_hex).unwrap();
    assert_eq!(me.level, 0, "loopback peer serves level L0 on the wire");
    assert_eq!(me.level_reason, "loopback path");
    assert!(me.direct);
    assert!(me.relay_url.is_empty());
    assert!(me.samples > 0);

    // peer(query): 0-or-1 semantics.
    let one = within(topo.peer(PeerQuery {
        node_id: client_hex.clone(),
    }))
    .await
    .unwrap();
    assert_eq!(one.peers.len(), 1);
    let none = within(topo.peer(PeerQuery {
        node_id: "0000000000000000000000000000000000000000000000000000000000000000".into(),
    }))
    .await
    .unwrap();
    assert!(none.peers.is_empty());

    srv.shutdown().await;
}

async fn poll_rpc(topo: &TopologyClient, expect_node: &str) -> aster::rpc::baseline::PeerList {
    for _ in 0..100 {
        let list = within(topo.peers()).await.unwrap();
        if list.peers.iter().any(|p| p.node_id == expect_node) {
            return list;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("peer never appeared in aster.net.Topology.peers()");
}

#[tokio::test]
async fn topology_rpc_absent_by_default() {
    let srv = AsterServer::builder().config(cfg()).start().await.unwrap();
    let client = Node::start(cfg()).await.unwrap();

    let conn = connect(&client, &srv).await;
    let topo = TopologyClient::new(conn);
    let res = within(topo.peers()).await;
    assert!(res.is_err(), "aster.net.Topology must be opt-in");

    srv.shutdown().await;
}

#[tokio::test]
async fn topology_rpc_can_be_gated() {
    let attrs = AttributeStore::new();
    let srv = AsterServer::builder()
        .config(cfg())
        .attributes(attrs.clone())
        .builtin_topology(true)
        .topology_requires(require_role("operator"))
        .start()
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();

    let conn = connect(&client, &srv).await;
    let topo = TopologyClient::new(conn);

    let err = within(topo.peers()).await;
    assert!(err.is_err(), "must be denied without the operator role");

    attrs.set_role(client.id().to_string(), "operator");
    let list = within(topo.peers()).await.unwrap();
    // Gate passed; content correctness is covered elsewhere.
    let _ = list;

    srv.shutdown().await;
}

#[tokio::test]
async fn topology_rpc_requires_monitoring() {
    let no_monitoring = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let err = AsterServer::builder()
        .config(no_monitoring)
        .builtin_topology(true)
        .start()
        .await;
    assert!(
        err.is_err(),
        "builtin_topology without monitoring must refuse to start"
    );
}
