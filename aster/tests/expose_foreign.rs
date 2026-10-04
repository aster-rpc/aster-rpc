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

use aster::binding_support::relay::{
    relay_streaming, RelayOptions, RelayReader, ResponseError, TryRead,
};
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

// ── consumer-side streaming relay (binding_support::relay) ─────────────────

/// What the foreign handler saw, per request: `start <uri>`, then the
/// request-body outcome and (for `/forever`) how its writes ended.
type Log = Arc<Mutex<Vec<String>>>;

fn log_has(log: &Log, line: &str) -> bool {
    log.lock().unwrap().iter().any(|l| l == line)
}

/// `/echo` (`/hold`: after a pause): read the whole request body, then
/// stream it back in 7000-byte writes. `/abort`: one chunk, then abort.
/// `/forever`: write until a write fails.
fn streaming_handler(log: Log) -> HttpFn {
    Arc::new(move |ctx, id, req| {
        log.lock().unwrap().push(format!("start {}", req.uri));
        let sid = req.stream_id;
        match req.uri.as_str() {
            "/echo" | "/hold" => {
                if req.uri == "/hold" {
                    // Let the upload back up before reading any of it.
                    std::thread::sleep(Duration::from_millis(500));
                }
                let mut body = Vec::new();
                loop {
                    match ctx.bodies.read_blocking(sid, 64 * 1024, WAIT) {
                        Ok(Some(c)) => body.extend_from_slice(&c),
                        Ok(None) => break,
                        Err(e) => {
                            // Sticky: a retry fails too, never a clean end.
                            let again = ctx.bodies.read_blocking(sid, 64 * 1024, WAIT);
                            let again = again.map_err(|e| e.code());
                            log.lock()
                                .unwrap()
                                .push(format!("read {} {again:?}", e.code()));
                            return;
                        }
                    }
                }
                log.lock().unwrap().push(format!("read ok {}", body.len()));
                ctx.respond(
                    id,
                    200,
                    &[("x-len", &body.len().to_string())],
                    HttpBodyMode::Stream,
                );
                for piece in body.chunks(7000) {
                    if ctx
                        .bodies
                        .write_blocking(sid, piece.to_vec(), WAIT)
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = ctx.bodies.finish(sid, Vec::new());
            }
            "/gate" => {
                // Read nothing until the test logs "open", then count the
                // whole body and answer with its length.
                while !log_has(&log, "open") {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let mut n = 0;
                loop {
                    match ctx.bodies.read_blocking(sid, 1 << 20, WAIT) {
                        Ok(Some(c)) => n += c.len(),
                        Ok(None) => break,
                        Err(e) => {
                            log.lock().unwrap().push(format!("read {} Err", e.code()));
                            return;
                        }
                    }
                }
                ctx.respond(
                    id,
                    200,
                    &[("x-len", &n.to_string())],
                    HttpBodyMode::Inline(Vec::new()),
                );
            }
            "/early" => {
                // Answer at once without reading the body.
                ctx.respond(id, 413, &[], HttpBodyMode::Inline(b"too big".to_vec()));
            }
            "/duplex" => {
                // Head first, then echo each request chunk as it arrives.
                ctx.respond(id, 200, &[], HttpBodyMode::Stream);
                loop {
                    match ctx.bodies.read_blocking(sid, 64 * 1024, WAIT) {
                        Ok(Some(c)) => {
                            if ctx.bodies.write_blocking(sid, c, WAIT).is_err() {
                                log.lock().unwrap().push("duplex write failed".into());
                                return;
                            }
                        }
                        Ok(None) => {
                            let _ = ctx.bodies.finish(sid, Vec::new());
                            log.lock().unwrap().push("duplex done".into());
                            return;
                        }
                        Err(e) => {
                            let again = ctx.bodies.read_blocking(sid, 64 * 1024, WAIT);
                            let again = again.map_err(|e| e.code());
                            log.lock()
                                .unwrap()
                                .push(format!("read {} {again:?}", e.code()));
                            let _ = ctx.bodies.abort(sid, "request failed");
                            return;
                        }
                    }
                }
            }
            "/abort" => {
                ctx.respond(id, 200, &[], HttpBodyMode::Stream);
                let _ = ctx.bodies.write_blocking(sid, vec![7; 600], WAIT);
                std::thread::sleep(Duration::from_millis(50));
                let _ = ctx.bodies.abort(sid, "handler failed");
            }
            "/forever" => {
                ctx.respond(id, 200, &[], HttpBodyMode::Stream);
                loop {
                    if let Err(e) = ctx.bodies.write_blocking(sid, vec![1; 16 * 1024], WAIT) {
                        log.lock().unwrap().push(format!("write {}", e.code()));
                        return;
                    }
                }
            }
            _ => ctx.respond(id, 404, &[], HttpBodyMode::Inline(Vec::new())),
        }
    })
}

async fn streaming_net(log: &Log) -> (Foreign, Ctx, Net) {
    let (queue, ctx) = setup();
    let foreign = Foreign::start(queue, ctx.clone(), true, streaming_handler(log.clone()));
    let c = ctx.clone();
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
    })
    .await;
    (foreign, ctx, n)
}

fn rt() -> tokio::runtime::Handle {
    tokio::runtime::Handle::current()
}

