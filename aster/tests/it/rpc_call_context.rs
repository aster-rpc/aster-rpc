#![cfg(feature = "rpc")]
//! Handler call context: an `#[aster::service]` method that declares a
//! `CallContext` parameter receives the call's peer, authenticated principal,
//! Gate-3 attributes, metadata, session, deadline and cancellation, in all four
//! patterns; `CallContext::current()` reaches the same context from a nested
//! async fn; and the parameter never changes the contract id.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::rpc::{
    async_trait, contract_id, require_role, AuthContext, AuthOutcome, Authenticator, CallContext,
    CallOptions, Fory, PayloadRegistry, RequestStream, ResponseSink, RpcConnection, RpcStatus,
    SerializationMode, Server, StreamHeader, WireField, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_derive::ForyStruct;
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "ctxtest/Num")]
struct Num {
    n: i32,
}

/// What a handler saw in its context.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    service: String,
    peer: String,
    principal: String,
    tenant: Option<String>,
    role: Option<String>,
    trace: Option<String>,
    session: Option<String>,
    remaining: Option<Duration>,
    /// `CallContext::current()` from a nested async fn, same call?
    current_matches: bool,
}

fn snapshot(ctx: &CallContext, current: Option<CallContext>) -> Seen {
    Seen {
        method: ctx.method().to_string(),
        service: ctx.service().to_string(),
        peer: ctx.peer().to_string(),
        principal: ctx.principal().to_string(),
        tenant: ctx.attribute("tenant").map(str::to_string),
        role: ctx.attribute("aster.role").map(str::to_string),
        trace: ctx.metadata_value("x-trace").map(str::to_string),
        session: ctx.session_id().map(str::to_string),
        remaining: ctx.remaining(),
        current_matches: current.is_some_and(|c| {
            c.method() == ctx.method()
                && c.principal() == ctx.principal()
                && c.call_id() == ctx.call_id()
        }),
    }
}

/// Code deep in the call that never received the context as a parameter.
async fn nested_lookup() -> Option<CallContext> {
    tokio::task::yield_now().await;
    CallContext::try_current()
}

#[aster::service(name = "CtxSvc", version = 1)]
trait CtxSvc {
    #[rpc(requires = require_role("reader"))]
    async fn unary(&self, req: Num, ctx: &CallContext) -> Result<Num>;

    /// No request, owned context.
    async fn whoami(&self, ctx: CallContext) -> Result<Num>;

    /// No context parameter: reads it through the task-local only.
    async fn plain(&self, req: Num) -> Result<Num>;

    #[rpc(server_stream)]
    async fn watch(&self, req: Num, out: ResponseSink<Num>, ctx: &CallContext) -> Result<()>;

    /// Context declared first, to prove position does not matter.
    #[rpc(client_stream)]
    async fn sum(&self, ctx: &CallContext, reqs: RequestStream<Num>) -> Result<Num>;

    #[rpc(bidi_stream)]
    async fn chat(
        &self,
        reqs: RequestStream<Num>,
        out: ResponseSink<Num>,
        ctx: CallContext,
    ) -> Result<()>;
}

#[derive(Clone)]
struct Impl {
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Fires with the method name when a `cancelled()` waiter resolves.
    cancelled: mpsc::UnboundedSender<&'static str>,
}

impl Impl {
    async fn record(&self, ctx: &CallContext) {
        let current = nested_lookup().await;
        self.seen.lock().unwrap().push(snapshot(ctx, current));
    }
}

#[async_trait]
impl CtxSvc for Impl {
    async fn unary(&self, req: Num, ctx: &CallContext) -> Result<Num> {
        self.record(ctx).await;
        Ok(req)
    }

    async fn whoami(&self, ctx: CallContext) -> Result<Num> {
        self.record(&ctx).await;
        Ok(Num { n: 7 })
    }

    async fn plain(&self, req: Num) -> Result<Num> {
        let ctx = CallContext::current();
        self.record(&ctx).await;
        Ok(req)
    }

