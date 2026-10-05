//! Cross-binding interop smoke: the Rust side of `tests/interop/`.
//!
//! Shared contract (must stay identical in every other language's program);
//! the full contract and the server semantics are documented in `contract.rs`
//! (shared with `benchmarks/dispatch/driver.rs`): `InteropEcho v1` (echo,
//! generate, ingest, chat, hold, cancelled, traceEcho) plus the session-scoped
//! `InteropSession v1` (add), defined here.
//!
//! Expose mini-matrix (every language serves, Rust and Python consume):
//! `expose-server` runs a node on ALPN `aster-expose/interop/1` exposing the
//! HTTP service `interop-http`, authorizing every peer:
//!
//! - `GET /hello` -> 200, body `hello from <lang>`
//! - `POST /echo` -> 200, the request body streamed back
//! - every response carries `x-server-lang: <lang>`
//!
//! `expose-client <ticket>` relays to it with the streaming relay.
//!
//! Usage (built as the `interop_echo` example of the `aster` crate, features
//! `rpc` + `expose`):
//!
//! ```text
//! interop_echo contract           # prints CONTRACT_ID <hex> and SESSION_CONTRACT_ID <hex>
//! interop_echo server             # prints the ids, then ADDRESS aster1…, serves until stdin closes
//! interop_echo client <aster1…>   # runs the RPC cases, prints one CASE line each, exit 0 on success
//! interop_echo expose-server      # prints ADDRESS aster1…, serves the expose service until stdin closes
//! interop_echo expose-client <aster1…>   # streaming-relay cases against an expose-server
//! ```
//!
//! Open gate, relay disabled, loopback bind: works offline.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aster::expose::{
    body_channel, relay_request_streaming, AuthorizeFn, BoxFut, HttpHandler, PeerContext,
    RelayBody, ResponseBody,
};
use aster::rpc::{
    contract_id, PayloadRegistry, RpcConnection, RpcSession, ScopeKind, SerializationMode,
    StatusCode, StreamHeader, WireField,
};
use aster::{AsterConfig, Credential, Node, RelayMode, Result, Ticket};
use http_body_util::BodyExt;

#[path = "contract.rs"]
mod contract;
use contract::{EchoImpl, EchoReply, EchoRequest, InteropEchoClient, InteropEchoServer};

const EXPOSE_ALPN: &[u8] = b"aster-expose/interop/1";
const EXPOSE_SERVICE: &str = "interop-http";

/// Session-scoped service (semantics in `contract.rs`). The macro always emits
/// a SHARED contract, so the contract id flips `scoped` itself and the client
/// below is hand-written to stamp a session id.
#[aster::service(name = "InteropSession", version = 1)]
pub trait InteropSession {
    async fn add(&self, req: EchoRequest) -> Result<EchoReply>;
}

#[derive(Default)]
struct SessionImpl {
    total: AtomicI32,
    calls: AtomicI32,
}

#[aster::rpc::async_trait]
impl InteropSession for SessionImpl {
    async fn add(&self, req: EchoRequest) -> Result<EchoReply> {
        let total = self
            .total
            .fetch_add(req.count, Ordering::SeqCst)
            .wrapping_add(req.count);
        let calls = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(EchoReply {
            message: format!("calls={calls}"),
            count: total,
            data: vec![],
            server_lang: "rust".into(),
        })
    }
}

fn session_contract_hex() -> String {
    let mut c = InteropSessionClient::contract();
    c.scoped = ScopeKind::Session;
    contract_id(&c)
}

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

fn contract_hex() -> String {
    contract_id(&InteropEchoClient::contract())
}

/// The shared cases. Must match `cases()` in the other drivers.
fn cases(client_lang: &str) -> Vec<(&'static str, EchoRequest)> {
    let small = EchoRequest {
        message: format!("hello from {client_lang}"),
        count: 41,
        data: vec![0, 1, 2, 255],
    };
    // > 4096 bytes on the wire and highly compressible: exercises zstd.
    let big = EchoRequest {
        message: "héllo ✓ ".repeat(200),
        count: -123_456,
        data: (0..20_000u32).map(|i| (i % 251) as u8).collect(),
    };
    // A single string field longer than 4095 bytes (Fory string-length edge).
    let long_string = EchoRequest {
        message: "x".repeat(5000),
        count: i32::MAX - 1,
        data: vec![],
    };
    vec![
        ("small", small),
        ("zstd", big),
        ("long_string", long_string),
    ]
}