async fn read_all(reader: &RelayReader, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(c) = tokio::time::timeout(WAIT, reader.read(max))
        .await
        .unwrap()
        .unwrap()
    {
        assert!(!c.is_empty() && c.len() <= max);
        out.extend_from_slice(&c);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_streaming_uploads_and_reads_in_bounded_chunks() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    let options = RelayOptions {
        max_chunk: 64 * 1024,
        upload_capacity: 2,
    };
    let data: Vec<u8> = (0..3 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        SVC,
        "POST",
        "/echo",
        &[("x-a".into(), b"1".to_vec())],
        options,
    )
    .unwrap();
    let response = tokio::spawn(response.wait());
    // One big write is split into max_chunk pieces under backpressure.
    tokio::time::timeout(WAIT, upload.write(Bytes::from(data.clone())))
        .await
        .unwrap()
        .unwrap();
    upload.finish().unwrap();
    assert_eq!(upload.finish(), Err(BodyError::Finished));
    assert_eq!(
        upload.write(Bytes::from_static(b"x")).await,
        Err(BodyError::Finished)
    );

    let (head, reader) = tokio::time::timeout(WAIT, response)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(head.status, 200);
    assert!(head
        .headers
        .contains(&("x-len".into(), data.len().to_string().into_bytes())));
    assert_eq!(read_all(&reader, 10_000).await, data);
    // The end is sticky and clean.
    assert_eq!(reader.read(10).await, Ok(None));
    assert!(log_has(&log, &format!("read ok {}", data.len())));

    // An empty upload.
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        SVC,
        "POST",
        "/echo",
        &[],
        RelayOptions::default(),
    )
    .unwrap();
    upload.finish().unwrap();
    let (head, reader) = tokio::time::timeout(WAIT, response.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.status, 200);
    assert_eq!(reader.read(100).await, Ok(None));

    // A bad request never reaches the wire.
    assert!(matches!(
        relay_streaming(
            &rt(),
            &n.conn,
            SVC,
            "BAD METHOD",
            "/",
            &[],
            RelayOptions::default()
        ),
        Err(aster::Error::InvalidArgument(_))
    ));
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

/// An upload that ends badly -- aborted, every handle dropped without
/// finish, or the response future dropped mid-upload -- reaches the handler
/// as a failed request body, never a clean (truncated) one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_streaming_failed_uploads_fail_the_request_body() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    // "abort-and-cancel" fails the upload and cancels the exchange at once
    // (as a binding's cancelled call does): the relay is torn down while
    // it handles the failed upload, and the reset must survive that.
    let cases = ["abort", "drop", "drop-future"]
        .into_iter()
        .chain(std::iter::repeat_n("abort-and-cancel", 20));
    for case in cases {
        log.lock().unwrap().clear();
        let (upload, response) = relay_streaming(
            &rt(),
            &n.conn,
            SVC,
            "PUT",
            "/echo",
            &[],
            RelayOptions::default(),
        )
        .unwrap();
        // The exchange runs without anyone waiting on it.
        upload.write(Bytes::from(vec![5; 50_000])).await.unwrap();
        wait_until("handler started", || log_has(&log, "start /echo")).await;
        match case {
            "abort" => {
                upload.abort("client gave up").unwrap();
                assert_eq!(upload.abort("again"), Err(BodyError::Finished));
                let res = tokio::time::timeout(WAIT, response.wait()).await.unwrap();
                assert!(res.is_err(), "{case}: response after an aborted upload");
            }
            "drop" => {
                drop(upload);
                let res = tokio::time::timeout(WAIT, response.wait()).await.unwrap();
                assert!(res.is_err(), "{case}: response after a dropped upload");
            }
            "abort-and-cancel" => {
                upload.abort("caller cancelled").unwrap();
                response.cancel();
                drop(response);
            }
            _ => {
                // Never waited on, then dropped: the exchange is cancelled.
                drop(response);
                // The request side is gone: writes now fail Cancelled.
                let mut res = Ok(());
                for _ in 0..200 {
                    res = upload.write(Bytes::from_static(b"more")).await;
                    if res.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(res, Err(BodyError::Cancelled), "{case}");
                assert!(upload.is_closed());
            }
        }
        wait_until(&format!("{case}: handler saw a failed body"), || {
            let l = log.lock().unwrap();
            l.iter()
                .any(|x| x.starts_with("read ") && !x.starts_with("read ok") && x.contains("Err"))
        })
        .await;
    }
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_streaming_aborted_response_fails_every_read() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        SVC,
        "GET",
        "/abort",
        &[],
        RelayOptions::default(),
    )
    .unwrap();
    upload.finish().unwrap();
    let (head, reader) = tokio::time::timeout(WAIT, response.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.status, 200);
    let mut got = 0;
    let err = loop {
        match tokio::time::timeout(WAIT, reader.read(1 << 20))
            .await
            .unwrap()
        {
            Ok(Some(c)) => got += c.len(),
            Ok(None) => panic!("aborted response ended cleanly after {got} bytes"),
            Err(e) => break e,
        }
    };
    assert_eq!(err.code(), 7, "{err:?}");
    // Sticky: a retry never turns the failure into a clean end.
    assert_eq!(reader.read(10).await.unwrap_err().code(), 7);
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_streaming_close_stops_the_handler() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        SVC,
        "GET",
        "/forever",
        &[],
        RelayOptions::default(),
    )
    .unwrap();
    upload.finish().unwrap();
    let (_, reader) = tokio::time::timeout(WAIT, response.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(reader.read(1000).await.unwrap().is_some());

    // A read parked on the wire is woken by close.
    let parked = {
        let r = reader.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = r.read(1 << 20).await {
                    return e;
                }
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    reader.close();
    assert!(reader.is_closed());
    let e = tokio::time::timeout(WAIT, parked).await.unwrap().unwrap();
    assert_eq!(e, BodyError::Cancelled);
    assert_eq!(reader.read(10).await, Err(BodyError::Cancelled));
    wait_until("handler write cancelled", || {
        log_has(&log, &format!("write {}", BodyError::Cancelled.code()))
    })
    .await;
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_streaming_dropped_reader_stops_the_handler() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        SVC,
        "GET",
        "/forever",
        &[],
        RelayOptions::default(),
    )
    .unwrap();
    upload.finish().unwrap();
    let (_, reader) = tokio::time::timeout(WAIT, response.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(reader.read(1000).await.unwrap().is_some());
    drop(reader);
    wait_until("handler write cancelled", || {
        log_has(&log, &format!("write {}", BodyError::Cancelled.code()))
    })
    .await;
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_streaming_try_write_and_finish_barrier() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    let options = RelayOptions {
        max_chunk: 1000,
        upload_capacity: 2,
    };
    let (upload, response) =
        relay_streaming(&rt(), &n.conn, SVC, "PUT", "/hold", &[], options).unwrap();
    assert_eq!(
        upload.try_write(Bytes::from(vec![0; 1001])),
        Err(BodyError::ChunkTooLarge {
            got: 1001,
            max: 1000
        })
    );
    // try_write never waits: once the buffer is full it says so.
    let (mut queued, mut saw_full) = (0, false);
    for _ in 0..10_000 {
        match upload.try_write(Bytes::from(vec![1; 1000])).unwrap() {
            true => queued += 1000,
            false => {
                saw_full = true;
                break;
            }
        }
    }
    assert!(queued > 0 && saw_full, "queued {queued}, full {saw_full}");

    // A split write blocked on backpressure stops at finish(): the pieces
    // it had not queued yet are dropped and it reports Finished.
    let writer = {
        let up = upload.clone();
        tokio::spawn(async move { up.write(Bytes::from(vec![2; 32 << 20])).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    upload.finish().unwrap();
    assert_eq!(
        tokio::time::timeout(WAIT, writer).await.unwrap().unwrap(),
        Err(BodyError::Finished)
    );
    let (head, reader) = tokio::time::timeout(WAIT, response.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.status, 200);
    let got = read_all(&reader, 1 << 20).await.len();
    assert!(
        got >= queued && got < queued + (32 << 20),
        "{got} vs {queued}"
    );
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_streaming_unknown_service_fails_the_response() {
    let log: Log = Arc::default();
    let (_f, _ctx, n) = streaming_net(&log).await;
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        "nope",
        "POST",
        "/",
        &[],
        RelayOptions::default(),
    )
    .unwrap();
    let _ = upload.write(Bytes::from_static(b"x")).await;
    let _ = upload.finish();
    assert!(tokio::time::timeout(WAIT, response.wait())
        .await
        .unwrap()
        .is_err());
    n.shutdown().await;
}

// ── full duplex ────────────────────────────────────────────────────────────

/// A handler that answers before reading the body: the consumer gets that
/// answer (it used to stall the upload and lose it), and the upload then
/// fails `Cancelled` once the handler side is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_early_response_arrives_during_the_upload() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;

    // Through the plain Rust API: a body far bigger than the flow-control
    // windows, which never gets read.
    let (tx, body) = body_channel(4);
    let req = http::Request::builder()
        .method("PUT")
        .uri("/early")
        .body(body)
        .unwrap();
    let feeder = tokio::spawn(async move {
        for _ in 0..1024 {
            if tx.send(Bytes::from(vec![9; 64 * 1024])).await.is_err() {
                return false;
            }
        }
        true
    });
    let resp = tokio::time::timeout(WAIT, relay_request_streaming(&n.conn, SVC, req))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 413);
    let body = tokio::time::timeout(WAIT, resp.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert_eq!(&body[..], b"too big");
    // The 64 MiB upload never completed: the request side went away.
    assert!(!tokio::time::timeout(WAIT, feeder).await.unwrap().unwrap());

    // Through the binding handles.
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        SVC,
        "PUT",
        "/early",
        &[],
        RelayOptions::default(),
    )
    .unwrap();
    let writer = {
        let up = upload.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = up.write(Bytes::from(vec![9; 64 * 1024])).await {
                    return e;
                }
            }
        })
    };
    let (head, reader) = tokio::time::timeout(WAIT, response.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.status, 413);
    assert_eq!(read_all(&reader, 1 << 20).await, b"too big");
    drop(reader);
    assert_eq!(
        tokio::time::timeout(WAIT, writer).await.unwrap().unwrap(),
        BodyError::Cancelled
    );
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

