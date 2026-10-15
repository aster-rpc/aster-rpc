#![cfg(all(
    feature = "rpc",
    feature = "docs",
    feature = "blobs",
    feature = "gossip"
))]
//! Late activation of an `AsterServer`'s RPC surface
//! (`docs/_internal/aster-server-late-activation.md`): a server built with
//! `deferred_surface()` runs its node without its application services, fails
//! their calls closed, advertises nothing, then activates one immutable
//! `RpcSurface` in one step, and can stop it again while the node stays up.
//!
//! The cases follow the acceptance tests of Portal's request
//! (`2026-08-22-aster-delayed-rpc-surface.md`), (a) to (g).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aster::rpc::baseline::NodeInfoClient;
use aster::rpc::{
    async_trait, fetch_and_verify_contract, AsterServer, Call, ManifestPublication, RpcConnection,
    RpcStatus, RpcSurface, ServiceDispatch, StatusCode, SurfaceState,
};
use aster::ShareMode;
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_derive::ForyStruct;
use tokio::time::timeout;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "surface/Inner")]
struct Inner {
    tag: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "surface/EchoRequest")]
struct EchoRequest {
    message: String,
    inner: Inner,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "surface/EchoResponse")]
struct EchoResponse {
    reply: String,
}

#[aster::service(name = "SurfaceEcho", version = 1)]
trait Echo {
    async fn echo(&self, req: EchoRequest) -> Result<EchoResponse>;
}

/// Never served anywhere: probes what an unknown name looks like.
#[allow(dead_code)] // only its generated client is used
#[aster::service(name = "NeverServed", version = 1)]
trait Ghost {
    async fn echo(&self, req: EchoRequest) -> Result<EchoResponse>;
}

/// Counts the calls that reach it, so a test can prove none did.
#[derive(Clone, Default)]
struct EchoImpl {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Echo for EchoImpl {
    async fn echo(&self, req: EchoRequest) -> Result<EchoResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(EchoResponse {
            reply: format!("echo: {}", req.message),
        })
    }
}

/// A hand-written dispatcher: no contract to publish.
struct Bare;

#[async_trait]
impl ServiceDispatch for Bare {
    fn name(&self) -> &str {
        "Bare"
    }
    fn version(&self) -> i32 {
        1
    }
    async fn dispatch(&self, _method: &str, call: Call) {
        let _ = call.finish(&RpcStatus::ok()).await;
    }
}

const VERSION_KEY: &[u8] = b"services/SurfaceEcho/versions/v1";

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

async fn within<F: std::future::Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(20), f)
        .await
        .expect("operation timed out")
}

async fn deferred() -> AsterServer {
    AsterServer::builder()
        .config(cfg())
        .deferred_surface()
        .start()
        .await
        .unwrap()
}

async fn client_of(srv: &AsterServer) -> (Node, RpcConnection) {
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(srv.node()).await;
    wait_for_addr(&client).await;
    client.add_peer(srv.node()).unwrap();
    srv.node().add_peer(&client).unwrap();
    let conn = within(client.rpc_connect(&srv.id())).await.unwrap();
    (client, conn)
}

fn request() -> EchoRequest {
    EchoRequest {
        message: "hi".into(),
        inner: Inner { tag: "t".into() },
    }
}

async fn echo_code(conn: &RpcConnection) -> std::result::Result<String, StatusCode> {
    match within(EchoClient::new(conn.clone()).echo(request())).await {
        Ok(resp) => Ok(resp.reply),
        Err(err) => Err(err
            .rpc_code()
            .unwrap_or_else(|| panic!("not an rpc error: {err}"))),
    }
}

async fn ghost_code(conn: &RpcConnection) -> StatusCode {
    let err = within(GhostClient::new(conn.clone()).echo(request()))
        .await
        .unwrap_err();
    err.rpc_code().unwrap()
}

async fn advertised(registry: &aster::Doc) -> bool {
    registry
        .query_latest_exact(VERSION_KEY.to_vec())
        .await
        .unwrap()
        .is_some()
}

