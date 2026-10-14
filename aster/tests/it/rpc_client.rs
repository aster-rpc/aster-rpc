//! The Rust RPC client over real QUIC (dynamic-dispatch design §3, R3d):
//! pooled unary, `CallOptions` (session, deadline, metadata, mode), client-side
//! deadlines for all four call shapes, `open_session`, incremental client
//! streaming, cancel / drop-cancels, and structured `Error::Rpc`.
//!
//! Plus a pooled-unary stress test (`pooled_unary_stress`): a short run in the
//! normal suite and a long, `#[ignore]`d one (`pooled_unary_stress_long`,
//! `ASTER_STRESS_CALLS` calls, default 2,000,000) for the rare reused-stream
//! `PROTOCOL_VIOLATION` reset (perf-baseline-r3 §5.3).

#![cfg(feature = "rpc")]

use std::future::Future;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aster::rpc::{
    Call, CallOptions, RpcConnection, RpcStatus, SerializationMode, Server, ServiceDispatch,
    StatusCode, StreamHeader, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode};

// ── test service ────────────────────────────────────────────────────────────

/// Observations the handlers report back.
#[derive(Default)]
struct Seen {
    /// Handlers that saw their call cancelled (peer cancel or stream error).
    cancelled: AtomicUsize,
    /// Streaming producers that stopped because their output was refused.
    producer_stopped: AtomicUsize,
    /// `hold` calls that reached their handler.
    hold_started: AtomicUsize,
}

struct Svc {
    seen: Arc<Seen>,
}

async fn wait_cancelled(call: &Call) {
    while !call.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Counts once when dropped, unless disarmed: the handler noticed the cancel
/// itself, or the server dropped it on the cancel (as it does for a peer
/// cancel or a deadline).
struct CountOnDrop<'a> {
    counter: &'a AtomicUsize,
    armed: bool,
}

impl<'a> CountOnDrop<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        Self {
            counter,
            armed: true,
        }
    }
}