async fn wait_for_addr(n: &Node) {
    for _ in 0..100 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Block until the driver closes our stdin (or kills us).
async fn wait_stdin_eof() {
    tokio::task::spawn_blocking(|| {
        let mut sink = String::new();
        while std::io::stdin()
            .read_line(&mut sink)
            .map(|n| n > 0)
            .unwrap_or(false)
        {
            sink.clear();
        }
    })
    .await
    .ok();
}

/// Start the observability pipeline from the environment (`run.py` points the
/// OTLP trace exporter at a closed local port): a tracer provider is what makes
/// the dispatch context carry a trace id and a caller's `traceparent` parent
/// the client span. No collector is needed. Kept alive by the caller.
#[cfg(feature = "otel")]
fn obs_init() -> Option<aster_otel::ObservabilityHandle> {
    match aster_otel::init(aster_otel::ObservabilityConfig::default()) {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("observability init failed: {e}");
            None
        }
    }
}

async fn serve() -> Result<()> {
    #[cfg(feature = "otel")]
    let _obs = obs_init();
    let srv = aster::rpc::AsterServer::builder()
        .config(cfg())
        .service(InteropEchoServer::new(EchoImpl))
        .register_session(|| InteropSessionServer::new(SessionImpl::default()))
        .builtin_node_info(false)
        .start()
        .await?;
    wait_for_addr(srv.node()).await;
    println!("ADDRESS {}", srv.address()?);
    wait_stdin_eof().await;
    srv.shutdown().await;
    Ok(())
}

type Verdict = std::result::Result<String, String>;

fn report(name: &str, verdict: Verdict, all_ok: &mut bool) {
    match verdict {
        Ok(server_lang) => println!("CASE {name} OK server_lang={server_lang}"),
        Err(why) => {
            *all_ok = false;
            println!("CASE {name} FAIL {why}");
        }
    }
}

async fn connect(node: &Node, ticket: &str) -> Result<aster::Connection> {
    let t = Ticket::from_base58(ticket)?;
    let id = node.add_ticket_addr(&t)?;
    node.connect(&id, EXPOSE_ALPN).await
}

async fn run_client(ticket: &str) -> bool {
    #[cfg(feature = "otel")]
    let _obs = obs_init();
    let node = match Node::start(cfg()).await {
        Ok(n) => n,
        Err(e) => {
            println!("CASE connect FAIL node start: {e}");
            return false;
        }
    };
    let conn: RpcConnection = match async {
        let t = Ticket::from_base58(ticket)?;
        let id = node.add_ticket_addr(&t)?;
        node.rpc_connect(&id).await
    }
    .await
    {
        Ok(c) => c,
        Err(e) => {
            println!("CASE connect FAIL {e}");
            return false;
        }
    };
    let client = InteropEchoClient::new(conn.clone());
    let mut all_ok = true;
    for (name, req) in cases("rust") {
        let res = tokio::time::timeout(Duration::from_secs(15), client.echo(req.clone())).await;
        let verdict = match res {
            Err(_) => Err("timeout".to_string()),
            Ok(Err(e)) => Err(format!("rpc error: {e}")),
            Ok(Ok(reply)) => check(&req, &reply),
        };
        report(name, verdict, &mut all_ok);
    }
    report("server_stream", stream_case(&client).await, &mut all_ok);
    report("client_stream", ingest_case(&client).await, &mut all_ok);
    report("bidi", chat_case(&client).await, &mut all_ok);
    report("deadline", deadline_case(&conn).await, &mut all_ok);
    report("cancel", cancel_case(&client).await, &mut all_ok);
    report("session", session_case(&conn).await, &mut all_ok);
    report("trace", trace_case(&client).await, &mut all_ok);
    node.shutdown().await;
    all_ok
}

/// `generate` with count 3: three replies, `count` 0..3, message/data echoed.
/// Must match the other drivers.
async fn stream_case(client: &InteropEchoClient) -> Verdict {
    let req = EchoRequest {
        message: "stream".into(),
        count: 3,
        data: vec![7, 8, 9],
    };
    let fut = async {
        let mut s = client.generate(req.clone()).await?;
        s.collect().await
    };
    let replies = match tokio::time::timeout(Duration::from_secs(15), fut).await {
        Err(_) => return Err("timeout".into()),
        Ok(Err(e)) => return Err(format!("rpc error: {e}")),
        Ok(Ok(r)) => r,
    };
    if replies.len() != 3 {
        return Err(format!("{} replies != 3", replies.len()));
    }
    for (i, r) in replies.iter().enumerate() {
        if r.count != i as i32 || r.message != req.message || r.data != req.data {
            return Err(format!("reply {i} mismatch: {replies:?}"));
        }
    }
    Ok(replies[0].server_lang.clone())
}

