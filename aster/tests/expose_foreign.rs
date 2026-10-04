#![cfg(all(feature = "expose", feature = "binding-support"))]
//! R9: expose served by a "foreign" handler. An in-process poll thread plays
//! the binding: it drains an `UpcallQueue`, answers kind 49 `expose_http`
//! with `Upcalls::respond`, and moves bodies through `HttpBodies` with the
//! blocking calls a C ABI poll thread would use. End to end over real QUIC
//! (`Expose` + `relay_request[_streaming]`), plus in-process `serve` calls
//! for the limits and trailers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use aster::binding_support::upcall::codec::{
    self, ExposeHttpRequest, ExposeHttpResponse, ExposeRouteSpec, HttpBodyMode,
};
use aster::binding_support::upcall::expose::{
    expose_foreign_http, foreign_route_policy, BodyError, ForeignHttpHandler, ForeignHttpOptions,
    HttpBodies,
};
use aster::binding_support::upcall::{
    CancelReason, HookFailureMode, UpcallEvent, UpcallKind, UpcallPolicy, UpcallQueue,
    UpcallStatus, Upcalls,
};
use aster::expose::bytes::Bytes;
use aster::expose::{
    body_channel, relay_request, relay_request_streaming, tower_handler, Expose, PeerContext,
    RelayBody, RouteProtocol, RouteSpec,
};
use aster::{AsterConfig, Connection, Node, RelayMode};
use http_body_util::{BodyExt, Full};

const ALPN: &[u8] = b"aster-expose/foreign-test";
const SVC: &str = "app";
const WAIT: Duration = Duration::from_secs(10);

// ── the "foreign" binding ──────────────────────────────────────────────────

#[derive(Clone)]
struct Ctx {
    upcalls: Upcalls,
    bodies: HttpBodies,
}

impl Ctx {
    fn respond(&self, id: u64, status: u16, headers: &[(&str, &str)], body: HttpBodyMode) {
        let resp = ExposeHttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.as_bytes().to_vec()))
                .collect(),
            body,
        };
        let _ = self.upcalls.respond(
            id,
            UpcallStatus::Ok,
            codec::encode_expose_http_response(&resp),
        );
    }

    /// Drain a streamed request body with blocking reads.
    fn read_all(&self, stream_id: u64) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = self
            .bodies
            .read_blocking(stream_id, 16 * 1024, WAIT)
            .unwrap()
        {
            out.extend_from_slice(&chunk);
        }
        out
    }
}

type HttpFn = Arc<dyn Fn(&Ctx, u64, ExposeHttpRequest) + Send + Sync>;

/// The poll thread. Each HTTP upcall runs on its own thread (as a binding
/// would run each on its own task); authorize upcalls are answered inline.
struct Foreign {
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<UpcallEvent>>>,
    thread: Option<JoinHandle<()>>,
}

impl Foreign {
    fn start(queue: Arc<UpcallQueue>, ctx: Ctx, allow: bool, http: HttpFn) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (s, v) = (stop.clone(), seen.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                if !queue.wait_blocking(Duration::from_millis(10)) {
                    continue;
                }
                queue.drain(64, |ev| {
                    v.lock().unwrap().push(ev.clone());
                    match ev.kind {
                        UpcallKind::ExposeAuthorize => {
                            let _ = ctx.upcalls.respond(
                                ev.invocation_id,
                                UpcallStatus::Ok,
                                codec::encode_decision(allow),
                            );
                        }
                        UpcallKind::ExposeHttp => {
                            let req = codec::decode_expose_http_request(&ev.payload).unwrap();
                            let (ctx, http) = (ctx.clone(), http.clone());
                            std::thread::spawn(move || http(&ctx, ev.invocation_id, req));
                        }
                        _ => {}
                    }
                });
            }
        });
        Self {
            stop,
            seen,
            thread: Some(thread),
        }
    }

    fn seen(&self, kind: UpcallKind) -> Vec<UpcallEvent> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind == kind)
            .cloned()
            .collect()
    }
}