    async fn watch(&self, req: Num, out: ResponseSink<Num>, ctx: &CallContext) -> Result<()> {
        self.record(ctx).await;
        out.send(&req).await?;
        // Wait for the caller to go away; the select-style early exit runs
        // before the dispatcher drops this future.
        ctx.cancelled().await;
        assert!(ctx.is_cancelled());
        let _ = self.cancelled.send("watch");
        Ok(())
    }

    async fn sum(&self, ctx: &CallContext, mut reqs: RequestStream<Num>) -> Result<Num> {
        self.record(ctx).await;
        let mut total = 0;
        while let Some(m) = reqs.recv().await? {
            total += m.n;
        }
        Ok(Num { n: total })
    }

    async fn chat(
        &self,
        mut reqs: RequestStream<Num>,
        out: ResponseSink<Num>,
        ctx: CallContext,
    ) -> Result<()> {
        self.record(&ctx).await;
        // Work that outlives the handler: a spawned task holding a clone.
        // It does not inherit the task-local.
        let tx = self.cancelled.clone();
        let watcher = ctx.clone();
        tokio::spawn(async move {
            assert!(CallContext::try_current().is_none());
            watcher.cancelled().await;
            let _ = tx.send("chat");
        });
        while let Some(m) = reqs.recv().await? {
            out.send(&m).await?;
        }
        Ok(())
    }
}

/// Resolves every caller to a principal and grants the `reader` role plus a
/// tenant attribute.
struct TokenAuth;

#[async_trait]
impl Authenticator for TokenAuth {
    async fn authenticate(&self, ctx: &AuthContext) -> std::result::Result<AuthOutcome, RpcStatus> {
        let mut attributes = HashMap::new();
        attributes.insert("aster.role".to_string(), "reader".to_string());
        attributes.insert("tenant".to_string(), "acme".to_string());
        Ok(AuthOutcome {
            principal: Some(format!("user:alice@{}", &ctx.peer_id[..8])),
            attributes,
        })
    }
}

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

/// Wait until `n` has a direct address to hand out: a condition wait
/// on the endpoint's address watcher, failing loudly after 30 s rather
/// than letting a later connect fail for an unrelated-looking reason.
async fn wait_for_addr(n: &Node) {
    use aster::native::iroh::Watcher;
    let mut addrs = n.native_endpoint().watch_addr();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while n.addr().direct_addresses.is_empty() {
            addrs.updated().await.expect("endpoint closed");
        }
    })
    .await
    .expect("the node had no direct address after 30 s");
}

async fn within<F: Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(15), f)
        .await
        .expect("rpc operation timed out")
}

fn header(method: &str) -> StreamHeader {
    StreamHeader {
        service: "CtxSvc".into(),
        method: method.into(),
        version: 1,
        call_id: 0,
        deadline: 0,
        serialization_mode: SerializationMode::Xlang.as_i8(),
        metadata_keys: vec![],
        metadata_values: vec![],
        session_id: 0,
    }
}

fn fory() -> Fory {
    let mut reg = PayloadRegistry::new();
    <Num as WireField>::register_payload(&mut reg);
    reg.into_fory()
}

fn opts(trace: &str) -> CallOptions {
    CallOptions::new()
        .deadline(Duration::from_secs(30))
        .metadata("x-trace", trace)
}

