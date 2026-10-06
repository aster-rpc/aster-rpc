#![cfg(all(feature = "rpc", feature = "binding-support"))]
//! Foreign (binding-implemented) services served by `aster::rpc::Server`
//! (design `docs/_internal/bindings/dynamic-dispatch-design.md`, slice R3c).
//!
//! The "binding" here is plain Rust standing in for one: a `std::thread`
//! drains a `RingSink` and hands each call to its own handler thread (as the
//! Java binding hands calls to virtual threads), and every response goes
//! through the shared `CallTable` by `call_id` -- never through a Rust
//! future. A real iroh client drives it over QUIC. Every test ends by
//! requiring the call table to be empty: whatever happened to a call, Rust
//! must have let go of it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use aster::binding_support::dispatch::{
    CallTable, CancelReason, DispatchEvent, ForeignCall, RingConsumer, RingSink, SubmitError,
    FLAG_FIRST_IS_END, FLAG_IROH_AUTHENTICATED, FLAG_NEW_SESSION,
};
use aster::rpc::baseline::NodeInfoClient;
use aster::rpc::{
    require_role, AsterServer, Call, DynamicService, MethodDesc, MethodPattern, RpcConnection,
    RpcStatus, ScopeKind, ServiceDispatch, StatusCode, StreamHeader, TypeRef, ROLE_ATTRIBUTE,
};
use aster::{AsterConfig, Error, Node, RelayMode};
use aster_transport_core::framing::{
    encode_frame, FLAG_CANCEL, FLAG_COMPRESSED, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER,
};
use tokio::time::timeout;

// ── the services ────────────────────────────────────────────────────────────

const ECHO: u32 = 0;
const COUNTER: u32 = 1;

// test.Echo methods, by index.
const ECHO_UNARY: u32 = 0;
const COUNT: u32 = 1;
const SUM: u32 = 2;
const CHAT: u32 = 3;
const FLOOD: u32 = 4;
const HANG: u32 = 5;
const SLOW: u32 = 6;
const DROP: u32 = 7;
const ADMIN: u32 = 8;
const SMALL: u32 = 9;
const PARK: u32 = 10;

// test.Counter methods.
const INCR: u32 = 0;
const SESSION_COUNT: u32 = 1;

/// Frames FLOOD sends after the first `QUEUE_FULL`.
const FLOOD_AFTER_FULL: usize = 8;
const FLOOD_FRAME: usize = 64 * 1024;

fn hash(b: u8) -> TypeRef {
    TypeRef::new(format!("{b:02x}").repeat(32))
}

fn method(name: &str, pattern: MethodPattern) -> MethodDesc {
    MethodDesc::new(name, pattern, hash(0xa1), hash(0xb2))
}

fn echo_service() -> DynamicService {
    let mut svc = DynamicService::new("test.Echo", 1);
    let mut slow = method("slow", MethodPattern::Unary);
    slow.default_timeout = Some(0.3);
    let mut admin = method("admin", MethodPattern::Unary);
    admin.requires = Some(require_role("admin"));
    let mut small = method("small", MethodPattern::Unary);
    small.limits.max_frame_size = Some(1024);
    svc.methods = vec![
        method("echo", MethodPattern::Unary),
        method("count", MethodPattern::ServerStream),
        method("sum", MethodPattern::ClientStream),
        method("chat", MethodPattern::BidiStream),
        method("flood", MethodPattern::ServerStream),
        method("hang", MethodPattern::ServerStream),
        slow,
        method("drop", MethodPattern::Unary),
        admin,
        small,
        method("park", MethodPattern::Unary),
    ];
    svc
}

fn counter_service() -> DynamicService {
    let mut svc = DynamicService::new("test.Counter", 1);
    svc.scope = ScopeKind::Session;
    svc.methods = vec![
        method("incr", MethodPattern::Unary),
        method("count", MethodPattern::ServerStream),
    ];
    svc
}

/// A native service registered next to the foreign ones.
struct NativePing;

#[async_trait::async_trait]
impl ServiceDispatch for NativePing {
    fn name(&self) -> &str {
        "test.Native"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["ping"]
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let req = call.recv_request().await.unwrap_or_default();
        let mut out = b"native:".to_vec();
        out.extend(req);
        let _ = call.respond(out, &RpcStatus::ok()).await;
    }
}

// ── the fake binding ────────────────────────────────────────────────────────

/// What the binding observed, for assertions.
#[derive(Default)]
struct Observed {
    cancelled: Vec<(u64, CancelReason, u32)>, // (call_id, reason, method_idx)
    sessions_opened: Vec<u64>,
    sessions_closed: Vec<u64>,
    connections_closed: Vec<u64>,
    calls: Vec<(u32, u32, u32)>, // (service_idx, method_idx, flags)
    /// Outcomes reported by handlers (e.g. "flood:full", "slow:not_found").
    notes: Vec<String>,
}

