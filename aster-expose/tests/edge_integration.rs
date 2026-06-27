#![cfg(feature = "edge")]
//! End-to-end for the public edge (cut 1, design doc §7 step 10): a Salvo
//! request → the `RelayHandler` → one Aster http-relay stream over real QUIC →
//! the origin's streaming handler, response streamed back. Driven by Salvo's
//! in-process `TestClient` (no socket) so the test stays hermetic; the QUIC hop
//! is real.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use http_body_util::BodyExt;

use aster_transport_core::reactor::{create_reactor, ReactorHandle};
use aster_transport_core::stream_io::BoxFut;
use aster_transport_core::{CoreConnection, CoreNetClient};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use aster_expose::body::{RelayBody, RelayError, ResponseBody};
use aster_expose::control::EdgeRouter;
use aster_expose::edge::{serve_edge, EdgeConfig, RelayHandler};
use aster_expose::relay::{expose_http_on_connection, AuthorizeFn, HttpHandler};

use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};

const TEST_ALPN: &[u8] = b"aster-test/edge";

type ServerConns = Arc<Mutex<Vec<CoreConnection>>>;

async fn start_server_reactor(
    server: CoreNetClient,
) -> (ReactorHandle, tokio::task::JoinHandle<()>, ServerConns) {
    let (handle, feeder) = create_reactor(&tokio::runtime::Handle::current(), 256);
    let conns: ServerConns = Arc::new(Mutex::new(Vec::new()));
    let conns_for_task = conns.clone();
    let accept_task = tokio::spawn(async move {
        loop {
            match server.accept().await {
                Ok(conn) => {
                    conns_for_task.lock().unwrap().push(conn.clone());
                    feeder.feed(conn);
                }
                Err(_) => return,
            }
        }
    });
    (handle, accept_task, conns)
}

async fn wait_for_one_server_conn(server_conns: &ServerConns) -> Result<CoreConnection> {
    for _ in 0..50 {
        if let Some(c) = server_conns.lock().unwrap().first().cloned() {
            return Ok(c);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(anyhow::anyhow!(
        "server reactor never observed a connection"
    ))
}

fn allow_all() -> AuthorizeFn {
    Arc::new(|_peer| Box::pin(async { Ok(()) }) as BoxFut<Result<()>>)
}

/// Streaming echo: reflects the path into a header and the body back.
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
        }) as BoxFut<Result<http::Response<ResponseBody>>>
    })
}

struct Harness {
    client_conn: CoreConnection,
    server_conn: CoreConnection,
    _server: CoreNetClient,
    _client: CoreNetClient,
    _reactor: ReactorHandle,
    _accept: tokio::task::JoinHandle<()>,
}

