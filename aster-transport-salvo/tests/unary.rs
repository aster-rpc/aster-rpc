//! End-to-end: an Aster service reached over HTTP via the Salvo unary handler.
//!
//! Uses a raw-bytes `ServiceDispatch` (no Fory) so the test exercises the
//! dispatcher seam + Aster framing over HTTP, not the payload codec. Proves a
//! POST to `/aster/{service}/{method}` flows through `Dispatcher::dispatch_parts`
//! and comes back as Aster-framed bytes (response frame + trailer).

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{Call, RpcStatus, Server, ServiceDispatch, StatusCode};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{decode_frame, encode_frame, FLAG_END_STREAM, FLAG_TRAILER};
use salvo::http::StatusCode as HttpStatus;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};

struct EchoService;

#[aster::rpc::async_trait]
impl ServiceDispatch for EchoService {
    fn name(&self) -> &str {
        "Echo"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["unary"]
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        match method {
            // unary: respond with the request bytes reversed.
            "unary" => {
                let req = call.recv_request().await.unwrap_or_default();
                let resp: Vec<u8> = req.into_iter().rev().collect();
                let _ = call.respond(resp, &RpcStatus::ok());
            }
            other => {
                let _ = call.finish(&RpcStatus::error(StatusCode::Unimplemented, other));
            }
        }
    }
}

#[tokio::test]
async fn unary_over_http() {
    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let node = Node::start(cfg).await.unwrap();

    // Register the service, snapshot the shared dispatcher (no Iroh serve loop
    // needed — the HTTP transport drives the same dispatcher).
    let dispatcher = Server::new(&node).register(EchoService).dispatcher();
    let service = Service::new(aster_transport_salvo::router(dispatcher));

    // Request body = one Aster frame carrying [1,2,3], end-of-stream.
    let body = encode_frame(&[1, 2, 3], FLAG_END_STREAM).unwrap();

    let mut res = TestClient::post("http://localhost/aster/Echo/unary")
        .body(body)
        .send(&service)
        .await;

    assert_eq!(res.status_code, Some(HttpStatus::OK));
    let bytes = res.take_bytes(None).await.unwrap();

    // First frame = response payload (reversed); second = trailer (RpcStatus ok).
    let (payload, _flags, consumed) = decode_frame(&bytes).unwrap();
    assert_eq!(payload, vec![3, 2, 1]);

    let (trailer, tflags, _) = decode_frame(&bytes[consumed..]).unwrap();
    assert!(
        tflags & FLAG_TRAILER != 0,
        "second frame must be the trailer"
    );
    let status = decode_rpc_status(&trailer).unwrap();
    assert_eq!(status.code, 0, "expected OK trailer, got {status:?}");

    node.shutdown().await;
}