/// (a) Before activation the node is fully usable: a stable id, blobs, docs,
/// gossip and host services -- even with no service at all on the builder.
#[tokio::test]
async fn node_is_live_before_activation() {
    let srv = AsterServer::builder()
        .config(cfg())
        .builtin_node_info(false)
        .deferred_surface()
        .start()
        .await
        .expect("a deferred server starts with no service");
    let id = srv.id();
    assert_eq!(srv.surface_state(), SurfaceState::Inactive);

    let hash = srv
        .blobs()
        .add_bytes(b"before activation".to_vec())
        .await
        .unwrap();
    assert_eq!(
        srv.blobs().read_to_bytes(&hash).await.unwrap(),
        b"before activation".to_vec()
    );
    let doc = srv.docs().create().await.unwrap();
    let author = srv.docs().default_author().await.unwrap();
    doc.set_bytes(&author, b"k".to_vec(), b"v".to_vec())
        .await
        .unwrap();
    let _gossip = srv.gossip();

    let calls = Arc::new(AtomicUsize::new(0));
    srv.activate(
        RpcSurface::builder()
            .service(EchoServer::new(EchoImpl {
                calls: calls.clone(),
            }))
            .build(),
    )
    .await
    .unwrap();
    assert_eq!(srv.id(), id, "activation keeps the node identity");
    srv.shutdown().await;
}

/// (b) Before activation `aster/1` fails closed: the application name is
/// answered `UNAVAILABLE` and no handler runs, while host services serve.
/// (c) The answer is the one any unknown name gets, so a probe learns
/// nothing, and the registry holds nothing.
#[tokio::test]
async fn calls_fail_closed_and_nothing_is_advertised_before_activation() {
    let srv = deferred().await;
    let registry = srv.docs().create().await.unwrap();
    let (client, conn) = client_of(&srv).await;

    assert_eq!(echo_code(&conn).await, Err(StatusCode::Unavailable));
    assert_eq!(ghost_code(&conn).await, StatusCode::Unavailable);
    // Host services are not part of the surface.
    let info = within(NodeInfoClient::new(conn.clone()).describe())
        .await
        .unwrap();
    assert_eq!(info.node_id, srv.id().to_string());
    assert!(!advertised(&registry).await);

    let calls = Arc::new(AtomicUsize::new(0));
    srv.activate(
        RpcSurface::builder()
            .service(EchoServer::new(EchoImpl {
                calls: calls.clone(),
            }))
            .manifest_publication(ManifestPublication::Strict)
            .registry(registry.clone())
            .build(),
    )
    .await
    .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no call reached the handler early"
    );
    assert!(advertised(&registry).await);
    // An unknown name is NOT_FOUND once a surface is active.
    assert_eq!(ghost_code(&conn).await, StatusCode::NotFound);

    srv.shutdown().await;
    client.shutdown().await;
}