#[tokio::test]
async fn handlers_see_the_call_context_in_every_pattern() {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (tx, mut cancelled) = mpsc::unbounded_channel();
    let svc = Impl {
        seen: seen.clone(),
        cancelled: tx,
    };
    let _h = Server::new(&server)
        .authenticator(TokenAuth)
        .register(CtxSvcServer::new(svc))
        .serve();

    let conn: RpcConnection = within(client.rpc_connect(&server.id())).await.unwrap();
    let f = fory();
    let enc = |n: i32| f.serialize(&Num { n }).unwrap();
    let dec = |b: &[u8]| f.deserialize::<Num>(b).unwrap();
    let client_id = client.id().as_str().to_string();
    let principal = format!("user:alice@{}", &client_id[..8]);

    // unary (Gate 3 requires `reader`, granted by the authenticator)
    let started = Instant::now();
    let r = within(conn.unary_with(&header("unary"), enc(5), &opts("t-unary")))
        .await
        .unwrap();
    assert_eq!(dec(&r), Num { n: 5 });
    // no-request unary, owned context
    let r = within(conn.unary_with(&header("whoami"), enc(0), &opts("t-whoami")))
        .await
        .unwrap();
    assert_eq!(dec(&r), Num { n: 7 });
    // no context parameter: task-local only
    let r = within(conn.unary_with(&header("plain"), enc(3), &opts("t-plain")))
        .await
        .unwrap();
    assert_eq!(dec(&r), Num { n: 3 });

    // client-stream
    let (sink, resp) = within(conn.client_stream_open(&header("sum"), &opts("t-sum")))
        .await
        .unwrap();
    sink.send(enc(1)).await.unwrap();
    sink.send_last(enc(2)).await.unwrap();
    assert_eq!(dec(&within(resp).await.unwrap()), Num { n: 3 });

    // server-stream: read one item, then cancel; the handler's `cancelled()`
    // resolves.
    let mut stream = within(conn.server_stream_with(&header("watch"), enc(9), &opts("t-watch")))
        .await
        .unwrap();
    let first = within(stream.next()).await.unwrap().unwrap();
    assert_eq!(dec(&first), Num { n: 9 });
    stream.cancel().await;
    assert_eq!(within(cancelled.recv()).await, Some("watch"));

    // bidi: echo one, then drop both halves; the spawned watcher's
    // `cancelled()` resolves.
    let (sink, mut stream) = within(conn.bidi_open_with(&header("chat"), &opts("t-chat")))
        .await
        .unwrap();
    sink.send(enc(4)).await.unwrap();
    let echoed = within(stream.next()).await.unwrap().unwrap();
    assert_eq!(dec(&echoed), Num { n: 4 });
    drop(sink);
    drop(stream);
    assert_eq!(within(cancelled.recv()).await, Some("chat"));

    let elapsed = started.elapsed();
    let seen = seen.lock().unwrap().clone();
    let methods: Vec<&str> = seen.iter().map(|s| s.method.as_str()).collect();
    assert_eq!(
        methods,
        ["unary", "whoami", "plain", "sum", "watch", "chat"],
        "{seen:#?}"
    );
    for s in &seen {
        assert_eq!(s.service, "CtxSvc");
        assert_eq!(s.peer, client_id, "transport peer for {}", s.method);
        assert_eq!(s.principal, principal, "principal for {}", s.method);
        assert_eq!(s.tenant.as_deref(), Some("acme"), "{}", s.method);
        assert_eq!(s.role.as_deref(), Some("reader"), "{}", s.method);
        assert_eq!(
            s.trace.as_deref(),
            Some(format!("t-{}", s.method).as_str()),
            "metadata for {}",
            s.method
        );
        assert_eq!(s.session, None, "{}", s.method);
        let remaining = s.remaining.expect("client deadline reaches the handler");
        assert!(
            remaining <= Duration::from_secs(30) && remaining + elapsed >= Duration::from_secs(25),
            "{}: remaining {remaining:?}",
            s.method
        );
        assert!(s.current_matches, "current() in {}", s.method);
    }

    client.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn deadline_cancels_the_context() {
    #[aster::service(name = "Slow", version = 1)]
    trait Slow {
        async fn wait(&self, req: Num, ctx: &CallContext) -> Result<Num>;
    }
    struct SlowImpl(mpsc::UnboundedSender<bool>);
    #[async_trait]
    impl Slow for SlowImpl {
        async fn wait(&self, req: Num, ctx: &CallContext) -> Result<Num> {
            let tx = self.0.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                ctx.cancelled().await;
                let _ = tx.send(ctx.is_cancelled());
            });
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(req)
        }
    }

    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _h = Server::new(&server)
        .register(SlowServer::new(SlowImpl(tx)))
        .serve();
    let conn: RpcConnection = within(client.rpc_connect(&server.id())).await.unwrap();
    let f = fory();
    // Header deadlines are whole seconds on the wire.
    let mut h = header("wait");
    h.service = "Slow".into();
    let err = within(conn.unary_with(
        &h,
        f.serialize(&Num { n: 1 }).unwrap(),
        &CallOptions::new().deadline(Duration::from_secs(1)),
    ))
    .await
    .unwrap_err();
    assert!(matches!(err, aster::Error::Rpc { .. }), "{err:?}");
    assert_eq!(within(rx.recv()).await, Some(true));

    client.shutdown().await;
    server.shutdown().await;
}

