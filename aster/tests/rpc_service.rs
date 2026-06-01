#![cfg(feature = "rpc")]
//! `#[aster::service]` end-to-end: a trait of unary methods generates a server
//! dispatcher + client stub + cross-binding contract. Verifies a typed
//! round-trip over the wire, the generated contract's identity (the echo
//! method's request type matches the cross-binding golden), and that a
//! `#[rpc(requires = ...)]` method is gated by the Gate-3 capability check.

use std::future::Future;
use std::time::Duration;

use aster::rpc::{
    async_trait, require_role, AttributeStore, RpcConnection, Server, StatusCode, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_derive::ForyStruct;
use tokio::time::timeout;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "echo/EchoRequest")]
struct EchoRequest {
    message: String,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "echo/EchoResponse")]
struct EchoResponse {
    reply: String,
}

#[aster::service(name = "EchoService", version = 1)]
trait Echo {
    async fn echo(&self, req: EchoRequest) -> Result<EchoResponse>;

    #[rpc(requires = require_role("operator"))]
    async fn admin_echo(&self, req: EchoRequest) -> Result<EchoResponse>;
}

struct EchoImpl;

#[async_trait]
impl Echo for EchoImpl {
    async fn echo(&self, req: EchoRequest) -> Result<EchoResponse> {
        Ok(EchoResponse {
            reply: format!("echo: {}", req.message),
        })
    }
    async fn admin_echo(&self, req: EchoRequest) -> Result<EchoResponse> {
        Ok(EchoResponse {
            reply: format!("admin: {}", req.message),
        })
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

#[tokio::test]
async fn service_macro_round_trip_and_auth() {
    // The generated contract carries the right shape + cross-binding type id.
    let contract = EchoClient::contract();
    assert_eq!(contract.name, "EchoService");
    assert_eq!(contract.version, 1);
    assert_eq!(contract.methods.len(), 2);
    let echo_md = contract.methods.iter().find(|m| m.name == "echo").unwrap();
    assert_eq!(
        echo_md.request_type, "4a2fa9b8f8cfbd325d72dc9739e416c86cd3cd5724882f22400ab08d2db49dd6",
        "generated request type must match the cross-binding golden"
    );

    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client_node = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client_node).await;
    client_node.add_peer(&server).unwrap();
    server.add_peer(&client_node).unwrap();

    let attrs = AttributeStore::new();
    let _h = Server::new(&server)
        .register(EchoServer::new(EchoImpl))
        .attributes(attrs.clone())
        .serve();

    let conn: RpcConnection = within(client_node.rpc_connect(&server.id())).await.unwrap();
    let client = EchoClient::new(conn);

    // Typed unary round-trip through the generated stub + dispatcher.
    let resp = within(client.echo(EchoRequest {
        message: "hi".into(),
    }))
    .await
    .unwrap();
    assert_eq!(resp.reply, "echo: hi");

    // Gated method denied before the role is injected.
    let err = within(client.admin_echo(EchoRequest {
        message: "x".into(),
    }))
    .await
    .unwrap_err();
    assert!(
        matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::PermissionDenied.as_i32()),
        "expected PERMISSION_DENIED, got {err:?}"
    );

    // Inject the role; the gated method now passes.
    attrs.set_role(client_node.id().as_str(), "operator");
    let resp = within(client.admin_echo(EchoRequest {
        message: "y".into(),
    }))
    .await
    .unwrap();
    assert_eq!(resp.reply, "admin: y");

    client_node.shutdown().await;
    server.shutdown().await;
}
