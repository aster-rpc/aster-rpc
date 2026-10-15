#![cfg(all(feature = "enrollment", feature = "docs", feature = "rpc"))]
//! Sessions are process memory, so after either side of an admitted pair
//! restarts, ordinary protocols between them must recover without any
//! explicit `Enrollment::connect`: the dialing node enrolls on demand before
//! a dial to a peer it holds no session with, and a node that lost its
//! session enrolls back with a dialer that still holds one. Exercised through
//! `AsterServer` and `[enrollment]` only, with Docs sync, a blob fetch and a
//! gossip join in both directions.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::enrollment::*;
use aster::rpc::AsterServer;
use aster::{
    AsterConfig, BlobFormat, Doc, EnrollmentConfig, GossipEvent, Node, NodeId, PublicKey,
    RelayMode, SecretKey, ShareMode,
};

const FLEET: &str = "reconnect-test";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn key(seed: u8) -> SecretKey {
    SecretKey::from_bytes([seed; 32])
}
fn public(seed: u8) -> [u8; 32] {
    JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
}
fn credential(seed: u8) -> String {
    let request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
    issue_credential(&key(1), &request, now())
        .unwrap()
        .into_chain()
}

/// A persistent enrolled server: same key, storage and state on restart, as
/// a deployed server keeps them. Port `0` leaves the port to the OS; a
/// persistent node then re-binds the port it last bound.
fn config(seed: u8, fleet: &str, storage: &std::path::Path, port: u16) -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr(format!("127.0.0.1:{port}"))
        .secret_key(key(seed))
        .root_pubkey(PublicKey::from_bytes(public(1)))
        .persistent(storage.join(format!("node-{seed}")))
        .enrollment_credential_chain(credential(seed))
        .enrollment(EnrollmentConfig {
            audience: Some(FLEET.into()),
            fleet_namespace: Some(fleet.into()),
            ..Default::default()
        })
        .build()
}

