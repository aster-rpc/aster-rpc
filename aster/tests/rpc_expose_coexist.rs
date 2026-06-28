#![cfg(all(feature = "rpc", feature = "expose"))]
//! One node, one identity, one accept loop — serving RPC (`aster/1`), an
//! aster-expose relay, and a bespoke custom-ALPN protocol simultaneously. This
//! is the Portal host shape: the RPC reactor is the single accept owner and
//! fans every non-`aster/1` ALPN out to its registered handler (no second
//! acceptor splitting connections).

use std::sync::Arc;
use std::time::Duration;

use aster::expose::{
    AuthorizeFn, BoxFut, Expose, HttpHandler, PeerContext, RelayBody, RelayError, ResponseBody,
};
use aster::rpc::{AsterServer, Call, RpcStatus, ServiceDispatch};
use aster::rpc::{SerializationMode, StreamHeader};
use aster::{AsterConfig, Node, RelayMode};
use http_body_util::BodyExt;
use tokio::sync::mpsc;
use tokio::time::timeout;

const SIGNALING_ALPN: &[u8] = b"portal/signaling/1";
const MEDIA_ALPN: &[u8] = b"portal/media/1";

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

fn header(method: &str) -> StreamHeader {
    StreamHeader {
        service: "Echo".into(),
        method: method.into(),
        version: 1,
        call_id: 0,
        deadline: 0,
        serialization_mode: SerializationMode::Xlang.as_i8(),
        metadata_keys: vec![],
        metadata_values: vec![],
        session_id: 0,
    }
}

/// Minimal unary RPC service: respond with the request bytes reversed.
struct EchoService;

#[async_trait::async_trait]
impl ServiceDispatch for EchoService {
    fn name(&self) -> &str {
        "Echo"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["unary"]
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let req = call.recv_request().await.unwrap_or_default();
        let resp: Vec<u8> = req.into_iter().rev().collect();
        let _ = call.respond(resp, &RpcStatus::ok());
    }
}

fn allow_all() -> AuthorizeFn {
    Arc::new(|_ctx: PeerContext| Box::pin(async { Ok(()) }) as BoxFut<anyhow::Result<()>>)
}

/// Echo the request body back with the path in a header.
fn echo_handler() -> HttpHandler {
    Arc::new(|req: http::Request<RelayBody>| {
        Box::pin(async move {
            let path = req.uri().path().to_owned();
            let body = req
                .into_body()
                .collect()
                .await
                .map_err(|e| anyhow::anyhow!("body: {e}"))?
                .to_bytes();
            let resp = http::Response::builder()
                .status(200)
                .header("x-echo-path", path)
                .body(
                    http_body_util::Full::new(body)
                        .map_err(|e: std::convert::Infallible| -> RelayError { match e {} })
                        .boxed_unsync(),
                )
                .unwrap();
            Ok(resp)
        }) as BoxFut<anyhow::Result<http::Response<ResponseBody>>>
    })
}

#[tokio::test]
async fn rpc_expose_and_custom_alpn_on_one_node() {
    // Expose registry for the signaling relay.
    let expose = Expose::new();
    expose.expose_http("signaling", allow_all(), echo_handler());

    // A bespoke custom-ALPN protocol: the handler spawns its own bi-stream echo
    // and reports the peer it served (proving the connection was routed + usable).
    let (media_tx, mut media_rx) = mpsc::unbounded_channel();

    let srv = AsterServer::builder()
        .config(cfg())
        .service(EchoService)
        .with_expose(SIGNALING_ALPN.to_vec(), expose.clone())
        .route_alpn(MEDIA_ALPN.to_vec(), move |conn| {
            let media_tx = media_tx.clone();
            tokio::spawn(async move {
                if let Ok((send, recv)) = conn.accept_bi().await {
                    let data = recv.read_to_end(1024).await.unwrap_or_default();
                    let _ = send.write_all(data).await;
                    let _ = send.finish().await;
                }
                let _ = media_tx.send(conn.peer());
                conn.closed().await;
            });
        })
        .start()
        .await
        .unwrap();

    // Client reaches the server's node by direct address (relay disabled).
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(srv.node()).await;
    wait_for_addr(&client).await;
    client.add_peer(srv.node()).unwrap();
    srv.node().add_peer(&client).unwrap();

    // 1) RPC on aster/1.
    let rpc = timeout(Duration::from_secs(10), client.rpc_connect(&srv.id()))
        .await
        .expect("rpc connect timed out")
        .unwrap();
    let resp = timeout(
        Duration::from_secs(10),
        rpc.unary(&header("unary"), vec![1, 2, 3]),
    )
    .await
    .expect("rpc timed out")
    .unwrap();
    assert_eq!(resp, vec![3, 2, 1], "RPC reversed-echo");

    // 2) aster-expose relay on the signaling ALPN.
    let sig_conn = client.connect(&srv.id(), SIGNALING_ALPN).await.unwrap();
    let req = http::Request::builder()
        .uri("/offer")
        .body(b"sdp".to_vec())
        .unwrap();
    let relayed = timeout(
        Duration::from_secs(10),
        aster::expose::relay_request(&sig_conn, "signaling", req),
    )
    .await
    .expect("relay timed out")
    .expect("relay failed");
    assert_eq!(relayed.status(), 200);
    assert_eq!(relayed.headers()["x-echo-path"], "/offer");
    assert_eq!(relayed.body(), b"sdp", "expose relayed body");

    // 3) Bespoke protocol on the media ALPN.
    let media_conn = client.connect(&srv.id(), MEDIA_ALPN).await.unwrap();
    let (media_send, media_recv) = media_conn.open_bi().await.unwrap();
    media_send.write_all(b"rtp".to_vec()).await.unwrap();
    media_send.finish().await.unwrap();
    let echo = timeout(Duration::from_secs(10), media_recv.read_to_end(64))
        .await
        .expect("media echo timed out")
        .unwrap();
    assert_eq!(echo, b"rtp", "custom-ALPN bi-stream echo");
    let served_peer = timeout(Duration::from_secs(10), media_rx.recv())
        .await
        .expect("media handler never fired")
        .unwrap();
    assert_eq!(served_peer, client.id(), "media handler saw the client");

    client.shutdown().await;
    srv.shutdown().await;
}
