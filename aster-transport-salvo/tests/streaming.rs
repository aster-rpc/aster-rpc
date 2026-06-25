//! Streaming patterns (server-stream, client-stream, bidi) over HTTP via the
//! Salvo transport. Raw-bytes `ServiceDispatch` (no Fory) — exercises the
//! framing + dispatcher seam for the three streaming patterns. Request frames
//! are sent up front (the Rust client is eager); the response is parsed back as
//! a sequence of Aster frames ending in a trailer.

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{Call, RpcStatus, Server, ServiceDispatch, StatusCode};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{decode_frame, encode_frame, FLAG_END_STREAM, FLAG_TRAILER};
use salvo::http::StatusCode as HttpStatus;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};

struct StreamSvc;

#[aster::rpc::async_trait]
impl ServiceDispatch for StreamSvc {
    fn name(&self) -> &str {
        "Stream"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["server_stream", "client_stream", "bidi"]
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        match method {
            // server-stream: request is a single byte N; emit frames [0]..=[N-1].
            "server_stream" => {
                let req = call.recv_request().await.unwrap_or_default();
                let n = req.first().copied().unwrap_or(0);
                for i in 0..n {
                    let _ = call.send(vec![i]);
                }
                let _ = call.finish(&RpcStatus::ok());
            }
            // client-stream: sum the first byte of every request; respond [sum].
            "client_stream" => {
                let mut sum: u32 = 0;
                while let Some(req) = call.recv_request().await {
                    sum += req.first().copied().unwrap_or(0) as u32;
                }
                let _ = call.respond(vec![sum as u8], &RpcStatus::ok());
            }
            // bidi: echo each request back reversed as it arrives.
            "bidi" => {
                while let Some(req) = call.recv_request().await {
                    let echoed: Vec<u8> = req.into_iter().rev().collect();
                    let _ = call.send(echoed);
                }
                let _ = call.finish(&RpcStatus::ok());
            }
            other => {
                let _ = call.finish(&RpcStatus::error(StatusCode::Unimplemented, other));
            }
        }
    }
}

/// Decode a response body into (data payloads, trailer status), asserting the
/// last frame is the trailer.
fn split_response(bytes: &[u8]) -> (Vec<Vec<u8>>, RpcStatus) {
    let mut buf = bytes;
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
    (data, trailer.expect("response missing trailer"))
}

async fn call(
    service: &Service,
    method: &str,
    req_frames: &[Vec<u8>],
) -> (Vec<Vec<u8>>, RpcStatus) {
    let mut body = Vec::new();
    for (i, f) in req_frames.iter().enumerate() {
        let last = i + 1 == req_frames.len();
        let flags = if last { FLAG_END_STREAM } else { 0 };
        body.extend_from_slice(&encode_frame(f, flags).unwrap());
    }
    let mut res = TestClient::post(format!("http://localhost/aster/Stream/{method}"))
        .body(body)
        .send(service)
        .await;
    assert_eq!(res.status_code, Some(HttpStatus::OK));
    let bytes = res.take_bytes(None).await.unwrap();
    split_response(&bytes)
}

#[tokio::test]
async fn streaming_patterns_over_http() {
    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let node = Node::start(cfg).await.unwrap();
    let dispatcher = Server::new(&node).register(StreamSvc).dispatcher();
    let service = Service::new(aster_transport_salvo::router(dispatcher));

    // server-stream: N=3 → [0],[1],[2]
    let (data, status) = call(&service, "server_stream", &[vec![3]]).await;
    assert_eq!(data, vec![vec![0u8], vec![1u8], vec![2u8]]);
    assert_eq!(status.code, 0);

    // client-stream: 5+7+9 = 21
    let (data, status) = call(&service, "client_stream", &[vec![5], vec![7], vec![9]]).await;
    assert_eq!(data, vec![vec![21u8]]);
    assert_eq!(status.code, 0);

    // bidi: each request echoed reversed
    let (data, status) = call(&service, "bidi", &[vec![1, 2], vec![3, 4]]).await;
    assert_eq!(data, vec![vec![2u8, 1u8], vec![4u8, 3u8]]);
    assert_eq!(status.code, 0);

    node.shutdown().await;
}