/// Request and response interleave: each echo is read back before the next
/// chunk is written, which only works if the head and body flow while the
/// upload is still open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_streams_both_ways_at_once() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        SVC,
        "POST",
        "/duplex",
        &[],
        RelayOptions::default(),
    )
    .unwrap();
    let (head, reader) = tokio::time::timeout(WAIT, response.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.status, 200);
    assert!(
        !upload.is_closed(),
        "the head arrived before the upload ended"
    );
    for i in 0..20u32 {
        let ping = format!("ping {i}");
        upload.write(Bytes::from(ping.clone())).await.unwrap();
        let mut echo = Vec::new();
        while echo.len() < ping.len() {
            let c = tokio::time::timeout(WAIT, reader.read(1 << 20))
                .await
                .unwrap()
                .unwrap()
                .expect("echo before end");
            echo.extend_from_slice(&c);
        }
        assert_eq!(echo, ping.as_bytes());
    }
    upload.finish().unwrap();
    assert_eq!(
        tokio::time::timeout(WAIT, reader.read(10)).await.unwrap(),
        Ok(None)
    );
    wait_until("handler done", || log_has(&log, "duplex done")).await;
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

/// After the head, an upload that is aborted -- or abandoned by dropping
/// the response -- still resets the request: the handler's read fails
/// (stickily), never ends clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_upload_failing_after_the_head_resets_the_request() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    for case in ["abort", "drop-response", "drop-upload"] {
        log.lock().unwrap().clear();
        let (upload, response) = relay_streaming(
            &rt(),
            &n.conn,
            SVC,
            "POST",
            "/duplex",
            &[],
            RelayOptions::default(),
        )
        .unwrap();
        let (_, reader) = tokio::time::timeout(WAIT, response.wait())
            .await
            .unwrap()
            .unwrap();
        upload.write(Bytes::from_static(b"one")).await.unwrap();
        assert_eq!(read_all_n(&reader, 3).await, b"one");
        match case {
            "abort" => upload.abort("client failed").unwrap(),
            "drop-response" => drop(reader.clone()), // a clone: still alive
            _ => drop(upload.clone()),
        }
        match case {
            "abort" => {
                // The handler aborts its response in turn: a failed read,
                // never a clean end.
                let e = loop {
                    match tokio::time::timeout(WAIT, reader.read(1 << 20))
                        .await
                        .unwrap()
                    {
                        Ok(Some(_)) => continue,
                        Ok(None) => panic!("{case}: response ended cleanly"),
                        Err(e) => break e,
                    }
                };
                assert_eq!(e.code(), 7, "{case}");
            }
            "drop-response" => {
                drop(reader);
                let mut res = Ok(());
                for _ in 0..500 {
                    res = upload.write(Bytes::from_static(b"more")).await;
                    if res.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(res, Err(BodyError::Cancelled), "{case}");
            }
            _ => {
                drop(upload);
                drop(reader);
            }
        }
        wait_until(&format!("{case}: handler saw a failed body"), || {
            let l = log.lock().unwrap();
            l.iter()
                .any(|x| x.starts_with("read ") && x.contains("Err"))
                || (case != "abort" && l.iter().any(|x| x == "duplex write failed"))
        })
        .await;
        assert!(
            !log_has(&log, "duplex done"),
            "{case}: handler saw a clean end"
        );
        wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    }
    n.shutdown().await;
}

