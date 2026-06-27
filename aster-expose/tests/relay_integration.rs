//! End-to-end integration for the L7 HTTP-object relay (design doc Stage A,
//! steps A3–A5): a real QUIC connection between two `CoreNetClient` endpoints,
//! the reactor's `FLAG_TUNNEL` subtype-1 dispatch, session admission, and the
//! `aster-expose` `ServiceAcceptor` decoding the H3-object and dispatching to a
//! buffered handler — driven by the `relay_request` client.
//!
//! Mirrors the connection-pair harness in `core/tests/tunnel_contract.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::time::timeout;

use aster_transport_core::datagram::DatagramRouter;
use aster_transport_core::reactor::{create_reactor, ReactorHandle};
use aster_transport_core::stream_io::BoxFut;
use aster_transport_core::{CoreConnection, CoreNetClient};

use http_body_util::BodyExt;

use aster_expose::body::{RelayBody, RelayError, ResponseBody};
use aster_expose::relay::{
    expose_http_on_connection, expose_local_http, relay_request, AuthorizeFn, HttpHandler,
    LocalHttpTarget,
};

const TEST_ALPN: &[u8] = b"aster-test/expose";
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

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

fn deny_all() -> AuthorizeFn {
    Arc::new(|_peer| Box::pin(async { Err(anyhow::anyhow!("nope")) }) as BoxFut<Result<()>>)
}

/// Echoes the request path into a header and the (streamed) request body back
/// as the response body.
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
    // Held so the endpoints/reactor stay alive for the test's duration.
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

fn post(path: &str, body: &[u8]) -> http::Request<Vec<u8>> {
    http::Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "backend.local")
        .body(body.to_vec())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_round_trip_same_process() -> Result<()> {
    let h = harness().await?;
    expose_http_on_connection(&h.server_conn, "echo", allow_all(), echo_handler());

    let resp = timeout(
        STEP_TIMEOUT,
        relay_request(&h.client_conn, "echo", post("/hello", b"ping")),
    )
    .await??;

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-echo-path").unwrap(), "/hello");
    assert_eq!(resp.body(), b"ping");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_request_reuses_admission() -> Result<()> {
    // Two requests on the same connection: the first admits (connection,
    // service), the second flows without re-authorizing. Each is its own stream.
    let h = harness().await?;
    expose_http_on_connection(&h.server_conn, "echo", allow_all(), echo_handler());

    for n in ["/a", "/b", "/c"] {
        let resp = timeout(
            STEP_TIMEOUT,
            relay_request(&h.client_conn, "echo", post(n, n.as_bytes())),
        )
        .await??;
        assert_eq!(resp.headers().get("x-echo-path").unwrap(), n);
        assert_eq!(resp.body(), n.as_bytes());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_service_drops_stream() -> Result<()> {
    let h = harness().await?;
    expose_http_on_connection(&h.server_conn, "echo", allow_all(), echo_handler());

    // No service named "ghost" → server drops the stream → client read errors.
    let r = timeout(
        STEP_TIMEOUT,
        relay_request(&h.client_conn, "ghost", post("/x", b"")),
    )
    .await?;
    assert!(r.is_err(), "unknown service must not yield a response");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_authorize_drops_stream() -> Result<()> {
    let h = harness().await?;
    expose_http_on_connection(&h.server_conn, "echo", deny_all(), echo_handler());

    let r = timeout(
        STEP_TIMEOUT,
        relay_request(&h.client_conn, "echo", post("/x", b"")),
    )
    .await?;
    assert!(r.is_err(), "denied authorize must not yield a response");
    Ok(())
}

/// Long-lived loopback TCP echo (mirrors `core/tests/tunnel_contract.rs`).
async fn spawn_echo() -> Result<std::net::SocketAddr> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = sock.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    Ok(addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_mode_raw_splice() -> Result<()> {
    // A6: `expose_local_http(Socket)` — the service-keyed admission path with a
    // raw TCP splice to a loopback server (protocol-blind). The consumer opens
    // the same http-relay stream but writes raw bytes instead of an H3-object.
    let h = harness().await?;
    let echo_addr = spawn_echo().await?;
    expose_local_http(
        &h.server_conn,
        "tcp",
        allow_all(),
        LocalHttpTarget::Socket(echo_addr),
    );

    let (send, recv) = h.client_conn.open_http_relay("tcp", &[]).await?;
    send.write_all(b"hello raw splice".to_vec()).await?;
    send.finish().await?;
    let echoed = timeout(STEP_TIMEOUT, recv.read_to_end(64 * 1024)).await??;
    assert_eq!(echoed, b"hello raw splice");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datagram_demux_round_trip() -> Result<()> {
    // Stage B step 7: flow-id-tagged datagrams over a real QUIC connection.
    // The server registers flow 42 and runs the demux loop; the client tags a
    // datagram for flow 42, which routes to that flow's queue.
    let h = harness().await?;

    let server_router = DatagramRouter::new(h.server_conn.clone());
    let _recv_loop = server_router.spawn_recv_loop();
    let mut flow = server_router.register(42);

    let client_router = DatagramRouter::new(h.client_conn.clone());
    // Datagrams are unreliable; retry a few times so loopback scheduling races
    // don't flake the test.
    let mut got = None;
    for _ in 0..10 {
        client_router.send(42, b"datagram payload")?;
        if let Ok(Some(p)) = timeout(Duration::from_millis(300), flow.recv()).await {
            got = Some(p);
            break;
        }
    }
    assert_eq!(
        got.expect("flow 42 received no datagram"),
        b"datagram payload"
    );
    Ok(())
}