impl Drop for Foreign {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn setup() -> (Arc<UpcallQueue>, Ctx) {
    let queue = UpcallQueue::new(64);
    let upcalls = Upcalls::new(queue.clone());
    (
        queue,
        Ctx {
            upcalls,
            bodies: HttpBodies::new(),
        },
    )
}

// ── network fixture ────────────────────────────────────────────────────────

struct Net {
    host: Node,
    client: Node,
    conn: Connection,
}

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

/// A host serving `expose`, and a connected client.
async fn net(expose_setup: impl FnOnce(&Expose)) -> Net {
    let host = Node::start_with_alpns(cfg(), vec![ALPN.to_vec()])
        .await
        .unwrap();
    let expose = host.expose();
    expose_setup(&expose);
    let (h, e) = (host.clone(), expose.clone());
    tokio::spawn(async move {
        let _ = h.serve_expose(&e, &[ALPN]).await;
    });
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&host).await;
    wait_for_addr(&client).await;
    client.add_peer(&host).unwrap();
    host.add_peer(&client).unwrap();
    let conn = client.connect(&host.id(), ALPN).await.unwrap();
    Net { host, client, conn }
}

impl Net {
    async fn shutdown(self) {
        self.client.shutdown().await;
        self.host.shutdown().await;
    }
}

async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn get(uri: &str) -> http::Request<Vec<u8>> {
    http::Request::builder().uri(uri).body(Vec::new()).unwrap()
}