/// `ingest` with three requests: one reply, count 3, last message echoed.
async fn ingest_case(client: &InteropEchoClient) -> Verdict {
    let reqs: Vec<EchoRequest> = (0..3)
        .map(|i| EchoRequest {
            message: format!("m{i}"),
            count: i,
            data: vec![i as u8],
        })
        .collect();
    let reply = match tokio::time::timeout(Duration::from_secs(15), client.ingest(reqs)).await {
        Err(_) => return Err("timeout".into()),
        Ok(Err(e)) => return Err(format!("rpc error: {e}")),
        Ok(Ok(r)) => r,
    };
    if reply.count != 3 || reply.message != "m2" {
        return Err(format!("reply mismatch: {reply:?}"));
    }
    if reply.server_lang.is_empty() {
        return Err("empty server_lang".into());
    }
    Ok(reply.server_lang)
}

/// `chat`, interleaved: send `c0`, read its reply, send `c1`, read its reply,
/// send `c2`, read its reply, then end the request stream and expect the
/// response stream to end. Reply `i` echoes request `i` with `count = i`.
async fn chat_case(client: &InteropEchoClient) -> Verdict {
    let fut = async {
        let (call, mut replies) = client
            .chat_streaming()
            .await
            .map_err(|e| format!("open: {e}"))?;
        let mut lang = String::new();
        for i in 0..3i32 {
            let req = EchoRequest {
                message: format!("c{i}"),
                count: 100 + i,
                data: vec![i as u8, 9],
            };
            call.send(&req)
                .await
                .map_err(|e| format!("send {i}: {e}"))?;
            let r = match replies.recv().await {
                Some(Ok(r)) => r,
                Some(Err(e)) => return Err(format!("reply {i}: {e}")),
                None => return Err(format!("stream ended before reply {i}")),
            };
            if r.count != i || r.message != req.message || r.data != req.data {
                return Err(format!("reply {i} mismatch: {r:?}"));
            }
            lang = r.server_lang;
        }
        call.finish().await.map_err(|e| format!("finish: {e}"))?;
        match replies.recv().await {
            None => {}
            Some(Ok(r)) => return Err(format!("unexpected extra reply {r:?}")),
            Some(Err(e)) => return Err(format!("end of stream: {e}")),
        }
        if lang.is_empty() {
            return Err("empty server_lang".into());
        }
        Ok(lang)
    };
    tokio::time::timeout(Duration::from_secs(15), fut)
        .await
        .map_err(|_| "timeout".to_string())?
}

fn probe() -> EchoRequest {
    EchoRequest {
        message: "probe".into(),
        ..Default::default()
    }
}

/// The known trace the `trace` case sends under.
const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

/// `traceEcho` called inside a `tracing` span whose OpenTelemetry parent is a
/// remote, sampled span context with a known trace id: the client span joins
/// that trace and injects `traceparent`, and the server must report the same
/// trace id from its dispatch context.
#[cfg(feature = "otel")]
async fn trace_case(client: &InteropEchoClient) -> Verdict {
    use aster_otel::opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use aster_otel::opentelemetry::Context;
    use aster_otel::OpenTelemetrySpanExt;
    use tracing::Instrument;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        | 1;
    let sc = SpanContext::new(
        TraceId::from_hex(TRACE_ID).map_err(|e| e.to_string())?,
        SpanId::from_bytes(nanos.to_be_bytes()),
        TraceFlags::SAMPLED,
        true,
        TraceState::NONE,
    );
    // Target under `aster` so the default span filter (`aster=info`) enables it.
    let span = tracing::info_span!(target: "aster::interop", "interop.trace");
    let _ = span.set_parent(Context::new().with_remote_span_context(sc));
    let reply = tokio::time::timeout(
        Duration::from_secs(15),
        client.trace_echo(probe()).instrument(span),
    )
    .await
    .map_err(|_| "traceEcho(): timeout".to_string())?
    .map_err(|e| format!("traceEcho(): {e}"))?;
    if reply.message != TRACE_ID {
        return Err(format!(
            "server saw trace id {:?}, want {TRACE_ID}",
            reply.message
        ));
    }
    Ok(reply.server_lang)
}

