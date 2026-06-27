//! End-to-end reverse-proxy demo (the §8 Rust-consumer gate).
//!
//! An **origin** node exposes a local HTTP service over Aster; an **edge** node
//! serves public HTTP and reverse-proxies browser traffic to the origin through
//! the Aster (QUIC) hop. Both run in one process here; in production they are
//! separate machines (the origin behind NAT, the edge public).
//!
//! Run it:
//! ```bash
//! cargo run -p aster-expose --example reverse_proxy --features edge
//! ```
//! It wires both nodes, registers a route, drives one real HTTP request through
//! the edge, prints the response, and exits.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use aster_transport_core::reactor::create_reactor;
use aster_transport_core::stream_io::BoxFut;
use aster_transport_core::tunnel::PeerContext;
use aster_transport_core::CoreNetClient;

use aster_expose::body::{RelayBody, RelayError, ResponseBody};
use aster_expose::control::{request_route, EdgeRouter, RoutePolicy, RouteProtocol, RouteSpec};
use aster_expose::edge::{serve_edge, EdgeConfig};
use aster_expose::node::ExposeNode;
use aster_expose::relay::{AuthorizeFn, HttpHandler};

const ALPN: &[u8] = b"aster-expose/demo";

/// Admission policy for the origin's service — here, allow any peer. A real one
/// inspects `ctx.peer_id` and `ctx.metadata` (a token/capability).
fn allow_all() -> AuthorizeFn {
    Arc::new(|_ctx: PeerContext| Box::pin(async { Ok(()) }) as BoxFut<Result<()>>)
}

/// The local service being exposed: a tiny "hello" handler.
fn hello_handler() -> HttpHandler {
    Arc::new(|req: http::Request<RelayBody>| {
        Box::pin(async move {
            let path = req.uri().path().to_owned();
            let _ = req.into_body().collect().await; // drain request body
            let body = http_body_util::Full::new(bytes::Bytes::from(format!(
                "hello from the origin (you asked for {path})\n"
            )))
            .map_err(|e: std::convert::Infallible| -> RelayError { match e {} })
            .boxed_unsync();
            Ok(http::Response::builder()
                .status(200)
                .header("content-type", "text/plain")
                .body(body)
                .unwrap())
        }) as BoxFut<Result<http::Response<ResponseBody>>>
    })
}

/// Edge route policy: only grant routes to peers presenting the right token,
/// and grant exactly what they ask for.
fn route_policy() -> RoutePolicy {
    Arc::new(|ctx: PeerContext, specs: Vec<RouteSpec>| {
        Box::pin(async move {
            if ctx.metadata != b"demo-token" {
                bail!("peer {} presented an invalid token", ctx.peer_id);
            }
            Ok(specs)
        }) as BoxFut<Result<Vec<RouteSpec>>>
    })
}

/// Minimal raw HTTP/1.1 GET over a real socket (stands in for a browser/curl).
async fn http_get(addr: &str, path: &str, host: &str) -> Result<String> {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[tokio::main]
async fn main() -> Result<()> {
    let rt = tokio::runtime::Handle::current();

    // ── Edge node ────────────────────────────────────────────────────────────
    // Accepts route registrations and reverse-proxies public HTTP to origins.
    let edge_net = CoreNetClient::create(ALPN.to_vec()).await?;
    let edge_id = edge_net.endpoint_id();

    let router = EdgeRouter::new();
    let edge_node = ExposeNode::new();
    edge_node.serve_routes(router.clone(), route_policy());

    // Reactor dispatches each accepted connection's streams; `attach` makes the
    // node-wide control service reachable on every one of them.
    let (_edge_reactor, edge_feeder) = create_reactor(&rt, 256);
    {
        let edge_net = edge_net.clone();
        let edge_node = edge_node.clone();
        tokio::spawn(async move {
            while let Ok(mut conn) = edge_net.accept().await {
                edge_node.attach(&mut conn);
                edge_feeder.feed(conn);
            }
        });
    }

    // Public HTTP listener on an ephemeral loopback port (plaintext for the
    // demo; use `EdgeConfig::tls(EdgeTls::Acme { .. })` in production).
    let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
    let http_port = probe.local_addr()?.port();
    drop(probe);
    let http_addr = format!("127.0.0.1:{http_port}");
    {
        let router = router.clone();
        let cfg = EdgeConfig::new(http_addr.clone());
        tokio::spawn(serve_edge(router, cfg));
    }

    // ── Origin node ──────────────────────────────────────────────────────────
    // Exposes a local service and registers a route with the edge.
    let origin_net = CoreNetClient::create(ALPN.to_vec()).await?;
    let origin_node = ExposeNode::new();
    origin_node.expose_http("web", allow_all(), hello_handler());

    let (_origin_reactor, origin_feeder) = create_reactor(&rt, 256);

    // Dial the edge, feed that connection to our reactor (so the edge's relay
    // streams reach "web"), and register a route. `attach` before feeding.
    let mut conn = origin_net.connect(edge_id, ALPN.to_vec()).await?;
    origin_node.attach(&mut conn);
    origin_feeder.feed(conn.clone());

    let reg = request_route(
        &conn,
        vec![RouteSpec {
            host: "127.0.0.1".into(),
            port: 0, // 0 = the edge listener's default port
            protocol: RouteProtocol::Http,
            service_id: "web".into(),
        }],
        b"demo-token".to_vec(),
    )
    .await?;
    println!(
        "origin registered {} route(s) with the edge",
        reg.granted().len()
    );

    // ── Drive one request through the edge ───────────────────────────────────
    let mut response = String::new();
    for _ in 0..50 {
        match http_get(&http_addr, "/hello", "127.0.0.1").await {
            Ok(r) if r.starts_with("HTTP/1.1") => {
                response = r;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }

    println!("\nGET http://{http_addr}/hello  (→ edge → Aster → origin)\n");
    println!("{response}");

    if !response.contains("hello from the origin") {
        bail!("the request did not reach the origin:\n{response}");
    }
    println!("✓ request reached the origin through the Aster edge");

    // In production the origin keeps `reg` alive and re-dials + re-registers on
    // disconnect (see the reconnect pattern in docs/aster-expose-getstarted.md).
    // Here we exit cleanly, skipping teardown of the background server/reactor
    // tasks (which would otherwise log spurious cancellation noise).
    let _keep_alive = &reg;
    std::process::exit(0);
}
