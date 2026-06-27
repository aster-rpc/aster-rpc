//! Node-level facade: a service registered once on an [`ExposeNode`] is
//! reachable on a connection after `attach`, over real QUIC.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use http_body_util::BodyExt;
use tokio::time::timeout;

use aster_transport_core::reactor::{create_reactor, ReactorHandle};
use aster_transport_core::stream_io::BoxFut;
use aster_transport_core::{CoreConnection, CoreNetClient};

use aster_expose::body::{RelayBody, RelayError, ResponseBody};
use aster_expose::node::ExposeNode;
use aster_expose::relay::{relay_request, AuthorizeFn, HttpHandler};

const TEST_ALPN: &[u8] = b"aster-test/node";
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

fn allow_all() -> AuthorizeFn {
    Arc::new(|_ctx| Box::pin(async { Ok(()) }) as BoxFut<Result<()>>)
}

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

/// Server reactor that `attach`es the node's shared registry to each accepted
/// connection before feeding it to the reactor.
async fn start_node_reactor(
    server: CoreNetClient,
    node: ExposeNode,
) -> (
    ReactorHandle,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<Vec<CoreConnection>>>,
) {
    let (handle, feeder) = create_reactor(&tokio::runtime::Handle::current(), 256);
    let conns: Arc<Mutex<Vec<CoreConnection>>> = Arc::new(Mutex::new(Vec::new()));
    let conns_for_task = conns.clone();
    let accept_task = tokio::spawn(async move {
        loop {
            match server.accept().await {
                Ok(mut conn) => {
                    node.attach(&mut conn); // node-wide services become reachable
                    conns_for_task.lock().unwrap().push(conn.clone());
                    feeder.feed(conn);
                }
                Err(_) => return,
            }
        }
    });
    (handle, accept_task, conns)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_level_expose_reachable_after_attach() -> Result<()> {
    let server = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let client = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let server_id = server.endpoint_id();

    // Register once on the node, before any connection exists.
    let node = ExposeNode::new();
    node.expose_http("web", allow_all(), echo_handler());

    let (_reactor, _accept, _conns) = start_node_reactor(server.clone(), node.clone()).await;
    let client_conn = client.connect(server_id, TEST_ALPN.to_vec()).await?;

    let resp = timeout(
        STEP_TIMEOUT,
        relay_request(
            &client_conn,
            "web",
            http::Request::builder()
                .uri("/node")
                .body(b"hi".to_vec())
                .unwrap(),
        ),
    )
    .await??;

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-echo-path").unwrap(), "/node");
    assert_eq!(resp.body(), b"hi");
    Ok(())
}