async fn read_all_n(reader: &RelayReader, n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while out.len() < n {
        let c = tokio::time::timeout(WAIT, reader.read(1 << 20))
            .await
            .unwrap()
            .unwrap()
            .expect("data before end");
        out.extend_from_slice(&c);
    }
    out
}

/// A native handler that answers `202` at once and reads the request body
/// to its end in the background, reporting the byte count (or the error).
fn accept_then_read(
    seen: tokio::sync::mpsc::UnboundedSender<Result<usize, String>>,
) -> aster::expose::HttpHandler {
    Arc::new(move |req: http::Request<RelayBody>| {
        let seen = seen.clone();
        Box::pin(async move {
            tokio::spawn(async move {
                let got = req.into_body().collect().await;
                let _ = seen.send(got.map(|c| c.to_bytes().len()).map_err(|e| e.to_string()));
            });
            let body = Full::new(Bytes::from_static(b"accepted"))
                .map_err(|e: std::convert::Infallible| -> aster::expose::RelayError { match e {} })
                .boxed_unsync();
            Ok(http::Response::builder().status(202).body(body).unwrap())
        })
            as aster::expose::BoxFut<anyhow::Result<http::Response<aster::expose::ResponseBody>>>
    })
}

/// A response that ends cleanly does not cancel the request body: the
/// upload still runs to its end, and the handler reads all of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_completed_response_leaves_the_upload_to_finish() {
    let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let allow: aster::expose::AuthorizeFn = Arc::new(|_ctx: PeerContext| {
        Box::pin(async { Ok(()) }) as aster::expose::BoxFut<anyhow::Result<()>>
    });
    let n = net(move |e| e.expose_http("bg", allow, accept_then_read(seen_tx))).await;
    const BIG: usize = 8 << 20; // well past the flow-control windows

    // Buffered: the whole response is collected while the upload is still
    // in flight.
    let req = http::Request::builder()
        .method("PUT")
        .uri("/")
        .body(vec![3u8; BIG])
        .unwrap();
    let resp = tokio::time::timeout(WAIT, relay_request(&n.conn, "bg", req))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), &resp.body()[..]),
        (202, &b"accepted"[..])
    );
    let got = tokio::time::timeout(WAIT, seen.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, Ok(BIG));

    // Binding handles: read the response to its end, then upload the rest.
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        "bg",
        "PUT",
        "/",
        &[],
        RelayOptions::default(),
    )
    .unwrap();
    upload.write(Bytes::from_static(b"first")).await.unwrap();
    let (head, reader) = tokio::time::timeout(WAIT, response.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.status, 202);
    assert_eq!(read_all(&reader, 1 << 20).await, b"accepted");
    drop(reader);
    tokio::time::timeout(WAIT, upload.write(Bytes::from(vec![4u8; BIG])))
        .await
        .unwrap()
        .unwrap();
    upload.finish().unwrap();
    let got = tokio::time::timeout(WAIT, seen.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, Ok(BIG + 5));

    // Closing the response before its end still abandons the upload.
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        "bg",
        "PUT",
        "/",
        &[],
        RelayOptions::default(),
    )
    .unwrap();
    upload.write(Bytes::from_static(b"first")).await.unwrap();
    let (_, reader) = tokio::time::timeout(WAIT, response.wait())
        .await
        .unwrap()
        .unwrap();
    reader.close();
    let got = tokio::time::timeout(WAIT, seen.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(got.is_err(), "abandoned upload ended clean: {got:?}");
    n.shutdown().await;
}

