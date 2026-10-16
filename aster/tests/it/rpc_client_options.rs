#![cfg(feature = "rpc")]
//! Per-call options on the generated client: `XClient::with_options` carries a
//! deadline and request metadata on every method in all four patterns (and a
//! session id); the handler sees them through `CallContext`, a client
//! interceptor sees them in `ClientCallContext`, and an expired deadline fails
//! with DEADLINE_EXCEEDED.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::rpc::{
    async_trait, CallContext, CallOptions, ClientCallContext, Interceptor, RequestStream,
    ResponseSink, RpcConnection, Server, ServerHandle, StatusCode, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_derive::ForyStruct;
use tokio::time::timeout;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "optstest/Num")]
struct Num {
    n: i32,
}

/// What a handler saw: method, `x-trace` / `x-user` metadata, time remaining,
/// session id.
type Seen = (
    String,
    Option<String>,
    Option<String>,
    Option<Duration>,
    Option<String>,
);

#[aster::service(name = "OptsSvc", version = 1)]
trait OptsSvc {
    async fn unary(&self, req: Num, ctx: &CallContext) -> Result<Num>;

    async fn noreq(&self, ctx: &CallContext) -> Result<Num>;

    #[rpc(server_stream)]
    async fn watch(&self, req: Num, out: ResponseSink<Num>, ctx: &CallContext) -> Result<()>;

    #[rpc(client_stream)]
    async fn sum(&self, reqs: RequestStream<Num>, ctx: &CallContext) -> Result<Num>;

    #[rpc(bidi_stream)]
    async fn chat(
        &self,
        reqs: RequestStream<Num>,
        out: ResponseSink<Num>,
        ctx: &CallContext,
    ) -> Result<()>;
}

#[derive(Clone)]
struct Impl {
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Handlers sleep this long before answering (deadline tests).
    delay: Duration,
}

impl Impl {
    async fn record(&self, ctx: &CallContext) {
        self.seen.lock().unwrap().push((
            ctx.method().to_string(),
            ctx.metadata_value("x-trace").map(str::to_string),
            ctx.metadata_value("x-user").map(str::to_string),
            ctx.remaining(),
            ctx.session_id().map(str::to_string),
        ));
        tokio::time::sleep(self.delay).await;
    }
}

#[async_trait]
impl OptsSvc for Impl {
    async fn unary(&self, req: Num, ctx: &CallContext) -> Result<Num> {
        self.record(ctx).await;
        Ok(req)
    }

    async fn noreq(&self, ctx: &CallContext) -> Result<Num> {
        self.record(ctx).await;
        Ok(Num { n: 7 })
    }

    async fn watch(&self, req: Num, out: ResponseSink<Num>, ctx: &CallContext) -> Result<()> {
        self.record(ctx).await;
        out.send(&req).await?;
        out.send(&req).await?;
        Ok(())
    }

    async fn sum(&self, mut reqs: RequestStream<Num>, ctx: &CallContext) -> Result<Num> {
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
        ctx: &CallContext,
    ) -> Result<()> {
        self.record(ctx).await;
        while let Some(m) = reqs.recv().await? {
            out.send(&m).await?;
        }
        Ok(())
    }
}

/// Method, deadline and metadata one interceptor call saw.
type Spied = (String, Option<Duration>, Vec<(String, String)>);

/// Records the deadline and metadata each unary call's interceptor sees.
#[derive(Clone, Default)]
struct Spy {
    seen: Arc<Mutex<Vec<Spied>>>,
}

#[async_trait]
impl Interceptor for Spy {
    async fn on_request(&self, ctx: &ClientCallContext, request: Vec<u8>) -> Result<Vec<u8>> {
        self.seen
            .lock()
            .unwrap()
            .push((ctx.method.clone(), ctx.deadline, ctx.metadata.clone()));
        Ok(request)
    }
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

async fn within<F: Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(15), f)
        .await
        .expect("rpc operation timed out")
}

/// A connected client/server pair serving `svc` (session-scoped when
/// `session` is set).
async fn pair(svc: Impl, session: bool) -> (Node, Node, RpcConnection, ServerHandle) {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();
    let srv = Server::new(&server);
    let srv = if session {
        srv.register_session(move || OptsSvcServer::new(svc.clone()))
    } else {
        srv.register(OptsSvcServer::new(svc))
    };
    let handle = srv.serve();
    let conn = within(client.rpc_connect(&server.id())).await.unwrap();
    (server, client, conn, handle)
}

fn opts(trace: &str) -> CallOptions {
    CallOptions::new()
        .deadline(Duration::from_secs(30))
        .metadata("x-trace", trace)
        .metadata("x-user", "alice")
}

