//! End-to-end for the routing control plane (design doc §7 step 9), Salvo-free:
//! an origin `request_route`s over real QUIC, the edge's `ControlAcceptor` runs
//! the policy and populates the shared `EdgeRouter`, and dropping the
//! registration evicts the routes. Mirrors the harness in `relay_integration`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use aster_transport_core::reactor::{create_reactor, ReactorHandle};
use aster_transport_core::stream_io::BoxFut;
use aster_transport_core::tunnel::PeerContext;
use aster_transport_core::{CoreConnection, CoreNetClient};

use aster_expose::control::{
    request_route, serve_routes_on_connection, EdgeRouter, RoutePolicy, RouteProtocol, RouteSpec,
};

const TEST_ALPN: &[u8] = b"aster-test/control";

type ServerConns = Arc<Mutex<Vec<CoreConnection>>>;

async fn start_server_reactor(
    server: CoreNetClient,
) -> (ReactorHandle, tokio::task::JoinHandle<()>, ServerConns) {
    let (handle, feeder) = create_reactor(&tokio::runtime::Handle::current(), 256);
    let conns: ServerConns = Arc::new(Mutex::new(Vec::new()));
    let conns_for_task = conns.clone();
    let accept_task = tokio::spawn(async move {
        loop {
            match server.accept().await {
                Ok(conn) => {
                    conns_for_task.lock().unwrap().push(conn.clone());
                    feeder.feed(conn);
                }
                Err(_) => return,
            }
        }
    });
    (handle, accept_task, conns)
}

async fn wait_for_one_server_conn(server_conns: &ServerConns) -> Result<CoreConnection> {
    for _ in 0..50 {
        if let Some(c) = server_conns.lock().unwrap().first().cloned() {
            return Ok(c);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(anyhow::anyhow!(
        "server reactor never observed a connection"
    ))
}

struct Harness {
    client_conn: CoreConnection,
    server_conn: CoreConnection,
    _server: CoreNetClient,
    _client: CoreNetClient,
    _reactor: ReactorHandle,
    _accept: tokio::task::JoinHandle<()>,
}

async fn harness() -> Result<Harness> {
    let server = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let client = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let server_id = server.endpoint_id();
    let (reactor, accept_task, server_conns) = start_server_reactor(server.clone()).await;
    let client_conn = client.connect(server_id, TEST_ALPN.to_vec()).await?;
    let server_conn = wait_for_one_server_conn(&server_conns).await?;
    Ok(Harness {
        client_conn,
        server_conn,
        _server: server,
        _client: client,
        _reactor: reactor,
        _accept: accept_task,
    })
}

/// Policy: reject all unless metadata is `b"token"`; otherwise grant every
/// requested route except host `blocked.local`.
fn token_policy() -> RoutePolicy {
    Arc::new(|ctx: PeerContext, specs: Vec<RouteSpec>| {
        Box::pin(async move {
            if ctx.metadata != b"token" {
                anyhow::bail!("missing/invalid token");
            }
            Ok(specs
                .into_iter()
                .filter(|s| s.host != "blocked.local")
                .collect())
        }) as BoxFut<Result<Vec<RouteSpec>>>
    })
}

fn http_route(host: &str, service_id: &str) -> RouteSpec {
    RouteSpec {
        host: host.into(),
        port: 0,
        protocol: RouteProtocol::Http,
        service_id: service_id.into(),
    }
}

async fn poll_until<F: Fn() -> bool>(f: F) -> bool {
    for _ in 0..50 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register_populates_router_and_evicts_on_close() -> Result<()> {
    let h = harness().await?;
    let router = EdgeRouter::new();
    // The edge (server side) accepts registrations into `router`.
    serve_routes_on_connection(&h.server_conn, router.clone(), token_policy());

    // The origin (client side) registers a route.
    let reg = request_route(
        &h.client_conn,
        vec![http_route("app.local", "web")],
        b"token".to_vec(),
    )
    .await?;
    assert_eq!(reg.granted(), &[http_route("app.local", "web")]);

    assert!(
        poll_until(|| router.lookup("app.local", 0).is_some()).await,
        "route should be installed after registration"
    );

    // Closing the registration evicts the route.
    reg.close().await?;
    assert!(
        poll_until(|| router.lookup("app.local", 0).is_none()).await,
        "route should be evicted after the control stream closes"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_grants_subset_and_enforces_metadata() -> Result<()> {
    let h = harness().await?;
    let router = EdgeRouter::new();
    serve_routes_on_connection(&h.server_conn, router.clone(), token_policy());

    // Wrong token → nothing granted.
    let denied = request_route(
        &h.client_conn,
        vec![http_route("app.local", "web")],
        b"wrong".to_vec(),
    )
    .await?;
    assert!(denied.granted().is_empty(), "bad token grants nothing");
    drop(denied);

    // Right token → only the non-blocked host is granted.
    let reg = request_route(
        &h.client_conn,
        vec![
            http_route("allowed.local", "web"),
            http_route("blocked.local", "web"),
        ],
        b"token".to_vec(),
    )
    .await?;
    assert_eq!(reg.granted(), &[http_route("allowed.local", "web")]);
    assert!(poll_until(|| router.lookup("allowed.local", 0).is_some()).await);
    assert!(router.lookup("blocked.local", 0).is_none());
    Ok(())
}
