#![cfg(all(feature = "rpc", feature = "binding-support"))]
//! `AsterServer::peer_events`: the peers connected now plus a stream of
//! connects and disconnects, per peer, only after admission. Run with:
//! cargo test -p aster --features rpc,binding-support --test peer_events
//!
//! Revocation (the `Revoked` reason) and the credential identity are driven
//! in `enrollment_aster_server.rs`; the `Lagged` signal is in the ledger's
//! unit tests (`aster/src/rpc/peers.rs`), which can write the thousand events
//! a real receiver needs to fall behind by.

use std::collections::BTreeSet;
use std::time::Duration;

use aster::native::iroh::Watcher;
use aster::rpc::{AsterServer, DisconnectReason, PeerEvent, PeerEventReceiver, RpcConnection};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::test_support::wait_budget;
use tokio::sync::broadcast::error::TryRecvError;

fn config() -> aster::AsterConfigBuilder {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
}

async fn within<F: std::future::Future>(what: &str, future: F) -> F::Output {
    tokio::time::timeout(wait_budget(), future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn wait_for_addr(node: &Node) {
    let mut addrs = node.native_endpoint().watch_addr();
    within("a direct address", async {
        while node.addr().direct_addresses.is_empty() {
            addrs.updated().await.expect("endpoint closed");
        }
    })
    .await;
}

async fn start(config: AsterConfig) -> AsterServer {
    let server = AsterServer::builder().config(config).start().await.unwrap();
    wait_for_addr(server.node()).await;
    server
}

/// A client node that can dial `server`.
async fn client(server: &AsterServer) -> Node {
    let client = Node::start(config().build()).await.unwrap();
    wait_for_addr(&client).await;
    client.add_peer(server.node()).unwrap();
    server.node().add_peer(&client).unwrap();
    client
}

async fn dial(client: &Node, server: &AsterServer) -> RpcConnection {
    within("the dial", client.rpc_connect(&server.id()))
        .await
        .unwrap()
}

async fn next(events: &mut PeerEventReceiver) -> PeerEvent {
    within("a peer event", events.recv()).await.unwrap()
}

async fn connected(events: &mut PeerEventReceiver) -> String {
    match next(events).await {
        PeerEvent::Connected { peer, .. } => peer,
        other => panic!("expected Connected, got {other:?}"),
    }
}

async fn disconnected(events: &mut PeerEventReceiver) -> (String, DisconnectReason) {
    match next(events).await {
        PeerEvent::Disconnected { peer, reason } => (peer, reason),
        other => panic!("expected Disconnected, got {other:?}"),
    }
}

/// Wait until the server reports `peer` with `connections` admitted
/// connections.
async fn until_connections(server: &AsterServer, peer: &str, connections: usize) {
    within("the connection count", async {
        loop {
            let (snapshot, _) = server.peer_events();
            let now = snapshot
                .iter()
                .find(|p| p.peer == peer)
                .map_or(0, |p| p.connections);
            if now == connections {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// A peer connecting while `peer_events` runs is in the snapshot or in the
/// stream, exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_connecting_during_the_call_appears_exactly_once() {
    const PEERS: usize = 4;
    let server = start(config().build()).await;
    let mut clients = Vec::new();
    for _ in 0..PEERS {
        clients.push(client(&server).await);
    }
    let ids: BTreeSet<String> = clients.iter().map(|c| c.id().to_string()).collect();
    let id = server.id();
    let dials: Vec<_> = clients
        .iter()
        .map(|c| {
            let c = c.clone();
            let id = id.clone();
            tokio::spawn(async move { c.rpc_connect(&id).await.unwrap() })
        })
        .collect();

    let (snapshot, mut events) = server.peer_events();
    let mut seen: Vec<String> = snapshot.into_iter().map(|p| p.peer).collect();
    while seen.len() < PEERS {
        seen.push(connected(&mut events).await);
    }
    let held = futures_join(dials).await;
    assert_eq!(
        seen.iter().cloned().collect::<BTreeSet<_>>(),
        ids,
        "every peer appears"
    );
    assert_eq!(seen.len(), PEERS, "none appears twice: {seen:?}");
    // The connections are all open now, so nothing more is coming.
    assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
    drop(held);
}

async fn futures_join<T>(handles: Vec<tokio::task::JoinHandle<T>>) -> Vec<T> {
    let mut out = Vec::new();
    for handle in handles {
        out.push(handle.await.unwrap());
    }
    out
}

/// The snapshot carries the peers connected before the call, with their
/// identity, and the stream starts after them.
#[tokio::test]
async fn the_snapshot_lists_current_peers_and_the_stream_follows() {
    let server = start(config().build()).await;
    let first = client(&server).await;
    let held = dial(&first, &server).await;
    until_connections(&server, &first.id().to_string(), 1).await;

    let (snapshot, mut events) = server.peer_events();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].peer, first.id().to_string());
    assert_eq!(snapshot[0].alpn, b"aster/1");
    assert_eq!(snapshot[0].connections, 1);
    assert_eq!(snapshot[0].identity.endpoint_id, first.id().to_string());
    assert!(snapshot[0].identity.roles.is_empty());
    assert!(
        snapshot[0].identity.principal.is_none(),
        "no enrollment, no credential principal"
    );

    let second = client(&server).await;
    let _second = dial(&second, &server).await;
    assert_eq!(connected(&mut events).await, second.id().to_string());
    drop(held);
}

/// Two connections from one peer are one `Connected` and one `Disconnected`,
/// the latter when the last closes.
#[tokio::test]
async fn several_connections_from_a_peer_give_one_connect_and_one_disconnect() {
    let server = start(config().build()).await;
    let node = client(&server).await;
    let peer = node.id().to_string();
    let (_, mut events) = server.peer_events();

    let one = dial(&node, &server).await;
    let two = dial(&node, &server).await;
    assert_eq!(connected(&mut events).await, peer);
    until_connections(&server, &peer, 2).await;

    one.core_connection().close(0, b"done".to_vec()).unwrap();
    until_connections(&server, &peer, 1).await;
    assert!(
        matches!(events.try_recv(), Err(TryRecvError::Empty)),
        "the peer is still connected"
    );

    two.core_connection().close(0, b"done".to_vec()).unwrap();
    assert_eq!(
        disconnected(&mut events).await,
        (peer.clone(), DisconnectReason::Closed)
    );
    until_connections(&server, &peer, 0).await;
    assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
}

/// A peer that closes its connection: `Closed`.
#[tokio::test]
async fn a_peer_closing_is_reported_closed() {
    let server = start(config().build()).await;
    let node = client(&server).await;
    let (_, mut events) = server.peer_events();
    let rpc = dial(&node, &server).await;
    assert_eq!(connected(&mut events).await, node.id().to_string());
    rpc.core_connection().close(7, b"bye".to_vec()).unwrap();
    assert_eq!(
        disconnected(&mut events).await,
        (node.id().to_string(), DisconnectReason::Closed)
    );
}

/// The drain closes a connected peer with the drain code: `Drained`. A peer
/// that dials while the server drains is refused at accept and produces no
/// event at all.
#[tokio::test]
async fn a_drain_reports_drained_and_a_refused_peer_produces_no_event() {
    let server = start(config().build()).await;
    let node = client(&server).await;
    let (_, mut events) = server.peer_events();
    let rpc = dial(&node, &server).await;
    assert_eq!(connected(&mut events).await, node.id().to_string());

    let report = within("the drain", server.drainer().drain(Duration::from_secs(30))).await;
    assert!(!report.deadline_reached);
    assert_eq!(
        disconnected(&mut events).await,
        (node.id().to_string(), DisconnectReason::Drained)
    );
    drop(rpc);

    // Refused at accept: the dial completes, the server closes it with the
    // drain code, and the server has counted the refusal by then.
    let late = client(&server).await;
    let refused = dial(&late, &server).await;
    within("the refusal", refused.core_connection().closed()).await;
    assert!(
        matches!(events.try_recv(), Err(TryRecvError::Empty)),
        "a refused peer is never connected"
    );
    let (snapshot, _) = server.peer_events();
    assert!(snapshot.is_empty());
}

/// A connection that goes silent past the server's idle timeout: `TimedOut`.
#[tokio::test]
async fn an_idle_timeout_is_reported_timed_out() {
    let server = start(
        config()
            .transport_max_idle_timeout_ms(1_000)
            .transport_keep_alive_interval_ms(600_000)
            .build(),
    )
    .await;
    let node = Node::start(config().transport_keep_alive_interval_ms(600_000).build())
        .await
        .unwrap();
    wait_for_addr(&node).await;
    node.add_peer(server.node()).unwrap();
    server.node().add_peer(&node).unwrap();
    let (_, mut events) = server.peer_events();
    let _rpc = dial(&node, &server).await;
    assert_eq!(connected(&mut events).await, node.id().to_string());
    assert_eq!(
        disconnected(&mut events).await,
        (node.id().to_string(), DisconnectReason::TimedOut)
    );
}
