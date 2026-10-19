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

use aster_expose::body::{body_channel, RelayBody, RelayError, ResponseBody};
use aster_expose::relay::{
    expose_http_on_connection, expose_local_http, relay_request, relay_request_streaming,
    AuthenticatedPeer, AuthorizeFn, HttpHandler, LocalHttpTarget,
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
    let server = CoreNetClient::create_loopback(TEST_ALPN.to_vec()).await?;
    let client = CoreNetClient::create_loopback(TEST_ALPN.to_vec()).await?;
    let server_addr = server.loopback_addr()?;
    let (reactor, accept_task, server_conns) = start_server_reactor(server.clone()).await;
    let client_conn = client
        .connect_node_addr(server_addr, TEST_ALPN.to_vec())
        .await?;
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

/// Reflects the transport-authenticated peer (from the request extension) into `x-authed-peer`, and any
/// forged `x-authenticated-peer` REQUEST header into `x-hdr-peer` — so a test can prove the extension
/// (not the header) is the authoritative identity.
fn peer_reflect_handler() -> HttpHandler {
    Arc::new(|req: http::Request<RelayBody>| {
        Box::pin(async move {
            let authed = req
                .extensions()
                .get::<AuthenticatedPeer>()
                .map(|p| p.as_str().to_owned())
                .unwrap_or_else(|| "<none>".to_owned());
            let hdr = req
                .headers()
                .get("x-authenticated-peer")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_owned();
            let resp = http::Response::builder()
                .status(200)
                .header("x-authed-peer", authed)
                .header("x-hdr-peer", hdr)
                .body(
                    http_body_util::Full::new(bytes::Bytes::new())
                        .map_err(|e: std::convert::Infallible| -> RelayError { match e {} })
                        .boxed_unsync(),
                )
                .unwrap();
            Ok(resp)
        }) as BoxFut<Result<http::Response<ResponseBody>>>
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_peer_is_the_connection_remote_id() -> Result<()> {
    // End-to-end over a REAL authenticated QUIC connection: the relay derives AuthenticatedPeer from
    // `conn.remote_id()`, so the handler's extension must equal the id the server authenticated the
    // client as — the identity the host turns into an `EdgeId`. A forged identity-looking REQUEST
    // header must NOT change it (it rides along inert), proving the transport peer — not HTTP input —
    // determines attribution. This is the remaining end-to-end link for the host's fail-closed
    // `EdgeId::from_authenticated_peer` derivation.
    let h = harness().await?;
    let expected_peer = h.server_conn.remote_id(); // what the server authenticated the client as
    expose_http_on_connection(&h.server_conn, "peer", allow_all(), peer_reflect_handler());

    let mut req = post("/x", b"");
    req.headers_mut()
        .insert("x-authenticated-peer", "attacker-peer".parse().unwrap());
    let resp = timeout(STEP_TIMEOUT, relay_request(&h.client_conn, "peer", req)).await??;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-authed-peer")
            .unwrap()
            .to_str()
            .unwrap(),
        expected_peer,
        "the authenticated peer must be the connection's remote id, not a header"
    );
    assert_eq!(
        resp.headers().get("x-hdr-peer").unwrap(),
        "attacker-peer",
        "the forged identity header rides along inert — the extension is authoritative"
    );
    assert_ne!(expected_peer, "attacker-peer");
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
        // short-wait-ok: datagrams are unreliable; each try waits briefly, then resends
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

// ── a producer that stops mid-body ──────────────────────────────────────────
//
// Design doc §6 / upcall-design §6: a body that fails partway must reach the
// other side as a failed body (the stream is reset), never as a clean finish
// of a truncated one. QUIC finishes a send half that is merely dropped, so
// every error path has to reset explicitly.

/// A response body that yields `chunk` and then fails.
struct FailsAfter {
    chunk: Option<bytes::Bytes>,
}

impl http_body::Body for FailsAfter {
    type Data = bytes::Bytes;
    type Error = RelayError;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<bytes::Bytes>, RelayError>>> {
        std::task::Poll::Ready(Some(match self.chunk.take() {
            Some(c) => Ok(http_body::Frame::data(c)),
            None => Err("upstream body failed".into()),
        }))
    }
}

/// Answers `/native` with a native body that errors after one chunk, and
/// `/channel` with a [`body_channel`] that is aborted after one chunk.
fn failing_body_handler() -> HttpHandler {
    Arc::new(|req: http::Request<RelayBody>| {
        Box::pin(async move {
            let body: ResponseBody = if req.uri().path() == "/channel" {
                let (tx, body) = body_channel(4);
                tokio::spawn(async move {
                    tx.send(bytes::Bytes::from_static(b"partial")).await.ok();
                    // Let the chunk reach the wire before the abort.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    tx.abort("producer failed");
                });
                body.boxed_unsync()
            } else {
                FailsAfter {
                    chunk: Some(bytes::Bytes::from_static(b"partial")),
                }
                .boxed_unsync()
            };
            Ok(http::Response::builder().status(200).body(body).unwrap())
        }) as BoxFut<Result<http::Response<ResponseBody>>>
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_body_failing_mid_stream_fails_the_consumer() -> Result<()> {
    let h = harness().await?;
    expose_http_on_connection(&h.server_conn, "fail", allow_all(), failing_body_handler());

    for path in ["/native", "/channel"] {
        // Buffered consumer: the whole exchange fails.
        let res = timeout(
            STEP_TIMEOUT,
            relay_request(&h.client_conn, "fail", post(path, b"")),
        )
        .await?;
        assert!(
            res.is_err(),
            "{path}: truncated body accepted: {:?}",
            res.map(|r| r.body().len())
        );

        // Streaming consumer: the body fails rather than ending cleanly. (A
        // reset discards unsent data, so when the body fails at once the reset
        // may overtake the head too; either way the exchange fails.)
        let req = http::Request::builder()
            .uri(path)
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let res = timeout(
            STEP_TIMEOUT,
            relay_request_streaming(&h.client_conn, "fail", req),
        )
        .await?;
        if let Ok(resp) = res {
            assert_eq!(resp.status(), 200);
            let collected = timeout(STEP_TIMEOUT, resp.into_body().collect()).await?;
            assert!(
                collected.is_err(),
                "{path}: streamed body ended cleanly instead of failing"
            );
        } else if path == "/channel" {
            // This producer waits before aborting, so its head is out first.
            panic!("{path}: head lost: {res:?}", res = res.err());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_response_that_finishes_still_ends_cleanly() -> Result<()> {
    let h = harness().await?;
    let handler: HttpHandler = Arc::new(|_req: http::Request<RelayBody>| {
        Box::pin(async move {
            let (tx, body) = body_channel(2);
            tokio::spawn(async move {
                for i in 0..8u8 {
                    tx.send(bytes::Bytes::from(vec![i; 4096])).await.ok();
                }
                tx.finish();
            });
            Ok(http::Response::builder()
                .status(200)
                .body(body.boxed_unsync())
                .unwrap())
        }) as BoxFut<Result<http::Response<ResponseBody>>>
    });
    expose_http_on_connection(&h.server_conn, "ok", allow_all(), handler);
    let resp = timeout(
        STEP_TIMEOUT,
        relay_request(&h.client_conn, "ok", post("/", b"")),
    )
    .await??;
    assert_eq!(resp.body().len(), 8 * 4096);
    Ok(())
}

type Seen = std::result::Result<usize, String>;

/// Reports what a handler read of its request body, or -- if the handler is
/// dropped first (the consumer gave up and the relay cancelled it) -- that it
/// never saw the body end. Either way, anything but `Ok`.
struct Report(Option<tokio::sync::oneshot::Sender<Seen>>);

impl Drop for Report {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(Err("handler dropped before the body ended".into()));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_aborted_mid_body_fails_the_handler_read() -> Result<()> {
    let h = harness().await?;
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<Seen>();
    let seen_tx = Arc::new(Mutex::new(Some(seen_tx)));
    // The handler is running (the head is through) -- abort only after that,
    // so the reset lands mid-body rather than before the request head.
    let started = Arc::new(tokio::sync::Notify::new());
    let started_h = started.clone();
    let handler: HttpHandler = Arc::new(move |req: http::Request<RelayBody>| {
        let (seen_tx, started) = (seen_tx.clone(), started_h.clone());
        Box::pin(async move {
            let mut report = Report(seen_tx.lock().unwrap().take());
            started.notify_one();
            let got = req
                .into_body()
                .collect()
                .await
                .map(|c| c.to_bytes().len())
                .map_err(|e| e.to_string());
            if let Some(tx) = report.0.take() {
                let _ = tx.send(got);
            }
            Ok(http::Response::builder()
                .status(200)
                .body(
                    http_body_util::Empty::new()
                        .map_err(|e: std::convert::Infallible| -> RelayError { match e {} })
                        .boxed_unsync(),
                )
                .unwrap())
        }) as BoxFut<Result<http::Response<ResponseBody>>>
    });
    expose_http_on_connection(&h.server_conn, "up", allow_all(), handler);

    let (tx, body) = body_channel(4);
    let req = http::Request::builder()
        .method("PUT")
        .uri("/upload")
        .body(body)
        .unwrap();
    let pump = tokio::spawn(async move {
        tx.send(bytes::Bytes::from_static(b"partial")).await.ok();
        started.notified().await;
        tx.abort("upload source failed");
    });
    let res = timeout(
        STEP_TIMEOUT,
        relay_request_streaming(&h.client_conn, "up", req),
    )
    .await?;
    assert!(res.is_err(), "an aborted upload must fail the request");
    pump.await?;

    let seen = timeout(STEP_TIMEOUT, seen_rx).await??;
    assert!(
        seen.is_err(),
        "the handler read a truncated upload as complete: {seen:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_cancelled_by_the_consumer_fails_the_body_read() -> Result<()> {
    let h = harness().await?;
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<Seen>();
    let seen_tx = Arc::new(Mutex::new(Some(seen_tx)));
    let started = Arc::new(tokio::sync::Notify::new());
    let started_h = started.clone();
    // Answers at once and reads the upload on its own task, so the read
    // outlives the exchange and only the stream's end tells it the outcome.
    let handler: HttpHandler = Arc::new(move |req: http::Request<RelayBody>| {
        let (seen_tx, started) = (seen_tx.clone(), started_h.clone());
        Box::pin(async move {
            let report = Report(seen_tx.lock().unwrap().take());
            tokio::spawn(async move {
                let mut report = report;
                let got = req
                    .into_body()
                    .collect()
                    .await
                    .map(|c| c.to_bytes().len())
                    .map_err(|e| e.to_string());
                if let Some(tx) = report.0.take() {
                    let _ = tx.send(got);
                }
            });
            started.notify_one();
            Ok(http::Response::builder()
                .status(200)
                .body(
                    http_body_util::Empty::new()
                        .map_err(|e: std::convert::Infallible| -> RelayError { match e {} })
                        .boxed_unsync(),
                )
                .unwrap())
        }) as BoxFut<Result<http::Response<ResponseBody>>>
    });
    expose_http_on_connection(&h.server_conn, "up", allow_all(), handler);

    let (tx, body) = body_channel(4);
    let req = http::Request::builder()
        .method("PUT")
        .uri("/upload")
        .body(body)
        .unwrap();
    tx.send(bytes::Bytes::from_static(b"partial")).await.ok();
    // The upload never ends; the consumer gives up once the handler runs --
    // by dropping the call, or (full duplex: the head can arrive while the
    // upload is still open) by dropping the response unread.
    let call = relay_request_streaming(&h.client_conn, "up", req);
    tokio::select! {
        resp = call => drop(resp?),
        () = started.notified() => {}
    }
    drop(tx);

    let seen = timeout(STEP_TIMEOUT, seen_rx).await??;
    assert!(
        seen.is_err(),
        "a cancelled upload was read as complete: {seen:?}"
    );
    Ok(())
}
