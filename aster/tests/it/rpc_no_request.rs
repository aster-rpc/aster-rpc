#![cfg(feature = "rpc")]
//! A unary method with **no request argument** (`async fn ping(&self) ->
//! Result<Pong>`). The macro substitutes the implicit `Empty` request; the
//! generated client `ping()` takes no argument and the round-trip works.

use std::time::Duration;

use aster::rpc::{async_trait, RpcConnection, Server, RPC_ALPN};
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_derive::ForyStruct;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "ping/Pong")]
struct Pong {
    ok: bool,
}

#[aster::service(name = "Pinger", version = 1)]
trait Pinger {
    // No request argument — request is the implicit Empty.
    async fn ping(&self) -> Result<Pong>;
}

struct PingerImpl;

#[async_trait]
impl Pinger for PingerImpl {
    async fn ping(&self) -> Result<Pong> {
        Ok(Pong { ok: true })
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
    tokio::time::timeout(Duration::from_secs(30), async {
        while n.addr().direct_addresses.is_empty() {
            addrs.updated().await.expect("endpoint closed");
        }
    })
    .await
    .expect("the node had no direct address after 30 s");
}

#[tokio::test]
async fn no_request_unary_round_trip() {
    // The implicit Empty shows up as the method's request type.
    let contract = PingerClient::contract();
    let md = contract.methods.iter().find(|m| m.name == "ping").unwrap();
    let empty_hash = <aster::rpc::Empty as aster::rpc::AsterType>::aster_type_hash_hex();
    assert_eq!(md.request_type, empty_hash);

    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client_node = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client_node).await;
    client_node.add_peer(&server).unwrap();
    server.add_peer(&client_node).unwrap();

    let _h = Server::new(&server)
        .register(PingerServer::new(PingerImpl))
        .serve();

    let conn: RpcConnection = client_node.rpc_connect(&server.id()).await.unwrap();
    let client = PingerClient::new(conn);

    // Generated stub takes no argument.
    let pong = client.ping().await.unwrap();
    assert!(pong.ok);

    client_node.shutdown().await;
    server.shutdown().await;
}