impl Drop for CountOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.counter.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl ServiceDispatch for Svc {
    fn name(&self) -> &str {
        "test.Client"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &[
            "echo", "empty", "fail", "meta", "hang", "park", "count", "forever", "sum", "hold",
            "chat",
        ]
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        match method {
            "echo" => {
                let req = call.recv_request().await.unwrap_or_default();
                let _ = call.respond(req, &RpcStatus::ok()).await;
            }
            "empty" => {
                let _ = call.respond(Vec::new(), &RpcStatus::ok()).await;
            }
            "fail" => {
                let mut status = RpcStatus::error(StatusCode::FailedPrecondition, "nope");
                status.detail_keys.push("why".into());
                status.detail_values.push("because".into());
                let _ = call.respond(Vec::new(), &status).await;
            }
            "meta" => {
                let h = call.header().clone();
                let pairs: Vec<String> = h
                    .metadata_keys
                    .iter()
                    .zip(&h.metadata_values)
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                let out = format!(
                    "deadline={} session={} meta={}",
                    h.deadline,
                    h.session_id,
                    pairs.join(",")
                );
                let _ = call.respond(out.into_bytes(), &RpcStatus::ok()).await;
            }
            "hang" => {
                // Never answers; reports the cancel when it arrives.
                let _cancelled = CountOnDrop::new(&self.seen.cancelled);
                wait_cancelled(&call).await;
            }
            "park" => {
                // Never answers and never looks at the cancel flag: only the
                // server dropping it ends it.
                let _cancelled = CountOnDrop::new(&self.seen.cancelled);
                std::future::pending::<()>().await;
            }
            "count" => {
                let n = call.recv_request().await.map(|r| r[0]).unwrap_or(0);
                for i in 0..n {
                    if call.send(vec![i]).await.is_err() {
                        return;
                    }
                }
                let _ = call.finish(&RpcStatus::ok()).await;
            }
            "forever" => {
                let _ = call.recv_request().await;
                // Stopped by a refused send, the flag, or being dropped.
                let _stopped = CountOnDrop::new(&self.seen.producer_stopped);
                loop {
                    if call.is_cancelled() || call.send(vec![1; 32]).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
            "sum" => {
                let mut sum = 0u32;
                while let Some(req) = call.recv_request().await {
                    sum += req.iter().map(|b| u32::from(*b)).sum::<u32>();
                }
                let _ = call
                    .respond(sum.to_le_bytes().to_vec(), &RpcStatus::ok())
                    .await;
            }
            "hold" => {
                self.seen.hold_started.fetch_add(1, Ordering::SeqCst);
                let mut cancelled = CountOnDrop::new(&self.seen.cancelled);
                // Read requests until the stream ends or the call is cancelled.
                loop {
                    match tokio::time::timeout(Duration::from_millis(5), call.recv_request()).await
                    {
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(_) if call.is_cancelled() => break,
                        Err(_) => {}
                    }
                }
                if !call.is_cancelled() {
                    cancelled.armed = false;
                    let _ = call.respond(vec![9], &RpcStatus::ok()).await;
                }
            }
            "chat" => {
                let mut cancelled = CountOnDrop::new(&self.seen.cancelled);
                loop {
                    match tokio::time::timeout(Duration::from_millis(5), call.recv_request()).await
                    {
                        Ok(Some(req)) => {
                            if call.send(req).await.is_err() {
                                cancelled.armed = call.is_cancelled();
                                return;
                            }
                        }
                        // A cancel also closes the request stream.
                        Ok(None) if !call.is_cancelled() => break,
                        Ok(None) | Err(_) if call.is_cancelled() => return,
                        Ok(None) => break,
                        Err(_) => {}
                    }
                }
                cancelled.armed = false;
                let _ = call.finish(&RpcStatus::ok()).await;
            }
            _ => {}
        }
    }
}

/// Session-scoped counter (one instance per `(connection, session id)`).
struct Counter {
    n: AtomicU32,
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

// ── harness ─────────────────────────────────────────────────────────────────

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn wait_for_addr(node: &Node) {
    for _ in 0..50 {
        if !node.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn within<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(20), f)
        .await
        .expect("rpc operation timed out")
}

/// Wait until `cond` holds (or fail after 10 s).
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let started = Instant::now();
    while !cond() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct Fixture {
    srv: Node,
    client: Node,
    conn: RpcConnection,
    seen: Arc<Seen>,
    _handle: aster::rpc::ServerHandle,
}

impl Fixture {
    async fn new() -> Self {
        let srv = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
            .await
            .unwrap();
        let client = Node::start(cfg()).await.unwrap();
        wait_for_addr(&srv).await;
        wait_for_addr(&client).await;
        client.add_peer(&srv).unwrap();
        srv.add_peer(&client).unwrap();
        let seen = Arc::new(Seen::default());
        let handle = Server::new(&srv)
            .register(Svc { seen: seen.clone() })
            .register_session(|| Counter {
                n: AtomicU32::new(0),
            })
            .serve();
        let conn = within(client.rpc_connect(&srv.id())).await.unwrap();
        Self {
            srv,
            client,
            conn,
            seen,
            _handle: handle,
        }
    }

    async fn shutdown(self) {
        drop(self.conn);
        self.client.shutdown().await;
        self.srv.shutdown().await;
    }
}

fn h(method: &str) -> StreamHeader {
    StreamHeader {
        service: "test.Client".into(),
        method: method.into(),
        version: 1,
        ..Default::default()
    }
}

fn code_of<T: std::fmt::Debug>(r: aster::Result<T>) -> StatusCode {
    match r {
        Err(e) => e
            .rpc_code()
            .unwrap_or_else(|| panic!("expected Error::Rpc, got {e:?}")),
        Ok(v) => panic!("expected an RPC error, got Ok({v:?})"),
    }
}

// ── unary ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn unary_round_trips_on_the_pool() {
    let f = Fixture::new().await;
    assert_eq!(f.conn.shared_pool_size(), 8);
    for i in 0..50u8 {
        let out = within(f.conn.unary(&h("echo"), vec![i; 10])).await.unwrap();
        assert_eq!(out, vec![i; 10]);
    }
    // An empty response (trailer only) is an empty payload, not an error.
    assert!(within(f.conn.unary(&h("empty"), vec![1]))
        .await
        .unwrap()
        .is_empty());
    // A large request and response cross the same pooled stream.
    let big = vec![7u8; 3 << 20];
    assert_eq!(
        within(f.conn.unary(&h("echo"), big.clone())).await.unwrap(),
        big
    );

    // More callers than pooled streams: all served; extra callers queue.
    let mut tasks = Vec::new();
    for i in 0..64u8 {
        let conn = f.conn.clone();
        tasks.push(tokio::spawn(async move {
            conn.unary(&h("echo"), vec![i]).await
        }));
    }
    for (i, t) in tasks.into_iter().enumerate() {
        assert_eq!(within(t).await.unwrap().unwrap(), vec![i as u8]);
    }

    #[cfg(feature = "binding-support")]
    {
        let pool = f.conn.core_connection().pool();
        assert!(pool.open_count(&None) <= 8, "{}", pool.open_count(&None));
        assert_eq!(
            pool.free_count(&None),
            pool.open_count(&None),
            "every stream returned to the pool"
        );
    }
    f.shutdown().await;
}

#[tokio::test]
async fn a_non_ok_trailer_is_a_structured_rpc_error() {
    let f = Fixture::new().await;
    match within(f.conn.unary(&h("fail"), vec![1])).await {
        Err(aster::Error::Rpc {
            code,
            message,
            details,
        }) => {
            assert_eq!(code, StatusCode::FailedPrecondition.as_i32());
            assert_eq!(message, "nope");
            assert_eq!(details, vec![("why".to_string(), "because".to_string())]);
        }
        other => panic!("expected Error::Rpc, got {other:?}"),
    }
    // The stream that carried the error is reused cleanly.
    assert_eq!(
        within(f.conn.unary(&h("echo"), vec![3])).await.unwrap(),
        vec![3]
    );
    f.shutdown().await;
}

#[tokio::test]
async fn call_options_reach_the_header() {
    let f = Fixture::new().await;
    let opts = CallOptions::new()
        .deadline(Duration::from_millis(1500))
        .metadata("k1", "v1")
        .metadata("k2", "v2");
    let mut base = h("meta");
    base.metadata_keys.push("base".into());
    base.metadata_values.push("b".into());
    let out = within(f.conn.unary_with(&base, vec![1], &opts))
        .await
        .unwrap();
    // 1.5 s rounds UP to 2 whole seconds on the wire.
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "deadline=2 session=0 meta=base=b,k1=v1,k2=v2"
    );

    // The serialization-mode override reaches the server too: this native
    // service accepts only XLANG.
    let native = CallOptions::new().serialization_mode(SerializationMode::Native);
    assert_eq!(
        code_of(within(f.conn.unary_with(&h("echo"), vec![1], &native)).await),
        StatusCode::InvalidArgument
    );
    f.shutdown().await;
}

#[tokio::test]
async fn a_unary_deadline_cancels_the_call_and_frees_the_stream() {
    let f = Fixture::new().await;
    // One pooled stream: a leaked or dirty stream would break the next call.
    f.conn.set_shared_pool_size(1);
    let opts = CallOptions::new().deadline(Duration::from_millis(300));

    let started = Instant::now();
    let r = within(f.conn.unary_with(&h("hang"), vec![1], &opts)).await;
    let took = started.elapsed();
    assert_eq!(code_of(r), StatusCode::DeadlineExceeded);
    assert!(took >= Duration::from_millis(300), "fired early: {took:?}");
    assert!(took < Duration::from_secs(3), "fired late: {took:?}");

    // The client cancelled the call, so the handler sees it well before the
    // server's own (1 s, rounded-up) deadline would.
    let seen = f.seen.clone();
    eventually("the handler observes the cancel", || {
        seen.cancelled.load(Ordering::SeqCst) == 1
    })
    .await;

    // The pool slot came back and the next call gets a clean stream.
    assert_eq!(
        within(f.conn.unary(&h("echo"), vec![5])).await.unwrap(),
        vec![5]
    );

    // Same through the connection-level pipeline deadline.
    let conn = f.conn.clone().with_deadline(Duration::from_millis(200));
    assert_eq!(
        code_of(within(conn.unary(&h("hang"), vec![1])).await),
        StatusCode::DeadlineExceeded
    );
    assert_eq!(
        within(conn.unary(&h("echo"), vec![6])).await.unwrap(),
        vec![6]
    );
    f.shutdown().await;
}

#[tokio::test]
async fn a_dropped_unary_future_cancels_and_does_not_poison_the_pool() {
    let f = Fixture::new().await;
    f.conn.set_shared_pool_size(1);
    let hang = h("hang");
    let fut = f.conn.unary(&hang, vec![1]);
    assert!(tokio::time::timeout(Duration::from_millis(200), fut)
        .await
        .is_err());
    let seen = f.seen.clone();
    eventually("the handler observes the cancel", || {
        seen.cancelled.load(Ordering::SeqCst) == 1
    })
    .await;
    for i in 0..5u8 {
        assert_eq!(
            within(f.conn.unary(&h("echo"), vec![i])).await.unwrap(),
            vec![i]
        );
    }
    f.shutdown().await;
}

/// A client cancel ends a unary handler that never polls the flag: the server
/// drops its future, as it does on a deadline (the server's own deadline is
/// 300 s here, so only the cancel can end it within the test).
#[tokio::test]
async fn a_client_cancel_drops_a_unary_handler() {
    let f = Fixture::new().await;
    f.conn.set_shared_pool_size(1);
    let park = h("park");
    let fut = f.conn.unary(&park, vec![1]);
    assert!(tokio::time::timeout(Duration::from_millis(200), fut)
        .await
        .is_err());
    let seen = f.seen.clone();
    eventually("the handler is dropped on the cancel", || {
        seen.cancelled.load(Ordering::SeqCst) == 1
    })
    .await;
    // The stream is clean for the next call.
    assert_eq!(
        within(f.conn.unary(&h("echo"), vec![7])).await.unwrap(),
        vec![7]
    );
    f.shutdown().await;
}

#[tokio::test]
async fn the_shared_pool_bound_is_configurable() {
    let f = Fixture::new().await;
    f.conn.set_shared_pool_size(2);
    assert_eq!(f.conn.shared_pool_size(), 2);
    // Clones share the connection's pool.
    assert_eq!(f.conn.clone().shared_pool_size(), 2);
    let mut tasks = Vec::new();
    for i in 0..20u8 {
        let conn = f.conn.clone();
        tasks.push(tokio::spawn(async move {
            conn.unary(&h("echo"), vec![i]).await
        }));
    }
    for t in tasks {
        within(t).await.unwrap().unwrap();
    }
    #[cfg(feature = "binding-support")]
    assert!(f.conn.core_connection().pool().open_count(&None) <= 2);
    f.shutdown().await;
}

// ── sessions ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn open_session_allocates_per_connection_ids_and_reaches_one_instance() {
    let f = Fixture::new().await;
    let s1 = f.conn.open_session().unwrap();
    let s2 = f.conn.open_session().unwrap();
    // The counter lives on the connection: a clone continues the sequence.
    let s3 = f.conn.clone().open_session().unwrap();
    assert_eq!((s1.id(), s2.id(), s3.id()), (1, 2, 3));
    #[cfg(feature = "binding-support")]
    assert_eq!(f.conn.core_connection().next_session_id(), Some(4));