#[cfg(not(feature = "otel"))]
async fn trace_case(_client: &InteropEchoClient) -> Verdict {
    Err("built without the `otel` feature".into())
}

/// The server's count of `hold` calls that observed a cancellation.
async fn cancelled_count(client: &InteropEchoClient) -> std::result::Result<i32, String> {
    match tokio::time::timeout(Duration::from_secs(10), client.cancelled(probe())).await {
        Err(_) => Err("cancelled(): timeout".into()),
        Ok(Err(e)) => Err(format!("cancelled(): {e}")),
        Ok(Ok(r)) => Ok(r.count),
    }
}

/// Poll `cancelled` until it reaches `want` (the server notices a cancel
/// asynchronously) or 5 s pass.
async fn expect_server_cancel(
    client: &InteropEchoClient,
    want: i32,
) -> std::result::Result<(), String> {
    let mut seen = 0;
    for _ in 0..50 {
        seen = cancelled_count(client).await?;
        if seen >= want {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "server never observed the cancellation (count {seen}, want {want})"
    ))
}

/// `hold` with a 1 s client deadline: the call must fail DEADLINE_EXCEEDED
/// (1 s is the smallest deadline every client can express) and the server must
/// observe a cancellation.
async fn deadline_case(conn: &RpcConnection) -> Verdict {
    let plain = InteropEchoClient::new(conn.clone());
    let before = cancelled_count(&plain).await?;
    let client = InteropEchoClient::new(conn.clone().with_deadline(Duration::from_secs(1)));
    let started = std::time::Instant::now();
    let res = tokio::time::timeout(Duration::from_secs(15), client.hold(probe())).await;
    match res {
        Err(_) => return Err("hold: no deadline error within 15 s".into()),
        Ok(Ok(r)) => return Err(format!("hold returned {r:?} instead of an error")),
        Ok(Err(e)) => match e.rpc_code() {
            Some(StatusCode::DeadlineExceeded) => {}
            other => {
                return Err(format!(
                    "hold: expected DEADLINE_EXCEEDED, got {other:?}: {e}"
                ))
            }
        },
    }
    if started.elapsed() > Duration::from_secs(10) {
        return Err(format!("deadline took {:?}", started.elapsed()));
    }
    expect_server_cancel(&plain, before + 1).await?;
    Ok("rust".into())
}

/// `hold`, cancelled by the client after 300 ms (dropping the call future
/// cancels it): the server must observe a cancellation.
async fn cancel_case(client: &InteropEchoClient) -> Verdict {
    let before = cancelled_count(client).await?;
    if tokio::time::timeout(Duration::from_millis(300), client.hold(probe()))
        .await
        .is_ok()
    {
        return Err("hold returned before the client cancelled it".into());
    }
    expect_server_cancel(client, before + 1).await?;
    Ok("rust".into())
}

/// One `InteropSession.add` call in `session`.
async fn session_add(
    session: &RpcSession,
    req_fory: &aster::rpc::Fory,
    reply_fory: &aster::rpc::Fory,
    count: i32,
) -> std::result::Result<EchoReply, String> {
    let header = StreamHeader {
        service: "InteropSession".into(),
        method: "add".into(),
        version: 1,
        serialization_mode: SerializationMode::Xlang.as_i8(),
        ..Default::default()
    };
    let req = EchoRequest {
        message: "add".into(),
        count,
        data: vec![],
    };
    let bytes = req_fory
        .serialize(&req)
        .map_err(|e| format!("encode: {e}"))?;
    let out = tokio::time::timeout(Duration::from_secs(15), session.unary(&header, bytes))
        .await
        .map_err(|_| "add: timeout".to_string())?
        .map_err(|e| format!("add: {e}"))?;
    reply_fory
        .deserialize::<EchoReply>(&out)
        .map_err(|e| format!("decode: {e}"))
}