/// Cancelling while a write is parked on flow control (the handler is not
/// reading yet) still resets the request: the handler's read fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_cancel_with_a_parked_write_resets_the_request() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    let (upload, response) = relay_streaming(
        &rt(),
        &n.conn,
        SVC,
        "PUT",
        "/hold",
        &[],
        RelayOptions::default(),
    )
    .unwrap();
    let writer = {
        let up = upload.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = up.write(Bytes::from(vec![6; 64 * 1024])).await {
                    return e;
                }
            }
        })
    };
    wait_until("handler started", || log_has(&log, "start /hold")).await;
    tokio::time::sleep(Duration::from_millis(100)).await; // parked by now
    response.cancel();
    drop(response);
    let e = tokio::time::timeout(WAIT, writer).await.unwrap().unwrap();
    assert_eq!(e, BodyError::Cancelled);
    wait_until("handler saw a failed body", || {
        log.lock()
            .unwrap()
            .iter()
            .any(|x| x.starts_with("read ") && !x.starts_with("read ok"))
    })
    .await;
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

// ── blocking / polling handles and stable codes (C ABI 4.3) ───────────────

use aster::binding_support::relay::{RelayResponse, RelayUpload};

/// Run `f` on a plain OS thread (not a runtime thread, where the blocking
/// calls refuse to run) and await its result.
async fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    tokio::time::timeout(WAIT * 3, rx).await.unwrap().unwrap()
}

fn start(
    n: &Net,
    svc: &str,
    method: &str,
    uri: &str,
    options: RelayOptions,
) -> (RelayUpload, RelayResponse) {
    relay_streaming(&rt(), &n.conn, svc, method, uri, &[], options).unwrap()
}

