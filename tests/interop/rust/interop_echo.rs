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
    contract_id, PayloadRegistry, RpcConnection, RpcSession, ScopeKind, SerializationMode, Server,
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

/// Optional read-only campaign control on the ordinary Rust server's stdin.
async fn campaign_server_control() {
    let _ = tokio::task::spawn_blocking(|| {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break; };
            let Ok(command) = serde_json::from_str::<serde_json::Value>(&line) else { continue; };
            let op = command["command"].as_str().or_else(|| command["cmd"].as_str()).or_else(|| command["op"].as_str());
            if op == Some("campaign_status") {
                let active = contract::active_holds();
                println!("{}", serde_json::json!({"phase":"campaign_server_status", "active_holds":active, "active_calls":contract::active_calls(), "active_calls_scope":"native_echo_handlers", "active_holds_scope":"native_hold_handlers", "pid":std::process::id()}));
            }
        }
    }).await;
}

async fn serve() -> Result<()> {
    let srv = aster::rpc::AsterServer::builder()
        .config(cfg())
        .service(InteropEchoServer::new(EchoImpl))
        .register_session(|| InteropSessionServer::new(SessionImpl::default()))
        .builtin_node_info(false)
        .start()
        .await?;
    wait_for_addr(srv.node()).await;
    println!("ADDRESS {}", srv.address()?);
    campaign_server_control().await;
    srv.shutdown().await;
    Ok(())
}

