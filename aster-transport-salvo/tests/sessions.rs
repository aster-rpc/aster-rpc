//! Session-scoped services over HTTP: `register_session` gives each
//! `aster-session-id` its own service instance (private state). Different
//! session ids are independent; the same id shares state; no id is rejected.

use std::sync::atomic::{AtomicU32, Ordering};

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{Call, RpcStatus, Server, ServiceDispatch, StatusCode};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{decode_frame, encode_frame, FLAG_END_STREAM, FLAG_TRAILER};
use salvo::http::StatusCode as HttpStatus;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};

/// Session-scoped counter — one instance per session, so its count is private.
#[derive(Default)]
struct Counter {
    n: AtomicU32,
}

#[aster::rpc::async_trait]
impl ServiceDispatch for Counter {
    fn name(&self) -> &str {
        "Counter"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["incr"]
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let _ = call.recv_request().await;
        let v = self.n.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = call.respond(vec![v as u8], &RpcStatus::ok());
    }
}

/// POST incr with an optional session id; return (data payloads, trailer).
async fn incr(service: &Service, session: Option<&str>) -> (Vec<Vec<u8>>, RpcStatus) {
    let body = encode_frame(&[0u8], FLAG_END_STREAM).unwrap();
    let mut b = TestClient::post("http://localhost/aster/Counter/incr").body(body);
    if let Some(s) = session {
        b = b.add_header("aster-session-id", s, true);
    }
    let mut res = b.send(service).await;
    assert_eq!(res.status_code, Some(HttpStatus::OK));
    let bytes = res.take_bytes(None).await.unwrap();
    let mut buf = bytes.as_ref();
    let (mut data, mut trailer) = (Vec::new(), None);
    while !buf.is_empty() {
        let (p, f, c) = decode_frame(buf).unwrap();
        if f & FLAG_TRAILER != 0 {
            trailer = Some(decode_rpc_status(&p).unwrap());
        } else {
            data.push(p);
        }
        buf = &buf[c..];
    }
    (data, trailer.expect("trailer"))
}

#[tokio::test]
async fn session_scoped_instances() {
    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let node = Node::start(cfg).await.unwrap();
    let dispatcher = Server::new(&node)
        .register_session(Counter::default)
        .dispatcher();
    let service = Service::new(aster_transport_salvo::router(dispatcher));

    // Session A counts independently.
    assert_eq!(incr(&service, Some("A")).await.0, vec![vec![1u8]]);
    assert_eq!(incr(&service, Some("A")).await.0, vec![vec![2u8]]);
    // Session B starts fresh.
    assert_eq!(incr(&service, Some("B")).await.0, vec![vec![1u8]]);
    // A keeps its own state.
    assert_eq!(incr(&service, Some("A")).await.0, vec![vec![3u8]]);

    // No session id → INVALID_ARGUMENT (session-scoped service requires one).
    let (data, status) = incr(&service, None).await;
    assert!(data.is_empty());
    assert_eq!(status.code, StatusCode::InvalidArgument.as_i32());

    node.shutdown().await;
}
