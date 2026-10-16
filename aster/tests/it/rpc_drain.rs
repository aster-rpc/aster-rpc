#![cfg(feature = "rpc")]
//! Server drain (`AsterServer::drain`, `ServerHandle::drain`): in-flight calls
//! of every pattern finish within the grace; a call still running at the
//! deadline is cancelled; while draining, a new connection is refused and a
//! new call on an existing connection answers UNAVAILABLE with the
//! `aster.draining` detail, but a call on an existing session runs; the
//! report counts what happened. Readiness is covered in `tests/health.rs`.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aster::rpc::{
    async_trait, AsterServer, CallContext, DrainReport, RequestStream, ResponseSink, RpcConnection,
    Server, StatusCode, DRAIN_CLOSE_CODE, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_derive::ForyStruct;
use tokio::sync::Semaphore;
use tokio::time::timeout;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "draintest/Num")]
struct Num {
    n: i32,
}

#[aster::service(name = "DrainSvc", version = 1)]
trait DrainSvc {
    async fn unary(&self, req: Num, ctx: &CallContext) -> Result<Num>;

    /// Answers at once (the probe for "is a new call admitted").
    async fn ping(&self, req: Num, ctx: &CallContext) -> Result<Num>;

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

/// Gated handlers: each counts itself started, then waits for a permit on
/// `gate` before answering. A handler dropped before it finished counts in
/// `dropped`.
#[derive(Clone)]
struct Gated {
    started: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
}

impl Default for Gated {
    fn default() -> Self {
        Self {
            started: Arc::default(),
            dropped: Arc::default(),
            gate: Arc::new(Semaphore::new(0)),
        }
    }
}

struct DropCount(Arc<AtomicUsize>, bool);

impl Drop for DropCount {
    fn drop(&mut self) {
        if self.1 {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Gated {
    async fn hold(&self) {
        let mut armed = DropCount(self.dropped.clone(), true);
        self.started.fetch_add(1, Ordering::SeqCst);
        self.gate.acquire().await.unwrap().forget();
        armed.1 = false;
    }
}

#[async_trait]
impl DrainSvc for Gated {
    async fn unary(&self, req: Num, _ctx: &CallContext) -> Result<Num> {
        self.hold().await;
        Ok(req)
    }

    async fn ping(&self, req: Num, _ctx: &CallContext) -> Result<Num> {
        Ok(req)
    }

    async fn watch(&self, req: Num, out: ResponseSink<Num>, _ctx: &CallContext) -> Result<()> {
        out.send(&req).await?;
        self.hold().await;
        out.send(&req).await?;
        Ok(())
    }

    async fn sum(&self, mut reqs: RequestStream<Num>, _ctx: &CallContext) -> Result<Num> {
        self.hold().await;
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
        _ctx: &CallContext,
    ) -> Result<()> {
        self.hold().await;
        while let Some(m) = reqs.recv().await? {
            out.send(&m).await?;
        }
        Ok(())
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
    timeout(Duration::from_secs(20), f)
        .await
        .expect("operation timed out")
}

/// Poll `cond` until it holds (generous bound).
async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn client_of(server: &Node) -> (Node, RpcConnection) {
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(server).await;
    wait_for_addr(&client).await;
    client.add_peer(server).unwrap();
    server.add_peer(&client).unwrap();
    let conn = within(client.rpc_connect(&server.id())).await.unwrap();
    (client, conn)
}

async fn gated_server(svc: Gated) -> AsterServer {
    AsterServer::builder()
        .config(cfg())
        .service(DrainSvcServer::new(svc))
        .start()
        .await
        .unwrap()
}

/// A new connection to a draining server is closed with the drain code; a
/// call on it fails as draining.
async fn assert_new_connection_refused(server: &Node) -> Node {
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&client).await;
    client.add_peer(server).unwrap();
    match within(client.rpc_connect(&server.id())).await {
        Ok(conn) => {
            let err = within(DrainSvcClient::new(conn.clone()).ping(Num { n: 1 }))
                .await
                .expect_err("a call on a refused connection fails");
            assert!(err.is_draining(), "{err:?}");
            until("the drain close", || conn.is_drained()).await;
        }
        Err(err) => {
            let text = err.to_string();
            assert!(text.contains("draining"), "{text}");
        }
    }
    client
}

#[tokio::test]
async fn in_flight_calls_of_every_pattern_finish_within_grace() {
    let svc = Gated::default();
    let srv = gated_server(svc.clone()).await;
    let server_node = srv.node().clone();
    let (client, conn) = client_of(&server_node).await;
    let api = DrainSvcClient::new(conn.clone());
    assert_eq!(within(api.ping(Num { n: 1 })).await.unwrap(), Num { n: 1 });

    // One call of each pattern, held in its handler.
    let unary = tokio::spawn({
        let api = api.clone();
        async move { api.unary(Num { n: 1 }).await }
    });
    let watch = tokio::spawn({
        let api = api.clone();
        async move { api.watch(Num { n: 2 }).await?.collect().await }
    });
    let sum = tokio::spawn({
        let api = api.clone();
        async move { api.sum(vec![Num { n: 3 }, Num { n: 4 }]).await }
    });
    let chat = tokio::spawn({
        let api = api.clone();
        async move { api.chat(vec![Num { n: 5 }]).await?.collect().await }
    });
    until("four handlers running", || {
        svc.started.load(Ordering::SeqCst) == 4
    })
    .await;

    let drain = tokio::spawn(srv.drain(Duration::from_secs(30)));

    // A new call on the existing connection is refused as draining.
    let mut refused = None;
    for _ in 0..400 {
        match within(api.ping(Num { n: 9 })).await {
            Ok(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            Err(err) => {
                refused = Some(err);
                break;
            }
        }
    }
    let refused = refused.expect("a new call is refused once draining");
    assert!(refused.is_draining(), "{refused:?}");
    assert_eq!(refused.rpc_code(), Some(StatusCode::Unavailable));
    // Not retried on the same connection: one refusal, not a retry storm.
    assert!(!conn.is_drained(), "busy connection stays open");

    // A new connection is refused.
    let other = assert_new_connection_refused(&server_node).await;

    // Release the handlers: every in-flight call completes normally.
    svc.gate.add_permits(4);
    assert_eq!(within(unary).await.unwrap().unwrap(), Num { n: 1 });
    assert_eq!(
        within(watch).await.unwrap().unwrap(),
        vec![Num { n: 2 }, Num { n: 2 }]
    );
    assert_eq!(within(sum).await.unwrap().unwrap(), Num { n: 7 });
    assert_eq!(within(chat).await.unwrap().unwrap(), vec![Num { n: 5 }]);

    let report: DrainReport = within(drain).await.unwrap();
    assert_eq!(report.calls_in_flight, 4, "{report:?}");
    assert_eq!(report.calls_completed, 4, "{report:?}");
    assert_eq!(report.calls_cancelled, 0, "{report:?}");
    assert!(report.calls_refused >= 1, "{report:?}");
    assert_eq!(report.connections_closed, 1, "{report:?}");
    assert!(report.connections_refused >= 1, "{report:?}");
    assert!(!report.deadline_reached, "{report:?}");
    assert!(report.elapsed < Duration::from_secs(30), "{report:?}");
    assert_eq!(svc.dropped.load(Ordering::SeqCst), 0);

    // The idle connection was closed with the drain code.
    until("the client connection closed as drained", || {
        conn.is_drained()
    })
    .await;
    let err = within(api.ping(Num { n: 1 })).await.unwrap_err();
    assert!(err.is_draining(), "{err:?}");
    client.shutdown().await;
    other.shutdown().await;
}

#[tokio::test]
async fn a_call_running_at_the_deadline_is_cancelled() {
    let svc = Gated::default();
    let srv = gated_server(svc.clone()).await;
    let server_node = srv.node().clone();
    let (client, conn) = client_of(&server_node).await;
    let api = DrainSvcClient::new(conn.clone());
    let stuck = tokio::spawn({
        let api = api.clone();
        async move { api.unary(Num { n: 1 }).await }
    });
    until("the handler running", || {
        svc.started.load(Ordering::SeqCst) == 1
    })
    .await;

    let report = within(srv.drain(Duration::from_millis(300))).await;
    assert_eq!(report.calls_in_flight, 1, "{report:?}");
    assert_eq!(report.calls_completed, 0, "{report:?}");
    assert_eq!(report.calls_cancelled, 1, "{report:?}");
    assert_eq!(report.connections_closed, 1, "{report:?}");
    assert!(report.deadline_reached, "{report:?}");
    assert!(report.elapsed >= Duration::from_millis(300), "{report:?}");

    // The handler was dropped and the caller told to go elsewhere.
    until("the handler cancelled", || {
        svc.dropped.load(Ordering::SeqCst) == 1
    })
    .await;
    let err = within(stuck).await.unwrap().unwrap_err();
    assert!(err.is_draining(), "{err:?}");
    assert!(conn.is_drained());
    client.shutdown().await;
}

#[tokio::test]
async fn a_live_session_keeps_running_and_new_sessions_are_refused() {
    let svc = Gated::default();
    let node = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let handle = Server::new(&node)
        .register_session({
            let svc = svc.clone();
            move || DrainSvcServer::new(svc.clone())
        })
        .serve();
    let (client, conn) = client_of(&node).await;
    let session = conn.open_session().unwrap();
    let in_session = DrainSvcClient::new(conn.clone()).with_options(session.options());
    assert_eq!(
        within(in_session.ping(Num { n: 1 })).await.unwrap(),
        Num { n: 1 }
    );

    let handle = Arc::new(handle);
    let drain = tokio::spawn({
        let handle = handle.clone();
        async move { handle.drain(Duration::from_millis(1500)).await }
    });
    until("the drain started", || handle.is_draining()).await;

    // A new session is refused once draining.
    let fresh = conn.open_session().unwrap();
    let refused = within(
        DrainSvcClient::new(conn.clone())
            .with_options(fresh.options())
            .ping(Num { n: 2 }),
    )
    .await
    .expect_err("a new session is refused once draining");
    assert!(refused.is_draining(), "{refused:?}");

    // The live session still runs calls (the connection is not idle).
    assert_eq!(
        within(in_session.ping(Num { n: 4 })).await.unwrap(),
        Num { n: 4 }
    );

    let report = within(drain).await.unwrap();
    assert!(
        report.deadline_reached,
        "the session held the connection: {report:?}"
    );
    assert!(report.calls_completed >= 1, "{report:?}");
    assert_eq!(report.calls_cancelled, 0, "{report:?}");
    assert!(report.calls_refused >= 1, "{report:?}");
    assert_eq!(report.connections_closed, 1, "{report:?}");
    until("the session's connection closed", || conn.is_drained()).await;
    // A second drain returns the same report.
    assert_eq!(handle.drain(Duration::ZERO).await, report);
    handle.abort();
    client.shutdown().await;
    node.shutdown().await;
}

#[test]
fn the_drain_close_code_is_stable() {
    assert_eq!(DRAIN_CLOSE_CODE, 503);
}
