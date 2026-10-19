//! Real owned PEM listeners: readiness, authentication, and bounded shutdown.

use aster_transport_core::test_support::wait_budget;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aster::rpc::codec::{decode_rpc_status, encode_stream_header, SerializationMode, StreamHeader};
use aster::rpc::{
    AuthContext, AuthOutcome, Authenticator, Call, PeerProvenance, RpcStatus, Server,
    ServiceDispatch, StatusCode,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{
    decode_frame, encode_frame, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER,
};
use aster_transport_salvo::{generate_webtransport_cert, HttpConfig, TlsMaterial};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use wtransport::{ClientConfig, Endpoint, VarInt};

struct TokenAuth;

#[aster::rpc::async_trait]
impl Authenticator for TokenAuth {
    async fn authenticate(&self, ctx: &AuthContext) -> Result<AuthOutcome, RpcStatus> {
        if ctx.metadata.get("authorization").map(String::as_str) != Some("Bearer secret") {
            return Err(RpcStatus::error(
                StatusCode::Unauthenticated,
                "credential required",
            ));
        }
        Ok(AuthOutcome {
            principal: Some("alice".into()),
            attributes: HashMap::new(),
        })
    }
}

struct Echo {
    started: mpsc::UnboundedSender<()>,
    dropped: mpsc::UnboundedSender<()>,
}

struct Dropped(mpsc::UnboundedSender<()>);
impl Drop for Dropped {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[aster::rpc::async_trait]
impl ServiceDispatch for Echo {
    fn name(&self) -> &str {
        "Echo"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["unary", "hold"]
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        assert_eq!(call.peer(), "alice");
        assert_eq!(call.provenance(), PeerProvenance::Other);
        if method == "hold" {
            let _dropped = Dropped(self.dropped.clone());
            self.started.send(()).unwrap();
            std::future::pending::<()>().await;
        }
        let payload = call.recv_request().await.unwrap();
        call.respond(payload, &RpcStatus::ok()).await.unwrap();
    }
}

async fn node() -> Node {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    Node::start(
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .build(),
    )
    .await
    .unwrap()
}

fn pem() -> TlsMaterial {
    let cert = generate_webtransport_cert("owned-test", None, &["127.0.0.1".into()]).unwrap();
    TlsMaterial::pem(cert.cert_pem, cert.key_pem)
}

fn setup(
    node: &Node,
) -> (
    aster::rpc::Dispatcher,
    mpsc::UnboundedReceiver<()>,
    mpsc::UnboundedReceiver<()>,
) {
    let (started, start_rx) = mpsc::unbounded_channel();
    let (dropped, drop_rx) = mpsc::unbounded_channel();
    (
        Server::new(node)
            .register(Echo { started, dropped })
            .dispatcher(),
        start_rx,
        drop_rx,
    )
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
}

fn header(method: &str, credential: bool) -> Vec<u8> {
    encode_stream_header(&StreamHeader {
        service: "Echo".into(),
        method: method.into(),
        version: 1,
        call_id: 0,
        deadline: 0,
        serialization_mode: SerializationMode::Xlang.as_i8(),
        metadata_keys: if credential {
            vec!["authorization".into()]
        } else {
            vec![]
        },
        metadata_values: if credential {
            vec!["Bearer secret".into()]
        } else {
            vec![]
        },
        session_id: 0,
    })
    .unwrap()
}

fn assert_reply(bytes: &[u8], wanted: &[u8]) {
    let (payload, _, n) = decode_frame(bytes).unwrap();
    assert_eq!(payload, wanted);
    let (payload, flags, used) = decode_frame(&bytes[n..]).unwrap();
    assert_ne!(flags & FLAG_TRAILER, 0);
    assert_eq!(decode_rpc_status(&payload).unwrap().code, 0);
    assert_eq!(n + used, bytes.len());
}

async fn next(rx: &mut mpsc::UnboundedReceiver<()>) {
    tokio::time::timeout(wait_budget(), rx.recv())
        .await
        .unwrap()
        .unwrap();
}

fn assert_released(tcp: std::net::SocketAddr, quic: bool) {
    let _tcp = std::net::TcpListener::bind(tcp).expect("TCP port still owned after shutdown");
    if quic {
        let _udp = std::net::UdpSocket::bind(tcp).expect("UDP port still owned after shutdown");
    }
}

#[tokio::test]
async fn authenticated_tcp_and_webtransport_are_ready_on_one_ephemeral_port() {
    let node = node().await;
    let (dispatcher, _, _) = setup(&node);
    let mut listener = HttpConfig::new("127.0.0.1:0", pem())
        .webtransport(true)
        .start_authenticated(dispatcher, Arc::new(TokenAuth))
        .await
        .unwrap();
    let addr = listener.tcp_addr();
    assert_ne!(addr.port(), 0);
    assert_eq!(listener.quic_addr(), Some(addr));
    let client = client();
    let body = encode_frame(&[1, 2, 255], FLAG_END_STREAM).unwrap();
    let url = format!("https://{addr}/aster/Echo/unary");
    let denied = client
        .post(&url)
        .body(body.clone())
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let (status, flags, _) = decode_frame(&denied).unwrap();
    assert_ne!(flags & FLAG_TRAILER, 0);
    assert_eq!(
        decode_rpc_status(&status).unwrap().code,
        StatusCode::Unauthenticated.as_i32()
    );
    let reply = client
        .post(&url)
        .header("authorization", "Bearer secret")
        .body(body)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_reply(&reply, &[1, 2, 255]);

    let endpoint = Endpoint::client(
        ClientConfig::builder()
            .with_bind_default()
            .with_no_cert_validation()
            .build(),
    )
    .unwrap();
    // No readiness retries: construction already bound both sockets.
    let conn = tokio::time::timeout(
        wait_budget(),
        endpoint.connect(format!("https://{addr}/aster/wt")),
    )
    .await
    .unwrap()
    .unwrap();
    let (mut denied_send, mut denied_recv) = conn.open_bi().await.unwrap().await.unwrap();
    denied_send
        .write_all(&encode_frame(&header("unary", false), FLAG_HEADER).unwrap())
        .await
        .unwrap();
    denied_send
        .write_all(&encode_frame(&[], FLAG_END_STREAM).unwrap())
        .await
        .unwrap();
    let mut denied = Vec::new();
    tokio::time::timeout(wait_budget(), denied_recv.read_to_end(&mut denied))
        .await
        .unwrap()
        .unwrap();
    let (status, flags, _) = decode_frame(&denied).unwrap();
    assert_ne!(flags & FLAG_TRAILER, 0);
    assert_eq!(
        decode_rpc_status(&status).unwrap().code,
        StatusCode::Unauthenticated.as_i32()
    );
    let (mut send, mut recv) = conn.open_bi().await.unwrap().await.unwrap();
    send.write_all(&encode_frame(&header("unary", true), FLAG_HEADER).unwrap())
        .await
        .unwrap();
    send.write_all(&encode_frame(&[9, 8], FLAG_END_STREAM).unwrap())
        .await
        .unwrap();
    let mut reply = Vec::new();
    tokio::time::timeout(wait_budget(), recv.read_to_end(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_reply(&reply, &[9, 8]);
    conn.close(VarInt::from_u32(0), b"done");
    drop(conn);
    drop(endpoint);
    listener.shutdown(Duration::from_millis(100)).await.unwrap();
    listener.stop(Duration::ZERO).await.unwrap();
    assert_released(addr, true);
    node.shutdown().await;
}

#[tokio::test]
async fn bind_failures_are_returned_and_partial_tcp_bind_is_released() {
    let node = node().await;
    let (dispatcher, _, _) = setup(&node);
    let occupied = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = occupied.local_addr().unwrap();
    assert!(HttpConfig::new(addr.to_string(), pem())
        .webtransport(true)
        .start_authenticated(dispatcher.clone(), Arc::new(TokenAuth))
        .await
        .is_err());
    assert_released(addr, false);
    assert!(HttpConfig::new(
        "127.0.0.1:0",
        TlsMaterial::pem(b"bad certificate", b"bad key")
    )
    .start_authenticated(dispatcher.clone(), Arc::new(TokenAuth))
    .await
    .is_err());
    assert!(HttpConfig::new(
        "127.0.0.1:0",
        TlsMaterial::self_signed(["localhost".into()])
    )
    .start_authenticated(dispatcher, Arc::new(TokenAuth))
    .await
    .is_err());
    node.shutdown().await;
}

#[tokio::test]
async fn shutdown_drains_until_grace_then_cancels_pending_http_handler() {
    let node = node().await;
    let (dispatcher, mut started, mut dropped) = setup(&node);
    let mut listener = HttpConfig::new("127.0.0.1:0", pem())
        .start_authenticated(dispatcher, Arc::new(TokenAuth))
        .await
        .unwrap();
    let addr = listener.tcp_addr();
    let response = client()
        .post(format!("https://{addr}/aster/Echo/hold"))
        .header("authorization", "Bearer secret")
        .body(encode_frame(&[], FLAG_END_STREAM).unwrap())
        .send()
        .await
        .unwrap();
    next(&mut started).await;
    let begin = Instant::now();
    tokio::time::timeout(wait_budget(), listener.stop(Duration::from_millis(50)))
        .await
        .unwrap()
        .unwrap();
    assert!(begin.elapsed() >= Duration::from_millis(40));
    next(&mut dropped).await;
    drop(response);
    assert_released(addr, false);
    node.shutdown().await;
}

#[tokio::test]
async fn webtransport_stop_and_connection_loss_cancel_idle_handlers() {
    let node = node().await;
    let (dispatcher, mut started, mut dropped) = setup(&node);
    let mut listener = HttpConfig::new("127.0.0.1:0", pem())
        .webtransport(true)
        .start_authenticated(dispatcher, Arc::new(TokenAuth))
        .await
        .unwrap();
    let addr = listener.tcp_addr();
    let endpoint = Endpoint::client(
        ClientConfig::builder()
            .with_bind_default()
            .with_no_cert_validation()
            .build(),
    )
    .unwrap();
    let conn = endpoint
        .connect(format!("https://{addr}/aster/wt"))
        .await
        .unwrap();
    for disconnected in [false, true] {
        let (mut send, recv) = conn.open_bi().await.unwrap().await.unwrap();
        send.write_all(&encode_frame(&header("hold", true), FLAG_HEADER).unwrap())
            .await
            .unwrap();
        send.write_all(&encode_frame(&[], FLAG_END_STREAM).unwrap())
            .await
            .unwrap();
        next(&mut started).await;
        if disconnected {
            conn.close(VarInt::from_u32(9), b"disconnect");
        } else {
            recv.stop(VarInt::from_u32(9));
        }
        next(&mut dropped).await;
    }
    drop(conn);
    drop(endpoint);
    listener.stop(Duration::from_millis(100)).await.unwrap();
    assert_released(addr, true);
    node.shutdown().await;
}

#[tokio::test]
async fn cancelling_stop_keeps_ownership_for_a_later_join() {
    let node = node().await;
    let (dispatcher, mut started, mut dropped) = setup(&node);
    let mut listener = HttpConfig::new("127.0.0.1:0", pem())
        .start_authenticated(dispatcher, Arc::new(TokenAuth))
        .await
        .unwrap();
    let addr = listener.tcp_addr();
    let response = client()
        .post(format!("https://{addr}/aster/Echo/hold"))
        .header("authorization", "Bearer secret")
        .body(encode_frame(&[], FLAG_END_STREAM).unwrap())
        .send()
        .await
        .unwrap();
    next(&mut started).await;
    // short-wait-ok: a probe that must still be pending; expiry is the pass
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        listener.stop(Duration::from_millis(100))
    )
    .await
    .is_err());
    tokio::time::timeout(wait_budget(), listener.stop(Duration::ZERO))
        .await
        .unwrap()
        .unwrap();
    next(&mut dropped).await;
    drop(response);
    assert_released(addr, false);
    node.shutdown().await;
}

#[tokio::test]
async fn stopping_listener_cancels_live_webtransport_and_retry_joins_quic_drain() {
    let node = node().await;
    let (dispatcher, mut started, mut dropped) = setup(&node);
    let mut listener = HttpConfig::new("127.0.0.1:0", pem())
        .webtransport(true)
        .start_authenticated(dispatcher, Arc::new(TokenAuth))
        .await
        .unwrap();
    let addr = listener.tcp_addr();
    let endpoint = Endpoint::client(
        ClientConfig::builder()
            .with_bind_default()
            .with_no_cert_validation()
            .build(),
    )
    .unwrap();
    let conn = endpoint
        .connect(format!("https://{addr}/aster/wt"))
        .await
        .unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap().await.unwrap();
    send.write_all(&encode_frame(&header("hold", true), FLAG_HEADER).unwrap())
        .await
        .unwrap();
    send.write_all(&encode_frame(&[], FLAG_END_STREAM).unwrap())
        .await
        .unwrap();
    next(&mut started).await;
    // Application work is forced immediately; the QUIC socket still has its
    // close/drain to finish. Cancelling this wait must retain that ownership.
    assert!(
        // short-wait-ok: a probe that must still be pending; expiry is the pass
        tokio::time::timeout(Duration::from_millis(1), listener.stop(Duration::ZERO))
            .await
            .is_err()
    );
    next(&mut dropped).await;
    tokio::time::timeout(wait_budget(), listener.stop(Duration::ZERO))
        .await
        .unwrap()
        .unwrap();
    // The peer was deliberately left connected throughout server shutdown.
    assert_released(addr, true);
    drop(conn);
    drop(endpoint);
    node.shutdown().await;
}