/// Refusals, connection loss and cancellation reach the consumer as
/// distinct stable codes (not message text), sticky across calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_failures_have_stable_codes() {
    let log: Log = Arc::default();
    let (queue, ctx) = setup();
    let _f = Foreign::start(queue, ctx.clone(), true, streaming_handler(log.clone()));
    let c = ctx.clone();
    let deny: aster::expose::AuthorizeFn = Arc::new(|_ctx: PeerContext| {
        Box::pin(async { Err(anyhow::anyhow!("not you")) })
            as aster::expose::BoxFut<anyhow::Result<()>>
    });
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        e.expose_http("locked", deny, accept_then_read(tx));
    })
    .await;

    for (svc, want) in [("nope", 10), ("locked", 11)] {
        let (upload, response) = start(&n, svc, "PUT", "/", RelayOptions::default());
        let _ = upload.write(Bytes::from_static(b"x")).await;
        let _ = upload.finish();
        let r = response.clone();
        let err = on_thread(move || r.wait_blocking(WAIT).unwrap_err()).await;
        assert_eq!(err.code(), want, "{svc}: {err}");
        match want {
            10 => assert!(
                matches!(err, ResponseError::UnknownService(ref m) if m.contains("not exposed")),
                "{err}"
            ),
            _ => assert!(
                matches!(err, ResponseError::Denied(ref m) if m.contains("denied")),
                "{err}"
            ),
        }
        // Sticky, and the same through the non-blocking and async calls.
        assert_eq!(response.try_wait().unwrap().unwrap_err(), err);
        let e = response.wait().await.unwrap_err();
        assert!(matches!(e, aster::Error::Transport(_)), "{e:?}");
    }

    // Cancelled before the head.
    let (_upload, response) = start(&n, SVC, "PUT", "/hold", RelayOptions::default());
    response.cancel();
    let err = response.try_wait().unwrap().unwrap_err();
    assert_eq!((err.code(), err), (2, ResponseError::Cancelled));
    assert!(matches!(
        response.wait().await,
        Err(aster::Error::Connection(_))
    ));

    // The connection goes away while the head is awaited, then a new
    // exchange on the closed connection.
    let (_upload, response) = start(&n, SVC, "PUT", "/hold", RelayOptions::default());
    let r = response.clone();
    let waiting = tokio::spawn(on_thread(move || r.wait_blocking(WAIT)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    n.conn.close(0, b"bye".to_vec()).unwrap();
    let err = waiting.await.unwrap().unwrap_err();
    assert_eq!(err.code(), 15, "{err}");
    let (_upload, response) = start(&n, SVC, "GET", "/", RelayOptions::default());
    let r = response.clone();
    let err = on_thread(move || r.wait_blocking(WAIT).unwrap_err()).await;
    assert_eq!(err.code(), 15, "{err}");
    assert!(matches!(
        response.wait().await,
        Err(aster::Error::Connection(_))
    ));
    n.shutdown().await;
}

/// The blocking calls on a binding thread, full duplex: an early answer
/// while uploading, an abort before the head, closing the response before
/// its end, and a clean end that leaves the upload to finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_blocking_calls_keep_the_full_duplex_contract() {
    let log: Log = Arc::default();
    let (queue, ctx) = setup();
    let _f = Foreign::start(queue, ctx.clone(), true, streaming_handler(log.clone()));
    let c = ctx.clone();
    let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let allow: aster::expose::AuthorizeFn = Arc::new(|_ctx: PeerContext| {
        Box::pin(async { Ok(()) }) as aster::expose::BoxFut<anyhow::Result<()>>
    });
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
        e.expose_http("bg", allow, accept_then_read(seen_tx));
    })
    .await;

    // 1. Early 413 while the upload is still being written.
    let (upload, response) = start(&n, SVC, "PUT", "/early", RelayOptions::default());
    let writer = {
        let up = upload.clone();
        tokio::spawn(on_thread(move || loop {
            if let Err(e) = up.write_blocking(Bytes::from(vec![9; 64 * 1024]), WAIT) {
                return e;
            }
        }))
    };
    let body = on_thread(move || {
        let (head, reader) = response.wait_blocking(WAIT).unwrap();
        let mut body = Vec::new();
        while let Some(c) = reader.read_blocking(1 << 20, WAIT).unwrap() {
            body.extend_from_slice(&c);
        }
        // The end is sticky and clean.
        assert_eq!(reader.read_blocking(10, WAIT), Ok(None));
        (head.status, body)
        // `response` (and with it the reader) drops here.
    })
    .await;
    assert_eq!(body, (413, b"too big".to_vec()));
    assert_eq!(writer.await.unwrap(), BodyError::Cancelled);
    drop(upload);

    // 2. Abort before the head: the call fails with UploadFailed and the
    // handler sees a failed body.
    log.lock().unwrap().clear();
    let (upload, response) = start(&n, SVC, "PUT", "/echo", RelayOptions::default());
    let up = upload.clone();
    on_thread(move || up.write_blocking(Bytes::from(vec![5; 50_000]), WAIT))
        .await
        .unwrap();
    // (A reset that beats the request head to the server would skip the
    // handler altogether.)
    wait_until("handler started", || log_has(&log, "start /echo")).await;
    let err = on_thread(move || {
        upload.abort("client gave up").unwrap();
        assert_eq!(upload.abort("again"), Err(BodyError::Finished));
        assert_eq!(
            upload.write_blocking(Bytes::from_static(b"x"), WAIT),
            Err(BodyError::Finished)
        );
        response.wait_blocking(WAIT).unwrap_err()
    })
    .await;
    assert_eq!(err.code(), 16, "{err}");
    wait_until("handler saw a failed body", || {
        log.lock()
            .unwrap()
            .iter()
            .any(|x| x.starts_with("read ") && !x.starts_with("read ok"))
    })
    .await;

    // 3. Closing the response before its end resets the unfinished upload.
    log.lock().unwrap().clear();
    let (upload, response) = start(&n, SVC, "POST", "/duplex", RelayOptions::default());
    let res = on_thread(move || {
        let (_, reader) = response.wait_blocking(WAIT).unwrap();
        upload
            .write_blocking(Bytes::from_static(b"one"), WAIT)
            .unwrap();
        let echo = reader.read_blocking(1 << 20, WAIT).unwrap().unwrap();
        response.close();
        assert_eq!(reader.read_blocking(10, WAIT), Err(BodyError::Cancelled));
        let mut res = Ok(());
        for _ in 0..500 {
            res = upload.write_blocking(Bytes::from_static(b"more"), WAIT);
            if res.is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        (echo, res)
    })
    .await;
    assert_eq!(res, (Bytes::from_static(b"one"), Err(BodyError::Cancelled)));
    wait_until("handler saw the reset", || {
        let l = log.lock().unwrap();
        l.iter()
            .any(|x| x.starts_with("read ") && x.contains("Err"))
            || l.iter().any(|x| x == "duplex write failed")
    })
    .await;
    assert!(!log_has(&log, "duplex done"));

    // 4. A response read to its clean end leaves the upload to finish.
    const BIG: usize = 4 << 20;
    let (upload, response) = start(&n, "bg", "PUT", "/", RelayOptions::default());
    let status = on_thread(move || {
        upload
            .write_blocking(Bytes::from_static(b"first"), WAIT)
            .unwrap();
        let (head, reader) = response.wait_blocking(WAIT).unwrap();
        let mut body = Vec::new();
        while let Some(c) = reader.read_blocking(1 << 20, WAIT).unwrap() {
            body.extend_from_slice(&c);
        }
        assert_eq!(body, b"accepted");
        drop(reader);
        drop(response);
        for piece in vec![4u8; BIG].chunks(upload.max_chunk()) {
            upload
                .write_blocking(Bytes::copy_from_slice(piece), WAIT)
                .unwrap();
        }
        upload.finish().unwrap();
        head.status
    })
    .await;
    assert_eq!(status, 202);
    let got = tokio::time::timeout(WAIT, seen.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, Ok(BIG + 5));
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

/// Timed-out blocking calls consume / queue nothing and leave the exchange
/// running; blocking calls on a runtime thread are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_blocking_timeouts_consume_nothing() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;

    // Runtime threads are refused, without side effects.
    let (upload, response) = start(&n, SVC, "PUT", "/gate", RelayOptions::default());
    assert_eq!(
        response.wait_blocking(WAIT).unwrap_err(),
        ResponseError::BlockingOnRuntime
    );
    assert_eq!(
        upload.write_blocking(Bytes::from_static(b"x"), WAIT),
        Err(BodyError::BlockingOnRuntime)
    );
    assert_eq!(
        upload.write_blocking(Bytes::from(vec![0; (1 << 20) + 1]), WAIT),
        Err(BodyError::ChunkTooLarge {
            got: (1 << 20) + 1,
            max: 1 << 20
        })
    );
    drop((upload, response));

    // Fill every buffer while the handler reads nothing: writes time out
    // eventually, and a timed-out write queued nothing.
    let options = RelayOptions {
        max_chunk: 64 * 1024,
        upload_capacity: 1,
    };
    let (upload, response) = start(&n, SVC, "PUT", "/gate", options);
    let (up, resp) = (upload.clone(), response.clone());
    let (written, timeouts) = on_thread(move || {
        assert_eq!(
            resp.wait_blocking(Duration::from_millis(50)).unwrap_err(),
            ResponseError::Timeout
        );
        let (mut written, mut timeouts) = (0usize, 0);
        while timeouts < 3 {
            match up.write_blocking(Bytes::from(vec![1; 64 * 1024]), Duration::from_millis(100)) {
                Ok(()) => written += 64 * 1024,
                Err(BodyError::Timeout) => timeouts += 1,
                Err(e) => panic!("write failed: {e}"),
            }
        }
        (written, timeouts)
    })
    .await;
    assert!(written > 0 && timeouts == 3, "{written} {timeouts}");
    log.lock().unwrap().push("open".into());
    let head = on_thread(move || {
        upload
            .write_blocking(Bytes::from_static(b"tail"), WAIT)
            .unwrap();
        upload.finish().unwrap();
        response.wait_blocking(WAIT).unwrap().0
    })
    .await;
    assert_eq!(head.status, 200);
    let len = (written + 4).to_string().into_bytes();
    assert!(
        head.headers.contains(&("x-len".into(), len)),
        "{head:?} vs {written}"
    );

    // A timed-out read consumed nothing.
    let (upload, response) = start(&n, SVC, "POST", "/duplex", RelayOptions::default());
    let got = on_thread(move || {
        let (_, reader) = response.wait_blocking(WAIT).unwrap();
        assert_eq!(
            reader.read_blocking(100, Duration::from_millis(50)),
            Err(BodyError::Timeout)
        );
        upload
            .write_blocking(Bytes::from_static(b"abcdef"), WAIT)
            .unwrap();
        let mut got = Vec::new();
        while got.len() < 6 {
            got.extend_from_slice(&reader.read_blocking(4, WAIT).unwrap().unwrap());
        }
        upload.finish().unwrap();
        assert_eq!(reader.read_blocking(4, WAIT), Ok(None));
        got
    })
    .await;
    assert_eq!(got, b"abcdef");
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

/// The non-blocking calls plus readiness futures drive a whole exchange,
/// as a poll loop would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_try_calls_and_readiness_drive_an_exchange() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    let options = RelayOptions {
        max_chunk: 64 * 1024,
        upload_capacity: 1,
    };

    // Upload readiness: full while the handler is gated, ready once open.
    let (upload, response) = start(&n, SVC, "PUT", "/gate", options);
    assert!(response.try_wait().is_none());
    let mut queued = 0;
    loop {
        if upload.try_write(Bytes::from(vec![2; 64 * 1024])).unwrap() {
            queued += 64 * 1024;
            continue;
        }
        // Full right now; the upload task may still drain into QUIC, so
        // wait for readiness briefly, else every buffer is full.
        match tokio::time::timeout(Duration::from_millis(200), upload.ready()).await {
            Ok(r) => r.unwrap(),
            Err(_) => break,
        }
    }
    assert!(queued > 0);
    log.lock().unwrap().push("open".into());
    tokio::time::timeout(WAIT, upload.ready())
        .await
        .unwrap()
        .unwrap();
    upload.finish().unwrap();
    assert_eq!(upload.ready().await, Err(BodyError::Finished));
    tokio::time::timeout(WAIT, response.ready()).await.unwrap();
    let (head, _) = response.try_wait().unwrap().unwrap();
    let len = queued.to_string().into_bytes();
    assert!(head.headers.contains(&("x-len".into(), len)), "{head:?}");

    // Response readiness + try_read: a whole streamed body, in order.
    let data: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
    let (upload, response) = start(&n, SVC, "POST", "/echo", RelayOptions::default());
    upload.write(Bytes::from(data.clone())).await.unwrap();
    upload.finish().unwrap();
    tokio::time::timeout(WAIT, response.ready()).await.unwrap();
    let (head, reader) = response.try_wait().unwrap().unwrap();
    assert_eq!(head.status, 200);
    let mut got = Vec::new();
    loop {
        match reader.try_read(5000).unwrap() {
            TryRead::Data(c) => {
                assert!(!c.is_empty() && c.len() <= 5000);
                got.extend_from_slice(&c);
            }
            TryRead::End => break,
            TryRead::Pending => tokio::time::timeout(WAIT, reader.ready())
                .await
                .unwrap()
                .unwrap(),
        }
    }
    assert_eq!(got, data);
    assert_eq!(reader.try_read(10), Ok(TryRead::End));

    // Readiness resolves on close, and try_read then fails Cancelled.
    let (upload, response) = start(&n, SVC, "GET", "/forever", RelayOptions::default());
    upload.finish().unwrap();
    tokio::time::timeout(WAIT, response.ready()).await.unwrap();
    let (_, reader) = response.try_wait().unwrap().unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, reader.read(10)).await.unwrap(),
        Ok(Some(_))
    ));
    response.close();
    assert_eq!(
        tokio::time::timeout(WAIT, reader.ready()).await.unwrap(),
        Err(BodyError::Cancelled)
    );
    assert_eq!(reader.try_read(10), Err(BodyError::Cancelled));
    wait_until("handler write cancelled", || {
        log_has(&log, &format!("write {}", BodyError::Cancelled.code()))
    })
    .await;
    drop((reader, response));
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

