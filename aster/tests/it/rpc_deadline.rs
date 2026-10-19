//! Server-side deadline enforcement (Aster-SPEC §6.8, dispatch design §2.4).
//!
//! The effective timeout is `min(StreamHeader.deadline, method_timeout,
//! Server::handler_timeout)`, measured from receipt. When it passes the
//! handler's future is dropped and the caller receives `DEADLINE_EXCEEDED`.

#![cfg(feature = "rpc")]

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{
    Call, CallParts, OutgoingFrame, PeerProvenance, RequestFrame, RpcStatus, Server,
    ServiceDispatch, StatusCode, StreamHeader, RPC_ALPN,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{decode_frame, FLAG_END_STREAM, FLAG_TRAILER};
use aster_transport_core::reactor::response_channel;
use tokio::sync::mpsc;

/// `sleeps` never answers on its own; `quick` answers at once; `bounded` has a
/// 150 ms method default. `drops` counts handler futures dropped mid-flight.
struct Slow {
    dropped: Arc<AtomicUsize>,
}

struct DropFlag(Arc<AtomicUsize>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ServiceDispatch for Slow {
    fn name(&self) -> &str {
        "test.Slow"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["sleeps", "quick", "bounded"]
    }
    fn method_timeout(&self, method: &str) -> Option<Duration> {
        (method == "bounded").then_some(Duration::from_millis(150))
    }
    async fn dispatch(&self, method: &str, call: Call) {
        if method == "quick" {
            let _ = call.respond(vec![1], &RpcStatus::ok()).await;
            return;
        }
        let _flag = DropFlag(self.dropped.clone());
        tokio::time::sleep(Duration::from_secs(3600)).await;
        let _ = call.respond(vec![2], &RpcStatus::ok()).await;
    }
}

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn call(
    d: &aster::rpc::Dispatcher,
    method: &str,
    deadline: i16,
) -> (Vec<Vec<u8>>, RpcStatus, Duration) {
    let hdr = aster::rpc::encode_stream_header(&StreamHeader {
        service: "test.Slow".into(),
        method: method.into(),
        version: 1,
        deadline,
        ..Default::default()
    })
    .unwrap();
    let (resp_tx, mut resp_rx) = response_channel();
    let (_req_tx, req_rx) = mpsc::channel::<RequestFrame>(1);
    let parts = CallParts::new(
        "peer".into(),
        PeerProvenance::Other,
        hdr,
        vec![0],
        FLAG_END_STREAM,
        resp_tx,
        req_rx,
        Arc::new(AtomicBool::new(false)),
    );
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(15), d.dispatch_parts(parts))
        .await
        .expect("dispatch must end at the deadline");
    let took = started.elapsed();

    let mut bytes = Vec::new();
    while let Ok(item) = resp_rx.try_recv() {
        let (frame, _lease) = item.into_parts();
        match frame {
            OutgoingFrame::Frame(b)
            | OutgoingFrame::Trailer(b)
            | OutgoingFrame::CompleteUnary(b) => bytes.extend_from_slice(&b),
        }
    }
    let (mut data, mut trailer) = (Vec::new(), None);
    let mut buf = bytes.as_slice();
    while !buf.is_empty() {
        let (p, f, c) = decode_frame(buf).unwrap();
        if f & FLAG_TRAILER != 0 {
            trailer = Some(decode_rpc_status(&p).unwrap());
        } else {
            data.push(p);
        }
        buf = &buf[c..];
    }
    (data, trailer.expect("trailer"), took)
}

fn code(s: &RpcStatus) -> StatusCode {
    StatusCode::from_i32(s.code)
}

#[tokio::test]
async fn the_header_deadline_cancels_the_handler() {
    let node = Node::start(cfg()).await.unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let d = Server::new(&node)
        .register(Slow {
            dropped: dropped.clone(),
        })
        .dispatcher();
    let (data, status, took) = call(&d, "sleeps", 1).await;
    assert!(data.is_empty());
    assert_eq!(code(&status), StatusCode::DeadlineExceeded, "{status:?}");
    assert!(took >= Duration::from_millis(900), "fired early: {took:?}");
    assert!(took < Duration::from_secs(5), "fired late: {took:?}");
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "handler future was not dropped"
    );
    node.shutdown().await;
}

