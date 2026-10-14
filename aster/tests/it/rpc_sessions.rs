//! Session-scoped services follow the per-connection rules of
//! Aster-multiplexed-streams §7.5, the same rules Python and Java implement in
//! `_resolve_instance` / `resolveInstance`:
//!
//! | Call                                         | Result                |
//! |----------------------------------------------|-----------------------|
//! | SHARED service, `sessionId != 0`             | `FAILED_PRECONDITION` |
//! | SESSION service, `sessionId == 0`            | `FAILED_PRECONDITION` |
//! | SESSION, `sessionId <= last_opened`, closed  | `NOT_FOUND`           |
//! | SESSION, new session over the cap            | `RESOURCE_EXHAUSTED`  |
//!
//! Sessions are keyed by `(connection, sessionId)`. The regression these tests
//! exist for: the Rust server used to key them by `(service, version,
//! sessionId)` across **all** connections, so any peer could reach another
//! peer's session instance by guessing its small integer id.

#![cfg(feature = "rpc")]

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{
    Call, CallParts, OutgoingFrame, PeerProvenance, RequestFrame, RpcConnection, RpcStatus, Server,
    ServiceDispatch, StatusCode, StreamHeader, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{decode_frame, FLAG_END_STREAM, FLAG_TRAILER};
use aster_transport_core::reactor::response_channel;
use tokio::sync::mpsc;

/// Session-scoped counter: each instance counts privately, and records its own
/// drop so tests can observe reaping.
struct Counter {
    n: AtomicU32,
    drops: Arc<AtomicUsize>,
}

impl Drop for Counter {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ServiceDispatch for Counter {
    fn name(&self) -> &str {
        "test.Counter"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["incr"]
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let _ = call.recv_request().await;
        let v = self.n.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = call.respond(vec![v as u8], &RpcStatus::ok()).await;
    }
}

/// A SHARED service, for the scope-mismatch rule.
struct Shared;

#[async_trait::async_trait]
impl ServiceDispatch for Shared {
    fn name(&self) -> &str {
        "test.Shared"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["ping"]
    }
    async fn dispatch(&self, _method: &str, call: Call) {
        let _ = call.respond(vec![7], &RpcStatus::ok()).await;
    }
}

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

fn server(node: &Node, drops: &Arc<AtomicUsize>) -> Server {
    let drops = drops.clone();
    Server::new(node)
        .register(Shared)
        .register_session(move || Counter {
            n: AtomicU32::new(0),
            drops: drops.clone(),
        })
}

fn header(service: &str, method: &str, session_id: i32) -> StreamHeader {
    StreamHeader {
        service: service.into(),
        method: method.into(),
        version: 1,
        session_id,
        ..Default::default()
    }
}

/// Dispatch one call on `connection_id` and return `(data frames, trailer)`.
async fn call(
    dispatcher: &aster::rpc::Dispatcher,
    connection_id: u64,
    service: &str,
    method: &str,
    session_id: i32,
) -> (Vec<Vec<u8>>, RpcStatus) {
    let hdr = aster::rpc::encode_stream_header(&header(service, method, session_id)).unwrap();
    let (resp_tx, mut resp_rx) = response_channel();
    let (_req_tx, req_rx) = mpsc::channel::<RequestFrame>(1);
    let parts = CallParts::new(
        "b7e1c0de00000000000000000000000000000000000000000000000000000000".into(),
        PeerProvenance::IrohAuthenticated,
        hdr,
        vec![0],
        FLAG_END_STREAM,
        resp_tx,
        req_rx,
        Arc::new(AtomicBool::new(false)),
    )
    .with_connection_id(connection_id);
    dispatcher.dispatch_parts(parts).await;

    let mut bytes = Vec::new();
    while let Ok(item) = resp_rx.try_recv() {
        let (frame, _lease) = item.into_parts();
        match frame {
            OutgoingFrame::Frame(b)
            | OutgoingFrame::Trailer(b)
            | OutgoingFrame::CompleteUnary(b) => bytes.extend_from_slice(&b),
        }
    }
    split(&bytes)
}

fn split(mut buf: &[u8]) -> (Vec<Vec<u8>>, RpcStatus) {
    let (mut data, mut trailer) = (Vec::new(), None);
    while !buf.is_empty() {
        let (p, f, c) = decode_frame(buf).unwrap();
        if f & FLAG_TRAILER != 0 {
            trailer = Some(decode_rpc_status(&p).unwrap());
        } else {
            data.push(p);
        }
        buf = &buf[c..];
    }
    (data, trailer.expect("every call ends with a trailer"))
}

async fn incr(d: &aster::rpc::Dispatcher, conn: u64, sid: i32) -> (Vec<Vec<u8>>, RpcStatus) {
    call(d, conn, "test.Counter", "incr", sid).await
}

fn code(status: &RpcStatus) -> StatusCode {
    StatusCode::from_i32(status.code)
}

/// The security regression: a second connection presenting the first
/// connection's session id must get its own instance, never the first one's.
#[tokio::test]
async fn a_second_connection_cannot_reach_another_connections_session() {
    let node = Node::start(cfg()).await.unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let d = server(&node, &drops).dispatcher();

    // Connection 1 opens session 1 and builds up state in it.
    assert_eq!(incr(&d, 1, 1).await.0, vec![vec![1u8]]);
    assert_eq!(incr(&d, 1, 1).await.0, vec![vec![2u8]]);

    // Connection 2 "guesses" session id 1. It must see a fresh instance
    // (count 1), not connection 1's (which would answer 3).
    let (data, status) = incr(&d, 2, 1).await;
    assert_eq!(status.code, 0, "{status:?}");
    assert_eq!(
        data,
        vec![vec![1u8]],
        "connection 2 reached connection 1's session"
    );

    // And connection 1's session is untouched by connection 2's calls.
    assert_eq!(incr(&d, 1, 1).await.0, vec![vec![3u8]]);
    node.shutdown().await;
}

#[tokio::test]
async fn shared_service_with_a_session_id_is_failed_precondition() {
    let node = Node::start(cfg()).await.unwrap();
    let d = server(&node, &Arc::default()).dispatcher();
    let (data, status) = call(&d, 1, "test.Shared", "ping", 4).await;
    assert!(data.is_empty());
    assert_eq!(code(&status), StatusCode::FailedPrecondition, "{status:?}");
    // sessionId 0 on the same service is served.
    assert_eq!(
        call(&d, 1, "test.Shared", "ping", 0).await.0,
        vec![vec![7u8]]
    );
    node.shutdown().await;
}

#[tokio::test]
async fn session_service_without_a_session_id_is_failed_precondition() {
    let node = Node::start(cfg()).await.unwrap();
    let d = server(&node, &Arc::default()).dispatcher();
    let (data, status) = incr(&d, 1, 0).await;
    assert!(data.is_empty());
    assert_eq!(code(&status), StatusCode::FailedPrecondition, "{status:?}");
    node.shutdown().await;
}

#[tokio::test]
async fn a_session_id_at_or_below_last_opened_that_is_not_active_is_not_found() {
    let node = Node::start(cfg()).await.unwrap();
    let d = server(&node, &Arc::default()).dispatcher();
    assert_eq!(incr(&d, 1, 5).await.0, vec![vec![1u8]]);
    // 3 was never opened, but it is below the high-water mark: graveyard.
    let (data, status) = incr(&d, 1, 3).await;
    assert!(data.is_empty());
    assert_eq!(code(&status), StatusCode::NotFound, "{status:?}");
    // The graveyard is per connection: another connection may open 3.
    assert_eq!(incr(&d, 2, 3).await.0, vec![vec![1u8]]);
    // The active session keeps working.
    assert_eq!(incr(&d, 1, 5).await.0, vec![vec![2u8]]);
    node.shutdown().await;
}

#[tokio::test]
async fn a_new_session_over_the_cap_is_resource_exhausted_without_bumping_last_opened() {
    let node = Node::start(cfg()).await.unwrap();
    let d = server(&node, &Arc::default())
        .max_sessions_per_connection(2)
        .dispatcher();
    assert_eq!(incr(&d, 1, 1).await.1.code, 0);
    assert_eq!(incr(&d, 1, 2).await.1.code, 0);

    let (data, status) = incr(&d, 1, 3).await;
    assert!(data.is_empty());
    assert_eq!(code(&status), StatusCode::ResourceExhausted, "{status:?}");
    // Retrying the same id is still RESOURCE_EXHAUSTED, not NOT_FOUND: the
    // rejected session did not move the graveyard counter.
    assert_eq!(code(&incr(&d, 1, 3).await.1), StatusCode::ResourceExhausted);
    // Existing sessions still work, and the cap is per connection.
    assert_eq!(incr(&d, 1, 2).await.0, vec![vec![2u8]]);
    assert_eq!(incr(&d, 2, 3).await.1.code, 0);
    node.shutdown().await;
}

#[tokio::test]
async fn closing_a_connection_reaps_its_sessions_and_only_its_sessions() {
    let node = Node::start(cfg()).await.unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let d = server(&node, &drops).dispatcher();
    assert_eq!(incr(&d, 1, 1).await.0, vec![vec![1u8]]);
    assert_eq!(incr(&d, 1, 2).await.0, vec![vec![1u8]]);
    assert_eq!(incr(&d, 2, 1).await.0, vec![vec![1u8]]);

    d.close_connection(1);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        2,
        "both of connection 1's sessions"
    );

    // Connection 2 is untouched.
    assert_eq!(incr(&d, 2, 1).await.0, vec![vec![2u8]]);
    // A connection id seen again starts from scratch: no graveyard, no state.
    assert_eq!(incr(&d, 1, 1).await.0, vec![vec![1u8]]);
    node.shutdown().await;
}

// ── over the real transport ──────────────────────────────────────────────────

async fn wait_for_addr(n: &Node) {
    for _ in 0..50 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn within<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(15), f)
        .await
        .expect("rpc operation timed out")
}