struct Binding {
    table: CallTable,
    observed: Arc<Mutex<Observed>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Binding {
    fn start(consumer: RingConsumer, table: CallTable, rt: tokio::runtime::Handle) -> Self {
        let observed = Arc::new(Mutex::new(Observed::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let observed = observed.clone();
            let stop = stop.clone();
            let table = table.clone();
            std::thread::spawn(move || event_loop(consumer, table, rt, observed, stop))
        };
        Binding {
            table,
            observed,
            stop,
            thread: Some(thread),
        }
    }

    fn observed(&self) -> std::sync::MutexGuard<'_, Observed> {
        self.observed.lock().unwrap()
    }

    /// Wait (on the test's runtime) until `f` holds.
    async fn until(&self, what: &str, f: impl Fn(&Observed) -> bool) {
        for _ in 0..500 {
            if f(&self.observed()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    /// Every call must have left the table.
    async fn assert_table_drains(&self) {
        for _ in 0..500 {
            if self.table.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} call(s) still in the CallTable", self.table.len());
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The binding's poll thread: drain the ring, start a handler thread per
/// call, and route per-call events (cancel, writable) to it.
fn event_loop(
    mut consumer: RingConsumer,
    table: CallTable,
    rt: tokio::runtime::Handle,
    observed: Arc<Mutex<Observed>>,
    stop: Arc<AtomicBool>,
) {
    let mut routes: HashMap<u64, mpsc::Sender<DispatchEvent>> = HashMap::new();
    let mut method_of: HashMap<u64, u32> = HashMap::new();
    while !stop.load(Ordering::SeqCst) {
        consumer.wait_blocking(Duration::from_millis(20));
        consumer.drain(64, |ev| match ev {
            DispatchEvent::Call(call) => {
                {
                    let mut o = observed.lock().unwrap();
                    o.calls
                        .push((call.service_idx, call.method_idx, call.flags));
                    if call.flags & FLAG_NEW_SESSION != 0 {
                        o.sessions_opened.push(call.session_key);
                    }
                }
                let (tx, rx) = mpsc::channel();
                routes.insert(call.call_id, tx);
                method_of.insert(call.call_id, call.method_idx);
                let table = table.clone();
                let rt = rt.clone();
                let observed = observed.clone();
                std::thread::spawn(move || handle(call, rx, table, rt, observed));
            }
            ev @ (DispatchEvent::Cancelled { .. }
            | DispatchEvent::ResponseWritable { .. }
            | DispatchEvent::ResponseClosed { .. }) => {
                let id = match &ev {
                    DispatchEvent::Cancelled { call_id, reason } => {
                        observed.lock().unwrap().cancelled.push((
                            *call_id,
                            *reason,
                            method_of.get(call_id).copied().unwrap_or(u32::MAX),
                        ));
                        *call_id
                    }
                    DispatchEvent::ResponseWritable { call_id }
                    | DispatchEvent::ResponseClosed { call_id } => *call_id,
                    _ => unreachable!(),
                };
                if let Some(tx) = routes.get(&id) {
                    let _ = tx.send(ev);
                }
            }
            DispatchEvent::SessionClosed { session_key } => {
                observed.lock().unwrap().sessions_closed.push(session_key);
            }
            DispatchEvent::ConnectionClosed { connection_id, .. } => {
                observed
                    .lock()
                    .unwrap()
                    .connections_closed
                    .push(connection_id);
            }
            _ => {}
        });
    }
}

fn note(observed: &Arc<Mutex<Observed>>, s: impl Into<String>) {
    observed.lock().unwrap().notes.push(s.into());
}

/// Send `payload`, parking on a reservation whenever the queue is full --
/// what the Java binding does with `QUEUE_FULL` today.
fn send_parked(
    table: &CallTable,
    id: u64,
    payload: &[u8],
    events: &mpsc::Receiver<DispatchEvent>,
) -> Result<(), SubmitError> {
    loop {
        match table.try_send(id, payload) {
            Err(SubmitError::QueueFull { required_len }) => {
                table.reserve(id, required_len)?;
                match events.recv_timeout(Duration::from_secs(10)) {
                    Ok(DispatchEvent::ResponseWritable { .. }) => continue,
                    _ => return Err(SubmitError::Closed),
                }
            }
            other => return other,
        }
    }
}

/// One call's handler thread: everything by `call_id` through the table.
fn handle(
    call: ForeignCall,
    events: mpsc::Receiver<DispatchEvent>,
    table: CallTable,
    rt: tokio::runtime::Handle,
    observed: Arc<Mutex<Observed>>,
) {
    let ForeignCall {
        call_id: id,
        service_idx,
        method_idx,
        flags,
        session_key,
        request,
        attributes,
        deadline_unix_ms,
        handle,
        ..
    } = call;
    let ok = RpcStatus::ok();

    if method_idx == DROP && service_idx == ECHO {
        // Lose the call without answering: dropping the owning handle must
        // answer `INTERNAL` for us.
        drop(handle);
        return;
    }
    let id_check = handle.detach();
    assert_eq!(id_check, id);

    match (service_idx, method_idx) {
        (ECHO, ECHO_UNARY) => {
            assert_ne!(flags & FLAG_FIRST_IS_END, 0);
            assert_ne!(flags & FLAG_IROH_AUTHENTICATED, 0);
            assert_eq!(session_key, 0);
            table.try_respond(id, &request, &ok).unwrap();
            // Terminal: the call is gone.
            assert_eq!(table.try_respond(id, b"x", &ok), Err(SubmitError::NotFound));
        }
        (ECHO, COUNT) | (COUNTER, SESSION_COUNT) => {
            let n = request[0];
            for i in 0..n {
                send_parked(&table, id, &[i, session_key as u8], &events).unwrap();
            }
            table.try_finish(id, &ok).unwrap();
        }
        (ECHO, SUM) => {
            assert_eq!(flags & FLAG_FIRST_IS_END, 0);
            let mut total: u32 = request.iter().map(|b| *b as u32).sum();
            while let Some(more) = rt.block_on(table.recv(id)).unwrap_or(None) {
                total += more.iter().map(|b| *b as u32).sum::<u32>();
            }
            match table.try_respond(id, &total.to_le_bytes(), &ok) {
                Ok(()) => {}
                // A corrupt request frame: Rust already failed the call.
                Err(SubmitError::NotFound) => note(&observed, format!("sum:ended:{total}")),
                Err(e) => panic!("sum: {e}"),
            }
        }
        (ECHO, CHAT) => {
            let mut next = Some(request);
            while let Some(msg) = next {
                let mut out = b"re:".to_vec();
                out.extend(msg);
                send_parked(&table, id, &out, &events).unwrap();
                next = rt.block_on(table.recv(id)).unwrap();
            }
            table.try_finish(id, &ok).unwrap();
        }
        (ECHO, FLOOD) => {
            // Incompressible, so zstd cannot make the queue roomier.
            let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
            let mut frame = vec![0u8; FLOOD_FRAME];
            let mut fill = |frame: &mut Vec<u8>| {
                for b in frame.iter_mut() {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    *b = x as u8;
                }
            };
            let mut sent = 0usize;
            let required = loop {
                fill(&mut frame);
                match table.try_send(id, &frame) {
                    Ok(()) => sent += 1,
                    Err(SubmitError::QueueFull { required_len }) => break required_len,
                    Err(e) => panic!("flood: {e}"),
                }
                assert!(sent < 100_000, "the response queue never filled");
            };
            note(&observed, format!("flood:full:{sent}"));
            // While the reservation is pending, the producer stays parked.
            table.reserve(id, required).unwrap();
            assert_eq!(table.reserve(id, required), Err(SubmitError::Invalid));
            match events.recv_timeout(Duration::from_secs(20)) {
                Ok(DispatchEvent::ResponseWritable { call_id }) => assert_eq!(call_id, id),
                other => panic!("expected ResponseWritable, got {other:?}"),
            }
            // The held permit is exactly this frame's size: the retry fits.
            table.try_send(id, &frame).unwrap();
            sent += 1;
            for _ in 1..FLOOD_AFTER_FULL {
                fill(&mut frame);
                send_parked(&table, id, &frame, &events).unwrap();
                sent += 1;
            }
            loop {
                match table.try_finish(id, &ok) {
                    Err(SubmitError::QueueFull { required_len }) => {
                        table.reserve(id, required_len).unwrap();
                        let _ = events.recv_timeout(Duration::from_secs(20));
                    }
                    r => break r.unwrap(),
                }
            }
            note(&observed, format!("flood:sent:{sent}"));
        }
        (ECHO, HANG) => {
            table.try_send(id, b"first").unwrap();
            // Produce until the peer cancels.
            match events.recv_timeout(Duration::from_secs(20)) {
                Ok(DispatchEvent::Cancelled { reason, .. }) => {
                    assert_eq!(reason, CancelReason::Peer)
                }
                other => panic!("expected Cancelled, got {other:?}"),
            }
            assert!(table.is_cancelled(id));
            // No trailer after a peer cancel; the call is gone.
            assert_eq!(table.try_send(id, b"late"), Err(SubmitError::NotFound));
            note(&observed, "hang:cancelled");
        }
        (ECHO, PARK) => {
            // A unary call that waits for its cancel instead of answering.
            note(&observed, "park:started");
            match events.recv_timeout(Duration::from_secs(20)) {
                Ok(DispatchEvent::Cancelled { reason, .. }) => {
                    assert_eq!(reason, CancelReason::Peer)
                }
                other => panic!("expected Cancelled{{Peer}}, got {other:?}"),
            }
            assert!(table.is_cancelled(id));
            // No trailer after a peer cancel; the call is gone.
            assert_eq!(
                table.try_respond(id, b"late", &ok),
                Err(SubmitError::NotFound)
            );
            note(&observed, "park:cancelled");
        }
        (ECHO, SLOW) => {
            assert!(deadline_unix_ms > 0, "method default_timeout -> deadline");
            match events.recv_timeout(Duration::from_secs(20)) {
                Ok(DispatchEvent::Cancelled { reason, .. }) => {
                    assert_eq!(reason, CancelReason::Deadline)
                }
                other => panic!("expected Cancelled{{Deadline}}, got {other:?}"),
            }
            // Rust already sent DEADLINE_EXCEEDED; a late answer goes nowhere.
            assert_eq!(
                table.try_respond(id, b"late", &ok),
                Err(SubmitError::NotFound)
            );
            note(&observed, "slow:expired");
        }
        (ECHO, ADMIN) => {
            let role = attributes.get(ROLE_ATTRIBUTE).unwrap_or("").to_string();
            table.try_respond(id, role.as_bytes(), &ok).unwrap();
        }
        (ECHO, SMALL) => {
            table.try_respond(id, &request, &ok).unwrap();
        }
        (COUNTER, INCR) => {
            // Per-session state lives in the binding, keyed by session_key.
            static COUNTS: Mutex<Option<HashMap<u64, u8>>> = Mutex::new(None);
            let mut counts = COUNTS.lock().unwrap();
            let counts = counts.get_or_insert_with(HashMap::new);
            assert_ne!(session_key, 0);
            if flags & FLAG_NEW_SESSION != 0 {
                assert!(counts.insert(session_key, 0).is_none());
            }
            let n = counts.get_mut(&session_key).expect("NEW_SESSION first");
            *n += 1;
            table.try_respond(id, &[*n], &ok).unwrap();
        }
        other => panic!("unexpected call {other:?}"),
    }
}

// ── harness ─────────────────────────────────────────────────────────────────

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(30), f)
        .await
        .expect("operation timed out")
}

async fn wait_for_addr(n: &Node) {
    for _ in 0..50 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Setup {
    srv: AsterServer,
    binding: Binding,
}

async fn start() -> Setup {
    let (sink, consumer) = RingSink::new(64);
    let srv = AsterServer::builder()
        .config(cfg())
        .service(NativePing)
        .dispatch_sink(sink)
        .dynamic_service(echo_service())
        .unwrap()
        .dynamic_service(counter_service())
        .unwrap()
        .start()
        .await
        .unwrap();
    let binding = Binding::start(
        consumer,
        srv.call_table(),
        tokio::runtime::Handle::current(),
    );
    Setup { srv, binding }
}

async fn client(srv: &AsterServer) -> (Node, RpcConnection) {
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(srv.node()).await;
    wait_for_addr(&client).await;
    client.add_peer(srv.node()).unwrap();
    srv.node().add_peer(&client).unwrap();
    let conn = within(client.rpc_connect(&srv.id())).await.unwrap();
    (client, conn)
}

fn header(service: &str, method: &str, session_id: i32) -> StreamHeader {
    StreamHeader {
        service: service.into(),
        method: method.into(),
        version: 1,
        session_id,
        ..Default::default()
    }
}

fn echo(method: &str) -> StreamHeader {
    header("test.Echo", method, 0)
}

/// Incompressible bytes, so zstd cannot shrink a frame under a limit.
fn noise(n: usize) -> Vec<u8> {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn rpc_code(r: Result<Vec<u8>, Error>) -> (i32, String) {
    match r {
        Err(Error::Rpc { code, message, .. }) => (code, message),
        other => panic!("expected an RPC error, got {other:?}"),
    }
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capacity_one_ring_resumes_a_real_stalled_foreign_call() {
    let (sink, consumer) = RingSink::new_with_limit(1, 4);
    let srv = AsterServer::builder()
        .config(cfg())
        .dispatch_sink(sink)
        .dynamic_service(echo_service())
        .unwrap()
        .dynamic_service(counter_service())
        .unwrap()
        .start()
        .await
        .unwrap();
    let table = srv.call_table();
    let (_client, conn) = client(&srv).await;
    let pending = tokio::spawn(async move { conn.unary(&echo("echo"), b"stalled".to_vec()).await });

    // Leave the real Call event in the capacity-one dispatch path. Starting
    // the consumer later must release its handoff and let the handler answer.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !pending.is_finished(),
        "call completed before its event was drained"
    );
    let binding = Binding::start(consumer, table, tokio::runtime::Handle::current());
    let out = within(pending).await.unwrap().unwrap();
    assert_eq!(out, b"stalled");
    binding.assert_table_drains().await;

    drop(binding);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undersized_entity_budget_rejects_before_dispatch_without_stalling() {
    let (sink, consumer) = RingSink::new_with_limit(1, 1);
    let srv = AsterServer::builder()
        .config(cfg())
        .dispatch_sink(sink)
        .dynamic_service(echo_service())
        .unwrap()
        .dynamic_service(counter_service())
        .unwrap()
        .start()
        .await
        .unwrap();
    let binding = Binding::start(
        consumer,
        srv.call_table(),
        tokio::runtime::Handle::current(),
    );
    let (_client, conn) = client(&srv).await;
    let active = conn.clone();
    let pending = tokio::spawn(async move { active.unary(&echo("park"), vec![1]).await });
    binding
        .until("first call holds the call admission", |observed| {
            observed.notes.iter().any(|note| note == "park:started")
        })
        .await;

    // A new session id is refused by the already-full call pool before its
    // fallible session factory runs or the id enters the session graveyard.
    let mut rejected = within(conn.server_stream(&header("test.Counter", "count", 1), vec![1]))
        .await
        .unwrap();
    let error = within(rejected.collect()).await.unwrap_err();
    let (code, message) = rpc_code(Err(error));
    assert_eq!(code, StatusCode::ResourceExhausted.as_i32());
    assert!(message.contains("admission limit"));
    assert_eq!(binding.observed().sessions_opened.len(), 0);

    pending.abort();
    let _ = pending.await;
    binding
        .until("first call cancellation releases admission", |observed| {
            observed.notes.iter().any(|note| note == "park:cancelled")
        })
        .await;
    let mut stream = within(conn.server_stream(&header("test.Counter", "count", 1), vec![1]))
        .await
        .unwrap();
    assert_eq!(within(stream.collect()).await.unwrap().len(), 1);
    assert_eq!(binding.observed().sessions_opened.len(), 1);
    binding.assert_table_drains().await;

    drop(binding);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoning_all_calls_returns_a_terminal_error_and_releases_admission() {
    let (sink, mut consumer) = RingSink::new_with_limit(1, 1);
    let srv = AsterServer::builder()
        .config(cfg())
        .dispatch_sink(sink)
        .dynamic_service(echo_service())
        .unwrap()
        .start()
        .await
        .unwrap();
    let table = srv.call_table();
    let (_client, conn) = client(&srv).await;
    let active = conn.clone();
    let pending = tokio::spawn(async move { active.unary(&echo("park"), vec![1]).await });
    consumer.ready().await;
    let mut call_id = None;
    consumer.drain(1, |event| match event {
        DispatchEvent::Call(call) => {
            call_id = Some(call.handle.detach());
        }
        other => panic!("expected Call, got {other:?}"),
    });
    assert!(call_id.is_some());

    assert_eq!(table.abandon_all(), 1);
    let (code, message) = rpc_code(within(pending).await.unwrap());
    assert_eq!(code, StatusCode::Internal.as_i32());
    assert!(message.contains("handler dropped call"));
    assert!(table.is_empty());
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_four_patterns_over_a_foreign_service() {
    let Setup { srv, binding } = start().await;
    let (_client, conn) = client(&srv).await;

    // Unary.
    let out = within(conn.unary(&echo("echo"), b"hello".to_vec()))
        .await
        .unwrap();
    assert_eq!(out, b"hello");

    // Unary, compressible and above the threshold: the binding sees the
    // inflated payload, and the response is compressed by Rust.
    let big = vec![b'z'; 64 * 1024];
    let out = within(conn.unary(&echo("echo"), big.clone()))
        .await
        .unwrap();
    assert_eq!(out, big);

    // Server streaming.
    let mut stream = within(conn.server_stream(&echo("count"), vec![5]))
        .await
        .unwrap();
    let items = within(stream.collect()).await.unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!(items[4], vec![4, 0]);

    // Client streaming.
    let out = within(conn.client_stream(&echo("sum"), vec![vec![1, 2], vec![3], vec![4, 5]]))
        .await
        .unwrap();
    assert_eq!(u32::from_le_bytes(out.try_into().unwrap()), 15);

    // Bidi, interleaved.
    let (sink, mut responses) = within(conn.bidi_open(&echo("chat"))).await.unwrap();
    for word in ["a", "bb", "ccc"] {
        within(sink.send(word.as_bytes().to_vec())).await.unwrap();
        let got = within(responses.next()).await.unwrap().unwrap();
        assert_eq!(got, format!("re:{word}").into_bytes());
    }
    within(sink.finish()).await.unwrap();
    assert!(within(responses.next()).await.is_none(), "OK trailer");

    // Routing failures never reach the binding.
    let (code, _) = rpc_code(within(conn.unary(&echo("nope"), vec![1])).await);
    assert_eq!(code, StatusCode::Unimplemented.as_i32());
    let (code, _) = rpc_code(within(conn.unary(&header("test.Echo", "echo", 7), vec![1])).await);
    assert_eq!(
        code,
        StatusCode::FailedPrecondition.as_i32(),
        "SHARED + sid"
    );

    binding.assert_table_drains().await;
    assert_eq!(binding.observed().calls.len(), 5, "refusals never reach it");
    drop(binding);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_scoped_foreign_service_follows_the_connection_rules() {
    let Setup { srv, binding } = start().await;
    let (client_a, a) = client(&srv).await;
    let (_client_b, b) = client(&srv).await;
    let incr = |sid| header("test.Counter", "incr", sid);

    for expect in 1..=3u8 {
        let out = within(a.unary(&incr(1), vec![0])).await.unwrap();
        assert_eq!(out, vec![expect]);
    }
    assert_eq!(within(a.unary(&incr(2), vec![0])).await.unwrap(), vec![1]);
    // Same id on another connection: its own session, never A's.
    assert_eq!(within(b.unary(&incr(1), vec![0])).await.unwrap(), vec![1]);
    assert_eq!(within(a.unary(&incr(1), vec![0])).await.unwrap(), vec![4]);
    // Streaming on a session, carrying the session key.
    let mut s = within(a.server_stream(&header("test.Counter", "count", 2), vec![2]))
        .await
        .unwrap();
    assert_eq!(within(s.collect()).await.unwrap().len(), 2);

    // §7.5 refusals, decided in Rust.
    let (code, _) = rpc_code(within(a.unary(&incr(0), vec![0])).await);
    assert_eq!(
        code,
        StatusCode::FailedPrecondition.as_i32(),
        "SESSION, sid 0"
    );

    let opened = binding.observed().sessions_opened.clone();
    assert_eq!(opened.len(), 3, "A:1, A:2, B:1 -- NEW_SESSION once each");

    // Closing A's connection reaps A's two sessions, then reports the close.
    client_a.shutdown().await;
    binding
        .until("A's sessions closed", |o| {
            o.sessions_closed.len() == 2 && !o.connections_closed.is_empty()
        })
        .await;
    {
        let o = binding.observed();
        assert!(o.sessions_closed.iter().all(|k| opened.contains(k)));
    }
    // B's session is untouched.
    assert_eq!(within(b.unary(&incr(1), vec![0])).await.unwrap(), vec![2]);

    binding.assert_table_drains().await;
    drop(binding);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_cancel_reaches_a_streaming_handler() {
    let Setup { srv, binding } = start().await;
    let (_client, conn) = client(&srv).await;

    let (send, recv) = within(conn.core_connection().open_streaming_substream())
        .await
        .unwrap();
    let hdr = aster::rpc::encode_stream_header(&echo("hang")).unwrap();
    send.write_all(encode_frame(&hdr, FLAG_HEADER).unwrap())
        .await
        .unwrap();
    send.write_all(encode_frame(&[1], FLAG_END_STREAM).unwrap())
        .await
        .unwrap();
    // Read the first item so the call is certainly live, then cancel.
    let len = within(recv.read_exact(4)).await.unwrap();
    let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
    let body = within(recv.read_exact(len)).await.unwrap();
    assert_eq!(&body[1..], b"first");
    send.write_all(encode_frame(&[], FLAG_CANCEL).unwrap())
        .await
        .unwrap();

    binding
        .until("hang cancelled", |o| {
            o.notes.iter().any(|n| n == "hang:cancelled")
        })
        .await;
    assert!(binding
        .observed()
        .cancelled
        .iter()
        .any(|(_, r, m)| *r == CancelReason::Peer && *m == HANG));
    binding.assert_table_drains().await;

    // The connection survives the cancel.
    let out = within(conn.unary(&echo("echo"), b"after".to_vec()))
        .await
        .unwrap();
    assert_eq!(out, b"after");
    binding.assert_table_drains().await;
    drop(binding);
    srv.shutdown().await;
}

/// A client cancel reaches a unary handler as `Cancelled{Peer}` (the reactor's
/// cancel signal, no task per call); the stream is poisoned, the connection
/// lives on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_cancel_reaches_a_unary_handler() {
    let Setup { srv, binding } = start().await;
    let (_client, conn) = client(&srv).await;

    let (send, _recv) = within(conn.core_connection().open_streaming_substream())
        .await
        .unwrap();
    let hdr = aster::rpc::encode_stream_header(&echo("park")).unwrap();
    send.write_all(encode_frame(&hdr, FLAG_HEADER).unwrap())
        .await
        .unwrap();
    send.write_all(encode_frame(&[1], FLAG_END_STREAM).unwrap())
        .await
        .unwrap();
    binding
        .until("park started", |o| {
            o.notes.iter().any(|n| n == "park:started")
        })
        .await;
    send.write_all(encode_frame(&[], FLAG_CANCEL).unwrap())
        .await
        .unwrap();

    binding
        .until("park cancelled", |o| {
            o.notes.iter().any(|n| n == "park:cancelled")
        })
        .await;
    assert!(binding
        .observed()
        .cancelled
        .iter()
        .any(|(_, r, m)| *r == CancelReason::Peer && *m == PARK));
    binding.assert_table_drains().await;

    // The connection survives the cancel.
    let out = within(conn.unary(&echo("echo"), b"after".to_vec()))
        .await
        .unwrap();
    assert_eq!(out, b"after");
    binding.assert_table_drains().await;
    drop(binding);
    srv.shutdown().await;
}

/// A unary call in flight when the whole peer goes away is cancelled too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_close_cancels_an_in_flight_unary_call() {
    let Setup { srv, binding } = start().await;
    let (client, conn) = client(&srv).await;

    let pending = tokio::spawn(async move { conn.unary(&echo("park"), vec![1]).await });
    binding
        .until("park started", |o| {
            o.notes.iter().any(|n| n == "park:started")
        })
        .await;
    client.shutdown().await;
    let _ = pending.await;

    binding
        .until("park cancelled", |o| {
            o.notes.iter().any(|n| n == "park:cancelled") && !o.connections_closed.is_empty()
        })
        .await;
    binding.assert_table_drains().await;
    drop(binding);
    srv.shutdown().await;
}

/// R10: a later request frame that does not inflate fails the call with
/// `INVALID_ARGUMENT` (sent by Rust), and the binding learns it through
/// `recv` ending and `Cancelled { InvalidRequest }`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_later_request_frame_fails_the_call() {
    let Setup { srv, binding } = start().await;
    let (_client, conn) = client(&srv).await;

    let (send, recv) = within(conn.core_connection().open_streaming_substream())
        .await
        .unwrap();
    let hdr = aster::rpc::encode_stream_header(&echo("sum")).unwrap();
    send.write_all(encode_frame(&hdr, FLAG_HEADER).unwrap())
        .await
        .unwrap();
    send.write_all(encode_frame(&[1, 2], 0).unwrap())
        .await
        .unwrap();
    send.write_all(encode_frame(b"not zstd", FLAG_COMPRESSED).unwrap())
        .await
        .unwrap();
    // The only frame back is the trailer.
    let len = within(recv.read_exact(4)).await.unwrap();
    let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
    let body = within(recv.read_exact(len)).await.unwrap();
    assert_ne!(body[0] & FLAG_TRAILER, 0, "trailer, not data");
    let status = aster::rpc::codec::decode_rpc_status(&body[1..]).unwrap();
    assert_eq!(
        status.code,
        StatusCode::InvalidArgument.as_i32(),
        "{status:?}"
    );
    assert!(status.message.starts_with("request payload:"), "{status:?}");

    binding
        .until("sum ended", |o| o.notes.iter().any(|n| n == "sum:ended:3"))
        .await;
    // `Cancelled` reaches the binding's event thread, not the handler thread
    // that saw `recv` end, so it may land after the note.
    binding
        .until("sum cancelled as InvalidRequest", |o| {
            o.cancelled
                .iter()
                .any(|(_, r, m)| *r == CancelReason::InvalidRequest && *m == SUM)
        })
        .await;
    binding.assert_table_drains().await;

    // The connection survives.
    let out = within(conn.unary(&echo("echo"), b"after".to_vec()))
        .await
        .unwrap();
    assert_eq!(out, b"after");
    drop(binding);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_close_cancels_in_flight_streaming_calls() {
    let Setup { srv, binding } = start().await;
    let (client, conn) = client(&srv).await;

    let mut stream = within(conn.server_stream(&echo("hang"), vec![1]))
        .await
        .unwrap();
    assert_eq!(within(stream.next()).await.unwrap().unwrap(), b"first");
    // The whole peer goes away mid-call.
    client.shutdown().await;

    binding
        .until("hang cancelled", |o| {
            o.notes.iter().any(|n| n == "hang:cancelled") && !o.connections_closed.is_empty()
        })
        .await;
    binding.assert_table_drains().await;
    drop(binding);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_deadline_is_enforced_for_foreign_calls() {
    let Setup { srv, binding } = start().await;
    let (_client, conn) = client(&srv).await;

    let (code, msg) = rpc_code(within(conn.unary(&echo("slow"), vec![1])).await);
    assert_eq!(code, StatusCode::DeadlineExceeded.as_i32(), "{msg}");
    binding
        .until("slow expired", |o| {
            o.notes.iter().any(|n| n == "slow:expired")
        })
        .await;
    assert!(binding
        .observed()
        .cancelled
        .iter()
        .any(|(_, r, m)| *r == CancelReason::Deadline && *m == SLOW));
    binding.assert_table_drains().await;
    drop(binding);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_full_parks_on_a_reservation_until_writable() {
    let Setup { srv, binding } = start().await;
    let (_client, conn) = client(&srv).await;

    let mut stream = within(conn.server_stream(&echo("flood"), vec![1]))
        .await
        .unwrap();
    // Read nothing until the handler has hit QUEUE_FULL.
    binding
        .until("flood full", |o| {
            o.notes.iter().any(|n| n.starts_with("flood:full"))
        })
        .await;
    let items = within(stream.collect()).await.unwrap();
    assert!(items.iter().all(|i| i.len() == FLOOD_FRAME));
    binding
        .until("flood done", |o| {
            o.notes.iter().any(|n| n.starts_with("flood:sent"))
        })
        .await;
    let sent: usize = binding
        .observed()
        .notes
        .iter()
        .find_map(|n| n.strip_prefix("flood:sent:"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(items.len(), sent, "every frame arrived, none lost on retry");
    binding.assert_table_drains().await;
    drop(binding);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_call_answers_internal() {
    let Setup { srv, binding } = start().await;
    let (_client, conn) = client(&srv).await;
    let (code, msg) = rpc_code(within(conn.unary(&echo("drop"), vec![1])).await);
    assert_eq!(code, StatusCode::Internal.as_i32());
    assert_eq!(msg, "handler dropped call");
    binding.assert_table_drains().await;
    drop(binding);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate3_and_frame_limits_apply_to_foreign_methods() {
    let Setup { srv, binding } = start().await;
    let (client, conn) = client(&srv).await;

    // `requires` is enforced in Rust before the binding sees anything.
    let (code, _) = rpc_code(within(conn.unary(&echo("admin"), vec![1])).await);
    assert_eq!(code, StatusCode::PermissionDenied.as_i32());
    srv.attributes().set_role(client.id().as_str(), "admin");
    let out = within(conn.unary(&echo("admin"), vec![1])).await.unwrap();
    assert_eq!(out, b"admin", "the binding sees the attributes Gate 3 saw");

    // Per-method frame limit, from `MethodLimits`, installed in the reactor.
    let out = within(conn.unary(&echo("small"), vec![7; 512]))
        .await
        .unwrap();
    assert_eq!(out, vec![7; 512]);
    let over = within(conn.unary(&echo("small"), noise(4096))).await;
    assert!(over.is_err(), "a 4 KiB frame on a 1 KiB method is refused");
    let small_calls = binding
        .observed()
        .calls
        .iter()
        .filter(|(s, m, _)| *s == ECHO && *m == SMALL)
        .count();
    assert_eq!(
        small_calls, 1,
        "the oversized call never reached the binding"
    );

    binding.assert_table_drains().await;
    drop(binding);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_foreign_and_baseline_services_side_by_side() {
    let Setup { srv, binding } = start().await;
    let (_client, conn) = client(&srv).await;

    let native = within(conn.unary(&header("test.Native", "ping", 0), b"x".to_vec()))
        .await
        .unwrap();
    assert_eq!(native, b"native:x");
    let foreign = within(conn.unary(&echo("echo"), b"y".to_vec()))
        .await
        .unwrap();
    assert_eq!(foreign, b"y");
    let identity = within(NodeInfoClient::new(conn.clone()).describe())
        .await
        .unwrap();
    assert_eq!(identity.node_id, srv.id().to_string());

    // Only the foreign call reached the binding.
    assert_eq!(binding.observed().calls, vec![(ECHO, ECHO_UNARY, 5)]);
    binding.assert_table_drains().await;
    drop(binding);
    srv.shutdown().await;
}

#[tokio::test]
async fn a_dynamic_service_cannot_shadow_another() {
    let (sink, _consumer) = RingSink::new(8);
    let mut dup = echo_service();
    dup.name = "test.Native".into();
    let err = AsterServer::builder()
        .config(cfg())
        .service(NativePing)
        .dispatch_sink(sink)
        .dynamic_service(dup)
        .unwrap()
        .start()
        .await
        .err()
        .expect("collision refused");
    assert!(err.to_string().contains("already registered"), "{err}");

    let err = AsterServer::builder()
        .config(cfg())
        .dynamic_service(echo_service())
        .unwrap()
        .start()
        .await
        .err()
        .expect("no sink refused");
    assert!(err.to_string().contains("dispatch_sink"), "{err}");
}