// ── end to end ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_answered_inline_with_the_authenticated_peer() {
    let (queue, ctx) = setup();
    let seen_req: Arc<Mutex<Option<ExposeHttpRequest>>> = Arc::default();
    let sr = seen_req.clone();
    let foreign = Foreign::start(
        queue,
        ctx.clone(),
        true,
        Arc::new(move |ctx, id, req| {
            *sr.lock().unwrap() = Some(req.clone());
            ctx.respond(
                id,
                200,
                &[
                    ("content-type", "text/plain"),
                    ("set-cookie", "a=1"),
                    ("set-cookie", "b=2"),
                ],
                HttpBodyMode::Inline(format!("hello {}", req.uri).into_bytes()),
            );
        }),
    );
    let c = ctx.clone();
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
    })
    .await;

    let req = http::Request::builder()
        .uri("/greet?x=1")
        .header("x-trace", "t1")
        .body(Vec::new())
        .unwrap();
    let resp = tokio::time::timeout(WAIT, relay_request(&n.conn, SVC, req))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), b"hello /greet?x=1");
    assert_eq!(resp.headers()["content-type"], "text/plain");
    let cookies: Vec<_> = resp.headers().get_all("set-cookie").iter().collect();
    assert_eq!(cookies, ["a=1", "b=2"]);

    let req = seen_req.lock().unwrap().clone().unwrap();
    assert_eq!(req.service_id, SVC);
    assert_eq!(req.method, "GET");
    assert_eq!(req.peer_id.as_deref(), Some(n.client.id().as_str()));
    assert!(req.headers.contains(&("x-trace".into(), b"t1".to_vec())));
    // No declared length: the body streams (and is empty).
    assert_eq!(req.body, HttpBodyMode::Stream);
    assert_eq!(foreign.seen(UpcallKind::ExposeAuthorize).len(), 1);
    wait_until("exchange closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_bodies_inline_and_streamed() {
    let (queue, ctx) = setup();
    let modes: Arc<Mutex<Vec<bool>>> = Arc::default();
    let m = modes.clone();
    let _foreign = Foreign::start(
        queue,
        ctx.clone(),
        true,
        Arc::new(move |ctx, id, req| {
            let body = match req.body {
                HttpBodyMode::Inline(b) => {
                    m.lock().unwrap().push(true);
                    b
                }
                HttpBodyMode::Stream => {
                    m.lock().unwrap().push(false);
                    ctx.read_all(req.stream_id)
                }
            };
            let sum: u64 = body.iter().map(|b| *b as u64).sum();
            ctx.respond(
                id,
                200,
                &[],
                HttpBodyMode::Inline(format!("{} {sum}", body.len()).into_bytes()),
            );
        }),
    );
    let c = ctx.clone();
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
    })
    .await;

    // Small, declared length: inline in the upcall.
    let req = http::Request::builder()
        .method("POST")
        .uri("/small")
        .header("content-length", "5")
        .body(b"abcde".to_vec())
        .unwrap();
    let resp = tokio::time::timeout(WAIT, relay_request(&n.conn, SVC, req))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.body(), b"5 495");

    // Streamed upload (no length) from a binding-style body channel: 40 x 8 KiB.
    let (tx, body) = body_channel(4);
    let req = http::Request::builder()
        .method("POST")
        .uri("/upload")
        .body(body)
        .unwrap();
    let pump = tokio::spawn(async move {
        for i in 0..40u8 {
            tx.send(Bytes::from(vec![i; 8192])).await.unwrap();
        }
        tx.finish();
    });
    let resp = tokio::time::timeout(WAIT, relay_request_streaming(&n.conn, SVC, req))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let expect: u64 = (0..40u64).map(|i| i * 8192).sum();
    assert_eq!(&body[..], format!("{} {expect}", 40 * 8192).as_bytes());
    pump.await.unwrap();
    assert_eq!(*modes.lock().unwrap(), [true, false]);
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_response_streams_through_body_writes() {
    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 48; // 3 MiB
    let (queue, ctx) = setup();
    let _foreign = Foreign::start(
        queue,
        ctx.clone(),
        true,
        Arc::new(move |ctx, id, req| {
            ctx.respond(
                id,
                200,
                &[("content-length", &(CHUNK * CHUNKS).to_string())],
                HttpBodyMode::Stream,
            );
            for i in 0..CHUNKS {
                ctx.bodies
                    .write_blocking(req.stream_id, vec![i as u8; CHUNK], WAIT)
                    .unwrap();
            }
            ctx.bodies.finish(req.stream_id, Vec::new()).unwrap();
        }),
    );
    let c = ctx.clone();
    let n = net(move |e| {
        // A small buffer, so the writer is held back by the consumer.
        let options = ForeignHttpOptions {
            response_buffer: 2,
            ..Default::default()
        };
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, options);
    })
    .await;

    let resp = tokio::time::timeout(WAIT, relay_request(&n.conn, SVC, get("/big")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.body();
    assert_eq!(body.len(), CHUNK * CHUNKS);
    for (i, chunk) in body.chunks(CHUNK).enumerate() {
        assert!(chunk.iter().all(|b| *b == i as u8), "chunk {i}");
    }
    wait_until("exchange closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

/// A streamed response that stops partway -- `HttpBodies::abort`, or a write
/// past `max_response_body` -- reaches the relay consumer as a failed
/// response: the relay resets the stream instead of finishing it, so the
/// consumer never takes the truncated body for a complete one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_stopped_mid_body_fails_over_the_relay() {
    let (queue, ctx) = setup();
    let _foreign = Foreign::start(
        queue,
        ctx.clone(),
        true,
        Arc::new(move |ctx, id, req| {
            ctx.respond(id, 200, &[], HttpBodyMode::Stream);
            let bodies = &ctx.bodies;
            if bodies
                .write_blocking(req.stream_id, vec![1; 600], WAIT)
                .is_err()
            {
                return;
            }
            // Give the first chunk time to reach the consumer, so the failure
            // lands mid-body rather than before the head.
            std::thread::sleep(Duration::from_millis(50));
            if req.uri.starts_with("/abort") {
                let _ = bodies.abort(req.stream_id, "handler failed");
            } else {
                // Crosses `max_response_body`: the write fails with
                // `TooLarge` and the body is aborted.
                let _ = bodies.write_blocking(req.stream_id, vec![1; 600], WAIT);
            }
        }),
    );
    let c = ctx.clone();
    let n = net(move |e| {
        let options = ForeignHttpOptions {
            max_response_body: Some(1000),
            ..Default::default()
        };
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, options);
    })
    .await;

    for path in ["/abort", "/cap"] {
        let res = tokio::time::timeout(WAIT, relay_request(&n.conn, SVC, get(path)))
            .await
            .unwrap();
        assert!(
            res.is_err(),
            "{path}: truncated body accepted: {:?}",
            res.map(|r| r.body().len())
        );

        let req = http::Request::builder()
            .uri(path)
            .body(Full::new(Bytes::new()))
            .unwrap();
        let resp = tokio::time::timeout(WAIT, relay_request_streaming(&n.conn, SVC, req))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resp.status(), 200);
        let collected = tokio::time::timeout(WAIT, resp.into_body().collect())
            .await
            .unwrap();
        assert!(
            collected.is_err(),
            "{path}: streamed body ended cleanly instead of failing"
        );
    }
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_keeps_headers_and_drops_the_body() {
    let (queue, ctx) = setup();
    let _foreign = Foreign::start(
        queue,
        ctx.clone(),
        true,
        Arc::new(move |ctx, id, req| {
            assert_eq!(req.method, "HEAD");
            ctx.respond(
                id,
                200,
                &[("content-length", "5")],
                HttpBodyMode::Inline(b"hello".to_vec()),
            );
        }),
    );
    let c = ctx.clone();
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
    })
    .await;
    let req = http::Request::builder()
        .method("HEAD")
        .uri("/file")
        .body(Vec::new())
        .unwrap();
    let resp = tokio::time::timeout(WAIT, relay_request(&n.conn, SVC, req))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-length"], "5");
    assert!(resp.body().is_empty());
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_failures_become_5xx() {
    let (queue, ctx) = setup();
    ctx.upcalls.set_policy(
        UpcallKind::ExposeHttp,
        UpcallPolicy::new(Duration::from_millis(300), HookFailureMode::FailClosed),
    );
    let _foreign = Foreign::start(
        queue,
        ctx.clone(),
        true,
        Arc::new(move |ctx, id, req| match req.uri.as_str() {
            "/error" => {
                let _ = ctx
                    .upcalls
                    .respond(id, UpcallStatus::Error, b"boom".to_vec());
            }
            "/unimplemented" => {
                let _ = ctx
                    .upcalls
                    .respond(id, UpcallStatus::Unimplemented, Vec::new());
            }
            "/garbage" => {
                let _ = ctx
                    .upcalls
                    .respond(id, UpcallStatus::Ok, b"not ucv1".to_vec());
            }
            "/bad-header" => ctx.respond(id, 200, &[("bad header", "x")], HttpBodyMode::Stream),
            _ => {} // "/silent": never answers -> timeout
        }),
    );
    let c = ctx.clone();
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
    })
    .await;
    for (uri, status) in [
        ("/error", 500),
        ("/unimplemented", 501),
        ("/garbage", 502),
        ("/bad-header", 502),
        ("/silent", 504),
    ] {
        let resp = tokio::time::timeout(WAIT, relay_request(&n.conn, SVC, get(uri)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resp.status(), status, "{uri}");
        // The foreign message never reaches the peer.
        assert!(resp.body().is_empty(), "{uri}");
    }
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_abort_cancels_the_upcall_and_closes_the_body() {
    let (queue, ctx) = setup();
    // Long enough that only the peer going away can end the upcall.
    ctx.upcalls.set_policy(
        UpcallKind::ExposeHttp,
        UpcallPolicy::new(Duration::from_secs(60), HookFailureMode::FailClosed),
    );
    let parked: Arc<Mutex<Vec<(u64, u64)>>> = Arc::default();
    let p = parked.clone();
    let foreign = Foreign::start(
        queue,
        ctx.clone(),
        true,
        Arc::new(move |_ctx, id, req| {
            // Hold the request without answering.
            p.lock().unwrap().push((id, req.stream_id));
        }),
    );
    let c = ctx.clone();
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
    })
    .await;

    // The consumer gives up: its request future (and stream) is dropped.
    let abandoned = tokio::time::timeout(
        Duration::from_millis(500),
        relay_request(&n.conn, SVC, get("/slow")),
    )
    .await;
    assert!(abandoned.is_err(), "the handler never answers");
    wait_until("the request to park", || parked.lock().unwrap().len() == 1).await;
    let (invocation, stream_id) = parked.lock().unwrap()[0];

    wait_until("cancel event", || {
        foreign
            .seen(UpcallKind::Cancel)
            .iter()
            .any(|e| e.invocation_id == invocation)
    })
    .await;
    let cancel = foreign
        .seen(UpcallKind::Cancel)
        .into_iter()
        .find(|e| e.invocation_id == invocation)
        .unwrap();
    assert_eq!(cancel.cancel_reason(), Some(CancelReason::Abandoned));
    // The body handle is gone too, and every body call on the cancelled
    // exchange says so with `Cancelled` (never `UnknownStream`); a late answer
    // is ignored.
    assert_eq!(ctx.bodies.open_count(), 0);
    assert_eq!(
        ctx.bodies.write(stream_id, b"late".to_vec()).await,
        Err(BodyError::Cancelled)
    );
    assert_eq!(
        ctx.bodies.try_write(stream_id, b"late".to_vec()),
        Err(BodyError::Cancelled)
    );
    assert_eq!(
        ctx.bodies.read(stream_id, 16).await,
        Err(BodyError::Cancelled)
    );
    assert_eq!(
        ctx.bodies.finish(stream_id, Vec::new()),
        Err(BodyError::Cancelled)
    );
    // The binding has now been told; the id is retired.
    assert_eq!(
        ctx.bodies.abort(stream_id, "late"),
        Err(BodyError::UnknownStream(stream_id))
    );
    // An id that never existed is still unknown.
    assert_eq!(
        ctx.bodies.write(9_999_999, b"x".to_vec()).await,
        Err(BodyError::UnknownStream(9_999_999))
    );
    assert!(ctx
        .upcalls
        .respond(invocation, UpcallStatus::Ok, Vec::new())
        .is_err());
    n.shutdown().await;
}