    let incr = StreamHeader {
        service: "test.Counter".into(),
        method: "incr".into(),
        version: 1,
        ..Default::default()
    };
    assert_eq!(within(s1.unary(&incr, vec![0])).await.unwrap(), vec![1]);
    assert_eq!(within(s1.unary(&incr, vec![0])).await.unwrap(), vec![2]);
    assert_eq!(within(s2.unary(&incr, vec![0])).await.unwrap(), vec![1]);
    assert_eq!(within(s1.unary(&incr, vec![0])).await.unwrap(), vec![3]);
    // `CallOptions::session_id` on the connection is the same session.
    let opts = CallOptions::new().session(s2.id());
    let via_opts = f.conn.unary_with(&incr, vec![0], &opts);
    assert_eq!(within(via_opts).await.unwrap(), vec![2]);

    // The session id is stamped on the header (and other options still apply).
    let out =
        within(s3.unary_with(&h("meta"), vec![0], &CallOptions::new().metadata("a", "b"))).await;
    // test.Client is SHARED: a session id on it is refused.
    assert_eq!(code_of(out), StatusCode::FailedPrecondition);
    f.shutdown().await;
}

// ── server streaming ────────────────────────────────────────────────────────

#[tokio::test]
async fn server_stream_reads_to_the_trailer() {
    let f = Fixture::new().await;
    let mut s = within(f.conn.server_stream(&h("count"), vec![5]))
        .await
        .unwrap();
    let items = within(s.collect()).await.unwrap();
    assert_eq!(items, (0..5u8).map(|i| vec![i]).collect::<Vec<_>>());
    assert!(within(s.next()).await.is_none());
    f.shutdown().await;
}