/// Two sessions on one connection: each keeps its own running state across
/// calls, and neither sees the other's.
async fn session_case(conn: &RpcConnection) -> Verdict {
    let fory_for = |register: fn(&mut PayloadRegistry)| {
        let mut reg = PayloadRegistry::new();
        register(&mut reg);
        reg.into_fory()
    };
    let req_fory = fory_for(<EchoRequest as WireField>::register_payload);
    let reply_fory = fory_for(<EchoReply as WireField>::register_payload);
    let a = conn
        .open_session()
        .map_err(|e| format!("open session: {e}"))?;
    let b = conn
        .open_session()
        .map_err(|e| format!("open session: {e}"))?;
    let steps: [(&RpcSession, i32, i32, &str); 5] = [
        (&a, 5, 5, "calls=1"),
        (&a, 7, 12, "calls=2"),
        (&b, 1, 1, "calls=1"),
        (&a, 0, 12, "calls=3"),
        (&b, 10, 11, "calls=2"),
    ];
    let mut lang = String::new();
    for (i, (s, add, total, calls)) in steps.iter().enumerate() {
        let r = session_add(s, &req_fory, &reply_fory, *add).await?;
        if r.count != *total || r.message != *calls {
            return Err(format!(
                "step {i}: got total {} {:?}, want total {total} {calls:?}",
                r.count, r.message
            ));
        }
        lang = r.server_lang;
    }
    if lang.is_empty() {
        return Err("empty server_lang".into());
    }
    Ok(lang)
}

fn check(req: &EchoRequest, reply: &EchoReply) -> std::result::Result<String, String> {
    if reply.message != req.message {
        return Err(format!(
            "message mismatch (len {} vs {})",
            reply.message.len(),
            req.message.len()
        ));
    }
    if reply.count != req.count.wrapping_add(1) {
        return Err(format!(
            "count {} != {}",
            reply.count,
            req.count.wrapping_add(1)
        ));
    }
    if reply.data != req.data {
        return Err(format!(
            "data mismatch (len {} vs {})",
            reply.data.len(),
            req.data.len()
        ));
    }
    if reply.server_lang.is_empty() {
        return Err("empty server_lang".into());
    }
    Ok(reply.server_lang.clone())
}

// ── expose mini-matrix ──────────────────────────────────────────────────────

fn expose_handler() -> HttpHandler {
    Arc::new(|req: http::Request<RelayBody>| {
        Box::pin(async move {
            let method = req.method().clone();
            let path = req.uri().path().to_owned();
            let builder = http::Response::builder().header("x-server-lang", "rust");
            let resp = match (method.as_str(), path.as_str()) {
                ("GET", "/hello") => builder.status(200).body(
                    http_body_util::Full::new(aster::expose::bytes::Bytes::from_static(
                        b"hello from rust",
                    ))
                    .map_err(|e: std::convert::Infallible| -> aster::expose::RelayError {
                        match e {}
                    })
                    .boxed_unsync(),
                ),
                // The request body streams straight back as the response body.
                ("POST", "/echo") => builder.status(200).body(req.into_body().boxed_unsync()),
                _ => builder.status(404).body(
                    http_body_util::Empty::new()
                        .map_err(|e: std::convert::Infallible| -> aster::expose::RelayError {
                            match e {}
                        })
                        .boxed_unsync(),
                ),
            };
            Ok(resp.unwrap())
        }) as BoxFut<anyhow::Result<http::Response<ResponseBody>>>
    })
}

async fn expose_serve() -> Result<()> {
    let node = Node::start_with_alpns(cfg(), vec![EXPOSE_ALPN.to_vec()]).await?;
    let allow_all: AuthorizeFn =
        Arc::new(|_ctx: PeerContext| Box::pin(async { Ok(()) }) as BoxFut<anyhow::Result<()>>);
    let expose = node.expose();
    expose.expose_http(EXPOSE_SERVICE, allow_all, expose_handler());
    let loop_node = node.clone();
    let loop_expose = expose.clone();
    tokio::spawn(async move {
        let _ = loop_node.serve_expose(&loop_expose, &[EXPOSE_ALPN]).await;
    });
    wait_for_addr(&node).await;
    println!(
        "ADDRESS {}",
        Ticket::from_node_addr(&node.addr(), Credential::Open)?.to_base58()?
    );
    wait_stdin_eof().await;
    node.shutdown().await;
    Ok(())
}

async fn expose_client(ticket: &str) -> bool {
    let node = match Node::start(cfg()).await {
        Ok(n) => n,
        Err(e) => {
            println!("CASE connect FAIL node start: {e}");
            return false;
        }
    };
    let conn = match connect(&node, ticket).await {
        Ok(c) => c,
        Err(e) => {
            println!("CASE connect FAIL {e}");
            return false;
        }
    };
    let mut all_ok = true;
    report(
        "expose_hello",
        tokio::time::timeout(Duration::from_secs(15), expose_hello(&conn))
            .await
            .unwrap_or_else(|_| Err("timeout".into())),
        &mut all_ok,
    );
    report(
        "expose_echo_stream",
        tokio::time::timeout(Duration::from_secs(15), expose_echo(&conn))
            .await
            .unwrap_or_else(|_| Err("timeout".into())),
        &mut all_ok,
    );
    node.shutdown().await;
    all_ok
}

