#![cfg(feature = "expose")]
//! End-to-end bridge test: a high-level `aster::Node` exposes an HTTP service,
//! and a peer relays a request to it over real QUIC — exercising
//! `Node::expose` / `Node::serve_expose` and the consumer `relay_request`.

use std::sync::Arc;
use std::time::Duration;

use aster::expose::{
    relay_request, AuthorizeFn, BoxFut, HttpHandler, PeerContext, RelayBody, RelayError,
    ResponseBody,
};
use aster::{AsterConfig, Node, RelayMode};
use http_body_util::BodyExt;
use tokio::time::timeout;

const ALPN: &[u8] = b"aster-expose/test";

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

/// Admit every peer.
fn allow_all() -> AuthorizeFn {
    Arc::new(|_ctx: PeerContext| Box::pin(async { Ok(()) }) as BoxFut<anyhow::Result<()>>)
}

/// Reject every peer (the origin's final veto).
fn deny_all() -> AuthorizeFn {
    Arc::new(|ctx: PeerContext| {
        Box::pin(async move { anyhow::bail!("peer {} not allowed", ctx.peer_id) })
            as BoxFut<anyhow::Result<()>>
    })
}

/// Echo the request path back in a header and the body verbatim.
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
async fn node_expose_http_round_trip() {
    // Host: start with the expose ALPN registered, expose "signaling", and let
    // the node own the accept loop.
    let host = Node::start_with_alpns(cfg(), vec![ALPN.to_vec()])
        .await
        .unwrap();
    let expose = host.expose();
    expose.expose_http("signaling", allow_all(), echo_handler());

    let host_for_loop = host.clone();
    let expose_for_loop = expose.clone();
    tokio::spawn(async move {
        let _ = host_for_loop.serve_expose(&expose_for_loop, &[ALPN]).await;
    });

    // Consumer: dial the host on the expose ALPN and relay an HTTP request.
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&host).await;
    wait_for_addr(&client).await;
    client.add_peer(&host).unwrap();
    host.add_peer(&client).unwrap();

    let conn = client.connect(&host.id(), ALPN).await.unwrap();

    let req = http::Request::builder()
        .uri("/hello")
        .body(b"ping".to_vec())
        .unwrap();
    let resp = timeout(
        Duration::from_secs(10),
        relay_request(&conn, "signaling", req),
    )
    .await
    .expect("relay timed out")
    .expect("relay failed");

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-echo-path"], "/hello");
    assert_eq!(resp.body(), b"ping");

    client.shutdown().await;
    host.shutdown().await;
}

#[tokio::test]
async fn node_expose_http_rejects_bad_token() {
    let host = Node::start_with_alpns(cfg(), vec![ALPN.to_vec()])
        .await
        .unwrap();
    let expose = host.expose();
    expose.expose_http("signaling", deny_all(), echo_handler());

    let host_for_loop = host.clone();
    let expose_for_loop = expose.clone();
    tokio::spawn(async move {
        let _ = host_for_loop.serve_expose(&expose_for_loop, &[ALPN]).await;
    });

    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&host).await;
    wait_for_addr(&client).await;
    client.add_peer(&host).unwrap();
    host.add_peer(&client).unwrap();

    let conn = client.connect(&host.id(), ALPN).await.unwrap();
    let req = http::Request::builder()
        .uri("/hello")
        .body(b"ping".to_vec())
        .unwrap();
    // The origin's admission rejects every peer, so the relay stream is closed
    // and the request errors.
    let result = timeout(
        Duration::from_secs(10),
        relay_request(&conn, "signaling", req),
    )
    .await
    .expect("relay timed out");
    assert!(
        result.is_err(),
        "expected admission rejection, got {result:?}"
    );

    client.shutdown().await;
    host.shutdown().await;
}