async fn start(config: AsterConfig) -> AsterServer {
    // A restart in the same process waits for the previous instance's tasks
    // to release the storage and policy-store locks.
    let mut last = None;
    for _ in 0..100 {
        match AsterServer::builder().config(config.clone()).start().await {
            Ok(server) => {
                for _ in 0..100 {
                    if !server.node().addr().direct_addresses.is_empty() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                return server;
            }
            Err(error) => {
                last = Some(error);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    panic!("server did not start: {last:?}");
}

/// Shut `b` down and start it again (same key, storage and enrollment
/// state), then check the state every step starts from: `a` still holds a
/// session for `b`, the restarted `b` holds none for `a`.
async fn restart(b: AsterServer, a: &AsterServer, config: &AsterConfig) -> AsterServer {
    let bound = b.node().native_endpoint().bound_sockets();
    b.shutdown().await;
    // A new process finds the old sockets closed; in-process, wait for the
    // previous instance to release them.
    released(&bound).await;
    let b = start(config.clone()).await;
    let rebound = b.node().native_endpoint().bound_sockets();
    let v4 = |addrs: &[std::net::SocketAddr]| addrs.iter().find(|a| a.is_ipv4()).map(|a| a.port());
    assert_eq!(v4(&rebound), v4(&bound), "b comes back on the same port");
    introduce(a.node(), b.node());
    assert!(session(a, public(3)), "a keeps its session for b");
    assert!(!session(&b, public(2)), "a restarted b holds no session");
    b
}

async fn released(addrs: &[std::net::SocketAddr]) {
    let bindable = |addr: &std::net::SocketAddr| {
        let socket = socket2::Socket::new(
            socket2::Domain::for_address(*addr),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        if addr.is_ipv6() {
            socket.set_only_v6(true).unwrap();
        }
        socket.bind(&(*addr).into()).is_ok()
    };
    for _ in 0..250 {
        if addrs.iter().all(bindable) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{addrs:?} were not released after shutdown");
}

/// Address books only: Iroh discovery is off in these tests.
fn introduce(a: &Node, b: &Node) {
    a.add_node_addr(b.addr()).unwrap();
    b.add_node_addr(a.addr()).unwrap();
}

fn session(server: &AsterServer, peer: [u8; 32]) -> bool {
    server.enrollment().unwrap().peer_session(&peer).is_some()
}

async fn value(doc: &Doc, key: &str) -> Option<Vec<u8>> {
    let entry = doc
        .query_latest_exact(key.as_bytes().to_vec())
        .await
        .ok()??;
    doc.read_entry_content(&entry.content_hash).await.ok()
}

/// Sync `doc` with `from` (no explicit enrollment) until `key` has
/// `expected`. Rounds are spaced out: both replicas also sync on their own,
/// and back-to-back attempts keep crossing theirs.
async fn sync_until(doc: &Doc, from: &NodeId, key: &str, expected: &[u8]) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let _ = doc.start_sync(std::slice::from_ref(from)).await;
            for _ in 0..20 {
                if value(doc, key).await.as_deref() == Some(expected) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("docs sync of {key} converged"));
}

/// `joiner` joins `topic` bootstrapping from `host`. A subscription resolves
/// only once it has a neighbour, so both subscribe concurrently; then a
/// broadcast from the joiner reaches the host.
async fn gossip_join(host: &Node, joiner: &Node, topic: [u8; 32]) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let host_id = [host.id()];
        let (host_gossip, joiner_gossip) = (host.gossip(), joiner.gossip());
        let (hosted, joined) = tokio::join!(
            host_gossip.subscribe(topic, &[]),
            joiner_gossip.subscribe(topic, &host_id),
        );
        let (hosted, joined) = (hosted.unwrap(), joined.unwrap());
        joined.broadcast(b"hello".to_vec()).await.unwrap();
        loop {
            if let GossipEvent::Received { data } = hosted.recv().await.unwrap() {
                assert_eq!(data, b"hello");
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("gossip join and delivery on topic {}", topic[0]));
}

async fn fetch(from: &Node, to: &Node, content: &[u8]) {
    let hash = from.blobs().add_bytes(content.to_vec()).await.unwrap();
    let bytes = tokio::time::timeout(
        Duration::from_secs(30),
        to.blobs().download_hash(&hash, &from.id(), BlobFormat::Raw),
    )
    .await
    .expect("blob fetch finished")
    .expect("blob fetch succeeded");
    assert_eq!(bytes, content);
}

/// B keeps a fixed, explicit port across restarts.
#[tokio::test]
async fn admitted_peers_reconnect_after_a_restart_without_explicit_enrollment() {
    let free_port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    reconnect_after_restarts(free_port).await;
}

/// B binds an OS-chosen port with persistent storage, so each restart
/// re-binds the port it recorded (`AsterConfig::reuse_last_port`).
#[tokio::test]
async fn admitted_peers_reconnect_after_a_restart_on_an_ephemeral_port() {
    reconnect_after_restarts(0).await;
}

async fn reconnect_after_restarts(b_port: u16) {
    tokio::time::timeout(Duration::from_secs(240), async {
        let files = tempfile::tempdir().unwrap();
        let fleet = {
            let root = Node::start(
                AsterConfig::builder()
                    .relay(RelayMode::Disabled)
                    .bind_addr("127.0.0.1:0")
                    .build(),
            )
            .await
            .unwrap();
            let doc = root.docs().create().await.unwrap();
            let id = doc.id().unwrap().to_string();
            root.shutdown().await;
            id
        };
        let a_config = config(2, &fleet, files.path(), 0);
        let b_config = config(3, &fleet, files.path(), b_port);
        let a = start(a_config).await;
        let mut b = start(b_config.clone()).await;
        introduce(a.node(), b.node());

        // First contact: B's Docs join dials A with no session on either
        // side; the dial enrolls on demand and both hold sessions after.
        let author = a.node().docs().default_author().await.unwrap();
        let shared = a.node().docs().create().await.unwrap();
        shared
            .set_bytes(&author, b"k1".to_vec(), b"v1".to_vec())
            .await
            .unwrap();
        let ticket = shared.share(ShareMode::Write).await.unwrap();
        let joined = b.node().docs().join(ticket).await.unwrap();
        sync_until(&joined, &a.node().id(), "k1", b"v1").await;
        assert!(session(&a, public(3)) && session(&b, public(2)));
        let doc_id = joined.id().unwrap();
        drop(joined);

        // Each step below restarts B first, so the protocol under test makes
        // the first contact: A still holds its session for B, B holds none.
        // The restarted side dials: it enrolls on demand before the dial.
        b = restart(b, &a, &b_config).await;
        shared
            .set_bytes(&author, b"k2".to_vec(), b"v2".to_vec())
            .await
            .unwrap();
        let doc = b.node().docs().open(doc_id).await.unwrap().unwrap();
        sync_until(&doc, &a.node().id(), "k2", b"v2").await;
        drop(doc);
        b = restart(b, &a, &b_config).await;
        fetch(a.node(), b.node(), b"b fetches after restarting").await;
        b = restart(b, &a, &b_config).await;
        gossip_join(a.node(), b.node(), [7; 32]).await;

        // The side that kept its (now stale) session dials: B, which lost
        // its session, enrolls back with A before gating A's connection.
        b = restart(b, &a, &b_config).await;
        fetch(b.node(), a.node(), b"a fetches from the restarted b").await;
        b = restart(b, &a, &b_config).await;
        gossip_join(b.node(), a.node(), [8; 32]).await;
        b = restart(b, &a, &b_config).await;
        let b_author = b.node().docs().default_author().await.unwrap();
        let served = b.node().docs().create().await.unwrap();
        served
            .set_bytes(&b_author, b"k3".to_vec(), b"v3".to_vec())
            .await
            .unwrap();
        // Sharing serves the document without dialing anyone.
        let ticket = served.share(ShareMode::Read).await.unwrap();
        let joined = a.node().docs().join(ticket).await.unwrap();
        sync_until(&joined, &b.node().id(), "k3", b"v3").await;
        assert!(session(&a, public(3)) && session(&b, public(2)));

        b.shutdown().await;
        a.shutdown().await;
    })
    .await
    .unwrap();
}