/// (d) One activation step: callable, and the manifest published (with its
/// transitive payload types) and verifiable when it returns.
#[tokio::test]
async fn activation_serves_and_publishes() {
    let srv = deferred().await;
    let (client, conn) = client_of(&srv).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let activation = srv
        .activate(
            RpcSurface::builder()
                .service(EchoServer::new(EchoImpl {
                    calls: calls.clone(),
                }))
                .manifest_publication(ManifestPublication::Strict)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(srv.surface_state(), SurfaceState::Active);
    assert_eq!(activation.services, vec![("SurfaceEcho".to_string(), 1)]);
    assert!(!activation.is_degraded());

    assert_eq!(echo_code(&conn).await, Ok("echo: hi".to_string()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let registry = activation.registry.clone().expect("server-owned registry");
    let fetched = within(fetch_and_verify_contract(
        &registry,
        &srv.blobs(),
        &srv.id(),
        "SurfaceEcho",
        1,
    ))
    .await
    .unwrap();
    let expected = aster::rpc::contract_id(&EchoServer::<EchoImpl>::contract());
    assert_eq!(fetched.contract_id, expected);
    assert_eq!(activation.published[0].contract_id, expected);
    // Request, its nested Inner, and the response.
    assert_eq!(fetched.manifest.type_count, 3);
    assert_eq!(fetched.type_defs.len(), 3);

    // A consumer on another node discovers it through a read-only share.
    let ticket = registry.share_with_addr(ShareMode::Read).await.unwrap();
    let remote = within(client.docs().join(ticket)).await.unwrap();
    let mut found = None;
    for _ in 0..100 {
        if let Ok(f) =
            fetch_and_verify_contract(&remote, &client.blobs(), &srv.id(), "SurfaceEcho", 1).await
        {
            found = Some(f);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(found.expect("remote discovery").contract_id, expected);

    // Surfaces are immutable: a second activation is refused.
    let again = srv
        .activate(RpcSurface::builder().service(Bare).build())
        .await
        .unwrap_err();
    assert_eq!(again.rpc_code(), Some(StatusCode::FailedPrecondition));

    srv.shutdown().await;
    client.shutdown().await;
}

/// (e) Strict publication that fails fails the activation: nothing callable,
/// nothing advertised, the server back where it was.
#[tokio::test]
async fn strict_publication_failure_fails_activation() {
    let srv = deferred().await;
    let (owner, conn) = client_of(&srv).await;

    // A registry this node can read but not write.
    let source = owner.docs().create().await.unwrap();
    let author = owner.docs().default_author().await.unwrap();
    source
        .set_bytes(&author, b"seed".to_vec(), b"x".to_vec())
        .await
        .unwrap();
    let ticket = source.share_with_addr(ShareMode::Read).await.unwrap();
    let read_only = within(srv.docs().join(ticket)).await.unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let err = srv
        .activate(
            RpcSurface::builder()
                .service(EchoServer::new(EchoImpl {
                    calls: calls.clone(),
                }))
                .manifest_publication(ManifestPublication::Strict)
                .registry(read_only.clone())
                .build(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("manifest publication"), "{err}");
    assert_eq!(srv.surface_state(), SurfaceState::Inactive);
    assert_eq!(echo_code(&conn).await, Err(StatusCode::Unavailable));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!advertised(&source).await);

    // A service with no contract cannot be published strictly either.
    let err = srv
        .activate(
            RpcSurface::builder()
                .service(Bare)
                .manifest_publication(ManifestPublication::Strict)
                .build(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no contract"), "{err}");
    assert_eq!(srv.surface_state(), SurfaceState::Inactive);

    // BestEffort activates anyway and says what it could not publish.
    let activation = srv
        .activate(
            RpcSurface::builder()
                .service(EchoServer::new(EchoImpl {
                    calls: calls.clone(),
                }))
                .manifest_publication(ManifestPublication::BestEffort)
                .registry(read_only)
                .build(),
        )
        .await
        .unwrap();
    assert!(activation.is_degraded(), "{activation:?}");
    assert!(activation.published.is_empty());
    assert_eq!(echo_code(&conn).await, Ok("echo: hi".to_string()));

    srv.shutdown().await;
    owner.shutdown().await;
}

/// (f) Deactivation stops dispatch and retracts discovery; the node keeps
/// serving blobs, docs and host services, and a new surface can follow.
#[tokio::test]
async fn deactivation_leaves_the_node_up() {
    let srv = deferred().await;
    let (client, conn) = client_of(&srv).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let surface = || {
        RpcSurface::builder()
            .service(EchoServer::new(EchoImpl {
                calls: calls.clone(),
            }))
            .manifest_publication(ManifestPublication::Strict)
            .build()
    };
    let activation = srv.activate(surface()).await.unwrap();
    let registry = activation.registry.clone().unwrap();
    assert_eq!(echo_code(&conn).await, Ok("echo: hi".to_string()));
    assert!(advertised(&registry).await);

    srv.deactivate().await.unwrap();
    assert_eq!(srv.surface_state(), SurfaceState::Inactive);
    assert_eq!(echo_code(&conn).await, Err(StatusCode::Unavailable));
    assert!(!advertised(&registry).await, "discovery retracted");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Idempotent.
    srv.deactivate().await.unwrap();

    // The node is untouched.
    let hash = srv.blobs().add_bytes(b"after".to_vec()).await.unwrap();
    assert_eq!(
        srv.blobs().read_to_bytes(&hash).await.unwrap(),
        b"after".to_vec()
    );
    let author = srv.docs().default_author().await.unwrap();
    registry
        .set_bytes(&author, b"still/writable".to_vec(), b"1".to_vec())
        .await
        .unwrap();
    within(NodeInfoClient::new(conn.clone()).describe())
        .await
        .unwrap();

    // Reactivation reuses the server's registry.
    let again = srv.activate(surface()).await.unwrap();
    assert_eq!(
        again.registry.unwrap().id().unwrap(),
        registry.id().unwrap()
    );
    assert_eq!(echo_code(&conn).await, Ok("echo: hi".to_string()));
    assert!(advertised(&registry).await);

    srv.shutdown().await;
    client.shutdown().await;
}

/// (g) The one-step quickstart is unchanged: services served from `start`,
/// unknown names `NOT_FOUND`, and no late activation.
#[tokio::test]
async fn quickstart_is_unchanged() {
    let calls = Arc::new(AtomicUsize::new(0));
    let srv = AsterServer::builder()
        .config(cfg())
        .service(EchoServer::new(EchoImpl {
            calls: calls.clone(),
        }))
        .start()
        .await
        .unwrap();
    assert_eq!(srv.surface_state(), SurfaceState::Active);
    let (client, conn) = client_of(&srv).await;
    assert_eq!(echo_code(&conn).await, Ok("echo: hi".to_string()));
    assert_eq!(ghost_code(&conn).await, StatusCode::NotFound);

    let err = srv
        .activate(RpcSurface::builder().service(Bare).build())
        .await
        .unwrap_err();
    assert_eq!(err.rpc_code(), Some(StatusCode::FailedPrecondition));
    let err = srv.deactivate().await.unwrap_err();
    assert_eq!(err.rpc_code(), Some(StatusCode::FailedPrecondition));
    assert_eq!(echo_code(&conn).await, Ok("echo: hi".to_string()));

    srv.shutdown().await;
    client.shutdown().await;
}

/// A surface may not shadow a host service.
#[tokio::test]
async fn surface_cannot_shadow_a_host_service() {
    let srv = AsterServer::builder()
        .config(cfg())
        .service(Bare)
        .deferred_surface()
        .start()
        .await
        .unwrap();
    let err = srv
        .activate(RpcSurface::builder().service(Bare).build())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already registered"), "{err}");
    let err = srv
        .activate(RpcSurface::builder().build())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("at least one service"), "{err}");
    assert_eq!(srv.surface_state(), SurfaceState::Inactive);
    srv.shutdown().await;
}

/// The same service name at version 10: its registry key
/// `services/SurfaceEcho/versions/v10` has v1's key as a prefix.
#[aster::service(name = "SurfaceEcho", version = 10)]
trait EchoTen {
    async fn echo(&self, req: EchoRequest) -> Result<EchoResponse>;
}

#[async_trait]
impl EchoTen for EchoImpl {
    async fn echo(&self, req: EchoRequest) -> Result<EchoResponse> {
        Echo::echo(self, req).await
    }
}

/// Publishing and retracting a surface's entries touch exactly its keys:
/// activating and then deactivating a surface that publishes v1 leaves v10,
/// written by the same node into the same registry, in place.
#[tokio::test]
async fn retraction_matches_exact_keys() {
    let srv = deferred().await;
    let registry = srv.docs().create().await.unwrap();
    let author = srv.docs().default_author().await.unwrap();
    let v10 = EchoTenServer::new(EchoImpl::default())
        .contract_artifacts()
        .unwrap();
    aster::rpc::publish_contract(
        &registry,
        &srv.blobs(),
        &author,
        &v10.contract,
        &v10.type_defs,
        1_700_000_000_000,
    )
    .await
    .unwrap();
    const V10: &[u8] = b"services/SurfaceEcho/versions/v10";
    let present = |key: &'static [u8]| {
        let registry = registry.clone();
        async move {
            registry
                .query_latest_exact(key.to_vec())
                .await
                .unwrap()
                .is_some()
        }
    };
    assert!(present(V10).await);

    srv.activate(
        RpcSurface::builder()
            .service(EchoServer::new(EchoImpl::default()))
            .manifest_publication(ManifestPublication::Strict)
            .registry(registry.clone())
            .build(),
    )
    .await
    .unwrap();
    assert!(present(VERSION_KEY).await);
    assert!(present(V10).await, "publishing v1 leaves v10 in place");
    srv.deactivate().await.unwrap();

    assert!(!present(VERSION_KEY).await, "v1 retracted");
    assert!(present(V10).await, "v10 is not v1's to retract");
    let fetched = within(fetch_and_verify_contract(
        &registry,
        &srv.blobs(),
        &srv.id(),
        "SurfaceEcho",
        10,
    ))
    .await
    .unwrap();
    assert_eq!(fetched.contract_id, aster::rpc::contract_id(&v10.contract));
    srv.shutdown().await;
}

/// Whether the registry holds any live entry.
async fn any_entry(registry: &aster::Doc) -> bool {
    !registry
        .query_key_prefix(Vec::new(), None)
        .await
        .unwrap()
        .is_empty()
}

/// An activation dropped while it is writing the registry leaves nothing
/// behind: its entries are retracted, dispatch is off, and calls answer
/// `UNAVAILABLE`. The future is polled by hand and dropped at the first poll
/// that leaves it pending after an entry has appeared.
#[tokio::test]
async fn cancelled_activation_leaves_nothing_behind() {
    use std::future::Future;
    use std::task::Poll;

    for _attempt in 0..20 {
        let srv = deferred().await;
        let registry = srv.docs().create().await.unwrap();
        let (client, conn) = client_of(&srv).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut activation = Box::pin(
            srv.activate(
                RpcSurface::builder()
                    .service(EchoServer::new(EchoImpl {
                        calls: calls.clone(),
                    }))
                    .service(EchoTenServer::new(EchoImpl {
                        calls: calls.clone(),
                    }))
                    .manifest_publication(ManifestPublication::Strict)
                    .registry(registry.clone())
                    .build(),
            ),
        );
        let mut caught = false;
        loop {
            let polled = std::future::poll_fn(|cx| Poll::Ready(activation.as_mut().poll(cx))).await;
            match polled {
                Poll::Ready(result) => {
                    // Finished before a write was observed: try again.
                    result.unwrap();
                    break;
                }
                Poll::Pending if any_entry(&registry).await => {
                    caught = true;
                    break;
                }
                Poll::Pending => tokio::time::sleep(Duration::from_millis(1)).await,
            }
        }
        drop(activation);
        if !caught {
            srv.shutdown().await;
            client.shutdown().await;
            continue;
        }

        // Retraction may run after the drop; give it a bounded moment.
        let mut clean = false;
        for _ in 0..100 {
            if !any_entry(&registry).await {
                clean = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(clean, "a cancelled activation left registry entries");
        assert_eq!(srv.surface_state(), SurfaceState::Inactive);
        assert_eq!(echo_code(&conn).await, Err(StatusCode::Unavailable));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // The server is usable: a later activation succeeds.
        srv.activate(
            RpcSurface::builder()
                .service(EchoServer::new(EchoImpl::default()))
                .manifest_publication(ManifestPublication::Strict)
                .registry(registry.clone())
                .build(),
        )
        .await
        .unwrap();
        assert!(advertised(&registry).await);
        srv.shutdown().await;
        client.shutdown().await;
        return;
    }
    panic!("never caught an activation between registry writes");
}
