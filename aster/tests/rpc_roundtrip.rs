#![cfg(feature = "rpc")]
//! Rust↔Rust RPC round-trip over the real transport — all four call patterns,
//! plus error / unknown-method / unknown-service propagation. Hermetic: relay
//! disabled, loopback bind, `add_peer` (no network / discovery), mirroring the
//! Step-1 facade tests. Payloads are raw bytes — the payload-Fory path is
//! covered by the codec unit tests; this exercises the dispatch + framing +
//! status spine.

use std::future::Future;
use std::time::Duration;

use aster::rpc::{
    Call, RpcConnection, RpcStatus, SerializationMode, Server, ServiceDispatch, StatusCode,
    StreamHeader, RPC_ALPN,
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
        service: "Echo".into(),
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

/// A raw-bytes echo/transform service exercising all four patterns.
struct EchoService;

#[async_trait::async_trait]
impl ServiceDispatch for EchoService {
    fn name(&self) -> &str {
        "Echo"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &[
            "unary",
            "server_stream",
            "client_stream",
            "bidi",
            "fails",
            "drops",
            "half_unary",
            "client_no_resp",
        ]
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        match method {
            // unary: respond with the request bytes reversed.
            "unary" => {
                let req = call.recv_request().await.unwrap_or_default();
                let resp: Vec<u8> = req.into_iter().rev().collect();
                let _ = call.respond(resp, &RpcStatus::ok());
            }
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
            // bidi: echo each request back (reversed) as it arrives.
            "bidi" => {
                while let Some(req) = call.recv_request().await {
                    let echoed: Vec<u8> = req.into_iter().rev().collect();
                    let _ = call.send(echoed);
                }
                let _ = call.finish(&RpcStatus::ok());
            }
            // always fails with NOT_FOUND, to exercise error propagation.
            "fails" => {
                let _ = call.recv_request().await;
                let _ = call.finish(&RpcStatus::error(StatusCode::NotFound, "nope"));
            }
            // drops the Call without sending a trailer — the client must observe
            // an error, not a clean/empty success.
            "drops" => {
                let _ = call.recv_request().await;
                drop(call);
            }
            // unary handler that writes a data frame then drops without a trailer:
            // the client must reject the (synthesized empty) trailer, not return Ok.
            "half_unary" => {
                let _ = call.recv_request().await;
                let _ = call.send(vec![9]);
                drop(call);
            }
            // client-stream handler that finishes OK with no response data frame:
            // the client must error (a client-stream owes exactly one response).
            "client_no_resp" => {
                while call.recv_request().await.is_some() {}
                let _ = call.finish(&RpcStatus::ok());
            }
            // Defensive: the server rejects unlisted methods (see `methods`)
            // before dispatch, so this is unreachable in practice.
            other => {
                let _ = call.finish(&RpcStatus::error(StatusCode::Unimplemented, other));
            }
        }
    }
}

#[tokio::test]
async fn all_patterns_and_errors() {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();

    // Dropping the handle detaches the loop (tokio doesn't abort on drop); keep
    // it bound for clarity.
    let _h = Server::new(&server).register(EchoService).serve();

    let conn: RpcConnection = within(client.rpc_connect(&server.id())).await.unwrap();

    // unary
    let resp = within(conn.unary(&header("unary"), vec![1, 2, 3]))
        .await
        .unwrap();
    assert_eq!(resp, vec![3, 2, 1]);

    // server-stream
    let mut s = within(conn.server_stream(&header("server_stream"), vec![3]))
        .await
        .unwrap();
    let items = within(s.collect()).await.unwrap();
    assert_eq!(items, vec![vec![0u8], vec![1u8], vec![2u8]]);

    // client-stream
    let resp =
        within(conn.client_stream(&header("client_stream"), vec![vec![5], vec![7], vec![9]]))
            .await
            .unwrap();
    assert_eq!(resp, vec![21u8]);

    // bidi
    let mut s = within(conn.bidi(&header("bidi"), vec![vec![1, 2], vec![3, 4]]))
        .await
        .unwrap();
    let items = within(s.collect()).await.unwrap();
    assert_eq!(items, vec![vec![2u8, 1u8], vec![4u8, 3u8]]);

    // error propagation (non-OK trailer → Error::Rpc)
    let err = within(conn.unary(&header("fails"), vec![0]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::NotFound.as_i32()),
        "expected NOT_FOUND, got {err:?}"
    );

    // unknown method → UNIMPLEMENTED
    let err = within(conn.unary(&header("ghost"), vec![0]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::Unimplemented.as_i32()),
        "expected UNIMPLEMENTED, got {err:?}"
    );

    // unknown service → NOT_FOUND (distinct from unknown method; Aster-SPEC §6.5)
    let mut h = header("unary");
    h.service = "Nope".into();
    let err = within(conn.unary(&h, vec![0])).await.unwrap_err();
    assert!(
        matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::NotFound.as_i32()),
        "expected NOT_FOUND for unknown service, got {err:?}"
    );

    // unsupported serialization mode → INVALID_ARGUMENT
    let mut h = header("unary");
    h.serialization_mode = 3; // JSON — not supported in Rust
    let err = within(conn.unary(&h, vec![1])).await.unwrap_err();
    assert!(
        matches!(&err, aster::Error::Rpc { code, .. } if *code == StatusCode::InvalidArgument.as_i32()),
        "expected INVALID_ARGUMENT for unsupported mode, got {err:?}"
    );

    // empty client-stream request list → fail early, client-side
    let err = within(conn.client_stream(&header("client_stream"), vec![]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, aster::Error::InvalidArgument(_)),
        "expected InvalidArgument for empty client-stream, got {err:?}"
    );

    // empty bidi request list → fail early, client-side (ResponseStream isn't
    // Debug, so match rather than unwrap_err)
    let res = within(conn.bidi(&header("bidi"), vec![])).await;
    assert!(
        matches!(res, Err(aster::Error::InvalidArgument(_))),
        "expected InvalidArgument for empty bidi"
    );

    // streaming handler drops Call without a trailer → error, not Ok(empty)
    let mut s = within(conn.server_stream(&header("drops"), vec![1]))
        .await
        .unwrap();
    let res = within(s.collect()).await;
    assert!(
        res.is_err(),
        "missing trailer must surface as an error, got {res:?}"
    );

    // unary handler sends a data frame then drops without a trailer → error
    let err = within(conn.unary(&header("half_unary"), vec![1]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, aster::Error::Connection(_)),
        "unary missing trailer must error, got {err:?}"
    );

    // client-stream handler finishes OK with no response frame → error
    let err = within(conn.client_stream(&header("client_no_resp"), vec![vec![1]]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, aster::Error::Connection(_)),
        "client-stream with no response must error, got {err:?}"
    );

    client.shutdown().await;
    server.shutdown().await;
}