#[tokio::test]
async fn generated_client_carries_deadline_and_metadata_in_every_pattern() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let svc = Impl {
        seen: seen.clone(),
        delay: Duration::ZERO,
    };
    let (server, client, conn, _h) = pair(svc, false).await;
    let spy = Spy::default();
    let api = OptsSvcClient::new(conn.with_interceptor(spy.clone()));

    // Default options: the plain methods are unchanged (no deadline, no metadata).
    assert_eq!(within(api.unary(Num { n: 1 })).await.unwrap(), Num { n: 1 });

    let r = within(api.with_options(opts("t-unary")).unary(Num { n: 5 }))
        .await
        .unwrap();
    assert_eq!(r, Num { n: 5 });
    let r = within(api.with_options(opts("t-noreq")).noreq())
        .await
        .unwrap();
    assert_eq!(r, Num { n: 7 });

    let mut s = within(api.with_options(opts("t-watch")).watch(Num { n: 9 }))
        .await
        .unwrap();
    assert_eq!(within(s.collect()).await.unwrap(), vec![Num { n: 9 }; 2]);

    let r = within(
        api.with_options(opts("t-sum"))
            .sum(vec![Num { n: 1 }, Num { n: 2 }]),
    )
    .await
    .unwrap();
    assert_eq!(r, Num { n: 3 });

    let mut s = within(api.with_options(opts("t-chat")).chat(vec![Num { n: 4 }]))
        .await
        .unwrap();
    assert_eq!(within(s.collect()).await.unwrap(), vec![Num { n: 4 }]);

    // The incremental bidi opener carries them too.
    let chat = api.with_options(opts("t-chat"));
    let (tx, mut rx) = within(chat.chat_streaming()).await.unwrap();
    tx.send(&Num { n: 6 }).await.unwrap();
    tx.finish().await.unwrap();
    assert_eq!(
        within(rx.recv()).await.unwrap().unwrap(),
        Num { n: 6 },
        "incremental bidi echo"
    );

    let seen = seen.lock().unwrap().clone();
    let methods: Vec<&str> = seen.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(
        methods,
        ["unary", "unary", "noreq", "watch", "sum", "chat", "chat"],
        "{seen:#?}"
    );
    // The plain call: no metadata, and only the server's handler timeout.
    let plain = &seen[0];
    assert_eq!((plain.1.as_deref(), plain.2.as_deref()), (None, None));
    assert!(plain.3.unwrap() > Duration::from_secs(60), "{plain:?}");
    for (method, trace, user, remaining, session) in &seen[1..] {
        assert_eq!(trace.as_deref(), Some(format!("t-{method}").as_str()));
        assert_eq!(user.as_deref(), Some("alice"), "{method}");
        let remaining = remaining.expect("client deadline reaches the handler");
        assert!(
            remaining <= Duration::from_secs(30) && remaining >= Duration::from_secs(20),
            "{method}: remaining {remaining:?}"
        );
        assert_eq!(*session, None);
    }

    // The unary interceptor path sees the same deadline and metadata.
    let spied = spy.seen.lock().unwrap().clone();
    assert_eq!(spied.len(), 3, "{spied:#?}");
    assert_eq!(spied[0], ("unary".to_string(), None, vec![]));
    for (method, deadline, metadata) in &spied[1..] {
        assert_eq!(*deadline, Some(Duration::from_secs(30)), "{method}");
        assert!(
            metadata.contains(&("x-trace".to_string(), format!("t-{method}")))
                && metadata.contains(&("x-user".to_string(), "alice".to_string())),
            "{method}: {metadata:?}"
        );
    }

    client.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn generated_client_deadline_expires_with_deadline_exceeded_in_every_pattern() {
    let svc = Impl {
        seen: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::from_secs(5),
    };
    let (server, client, conn, _h) = pair(svc, false).await;
    let api = OptsSvcClient::new(conn)
        .with_options(CallOptions::new().deadline(Duration::from_millis(300)));
    let code = |e: aster::Error| e.rpc_code();
    let exceeded = Some(StatusCode::DeadlineExceeded);

    let err = within(api.unary(Num { n: 1 })).await.unwrap_err();
    assert_eq!(code(err), exceeded, "unary");

    let err = within(api.noreq()).await.unwrap_err();
    assert_eq!(code(err), exceeded, "noreq");

    let mut s = within(api.watch(Num { n: 1 })).await.unwrap();
    let err = within(s.recv()).await.unwrap().unwrap_err();
    assert_eq!(code(err), exceeded, "server stream");

    let err = within(api.sum(vec![Num { n: 1 }])).await.unwrap_err();
    assert_eq!(code(err), exceeded, "client stream");

    let mut s = within(api.chat(vec![Num { n: 1 }])).await.unwrap();
    let err = within(s.recv()).await.unwrap().unwrap_err();
    assert_eq!(code(err), exceeded, "bidi");

    client.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn generated_client_options_select_a_session() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let svc = Impl {
        seen: seen.clone(),
        delay: Duration::ZERO,
    };
    let (server, client, conn, _h) = pair(svc, true).await;
    let session = conn.open_session().unwrap();
    let api = OptsSvcClient::new(conn).with_options(session.options().metadata("x-user", "bob"));

    assert_eq!(within(api.noreq()).await.unwrap(), Num { n: 7 });
    let mut s = within(api.watch(Num { n: 2 })).await.unwrap();
    assert_eq!(within(s.collect()).await.unwrap(), vec![Num { n: 2 }; 2]);

    let sid = session.id().to_string();
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:#?}");
    for (method, _, user, _, session) in &seen {
        assert_eq!(user.as_deref(), Some("bob"), "{method}");
        assert_eq!(session.as_deref(), Some(sid.as_str()), "{method}");
    }

    client.shutdown().await;
    server.shutdown().await;
}