fn lang_header(resp: &http::HeaderMap) -> Verdict {
    match resp.get("x-server-lang").and_then(|v| v.to_str().ok()) {
        Some(l) if !l.is_empty() => Ok(l.to_string()),
        _ => Err("missing x-server-lang header".into()),
    }
}

/// `GET /hello`: 200, `hello from <x-server-lang>`.
async fn expose_hello(conn: &aster::Connection) -> Verdict {
    let (_tx, body) = body_channel(4);
    let req = http::Request::builder()
        .method("GET")
        .uri("/hello")
        .body(body)
        .map_err(|e| e.to_string())?;
    drop(_tx);
    let resp = relay_request_streaming(conn, EXPOSE_SERVICE, req)
        .await
        .map_err(|e| format!("relay: {e}"))?;
    if resp.status() != 200 {
        return Err(format!("status {}", resp.status()));
    }
    let lang = lang_header(resp.headers())?;
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("body: {e}"))?
        .to_bytes();
    if body.as_ref() != format!("hello from {lang}").as_bytes() {
        return Err(format!("body {:?}", String::from_utf8_lossy(&body)));
    }
    Ok(lang)
}

/// `POST /echo` with a three-chunk upload: the response body is the upload
/// echoed back, read incrementally while the upload is still being produced.
async fn expose_echo(conn: &aster::Connection) -> Verdict {
    let (tx, body) = body_channel(4);
    let req = http::Request::builder()
        .method("POST")
        .uri("/echo")
        .body(body)
        .map_err(|e| e.to_string())?;
    let resp = relay_request_streaming(conn, EXPOSE_SERVICE, req)
        .await
        .map_err(|e| format!("relay: {e}"))?;
    if resp.status() != 200 {
        return Err(format!("status {}", resp.status()));
    }
    let lang = lang_header(resp.headers())?;
    let mut body = resp.into_body();
    let mut got: Vec<u8> = Vec::new();
    let mut want: Vec<u8> = Vec::new();
    for i in 0..3u8 {
        let chunk = vec![b'a' + i; 4000 + i as usize];
        want.extend_from_slice(&chunk);
        tx.send(aster::expose::bytes::Bytes::from(chunk))
            .await
            .map_err(|e| format!("upload chunk {i}: {e}"))?;
        // Interleaved: read until everything sent so far has come back.
        while got.len() < want.len() {
            match body.frame().await {
                Some(Ok(f)) => {
                    if let Ok(d) = f.into_data() {
                        got.extend_from_slice(&d);
                    }
                }
                Some(Err(e)) => return Err(format!("response chunk {i}: {e}")),
                None => return Err(format!("response ended early at chunk {i}")),
            }
        }
    }
    drop(tx);
    while let Some(f) = body.frame().await {
        match f {
            Ok(f) => {
                if let Ok(d) = f.into_data() {
                    got.extend_from_slice(&d);
                }
            }
            Err(e) => return Err(format!("response tail: {e}")),
        }
    }
    if got != want {
        return Err(format!(
            "echo mismatch (len {} vs {})",
            got.len(),
            want.len()
        ));
    }
    Ok(lang)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("CONTRACT_ID {}", contract_hex());
    println!("SESSION_CONTRACT_ID {}", session_contract_hex());
    match args.first().map(String::as_str) {
        Some("contract") => {}
        Some("server") => {
            if let Err(e) = serve().await {
                eprintln!("server error: {e}");
                std::process::exit(1);
            }
        }
        Some("client") if args.len() == 2 => {
            if !run_client(&args[1]).await {
                std::process::exit(1);
            }
        }
        Some("expose-server") => {
            if let Err(e) = expose_serve().await {
                eprintln!("expose server error: {e}");
                std::process::exit(1);
            }
        }
        Some("expose-client") if args.len() == 2 => {
            if !expose_client(&args[1]).await {
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!(
                "usage: interop_echo contract | server | client <aster1-ticket> | expose-server | expose-client <aster1-ticket>"
            );
            std::process::exit(2);
        }
    }
}