#[tokio::test]
async fn the_server_cap_applies_without_a_header_deadline() {
    let node = Node::start(cfg()).await.unwrap();
    let d = Server::new(&node)
        .register(Slow {
            dropped: Arc::default(),
        })
        .handler_timeout(Some(Duration::from_millis(100)))
        .dispatcher();
    // A generous caller deadline does not lift the server cap.
    let (_, status, took) = call(&d, "sleeps", 30).await;
    assert_eq!(code(&status), StatusCode::DeadlineExceeded, "{status:?}");
    assert!(took < Duration::from_secs(1), "{took:?}");
    node.shutdown().await;
}

#[tokio::test]
async fn the_method_default_applies() {
    let node = Node::start(cfg()).await.unwrap();
    let d = Server::new(&node)
        .register(Slow {
            dropped: Arc::default(),
        })
        .dispatcher();
    let (_, status, took) = call(&d, "bounded", 0).await;
    assert_eq!(code(&status), StatusCode::DeadlineExceeded, "{status:?}");
    assert!(took >= Duration::from_millis(140), "{took:?}");
    assert!(took < Duration::from_secs(1), "{took:?}");
    node.shutdown().await;
}

#[tokio::test]
async fn a_call_that_finishes_in_time_is_untouched() {
    let node = Node::start(cfg()).await.unwrap();
    let d = Server::new(&node)
        .register(Slow {
            dropped: Arc::default(),
        })
        .handler_timeout(Some(Duration::from_millis(200)))
        .dispatcher();
    for _ in 0..50 {
        let (data, status, _) = call(&d, "quick", 1).await;
        assert_eq!(status.code, 0, "{status:?}");
        assert_eq!(data, vec![vec![1u8]]);
    }
    // Let every registered deadline pass: none may produce a stray trailer
    // or panic after its call completed.
    tokio::time::sleep(Duration::from_millis(300)).await;
    node.shutdown().await;
}

#[tokio::test]
async fn no_cap_and_no_header_deadline_means_no_deadline() {
    let node = Node::start(cfg()).await.unwrap();
    let d = Server::new(&node)
        .register(Slow {
            dropped: Arc::default(),
        })
        .handler_timeout(None)
        .dispatcher();
    let hdr = aster::rpc::encode_stream_header(&StreamHeader {
        service: "test.Slow".into(),
        method: "sleeps".into(),
        version: 1,
        ..Default::default()
    })
    .unwrap();
    let (resp_tx, _resp_rx) = response_channel();
    let (_req_tx, req_rx) = mpsc::channel::<RequestFrame>(1);
    let parts = CallParts::new(
        "peer".into(),
        PeerProvenance::Other,
        hdr,
        vec![0],
        FLAG_END_STREAM,
        resp_tx,
        req_rx,
        Arc::new(AtomicBool::new(false)),
    );
    // short-wait-ok: the handler must still be running; expiry is the pass
    let outcome = tokio::time::timeout(Duration::from_millis(500), d.dispatch_parts(parts)).await;
    assert!(outcome.is_err(), "the handler should still be running");
    node.shutdown().await;
}

async fn within<F: Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(15), f)
        .await
        .expect("rpc operation timed out")
}

/// Over the real transport the client sees `DEADLINE_EXCEEDED` as an RPC error.
#[tokio::test]
async fn deadline_exceeded_reaches_an_iroh_client() {
    let srv = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();
    for n in [&srv, &client] {
        for _ in 0..50 {
            if !n.addr().direct_addresses.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    client.add_peer(&srv).unwrap();
    srv.add_peer(&client).unwrap();
    let _h = Server::new(&srv)
        .register(Slow {
            dropped: Arc::default(),
        })
        .serve();
    let conn = within(client.rpc_connect(&srv.id())).await.unwrap();
    let h = StreamHeader {
        service: "test.Slow".into(),
        method: "sleeps".into(),
        version: 1,
        deadline: 1,
        ..Default::default()
    };
    match within(conn.unary(&h, vec![0])).await {
        Err(aster::Error::Rpc { code, .. }) => {
            assert_eq!(code, StatusCode::DeadlineExceeded.as_i32())
        }
        other => panic!("expected DEADLINE_EXCEEDED, got {other:?}"),
    }
    client.shutdown().await;
    srv.shutdown().await;
}