/// Fill every buffer of a `/gate` upload (the handler reads nothing until
/// the gate opens): bytes queued before writes start timing out.
fn fill_until_full(up: &RelayUpload) -> usize {
    let (mut written, mut timeouts) = (0usize, 0);
    while timeouts < 3 {
        match up.write_blocking(Bytes::from(vec![1; 64 * 1024]), Duration::from_millis(100)) {
            Ok(()) => written += 64 * 1024,
            Err(BodyError::Timeout) => timeouts += 1,
            Err(e) => panic!("write failed: {e}"),
        }
    }
    written
}

/// `finish` / `abort` from another thread wake a write parked on a full
/// upload: it returns `Finished` and its chunk is never sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_finish_or_abort_wakes_a_parked_write() {
    let log: Log = Arc::default();
    let (_f, ctx, n) = streaming_net(&log).await;
    let options = RelayOptions {
        max_chunk: 64 * 1024,
        upload_capacity: 1,
    };
    for case in ["finish", "abort"] {
        log.lock().unwrap().clear();
        let (upload, response) = start(&n, SVC, "PUT", "/gate", options);
        let up = upload.clone();
        let written = on_thread(move || fill_until_full(&up)).await;
        assert!(written > 0);
        // Thread A parks in a blocking write.
        let up = upload.clone();
        let parked =
            std::thread::spawn(move || up.write_blocking(Bytes::from(vec![7; 64 * 1024]), WAIT));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!parked.is_finished(), "{case}: the write should be parked");
        // Thread B (this one) ends the upload.
        match case {
            "finish" => upload.finish().unwrap(),
            _ => upload.abort("caller gave up").unwrap(),
        }
        let res = on_thread(move || parked.join().unwrap()).await;
        assert_eq!(res, Err(BodyError::Finished), "{case}");
        log.lock().unwrap().push("open".into());
        let r = response.clone();
        let outcome = on_thread(move || r.wait_blocking(WAIT)).await;
        match case {
            "finish" => {
                // The handler counted exactly what was queued before: not
                // the parked chunk.
                let (head, _) = outcome.unwrap();
                let len = written.to_string().into_bytes();
                assert!(
                    head.headers.contains(&("x-len".into(), len)),
                    "{head:?} vs {written}"
                );
            }
            _ => {
                assert_eq!(outcome.unwrap_err().code(), 16);
                wait_until("handler saw the aborted body", || {
                    log.lock()
                        .unwrap()
                        .iter()
                        .any(|l| l.starts_with("read ") && l.contains("Err"))
                })
                .await;
            }
        }
    }
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}