/// Chain leaf with an in-memory trace provider so the existing Rust-native
/// baggage extractor seam can observe (and record) the actual incoming W3C
/// baggage without exporting telemetry or changing the public SDK surface.
async fn chain_serve() -> Result<()> {
    let exporters = aster_otel::testing::TestExporters::new();
    let _observability = aster_otel::init(aster_otel::ObservabilityConfig {
        from_env: false,
        disabled: Some(false),
        traces_exporter: Some(vec![]),
        test_exporters: Some(exporters),
        ..Default::default()
    })
    .map_err(|error| anyhow::anyhow!("chain tracing init: {error}"))?;
    let node = Node::start_with_alpns(cfg(), vec![aster::rpc::RPC_ALPN.to_vec()]).await?;
    let _server = Server::new(&node)
        .register(InteropEchoServer::new(EchoImpl))
        .register_session(|| InteropSessionServer::new(SessionImpl::default()))
        .attribute_extractor("interop.chain.tenant", |ctx| {
            let tenant = ctx.baggage("tenant")?;
            let request = ctx.baggage("request")?;
            contract::record_chain_baggage(request, tenant);
            Some(tenant.to_owned())
        })
        .serve();
    wait_for_addr(&node).await;
    println!(
        "ADDRESS {}",
        Ticket::from_node_addr(&node.addr(), Credential::Open)?.to_base58()?
    );
    wait_stdin_eof().await;
    node.shutdown().await;
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

/// `traceEcho` called under a known remote, sampled trace context with no
/// observability pipeline: the client puts that context on the wire and the
/// server must report the same trace id from its dispatch context.
#[cfg(feature = "otel")]
async fn trace_case(client: &InteropEchoClient) -> Verdict {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        | 1;
    let mut trace_id = [0u8; 16];
    for (i, b) in trace_id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&TRACE_ID[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    let ctx = aster_otel::TraceContext {
        trace_id,
        span_id: nanos.to_be_bytes(),
        trace_flags: aster_otel::TraceContext::SAMPLED,
        is_remote: true,
    };
    let reply = tokio::time::timeout(
        Duration::from_secs(15),
        aster::rpc::with_trace_context(ctx, client.trace_echo(probe())),
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
    // short-wait-ok: the client cancels hold by dropping it after 300 ms
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

// Campaign state is bounded: one adapter admission queue and 27 histogram bins.
// It deliberately makes no claim about native SDK queue depths.
fn campaign_status(error: &aster::Error) -> String {
    let Some(code) = error.rpc_code() else {
        return "local_error".into();
    };
    let name = format!("{code:?}");
    let mut out = String::new();
    for (i, ch) in name.chars().enumerate() {
        if i > 0 && ch.is_uppercase() {
            out.push('_');
        }
        out.extend(ch.to_uppercase());
    }
    out.to_lowercase()
}

async fn campaign_connect(node: &Node, ticket: &str) -> anyhow::Result<RpcConnection> {
    let t = Ticket::from_base58(ticket)?;
    let id = node.add_ticket_addr(&t)?;
    // short-wait-ok: campaign mode: expiry is a classified outcome shared by every language leg
    Ok(tokio::time::timeout(Duration::from_secs(2), node.rpc_connect(&id)).await??)
}

async fn campaign_request(
    client: &InteropEchoClient,
    sequence: u64,
    slow_ms: f64,
) -> std::result::Result<&'static str, String> {
    let kind = ["unary", "stream", "unary", "cancel"][sequence as usize % 4];
    let req = EchoRequest {
        message: "campaign".into(),
        count: if kind == "stream" { 5 } else { 41 },
        data: (0..16384)
            .map(|i| ((i * 131 + i / 8) % 251) as u8)
            .collect(),
    };
    if kind == "cancel" {
        // Dropping this public call future sends caller cancellation; the hold
        // connection carries a ten-second RPC budget independently of the drop.
        // short-wait-ok: campaign mode: expiry is a classified outcome shared by every language leg
        match tokio::time::timeout(Duration::from_millis(50), client.hold(req)).await {
            Err(_) => return Ok(kind),
            Ok(Ok(_)) => return Err("INTEGRITY".into()),
            Ok(Err(error)) => return Err(campaign_status(&error)),
        }
    }
    if kind == "unary" {
        let reply = client
            .echo(req.clone())
            .await
            .map_err(|e| campaign_status(&e))?;
        check(&req, &reply).map_err(|_| "INTEGRITY".to_owned())?;
    } else {
        let mut stream = client
            .generate(req.clone())
            .await
            .map_err(|e| campaign_status(&e))?;
        let mut count = 0;
        loop {
            // short-wait-ok: campaign mode: expiry is a classified outcome shared by every language leg
            let next = match tokio::time::timeout(Duration::from_secs(2), stream.recv()).await {
                Ok(next) => next,
                Err(_) => {
                    // The generated typed stream has no CallOptions overload;
                    // dropping it is the public cancellation operation.
                    drop(stream);
                    return Err("planned_stream_cancel".into());
                }
            };
            let Some(reply) = next else {
                break;
            };
            let reply = reply.map_err(|e| campaign_status(&e))?;
            if reply.count != count
                || reply.message != req.message
                || reply.data != req.data
                || reply.server_lang.is_empty()
            {
                return Err("INTEGRITY".into());
            }
            count += 1;
            if slow_ms > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(slow_ms / 1000.0)).await;
            }
        }
        if count != 5 {
            return Err("INTEGRITY".into());
        }
    }
    Ok(kind)
}

type CampaignStats = Arc<std::sync::Mutex<serde_json::Value>>;
fn campaign_inc(stats: &mut serde_json::Value, field: &str) {
    stats[field] = (stats[field].as_u64().unwrap_or(0) + 1).into();
}
struct CampaignActive(CampaignStats);
impl Drop for CampaignActive {
    fn drop(&mut self) {
        let mut stats = self.0.lock().unwrap();
        stats["active_calls"] = stats["active_calls"]
            .as_u64()
            .unwrap()
            .saturating_sub(1)
            .into();
    }
}
async fn campaign_flush(command: &serde_json::Value, stats: &mut serde_json::Value) {
    let result = tokio::task::spawn_blocking(|| aster_otel::flush(Duration::from_secs(1))).await;
    let category = match result {
        Ok(Ok(())) => "ok",
        Ok(Err(aster_otel::Error::Timeout(_))) => "timeout",
        Ok(Err(aster_otel::Error::Sdk(_))) => "sdk",
        _ => "other",
    };
    stats["flush_status"] = category.into();
    let accepted = command["phase"] == "fault"
        && command["expected_flush_errors"]
            .as_array()
            .is_some_and(|names| names.iter().any(|name| name == category));
    if category != "ok" && !accepted {
        campaign_inc(stats, "unexpected_errors");
    }
}

async fn campaign_window(
    client: Arc<InteropEchoClient>,
    cancel_client: Arc<InteropEchoClient>,
    command: &serde_json::Value,
    generation: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    let duration = command["duration_s"].as_f64().unwrap_or(5.0);
    anyhow::ensure!(
        duration.is_finite() && duration > 0.0 && duration <= 300.0,
        "duration_s must be in (0,300]"
    );
    let phase = command["phase"].as_str().unwrap_or("healthy");
    anyhow::ensure!(
        matches!(phase, "healthy" | "fault"),
        "phase must be healthy or fault"
    );
    let expected = command["expected_errors"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    anyhow::ensure!(
        expected.iter().all(|v| v.is_string()),
        "expected_errors must be status-name strings"
    );
    let slow = command["slow_ms"].as_f64().unwrap_or(0.0);
    anyhow::ensure!(
        slow.is_finite() && (0.0..=1000.0).contains(&slow),
        "slow_ms must be in [0,1000]"
    );
    let expected = Arc::new(if phase == "fault" { expected } else { vec![] });
    let window_id = command["window_id"].as_str().unwrap_or("window");
    anyhow::ensure!(
        window_id.len() <= 128,
        "window_id must be at most 128 bytes"
    );
    let marker = format!("campaign:rust:{window_id};");
    let stats = Arc::new(std::sync::Mutex::new(json!({
        "phase": "campaign_window", "window_phase": phase, "generation": generation, "log_marker":marker,
        "completed": 0, "attempted": 0, "unary": 0, "stream": 0, "cancel": 0,
        "unexpected_errors": 0, "integrity_failures": 0, "active_calls": 0,
        "planned_stream_cancellations": 0,
        "queue_capacity": 128, "queue_scope": "adapter_admission", "queue_depth": 0,
        "queue_high_water": 0, "latency_histogram": vec![0u64;27],
        "first_success_elapsed_ms": null, "last_success_elapsed_ms": null,
        "error_classes": {}, "app_log_emitted": 0
    })));
    let (tx, rx) = tokio::sync::mpsc::channel::<u64>(128);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let accepting = Arc::new(AtomicBool::new(true));
    let started = std::time::Instant::now();
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let (client, cancel_client, stats, expected, rx, accepting) = (
            client.clone(),
            cancel_client.clone(),
            stats.clone(),
            expected.clone(),
            rx.clone(),
            accepting.clone(),
        );
        let marker = marker.clone();
        workers.spawn(async move {
            while accepting.load(Ordering::SeqCst) {
                let Some(sequence) = rx.lock().await.recv().await else {
                    break;
                };
                if !accepting.load(Ordering::SeqCst) {
                    break;
                }
                {
                    campaign_inc(&mut stats.lock().unwrap(), "active_calls");
                }
                let active = CampaignActive(stats.clone());
                let began = std::time::Instant::now();
                let caller = if sequence % 4 == 3 {
                    &cancel_client
                } else {
                    &client
                };
                // short-wait-ok: campaign mode: expiry is a classified outcome shared by every language leg
                let outcome = match tokio::time::timeout(
                    Duration::from_secs(3),
                    campaign_request(caller, sequence, slow),
                )
                .await
                {
                    Ok(value) => value,
                    Err(_) => Err("local_timeout".into()),
                };
                let status = match &outcome {
                    Ok(_) => "OK",
                    Err(status) => status.as_str(),
                };
                let success = outcome.is_ok();
                let emitted = success
                    && aster_otel::bridge::emit_app_log(&aster_otel::bridge::AppLogRecord {
                        level: Some(aster_otel::bridge::LogLevel::Info),
                        timestamp_unix_ns: 0,
                        logger: "app.campaign.rust",
                        message: &marker,
                        fields: &[("status", status)],
                        context: None,
                    })
                    .is_ok();
                {
                    let mut stats = stats.lock().unwrap();
                    campaign_inc(&mut stats, "attempted");
                    if success {
                        campaign_inc(&mut stats, "completed");
                    }
                    if emitted {
                        campaign_inc(&mut stats, "app_log_emitted");
                    } else if success {
                        campaign_inc(&mut stats, "unexpected_errors");
                    }
                    let micros = began.elapsed().as_micros().max(1) as u64;
                    let bin = (64 - (micros - 1).leading_zeros()).min(26) as usize;
                    let current = stats["latency_histogram"][bin].as_u64().unwrap();
                    if success {
                        stats["latency_histogram"][bin] = (current + 1).into();
                    }
                    match outcome {
                        Ok(kind) => {
                            campaign_inc(&mut stats, kind);
                            // Recovery timing requires a real peer reply, never a local future drop.
                            if kind != "cancel" {
                                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                                if stats["first_success_elapsed_ms"].is_null() {
                                    stats["first_success_elapsed_ms"] = elapsed.into();
                                }
                                stats["last_success_elapsed_ms"] = elapsed.into();
                            }
                        }
                        Err(status) if status == "INTEGRITY" => {
                            campaign_inc(&mut stats, "integrity_failures")
                        }
                        Err(status) => {
                            if status == "planned_stream_cancel" {
                                campaign_inc(&mut stats, "planned_stream_cancellations");
                            }
                            let n = stats["error_classes"][&status].as_u64().unwrap_or(0) + 1;
                            stats["error_classes"][&status] = n.into();
                            if !expected.iter().any(|name| name == &status)
                                || !matches!(
                                    status.as_str(),
                                    "unavailable"
                                        | "deadline_exceeded"
                                        | "cancelled"
                                        | "planned_stream_cancel"
                                )
                            {
                                campaign_inc(&mut stats, "unexpected_errors");
                            }
                        }
                    }
                }
                drop(active);
            }
        });
    }
    let mut sequence = 0;
    while started.elapsed().as_secs_f64() < duration {
        if tx.try_send(sequence).is_ok() {
            sequence += 1;
            let depth = 128 - tx.capacity();
            let mut stats = stats.lock().unwrap();
            if depth as u64 > stats["queue_high_water"].as_u64().unwrap() {
                stats["queue_high_water"] = depth.into();
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    accepting.store(false, Ordering::SeqCst);
    drop(tx);
    // short-wait-ok: campaign mode: expiry is a classified outcome shared by every language leg
    let drained = tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(result) = workers.join_next().await {
            if result.is_err() {
                campaign_inc(&mut stats.lock().unwrap(), "unexpected_errors");
            }
        }
    })
    .await;
    if drained.is_err() {
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        campaign_inc(&mut stats.lock().unwrap(), "unexpected_errors");
    }
    let mut receiver = rx.lock().await;
    while receiver.try_recv().is_ok() {}
    drop(receiver);
    let mut result = stats.lock().unwrap().clone();
    result["latency_histogram"] = json!({"unit":"us", "upper_bounds_powers_of_two":true, "counts":result["latency_histogram"].clone()});
    campaign_flush(command, &mut result).await;
    Ok(result)
}

async fn campaign_client(ticket: &str) -> anyhow::Result<()> {
    use serde_json::{json, Value};
    let endpoint = std::env::var("ASTER_CAMPAIGN_COLLECTOR")?;
    let mut config = aster_otel::ObservabilityConfig {
        from_env: false,
        service_name: Some("rust-campaign".into()),
        log_bridge: Some(false),
        traces_exporter: Some(vec![]),
        metrics_exporter: Some(vec![]),
        logs_exporter: Some(vec![aster_otel::ExporterKind::Otlp]),
        ..Default::default()
    };
    config.otlp.endpoint = Some(endpoint);
    config.otlp.timeout = Some(Duration::from_secs(1));
    let observability = aster_otel::init(config)?;
    let node = Node::start(cfg()).await?;
    let result = async {
        let mut conn = campaign_connect(&node, ticket).await?;
        let mut client = Arc::new(InteropEchoClient::new(conn.clone().with_deadline(Duration::from_secs(2))));
        let mut cancel_client = Arc::new(InteropEchoClient::new(conn.clone().with_deadline(Duration::from_secs(10))));
        let identity = node.id().to_string();
        let pid = std::process::id();
        let mut generation = json!(0);
        println!("{}", json!({"phase":"campaign_ready", "owner_identity": identity, "pid":pid, "generation":generation}));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);
        tokio::task::spawn_blocking(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break; };
                let stop = serde_json::from_str::<Value>(&line).ok().is_some_and(|v| v["command"] == "stop" || v["cmd"] == "stop" || v["op"] == "stop");
                if tx.blocking_send(line).is_err() || stop { break; }
            }
        });
        let mut settings = json!({});
        while let Some(line) = rx.recv().await {
            let command_result: anyhow::Result<(Value, bool)> = async {
                let command: Value = serde_json::from_str(&line)?;
                let op = command["command"].as_str().or_else(|| command["cmd"].as_str())
                    .or_else(|| command["op"].as_str()).unwrap_or("");
                let mut merged = settings.clone();
                for (key, value) in command.as_object().ok_or_else(|| anyhow::anyhow!("command must be an object"))? { merged[key] = value.clone(); }
                let result = match op {
                    "configure" => { settings = merged; json!({"phase":"campaign_configured"}) }
                    "baseline" | "run" => campaign_window(client.clone(), cancel_client.clone(), &merged, &generation).await?,
                    "replace_peer" => {
                        conn = campaign_connect(&node, command["ticket"].as_str().ok_or_else(|| anyhow::anyhow!("ticket required"))?).await?;
                        client = Arc::new(InteropEchoClient::new(conn.clone().with_deadline(Duration::from_secs(2))));
                        cancel_client = Arc::new(InteropEchoClient::new(conn.clone().with_deadline(Duration::from_secs(10))));
                        generation = command["generation"].clone();
                        json!({"phase":"campaign_peer_replaced", "generation":generation})
                    }
                    "pause" | "stop" => {
                        let mut result = json!({"phase":format!("campaign_{op}"), "active_calls":0, "unexpected_errors":0});
                        campaign_flush(&merged, &mut result).await;
                        result
                    }
                    _ => anyhow::bail!("unknown campaign command"),
                };
                Ok((result, op == "stop"))
            }.await;
            match command_result {
                Ok((mut result, stop)) => {
                    result["owner_identity"] = identity.clone().into(); result["pid"] = pid.into();
                    println!("{result}");
                    if stop { break; }
                }
                Err(error) => println!("{}", json!({"phase":"campaign_error", "detail":error.to_string().chars().take(256).collect::<String>()})),
            }
        }
        Ok::<(), anyhow::Error>(())
    }.await;
    node.shutdown().await;
    drop(observability);
    result
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("campaign-client") {
        println!("CONTRACT_ID {}", contract_hex());
        println!("SESSION_CONTRACT_ID {}", session_contract_hex());
    }
    match args.first().map(String::as_str) {
        Some("campaign-client") if args.len() == 2 => {
            if let Err(error) = campaign_client(&args[1]).await {
                eprintln!("campaign error: {error}");
                std::process::exit(1);
            }
        }
        Some("contract") => {}
        Some("server") => {
            if let Err(e) = serve().await {
                eprintln!("server error: {e}");
                std::process::exit(1);
            }
        }
        Some("chain-server") => {
            if let Err(e) = chain_serve().await {
                eprintln!("chain server error: {e}");
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
