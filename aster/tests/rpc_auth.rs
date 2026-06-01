#![cfg(feature = "rpc")]
//! Gate-3 (per-call capability) auth: a method that `requires` a role is
//! rejected with PERMISSION_DENIED until the application injects that role for
//! the caller into the server's AttributeStore; an ungated method always works.

use std::future::Future;
use std::time::Duration;

use aster::rpc::{
    require_role, AttributeStore, Call, CapabilityRequirement, RpcConnection, RpcStatus,
    SerializationMode, Server, ServiceDispatch, StatusCode, StreamHeader, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode};
use tokio::time::timeout;

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

fn header(method: &str) -> StreamHeader {
    StreamHeader {
        service: "Admin".into(),
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

/// `restart` requires the `operator` role; `ping` is open.
struct AdminService;

#[async_trait::async_trait]
impl ServiceDispatch for AdminService {
    fn name(&self) -> &str {
        "Admin"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["ping", "restart"]
    }
    fn method_requires(&self, method: &str) -> Option<CapabilityRequirement> {
        match method {
            "restart" => Some(require_role("operator")),
            _ => None,
        }
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let _ = call.recv_request().await;
        // Echo back the caller's role so a passing call is observable.
        let role = call
            .attributes()
            .get("aster.role")
            .cloned()
            .unwrap_or_default();
        let _ = call.respond(role.into_bytes(), &RpcStatus::ok());
    }
}

#[tokio::test]
async fn gate3_capability_check() {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    let attrs = AttributeStore::new();
    let _h = Server::new(&server)
        .register(AdminService)
        .attributes(attrs.clone())
        .serve();

    let conn: RpcConnection = within(client.rpc_connect(&server.id())).await.unwrap();

    // Open method works for anyone.
    let resp = within(conn.unary(&header("ping"), vec![1])).await.unwrap();
    assert_eq!(resp, Vec::<u8>::new(), "no role injected yet");

    // Gated method is denied before the role is injected.
    let err = within(conn.unary(&header("restart"), vec![1]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::PermissionDenied.as_i32()),
        "expected PERMISSION_DENIED, got {err:?}"
    );

    // Application injects the operator role for this peer (e.g. after verifying
    // its attestation chain at admission).
    attrs.set_role(client.id().as_str(), "operator");

    // Now the gated method passes and the handler sees the role.
    let resp = within(conn.unary(&header("restart"), vec![1]))
        .await
        .unwrap();
    assert_eq!(resp, b"operator".to_vec());

    // Revoking the role re-closes the gate.
    attrs.remove(client.id().as_str());
    let err = within(conn.unary(&header("restart"), vec![1]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::PermissionDenied.as_i32()),
        "expected PERMISSION_DENIED after revoke, got {err:?}"
    );

    client.shutdown().await;
    server.shutdown().await;
}