/// A handler that fails before its head makes the relay reset the stream:
/// `Reset` (13), not a message. And several threads may wait on clones of
/// one response at once; all see the same head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_reset_before_the_head_and_concurrent_waiters() {
    let log: Log = Arc::default();
    let (queue, ctx) = setup();
    let _f = Foreign::start(queue, ctx.clone(), true, streaming_handler(log.clone()));
    let c = ctx.clone();
    let allow: aster::expose::AuthorizeFn = Arc::new(|_ctx: PeerContext| {
        Box::pin(async { Ok(()) }) as aster::expose::BoxFut<anyhow::Result<()>>
    });
    let fails: aster::expose::HttpHandler = Arc::new(|_req: http::Request<RelayBody>| {
        Box::pin(async { Err(anyhow::anyhow!("handler exploded")) })
            as aster::expose::BoxFut<anyhow::Result<http::Response<aster::expose::ResponseBody>>>
    });
    let n = net(move |e| {
        expose_foreign_http(e, &c.upcalls, &c.bodies, SVC, ForeignHttpOptions::default());
        e.expose_http("fails", allow, fails);
    })
    .await;

    let (upload, response) = start(&n, "fails", "POST", "/", RelayOptions::default());
    upload.finish().unwrap();
    let err = on_thread(move || response.wait_blocking(WAIT).unwrap_err()).await;
    assert_eq!(err.code(), 13, "{err}");
    assert!(matches!(err, ResponseError::Reset(_)));

    let (upload, response) = start(&n, SVC, "PUT", "/gate", RelayOptions::default());
    let waiters: Vec<_> = (0..2)
        .map(|_| {
            let r = response.clone();
            std::thread::spawn(move || r.wait_blocking(WAIT))
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    upload.write(Bytes::from_static(b"abc")).await.unwrap();
    upload.finish().unwrap();
    log.lock().unwrap().push("open".into());
    let heads = on_thread(move || {
        waiters
            .into_iter()
            .map(|w| w.join().unwrap().unwrap().0)
            .collect::<Vec<_>>()
    })
    .await;
    assert_eq!(heads[0], heads[1]);
    assert!(heads[0].headers.contains(&("x-len".into(), b"3".to_vec())));
    drop(response);
    wait_until("exchanges closed", || ctx.bodies.open_count() == 0).await;
    n.shutdown().await;
}
