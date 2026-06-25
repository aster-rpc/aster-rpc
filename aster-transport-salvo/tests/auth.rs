//! Pluggable server-side auth over HTTP. A custom `Authenticator` reads the
//! `authorization` header (carried into Aster metadata by the transport) and
//! grants a role; a capability-gated method then passes or is denied. Proves the
//! pre-dispatch auth hook + Gate-3 work end-to-end over HTTP.

use std::collections::HashMap;

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{
    require_role, AuthContext, AuthOutcome, Authenticator, Call, CapabilityRequirement, RpcStatus,
    Server, ServiceDispatch, StatusCode,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{decode_frame, encode_frame, FLAG_END_STREAM, FLAG_TRAILER};
use salvo::http::StatusCode as HttpStatus;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};

/// A service whose `admin` method requires the `operator` role (Gate 3).
struct Secured;

#[aster::rpc::async_trait]
impl ServiceDispatch for Secured {
    fn name(&self) -> &str {
        "Secured"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["admin"]
    }
    fn method_requires(&self, method: &str) -> Option<CapabilityRequirement> {
        (method == "admin").then(|| require_role("operator"))
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let req = call.recv_request().await.unwrap_or_default();
        let resp: Vec<u8> = req.into_iter().rev().collect();
        let _ = call.respond(resp, &RpcStatus::ok());
    }
}

/// Grants the `operator` role iff `authorization: Bearer secret` is present.
struct HeaderAuth;

#[aster::rpc::async_trait]
impl Authenticator for HeaderAuth {
    async fn authenticate(&self, ctx: &AuthContext) -> Result<AuthOutcome, RpcStatus> {
        let mut attributes = HashMap::new();
        if ctx.metadata.get("authorization").map(String::as_str) == Some("Bearer secret") {
            attributes.insert("aster.role".to_string(), "operator".to_string());
        }
        Ok(AuthOutcome {
            principal: None,
            attributes,
        })
    }
}

/// POST one request frame; return (data payloads, trailer status).
async fn call(
    service: &Service,
    auth_header: Option<&str>,
    req: Vec<u8>,
) -> (Vec<Vec<u8>>, RpcStatus) {
    let body = encode_frame(&req, FLAG_END_STREAM).unwrap();
    let mut builder = TestClient::post("http://localhost/aster/Secured/admin").body(body);
    if let Some(h) = auth_header {
        builder = builder.add_header("authorization", h, true);
    }
    let mut res = builder.send(service).await;
    assert_eq!(res.status_code, Some(HttpStatus::OK)); // HTTP 200; Aster status in trailer
    let bytes = res.take_bytes(None).await.unwrap();

    let mut buf = bytes.as_ref();
    let mut data = Vec::new();
    let mut trailer = None;
    while !buf.is_empty() {
        let (payload, flags, consumed) = decode_frame(buf).unwrap();
        if flags & FLAG_TRAILER != 0 {
            trailer = Some(decode_rpc_status(&payload).unwrap());
        } else {
            data.push(payload);
        }
        buf = &buf[consumed..];
    }
    (data, trailer.expect("missing trailer"))
}

#[tokio::test]
async fn auth_hook_gates_over_http() {
    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let node = Node::start(cfg).await.unwrap();
    let dispatcher = Server::new(&node)
        .register(Secured)
        .authenticator(HeaderAuth)
        .dispatcher();
    let service = Service::new(aster_transport_salvo::router(dispatcher));

    // No credential → operator role not granted → PERMISSION_DENIED.
    let (data, status) = call(&service, None, vec![1, 2, 3]).await;
    assert!(data.is_empty());
    assert_eq!(status.code, StatusCode::PermissionDenied.as_i32());

    // Wrong credential → still denied.
    let (_, status) = call(&service, Some("Bearer nope"), vec![1, 2, 3]).await;
    assert_eq!(status.code, StatusCode::PermissionDenied.as_i32());

    // Correct credential → role granted → call succeeds.
    let (data, status) = call(&service, Some("Bearer secret"), vec![1, 2, 3]).await;
    assert_eq!(status.code, 0);
    assert_eq!(data, vec![vec![3, 2, 1]]);

    node.shutdown().await;
}