#[test]
fn context_parameter_does_not_change_the_contract_id() {
    mod with_ctx {
        use super::*;
        #[allow(dead_code)]
        #[aster::service(name = "Same", version = 3)]
        pub trait WithCtx {
            async fn get(&self, req: Num, ctx: &CallContext) -> Result<Num>;
            async fn noreq(&self, ctx: CallContext) -> Result<Num>;
            #[rpc(server_stream)]
            async fn watch(
                &self,
                req: Num,
                out: ResponseSink<Num>,
                ctx: &CallContext,
            ) -> Result<()>;
            #[rpc(client_stream)]
            async fn sum(&self, ctx: &CallContext, reqs: RequestStream<Num>) -> Result<Num>;
            #[rpc(bidi_stream)]
            async fn chat(
                &self,
                reqs: RequestStream<Num>,
                out: ResponseSink<Num>,
                ctx: aster::rpc::CallContext,
            ) -> Result<()>;
        }
    }
    mod without_ctx {
        use super::*;
        #[allow(dead_code)]
        #[aster::service(name = "Same", version = 3)]
        pub trait WithoutCtx {
            async fn get(&self, req: Num) -> Result<Num>;
            async fn noreq(&self) -> Result<Num>;
            #[rpc(server_stream)]
            async fn watch(&self, req: Num, out: ResponseSink<Num>) -> Result<()>;
            #[rpc(client_stream)]
            async fn sum(&self, reqs: RequestStream<Num>) -> Result<Num>;
            #[rpc(bidi_stream)]
            async fn chat(&self, reqs: RequestStream<Num>, out: ResponseSink<Num>) -> Result<()>;
        }
    }
    let a = with_ctx::WithCtxClient::contract();
    let b = without_ctx::WithoutCtxClient::contract();
    assert_eq!(contract_id(&a), contract_id(&b));
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
}

#[tokio::test]
async fn session_scoped_handlers_see_the_session_and_no_authenticator_means_principal_is_peer() {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (tx, _cancelled) = mpsc::unbounded_channel();
    let svc = Impl {
        seen: seen.clone(),
        cancelled: tx,
    };
    let _h = Server::new(&server)
        .register_session(move || CtxSvcServer::new(svc.clone()))
        .serve();

    let conn: RpcConnection = within(client.rpc_connect(&server.id())).await.unwrap();
    let session = conn.open_session().unwrap();
    let f = fory();
    let r = within(session.unary_with(
        &header("whoami"),
        f.serialize(&Num { n: 0 }).unwrap(),
        &CallOptions::new().metadata("x-trace", "t-whoami"),
    ))
    .await
    .unwrap();
    assert_eq!(f.deserialize::<Num>(&r).unwrap(), Num { n: 7 });

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:#?}");
    let s = &seen[0];
    let client_id = client.id().as_str().to_string();
    assert_eq!(
        s.session.as_deref(),
        Some(session.id().to_string().as_str())
    );
    assert_eq!(s.peer, client_id);
    assert_eq!(s.principal, client_id);
    // No client deadline: the server's default handler timeout still bounds it.
    let remaining = s.remaining.expect("handler timeout");
    assert!(
        remaining <= aster::rpc::DEFAULT_HANDLER_TIMEOUT && remaining > Duration::from_secs(60)
    );
    assert!(s.current_matches);

    client.shutdown().await;
    server.shutdown().await;
}