#[tokio::test]
async fn a_server_stream_deadline_cancels_the_producer() {
    let f = Fixture::new().await;
    let opts = CallOptions::new().deadline(Duration::from_millis(300));
    let mut s = within(f.conn.server_stream_with(&h("forever"), vec![1], &opts))
        .await
        .unwrap();
    let started = Instant::now();
    let mut items = 0;
    let err = loop {
        match within(s.next()).await {
            Some(Ok(_)) => items += 1,
            Some(Err(e)) => break e,
            None => panic!("the stream never ends on its own"),
        }
    };
    assert!(items > 0);
    assert_eq!(
        err.rpc_code(),
        Some(StatusCode::DeadlineExceeded),
        "{err:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        within(s.next()).await.is_none(),
        "the stream ends after the error"
    );
    let seen = f.seen.clone();
    eventually("the producer stops", || {
        seen.producer_stopped.load(Ordering::SeqCst) == 1
    })
    .await;
    f.shutdown().await;
}

#[tokio::test]
async fn cancelling_or_dropping_a_response_stream_stops_the_producer() {
    let f = Fixture::new().await;
    let seen = f.seen.clone();

    let mut s = within(f.conn.server_stream(&h("forever"), vec![1]))
        .await
        .unwrap();
    for _ in 0..3 {
        within(s.next()).await.unwrap().unwrap();
    }
    within(s.cancel()).await;
    assert!(within(s.next()).await.is_none());
    eventually("cancel stops the producer", || {
        seen.producer_stopped.load(Ordering::SeqCst) == 1
    })
    .await;

    let mut s = within(f.conn.server_stream(&h("forever"), vec![1]))
        .await
        .unwrap();
    within(s.next()).await.unwrap().unwrap();
    drop(s);
    eventually("drop stops the producer", || {
        seen.producer_stopped.load(Ordering::SeqCst) == 2
    })
    .await;

    // The connection is unaffected.
    assert_eq!(
        within(f.conn.unary(&h("echo"), vec![1])).await.unwrap(),
        vec![1]
    );
    f.shutdown().await;
}