async fn harness() -> Result<Harness> {
    let server = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let client = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let server_id = server.endpoint_id();
    let (reactor, accept_task, server_conns) = start_server_reactor(server.clone()).await;
    let client_conn = client.connect(server_id, TEST_ALPN.to_vec()).await?;
    let server_conn = wait_for_one_server_conn(&server_conns).await?;
    Ok(Harness {
        client_conn,
        server_conn,
        _server: server,
        _client: client,
        _reactor: reactor,
        _accept: accept_task,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edge_relays_browser_request_through_aster() -> Result<()> {
    let h = harness().await?;
    // Origin exposes a service; the edge holds the relaying side (client_conn).
    expose_http_on_connection(&h.server_conn, "web", allow_all(), echo_handler());

    let router = EdgeRouter::new();
    router.register("backend.local", h.client_conn.clone(), "web");
    let service = Service::new(Router::with_path("{**path}").goal(RelayHandler::new(router)));

    let mut res = TestClient::post("http://backend.local/hello")
        .add_header("host", "backend.local", true)
        .bytes(b"ping".to_vec())
        .send(&service)
        .await;

    assert_eq!(res.status_code, Some(StatusCode::OK));
    assert_eq!(res.headers().get("x-echo-path").unwrap(), "/hello");
    assert_eq!(res.take_string().await?, "ping");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_host_returns_404() -> Result<()> {
    let h = harness().await?;
    expose_http_on_connection(&h.server_conn, "web", allow_all(), echo_handler());

    let router = EdgeRouter::new();
    router.register("backend.local", h.client_conn.clone(), "web");
    let service = Service::new(Router::with_path("{**path}").goal(RelayHandler::new(router)));

    let res = TestClient::get("http://ghost.local/")
        .add_header("host", "ghost.local", true)
        .send(&service)
        .await;
    assert_eq!(res.status_code, Some(StatusCode::NOT_FOUND));
    Ok(())
}

/// Send a raw HTTP/1.1 POST over a real TCP socket and return the full response
/// text (server sends `Connection: close`, so read-to-EOF terminates).
async fn raw_post(addr: &str, path: &str, host: &str, body: &str) -> Result<String> {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_edge_plaintext_real_socket() -> Result<()> {
    let h = harness().await?;
    expose_http_on_connection(&h.server_conn, "web", allow_all(), echo_handler());

    let router = EdgeRouter::new();
    router.register("127.0.0.1", h.client_conn.clone(), "web");

    // Grab a free port, then bind the edge on it (drop the probe first).
    let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = probe.local_addr()?.port();
    drop(probe);
    let addr = format!("127.0.0.1:{port}");

    let server = tokio::spawn(serve_edge(router, EdgeConfig::new(addr.clone())));

    // Poll the socket until the server is accepting, then drive one request
    // through the full path: TCP → Salvo H1 → QUIC relay → origin echo.
    let mut last = String::new();
    for _ in 0..50 {
        match raw_post(&addr, "/hi", "127.0.0.1", "ping").await {
            Ok(resp) if resp.starts_with("HTTP/1.1") => {
                last = resp;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    server.abort();

    assert!(last.contains("200"), "expected 200, got:\n{last}");
    assert!(
        last.to_ascii_lowercase().contains("x-echo-path: /hi"),
        "missing echoed path header:\n{last}"
    );
    // Body is chunk-framed (transfer-encoding: chunked); the echoed payload
    // appears within it.
    assert!(last.contains("ping"), "missing echoed body:\n{last}");
    Ok(())
}

/// A Salvo middleware that 403s a specific path before it reaches the relay.
struct BlockPath(&'static str);

#[async_trait]
impl Handler for BlockPath {
    async fn handle(
        &self,
        req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        if req.uri().path() == self.0 {
            res.status_code(StatusCode::FORBIDDEN);
            res.render("blocked by hoop");
            ctrl.skip_rest();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_edge_hoop_blocks_before_relay() -> Result<()> {
    let h = harness().await?;
    expose_http_on_connection(&h.server_conn, "web", allow_all(), echo_handler());

    let router = EdgeRouter::new();
    router.register("127.0.0.1", h.client_conn.clone(), "web");

    let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = probe.local_addr()?.port();
    drop(probe);
    let addr = format!("127.0.0.1:{port}");

    // Mount a middleware that blocks `/blocked`; `/ok` still relays.
    let cfg = EdgeConfig::new(addr.clone()).hoop(BlockPath("/blocked"));
    let server = tokio::spawn(serve_edge(router, cfg));

    // Wait for liveness via an allowed path.
    let mut ok = String::new();
    for _ in 0..50 {
        match raw_post(&addr, "/ok", "127.0.0.1", "x").await {
            Ok(resp) if resp.starts_with("HTTP/1.1") => {
                ok = resp;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let blocked = raw_post(&addr, "/blocked", "127.0.0.1", "x").await?;
    server.abort();

    assert!(ok.contains("200"), "allowed path should relay, got:\n{ok}");
    assert!(
        blocked.contains("403"),
        "blocked path should be 403, got:\n{blocked}"
    );
    Ok(())
}