/// After the head, the consumer vanishes and the relay tears the exchange
/// down. A write that arrives only afterwards (the handler was idle) gets
/// `Cancelled`, not `UnknownStream`; so do reads and a finish. A handler that
/// already finished its response keeps getting `UnknownStream`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_after_peer_gone_teardown_is_cancelled() {
    let (queue, ctx) = setup();
    let parked: Arc<Mutex<Vec<u64>>> = Arc::default();
    let p = parked.clone();
    let _foreign = Foreign::start(
        queue,
        ctx.clone(),
        true,
        Arc::new(move |ctx, id, req| {
            ctx.respond(id, 200, &[], HttpBodyMode::Stream);
            if req.uri.starts_with("/done") {
                // Completes normally: the entry goes away with the response.
                let _ = ctx.bodies.write_blocking(req.stream_id, vec![1; 8], WAIT);
                let _ = ctx.bodies.finish(req.stream_id, Vec::new());
            }
            // The streaming handler goes idle without writing again.
            p.lock().unwrap().push(req.stream_id);
        }),
    );
    let c = ctx.clone();
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
    })
    .await;

    // A response that finishes cleanly.
    let done = relay_request(&n.conn, SVC, get("/done")).await.unwrap();
    assert_eq!(done.body().len(), 8);
    wait_until("both handlers parked", || parked.lock().unwrap().len() == 1).await;
    let finished_id = parked.lock().unwrap()[0];
    wait_until("exchange closed", || ctx.bodies.open_count() == 0).await;
    assert_eq!(
        ctx.bodies.write(finished_id, b"x".to_vec()).await,
        Err(BodyError::UnknownStream(finished_id)),
        "a finished exchange stays unknown"
    );

    // A streaming response whose consumer then goes away.
    let req = http::Request::builder()
        .uri("/idle")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = tokio::time::timeout(WAIT, relay_request_streaming(&n.conn, SVC, req))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_until("idle handler parked", || parked.lock().unwrap().len() == 2).await;
    let stream_id = parked.lock().unwrap()[1];
    drop(resp);
    n.conn.close(0, b"bye").unwrap();
    // The relay notices the vanished peer when it next writes a body frame, so
    // probe writes (the first may still be accepted into the buffer) prompt
    // the teardown.
    for _ in 0..100 {
        if ctx.bodies.open_count() == 0 {
            break;
        }
        let _ = ctx.bodies.try_write(stream_id, b"probe".to_vec());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(ctx.bodies.open_count(), 0, "exchange never torn down");

    assert_eq!(
        ctx.bodies.write(stream_id, b"late".to_vec()).await,
        Err(BodyError::Cancelled)
    );
    assert_eq!(
        ctx.bodies.try_write(stream_id, b"late".to_vec()),
        Err(BodyError::Cancelled)
    );
    assert_eq!(
        ctx.bodies.read(stream_id, 16).await,
        Err(BodyError::Cancelled)
    );
    assert_eq!(
        ctx.bodies.finish(stream_id, Vec::new()),
        Err(BodyError::Cancelled)
    );
    assert_eq!(
        ctx.bodies.abort(stream_id, "late"),
        Err(BodyError::UnknownStream(stream_id))
    );
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_authorize_deny_refuses_before_any_http_upcall() {
    let (queue, ctx) = setup();
    let foreign = Foreign::start(
        queue,
        ctx.clone(),
        false,
        Arc::new(|ctx, id, _req| ctx.respond(id, 200, &[], HttpBodyMode::Inline(Vec::new()))),
    );
    let c = ctx.clone();
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
    })
    .await;
    let result = tokio::time::timeout(WAIT, relay_request(&n.conn, SVC, get("/")))
        .await
        .unwrap();
    assert!(
        result.is_err(),
        "expected admission rejection, got {result:?}"
    );
    let authz = foreign.seen(UpcallKind::ExposeAuthorize);
    assert_eq!(authz.len(), 1);
    let (service, peer, _) = codec::decode_expose_authorize_request(&authz[0].payload).unwrap();
    assert_eq!(service, SVC);
    assert_eq!(peer, n.client.id().to_string());
    assert!(foreign.seen(UpcallKind::ExposeHttp).is_empty());
    n.shutdown().await;
}