// ── client streaming ────────────────────────────────────────────────────────

#[tokio::test]
async fn client_stream_open_sends_incrementally() {
    let f = Fixture::new().await;
    let (sink, response) = within(f.conn.client_stream_open(&h("sum"), &CallOptions::new()))
        .await
        .unwrap();
    for i in 1..=4u8 {
        within(sink.send(vec![i, i])).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    within(sink.finish()).await.unwrap();
    within(sink.finish()).await.unwrap(); // idempotent
    let out = within(response).await.unwrap();
    assert_eq!(u32::from_le_bytes(out.try_into().unwrap()), 20);
    assert!(
        within(sink.send(vec![1])).await.is_err(),
        "send after finish"
    );

    // The Vec form wraps it.
    let out = within(f.conn.client_stream(&h("sum"), vec![vec![1], vec![2, 3]]))
        .await
        .unwrap();
    assert_eq!(u32::from_le_bytes(out.try_into().unwrap()), 6);
    f.shutdown().await;
}

#[tokio::test]
async fn finishing_a_client_stream_with_no_request_is_refused() {
    let f = Fixture::new().await;
    let (sink, response) = within(f.conn.client_stream_open(&h("sum"), &CallOptions::new()))
        .await
        .unwrap();
    assert!(matches!(
        within(sink.finish()).await,
        Err(aster::Error::InvalidArgument(_))
    ));
    assert_eq!(code_of(within(response).await), StatusCode::Cancelled);
    f.shutdown().await;
}

#[tokio::test]
async fn client_stream_cancel_and_drop_reach_the_handler() {
    let f = Fixture::new().await;
    let seen = f.seen.clone();

    // Explicit cancel: the response side reports CANCELLED.
    let (sink, response) = within(f.conn.client_stream_open(&h("hold"), &CallOptions::new()))
        .await
        .unwrap();
    within(sink.send(vec![1])).await.unwrap();
    // Cancel once the call is running (a cancel that overtakes the call's
    // first frames just means the server never starts it).
    eventually("the call starts", || {
        seen.hold_started.load(Ordering::SeqCst) == 1
    })
    .await;
    within(sink.cancel()).await;
    assert_eq!(code_of(within(response).await), StatusCode::Cancelled);
    assert_eq!(
        code_of(within(sink.send(vec![2])).await),
        StatusCode::Cancelled
    );
    eventually("cancel reaches the handler", || {
        seen.cancelled.load(Ordering::SeqCst) == 1
    })
    .await;

    // Dropping the sink without finishing cancels too.
    let (sink, response) = within(f.conn.client_stream_open(&h("hold"), &CallOptions::new()))
        .await
        .unwrap();
    within(sink.send(vec![1])).await.unwrap();
    eventually("the call starts", || {
        seen.hold_started.load(Ordering::SeqCst) == 2
    })
    .await;
    drop(sink);
    eventually("drop-without-finish reaches the handler", || {
        seen.cancelled.load(Ordering::SeqCst) == 2
    })
    .await;
    assert!(within(response).await.is_err());
    f.shutdown().await;
}

#[tokio::test]
async fn a_client_stream_deadline_cancels_the_call() {
    let f = Fixture::new().await;
    let opts = CallOptions::new().deadline(Duration::from_millis(300));
    let (sink, response) = within(f.conn.client_stream_open(&h("hold"), &opts))
        .await
        .unwrap();
    within(sink.send(vec![1])).await.unwrap();
    let started = Instant::now();
    assert_eq!(
        code_of(within(response).await),
        StatusCode::DeadlineExceeded
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    let seen = f.seen.clone();
    eventually("the deadline cancels the handler", || {
        seen.cancelled.load(Ordering::SeqCst) == 1
    })
    .await;
    // Past the deadline, sends fail too.
    assert!(within(sink.send(vec![2])).await.is_err());
    f.shutdown().await;
}

// ── bidi ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn bidi_echoes_and_cancels() {
    let f = Fixture::new().await;
    let (sink, mut resp) = within(f.conn.bidi_open(&h("chat"))).await.unwrap();
    for i in 0..3u8 {
        within(sink.send(vec![i])).await.unwrap();
        assert_eq!(within(resp.next()).await.unwrap().unwrap(), vec![i]);
    }
    within(sink.finish()).await.unwrap();
    assert!(
        within(resp.next()).await.is_none(),
        "OK trailer after finish"
    );

    // Cancel from the sink while the response side is waiting.
    let (sink, mut resp) = within(f.conn.bidi_open(&h("chat"))).await.unwrap();
    within(sink.send(vec![1])).await.unwrap();
    assert_eq!(within(resp.next()).await.unwrap().unwrap(), vec![1]);
    let reader = tokio::spawn(async move { resp.next().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    within(sink.cancel()).await;
    let read = within(reader).await.unwrap();
    assert_eq!(
        read.expect("the pending read fails")
            .unwrap_err()
            .rpc_code(),
        Some(StatusCode::Cancelled)
    );
    let seen = f.seen.clone();
    eventually("cancel reaches the handler", || {
        seen.cancelled.load(Ordering::SeqCst) == 1
    })
    .await;

    // A deadline bounds the whole bidi call.
    let opts = CallOptions::new().deadline(Duration::from_millis(300));
    let (sink, mut resp) = within(f.conn.bidi_open_with(&h("chat"), &opts))
        .await
        .unwrap();
    within(sink.send(vec![1])).await.unwrap();
    assert_eq!(within(resp.next()).await.unwrap().unwrap(), vec![1]);
    let e = within(resp.next()).await.unwrap().unwrap_err();
    assert_eq!(e.rpc_code(), Some(StatusCode::DeadlineExceeded));
    eventually("the deadline reaches the handler", || {
        seen.cancelled.load(Ordering::SeqCst) == 2
    })
    .await;
    f.shutdown().await;
}

// ── stress ──────────────────────────────────────────────────────────────────

/// `calls` pooled unary calls at `conc` in flight on one connection, with a
/// server-streaming and a client-streaming caller running alongside. Every
/// call must succeed: a reused stream that gets reset (the old
/// `PROTOCOL_VIOLATION` race) or returns another call's bytes fails it.
async fn stress(calls: u64, conc: usize) {
    let f = Fixture::new().await;
    let done = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut workers = Vec::new();
    for w in 0..conc {
        let (conn, done, errors) = (f.conn.clone(), done.clone(), errors.clone());
        workers.push(tokio::spawn(async move {
            let mut first_err = None;
            let mut i = 0u32;
            while done.fetch_add(1, Ordering::Relaxed) < calls {
                let req = [w as u8, (i & 0xff) as u8, (i >> 8) as u8];
                match conn.unary(&h("echo"), req.to_vec()).await {
                    Ok(out) if out == req => {}
                    Ok(out) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        first_err.get_or_insert(format!("wrong reply {out:?} for {req:?}"));
                    }
                    Err(e) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        first_err.get_or_insert(e.to_string());
                    }
                }
                i = i.wrapping_add(1);
            }
            first_err
        }));
    }
    // Streaming alongside: dedicated substreams on the same connection.
    let mut streamers = Vec::new();
    {
        let (conn, stop) = (f.conn.clone(), stop.clone());
        streamers.push(tokio::spawn(async move {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let mut s = conn.server_stream(&h("count"), vec![50]).await.unwrap();
                assert_eq!(s.collect().await.unwrap().len(), 50);
                n += 1;
            }
            n
        }));
    }
    {
        let (conn, stop) = (f.conn.clone(), stop.clone());
        streamers.push(tokio::spawn(async move {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let reqs = (0..20).map(|_| vec![1u8]).collect();
                let out = conn.client_stream(&h("sum"), reqs).await.unwrap();
                assert_eq!(u32::from_le_bytes(out.try_into().unwrap()), 20);
                n += 1;
            }
            n
        }));
    }

    let mut first_err = None;
    for w in workers {
        if let Some(e) = w.await.unwrap() {
            first_err.get_or_insert(e);
        }
    }
    stop.store(true, Ordering::Relaxed);
    let mut streamed = 0;
    for s in streamers {
        streamed += s.await.unwrap();
    }
    let errors = errors.load(Ordering::Relaxed);
    eprintln!("stress: {calls} unary calls at {conc} in flight, {streamed} streaming calls alongside, {errors} errors");
    assert_eq!(errors, 0, "first error: {first_err:?}");
    assert!(streamed > 0);
    f.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pooled_unary_stress() {
    stress(20_000, 10).await;
}

/// The long run: `ASTER_STRESS_CALLS` (default 2,000,000) pooled unary calls.
/// Release build recommended:
///
/// ```text
/// ASTER_STRESS_CALLS=5000000 cargo test --release -p aster --features rpc \
///     --test it rpc_client::pooled_unary_stress_long -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "long-running stress test; run explicitly"]
async fn pooled_unary_stress_long() {
    let calls = std::env::var("ASTER_STRESS_CALLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    stress(calls, 10).await;
}