async fn connect(server: &Node) -> (Node, RpcConnection) {
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&client).await;
    client.add_peer(server).unwrap();
    server.add_peer(&client).unwrap();
    let conn = within(client.rpc_connect(&server.id())).await.unwrap();
    (client, conn)
}

/// End to end: two real QUIC connections using the same session id are
/// isolated, and `Server::serve` reaps a connection's sessions when it closes.
#[tokio::test]
async fn sessions_are_per_connection_and_reaped_on_close_over_iroh() {
    let srv = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    wait_for_addr(&srv).await;
    let drops = Arc::new(AtomicUsize::new(0));
    let _h = server(&srv, &drops).serve();

    let (client_a, conn_a) = connect(&srv).await;
    let (client_b, conn_b) = connect(&srv).await;
    let h = header("test.Counter", "incr", 1);

    assert_eq!(within(conn_a.unary(&h, vec![0])).await.unwrap(), vec![1u8]);
    assert_eq!(within(conn_a.unary(&h, vec![0])).await.unwrap(), vec![2u8]);
    // Same session id, different connection: a fresh instance.
    assert_eq!(within(conn_b.unary(&h, vec![0])).await.unwrap(), vec![1u8]);
    assert_eq!(within(conn_a.unary(&h, vec![0])).await.unwrap(), vec![3u8]);

    // Closing A's connection (by shutting its node down) reaps exactly A's
    // session.
    drop(conn_a);
    client_a.shutdown().await;
    within(async {
        while drops.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    // B's session survives.
    assert_eq!(within(conn_b.unary(&h, vec![0])).await.unwrap(), vec![2u8]);

    client_b.shutdown().await;
    srv.shutdown().await;
}