// ── in process: limits, trailers, route policy ─────────────────────────────

fn request(method: &str, len: Option<usize>, body: Vec<u8>) -> http::Request<Full<Bytes>> {
    let mut b = http::Request::builder().method(method).uri("/x");
    if let Some(len) = len {
        b = b.header("content-length", len.to_string());
    }
    b.body(Full::new(Bytes::from(body))).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_and_response_size_limits() {
    let (queue, ctx) = setup();
    let results: Arc<Mutex<Vec<Result<(), BodyError>>>> = Arc::default();
    let r = results.clone();
    let _foreign = Foreign::start(
        queue,
        ctx.clone(),
        true,
        Arc::new(move |ctx, id, req| {
            if req.uri == "/x" && req.method == "PUT" {
                // Streamed request over the cap: the read fails.
                let mut got = Ok(());
                loop {
                    match ctx.bodies.read_blocking(req.stream_id, 1024, WAIT) {
                        Ok(Some(_)) => continue,
                        Ok(None) => break,
                        Err(e) => {
                            got = Err(e);
                            break;
                        }
                    }
                }
                r.lock().unwrap().push(got);
                ctx.respond(id, 413, &[], HttpBodyMode::Inline(Vec::new()));
            } else {
                // Response over the cap: the write that crosses it fails.
                ctx.respond(id, 200, &[], HttpBodyMode::Stream);
                let bodies = &ctx.bodies;
                r.lock()
                    .unwrap()
                    .push(bodies.write_blocking(req.stream_id, vec![1; 600], WAIT));
                r.lock()
                    .unwrap()
                    .push(bodies.write_blocking(req.stream_id, vec![1; 600], WAIT));
                r.lock()
                    .unwrap()
                    .push(bodies.write_blocking(req.stream_id, vec![1; 10], WAIT));
            }
        }),
    );
    let handler = ForeignHttpHandler::new(
        ctx.upcalls.clone(),
        ctx.bodies.clone(),
        SVC,
        ForeignHttpOptions {
            inline_request_max: 16,
            max_request_body: Some(1000),
            max_response_body: Some(1000),
            max_chunk: 2048,
            response_buffer: 4,
        },
    );

    // Declared length over the cap: 413 without an upcall.
    let resp = handler
        .serve(request("POST", Some(5000), vec![0; 5000]))
        .await;
    assert_eq!(resp.status(), 413);

    // Unknown length, streamed past the cap.
    let resp = handler.serve(request("PUT", None, vec![0; 3000])).await;
    assert_eq!(resp.status(), 413);
    assert_eq!(results.lock().unwrap()[0], Err(BodyError::TooLarge(1000)));

    // Response: 600 ok, 1200 > 1000 aborts, and the body fails.
    let resp = handler.serve(request("GET", None, Vec::new())).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.into_body().collect().await.is_err());
    wait_until("the writes", || results.lock().unwrap().len() == 4).await;
    let r = results.lock().unwrap().clone();
    assert_eq!(r[1], Ok(()));
    assert_eq!(r[2], Err(BodyError::TooLarge(1000)));
    // After the abort the handle is closed (or finished).
    assert!(matches!(
        r[3],
        Err(BodyError::UnknownStream(_)) | Err(BodyError::Finished)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_response_carries_trailers_in_process() {
    let (queue, ctx) = setup();
    let _foreign = Foreign::start(
        queue,
        ctx.clone(),
        true,
        Arc::new(move |ctx, id, req| {
            // Echo the inline request body back as a stream, then trailers.
            let HttpBodyMode::Inline(body) = req.body else {
                panic!("expected inline")
            };
            ctx.respond(id, 200, &[], HttpBodyMode::Stream);
            assert_eq!(
                ctx.bodies
                    .write_blocking(req.stream_id, vec![0; 4096], WAIT),
                Err(BodyError::ChunkTooLarge {
                    got: 4096,
                    max: 1024
                })
            );
            ctx.bodies
                .write_blocking(req.stream_id, body, WAIT)
                .unwrap();
            ctx.bodies
                .finish(req.stream_id, vec![("grpc-status".into(), b"0".to_vec())])
                .unwrap();
        }),
    );
    let handler = ForeignHttpHandler::new(
        ctx.upcalls.clone(),
        ctx.bodies.clone(),
        SVC,
        ForeignHttpOptions {
            max_chunk: 1024,
            ..Default::default()
        },
    );
    // Through the `tower::Service` impl, as `tower_handler` / Salvo drive it,
    // with the relay's own request body type.
    let handler = tower_handler(handler);
    let req = http::Request::builder()
        .method("POST")
        .uri("/x")
        .header("content-length", "3")
        .body(RelayBody::new(Box::pin(&b"abc"[..])))
        .unwrap();
    let resp = handler(req).await.unwrap();
    let collected = resp.into_body().collect().await.unwrap();
    assert_eq!(collected.trailers().unwrap()["grpc-status"], "0");
    assert_eq!(&collected.to_bytes()[..], b"abc");
    wait_until("exchange closed", || ctx.bodies.open_count() == 0).await;
}

#[tokio::test]
async fn route_policy_grants_the_answered_subset() {
    let (queue, ctx) = setup();
    let upcalls = ctx.upcalls.clone();
    let answer: Arc<Mutex<Vec<u8>>> = Arc::default();
    let a = answer.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    type Seen = (String, Vec<u8>, Vec<ExposeRouteSpec>);
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let sn = seen.clone();
    let thread = std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            if queue.wait_blocking(Duration::from_millis(10)) {
                queue.drain(8, |ev| {
                    if ev.kind == UpcallKind::ExposeRoutePolicy {
                        sn.lock()
                            .unwrap()
                            .push(codec::decode_expose_route_policy_request(&ev.payload).unwrap());
                        let _ = upcalls.respond(
                            ev.invocation_id,
                            UpcallStatus::Ok,
                            a.lock().unwrap().clone(),
                        );
                    }
                });
            }
        }
    });
    let specs = vec![
        RouteSpec {
            host: "a.example".into(),
            port: 443,
            protocol: RouteProtocol::Http,
            service_id: "web".into(),
        },
        RouteSpec {
            host: String::new(),
            port: 5432,
            protocol: RouteProtocol::Tcp,
            service_id: "db".into(),
        },
    ];
    let peer = PeerContext {
        peer_id: "p1".into(),
        metadata: b"tok".to_vec(),
    };
    let policy = foreign_route_policy(ctx.upcalls.clone());

    *answer.lock().unwrap() = codec::encode_expose_route_grant(&[1]);
    let granted = policy(peer.clone(), specs.clone()).await.unwrap();
    assert_eq!(granted, vec![specs[1].clone()]);
    let (p, md, wire) = seen.lock().unwrap()[0].clone();
    assert_eq!((p.as_str(), md.as_slice()), ("p1", &b"tok"[..]));
    assert_eq!(wire[1].protocol, 1);
    assert_eq!(wire[0].port, 443);

    // An index out of range is malformed: fail-closed grants nothing...
    *answer.lock().unwrap() = codec::encode_expose_route_grant(&[0, 2]);
    assert!(policy(peer.clone(), specs.clone())
        .await
        .unwrap()
        .is_empty());
    // ...fail-open grants everything.
    ctx.upcalls.set_policy(
        UpcallKind::ExposeRoutePolicy,
        UpcallPolicy::new(Duration::from_secs(5), HookFailureMode::FailOpen),
    );
    assert_eq!(policy(peer, specs.clone()).await.unwrap(), specs);

    stop.store(true, Ordering::Relaxed);
    thread.join().unwrap();
}
