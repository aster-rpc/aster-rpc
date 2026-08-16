//! §3.3 / Slice 0: an `iroh_only` service must not be reachable over a
//! transport that cannot authenticate the peer.
//!
//! The failure this guards against is impersonation, not eavesdropping. A
//! coordination voter's identity *is* its authority — it decides who holds a
//! lease and who may change an ACL — so admitting the same apparent NodeId over
//! an HTTP or WebTransport front end, where `peer_id` may be a header the
//! client supplied, would let anyone speak as a voter.
//!
//! These tests drive `Dispatcher::dispatch_parts` directly. The two calls are
//! byte-identical apart from provenance, which is the whole property.

#![cfg(feature = "rpc")]

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use aster::rpc::{
    Call, CallParts, OutgoingFrame, PeerProvenance, RequestFrame, RpcStatus, Server,
    ServiceDispatch, StreamHeader,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::FLAG_END_STREAM;
use aster_transport_core::reactor::response_channel;
use tokio::sync::mpsc;

struct Guarded;
#[async_trait::async_trait]
impl ServiceDispatch for Guarded {
    fn name(&self) -> &str {
        "test.Guarded"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["ping"]
    }
    fn iroh_only(&self) -> bool {
        true
    }
    async fn dispatch(&self, _method: &str, call: Call) {
        // Reaching here at all is what the guarded test asserts cannot happen
        // over an unauthenticated transport. The marker payload is how we tell
        // "the handler ran" from "dispatch refused".
        let _ = call
            .respond(b"handler-ran".to_vec(), &RpcStatus::ok())
            .await;
    }
}

struct Ordinary;
#[async_trait::async_trait]
impl ServiceDispatch for Ordinary {
    fn name(&self) -> &str {
        "test.Ordinary"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["ping"]
    }
    async fn dispatch(&self, _method: &str, call: Call) {
        let _ = call
            .respond(b"handler-ran".to_vec(), &RpcStatus::ok())
            .await;
    }
}

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

/// Dispatch one call at `service` with the given provenance; return every byte
/// written back to the caller.
async fn dispatch(service: &str, provenance: PeerProvenance) -> Vec<u8> {
    let node = Node::start(cfg()).await.unwrap();
    let dispatcher = Server::new(&node)
        .register(Guarded)
        .register(Ordinary)
        .dispatcher();

    let header = aster::rpc::encode_stream_header(&StreamHeader {
        service: service.to_string(),
        version: 1,
        method: "ping".to_string(),
        ..Default::default()
    })
    .expect("encode header");

    let (resp_tx, mut resp_rx) = response_channel();
    let (_req_tx, req_rx) = mpsc::channel::<RequestFrame>(1);
    let parts = CallParts::new(
        // The SAME apparent peer id in both cases: the calls differ only in
        // what the transport can vouch for.
        "b7e1c0de00000000000000000000000000000000000000000000000000000000".into(),
        provenance,
        header,
        Vec::new(),
        FLAG_END_STREAM,
        resp_tx,
        req_rx,
        Arc::new(AtomicBool::new(false)),
    );
    dispatcher.dispatch_parts(parts).await;

    let mut out = Vec::new();
    while let Ok(item) = resp_rx.try_recv() {
        let (frame, _lease) = item.into_parts();
        let bytes = match frame {
            OutgoingFrame::Frame(b)
            | OutgoingFrame::Trailer(b)
            | OutgoingFrame::CompleteUnary(b) => b,
        };
        out.extend_from_slice(&bytes);
    }
    node.shutdown().await;
    out
}

#[tokio::test]
async fn iroh_only_service_is_unreachable_over_an_unauthenticated_transport() {
    let out = dispatch("test.Guarded", PeerProvenance::Other).await;
    assert!(!out.is_empty(), "a trailer must still be emitted");
    assert!(
        !out.windows(11).any(|w| w == b"handler-ran"),
        "the handler ran despite an unauthenticated peer"
    );
    // Rejected as not-found rather than permission-denied: over a transport
    // that cannot authenticate, the service's existence is itself not
    // something to confirm.
    assert!(
        out.windows(9).any(|w| w == b"not found"),
        "expected a not-found trailer, got {:?}",
        String::from_utf8_lossy(&out)
    );
}

#[tokio::test]
async fn iroh_only_service_is_reachable_over_an_authenticated_transport() {
    let out = dispatch("test.Guarded", PeerProvenance::IrohAuthenticated).await;
    assert!(
        out.windows(11).any(|w| w == b"handler-ran"),
        "an authenticated peer must reach the handler"
    );
}

/// The default is off, so no existing service changes behaviour.
#[tokio::test]
async fn an_ordinary_service_still_accepts_an_unauthenticated_transport() {
    let out = dispatch("test.Ordinary", PeerProvenance::Other).await;
    assert!(
        out.windows(11).any(|w| w == b"handler-ran"),
        "iroh_only defaults to false; this call must be served"
    );
}

/// Only the reactor path may claim authentication. This states behaviourally
/// what `#[non_exhaustive]` on `CallParts` enforces at compile time: a new
/// transport cannot construct one with a struct literal and get a defaulted
/// provenance.
#[test]
fn provenance_distinguishes_proof_from_hint() {
    assert!(PeerProvenance::IrohAuthenticated.is_authenticated());
    assert!(!PeerProvenance::Other.is_authenticated());
}
