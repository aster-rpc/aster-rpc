//! The server-side error-mapping table (dispatch design §2.7), one test per
//! row of `conformance/scenarios/error-mapping.yaml` that the Rust server can
//! produce on its own. Test names match the YAML's `rust_test` keys.
//!
//! Every call goes through `Dispatcher::dispatch_parts` with a connection id,
//! so session rules use the §7.5 connection scope.

#![cfg(feature = "rpc")]

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{
    require_role, Call, CallParts, CapabilityRequirement, Dispatcher, OutgoingFrame,
    PeerProvenance, RequestFrame, RpcStatus, Server, ServiceDispatch, StatusCode, StreamHeader,
};
use aster::{AsterConfig, Node, RelayMode};
use aster_transport_core::framing::{decode_frame, FLAG_COMPRESSED, FLAG_END_STREAM, FLAG_TRAILER};
use aster_transport_core::reactor::response_channel;
use tokio::sync::mpsc;

struct ErrorMapping;

#[async_trait::async_trait]
impl ServiceDispatch for ErrorMapping {
    fn name(&self) -> &str {
        "ErrorMapping"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["echo", "guarded", "sleeps", "raises"]
    }
    fn method_requires(&self, method: &str) -> Option<CapabilityRequirement> {
        (method == "guarded").then(|| require_role("admin"))
    }
    async fn dispatch(&self, method: &str, mut call: Call) {
        match method {
            "sleeps" => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            "raises" => {
                let mut st = RpcStatus::error(StatusCode::AlreadyExists, "custom");
                st.detail_keys.push("k".into());
                st.detail_values.push("v".into());
                let _ = call.finish(&st).await;
            }
            _ => {
                let req = call.recv_request().await.unwrap_or_default();
                let _ = call.respond(req, &RpcStatus::ok()).await;
            }
        }
    }
}

struct IrohOnly;

#[async_trait::async_trait]
impl ServiceDispatch for IrohOnly {
    fn name(&self) -> &str {
        "IrohOnly"
    }
    fn version(&self) -> i32 {
        1
    }
    fn iroh_only(&self) -> bool {
        true
    }
    async fn dispatch(&self, _method: &str, call: Call) {
        let _ = call.respond(vec![1], &RpcStatus::ok()).await;
    }
}

#[derive(Default)]
struct ErrorMappingSession;

#[async_trait::async_trait]
impl ServiceDispatch for ErrorMappingSession {
    fn name(&self) -> &str {
        "ErrorMappingSession"
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["echo"]
    }
    async fn dispatch(&self, _method: &str, call: Call) {
        let _ = call.respond(vec![1], &RpcStatus::ok()).await;
    }
}

struct Fixture {
    node: Node,
    d: Dispatcher,
}

async fn fixture() -> Fixture {
    let cfg = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build();
    let node = Node::start(cfg).await.unwrap();
    let d = Server::new(&node)
        .register(ErrorMapping)
        .register(IrohOnly)
        .register_session(ErrorMappingSession::default)
        .max_sessions_per_connection(1)
        .dispatcher();
    Fixture { node, d }
}

fn hdr(service: &str, version: i32, method: &str) -> StreamHeader {
    StreamHeader {
        service: service.into(),
        version,
        method: method.into(),
        ..Default::default()
    }
}

struct Req {
    header: Vec<u8>,
    payload: Vec<u8>,
    flags: u8,
    provenance: PeerProvenance,
}

impl Req {
    fn new(h: StreamHeader) -> Self {
        Self {
            header: aster::rpc::encode_stream_header(&h).unwrap(),
            payload: vec![9],
            flags: FLAG_END_STREAM,
            provenance: PeerProvenance::IrohAuthenticated,
        }
    }
}

async fn send(d: &Dispatcher, req: Req) -> RpcStatus {
    let (resp_tx, mut resp_rx) = response_channel();
    let (_req_tx, req_rx) = mpsc::channel::<RequestFrame>(1);
    let parts = CallParts::new(
        "peer-without-roles".into(),
        req.provenance,
        req.header,
        req.payload,
        req.flags,
        resp_tx,
        req_rx,
        Arc::new(AtomicBool::new(false)),
    )
    .with_connection_id(1);
    tokio::time::timeout(Duration::from_secs(15), d.dispatch_parts(parts))
        .await
        .expect("dispatch hung");
    let mut bytes = Vec::new();
    while let Ok(item) = resp_rx.try_recv() {
        let (frame, _lease) = item.into_parts();
        match frame {
            OutgoingFrame::Frame(b)
            | OutgoingFrame::Trailer(b)
            | OutgoingFrame::CompleteUnary(b) => bytes.extend_from_slice(&b),
        }
    }
    let mut buf = bytes.as_slice();
    let mut trailer = None;
    while !buf.is_empty() {
        let (p, f, c) = decode_frame(buf).unwrap();
        if f & FLAG_TRAILER != 0 {
            trailer = Some(decode_rpc_status(&p).unwrap());
        }
        buf = &buf[c..];
    }
    trailer.expect("every row ends with a trailer")
}

async fn expect(req: Req, want: StatusCode) -> RpcStatus {
    let f = fixture().await;
    let st = send(&f.d, req).await;
    assert_eq!(StatusCode::from_i32(st.code), want, "{st:?}");
    f.node.shutdown().await;
    st
}

#[tokio::test]
async fn malformed_header() {
    let mut r = Req::new(hdr("ErrorMapping", 1, "echo"));
    r.header = b"\xff\xff not a header".to_vec();
    expect(r, StatusCode::InvalidArgument).await;
}

#[tokio::test]
async fn missing_service() {
    expect(Req::new(hdr("", 1, "echo")), StatusCode::InvalidArgument).await;
}

#[tokio::test]
async fn unknown_service() {
    expect(
        Req::new(hdr("NoSuchService", 1, "echo")),
        StatusCode::NotFound,
    )
    .await;
}

#[tokio::test]
async fn unknown_version() {
    expect(
        Req::new(hdr("ErrorMapping", 2, "echo")),
        StatusCode::NotFound,
    )
    .await;
}

#[tokio::test]
async fn iroh_only_over_other_transport() {
    let mut r = Req::new(hdr("IrohOnly", 1, "echo"));
    r.provenance = PeerProvenance::Other;
    expect(r, StatusCode::NotFound).await;
}

#[tokio::test]
async fn unknown_method() {
    expect(
        Req::new(hdr("ErrorMapping", 1, "nope")),
        StatusCode::Unimplemented,
    )
    .await;
}

#[tokio::test]
async fn unsupported_mode() {
    let mut h = hdr("ErrorMapping", 1, "echo");
    h.serialization_mode = 3;
    expect(Req::new(h), StatusCode::InvalidArgument).await;
}

#[tokio::test]
async fn shared_with_session_id() {
    let mut h = hdr("ErrorMapping", 1, "echo");
    h.session_id = 7;
    expect(Req::new(h), StatusCode::FailedPrecondition).await;
}

#[tokio::test]
async fn session_without_session_id() {
    expect(
        Req::new(hdr("ErrorMappingSession", 1, "echo")),
        StatusCode::FailedPrecondition,
    )
    .await;
}

#[tokio::test]
async fn session_graveyard() {
    let f = fixture().await;
    let mut h = hdr("ErrorMappingSession", 1, "echo");
    h.session_id = 5;
    assert_eq!(send(&f.d, Req::new(h.clone())).await.code, 0);
    h.session_id = 3;
    let st = send(&f.d, Req::new(h)).await;
    assert_eq!(
        StatusCode::from_i32(st.code),
        StatusCode::NotFound,
        "{st:?}"
    );
    f.node.shutdown().await;
}

#[tokio::test]
async fn session_cap() {
    let f = fixture().await;
    let mut h = hdr("ErrorMappingSession", 1, "echo");
    h.session_id = 1;
    assert_eq!(send(&f.d, Req::new(h.clone())).await.code, 0);
    h.session_id = 2;
    for _ in 0..2 {
        let st = send(&f.d, Req::new(h.clone())).await;
        assert_eq!(
            StatusCode::from_i32(st.code),
            StatusCode::ResourceExhausted,
            "{st:?}"
        );
    }
    f.node.shutdown().await;
}

#[tokio::test]
async fn capability_denied() {
    expect(
        Req::new(hdr("ErrorMapping", 1, "guarded")),
        StatusCode::PermissionDenied,
    )
    .await;
}

#[tokio::test]
async fn bad_compressed_request() {
    let mut r = Req::new(hdr("ErrorMapping", 1, "echo"));
    r.payload = b"definitely not zstd".to_vec();
    r.flags |= FLAG_COMPRESSED;
    expect(r, StatusCode::InvalidArgument).await;
}

#[tokio::test]
async fn deadline_exceeded() {
    let mut h = hdr("ErrorMapping", 1, "sleeps");
    h.deadline = 1;
    expect(Req::new(h), StatusCode::DeadlineExceeded).await;
}

#[tokio::test]
async fn handler_status_passes_through() {
    let st = expect(
        Req::new(hdr("ErrorMapping", 1, "raises")),
        StatusCode::AlreadyExists,
    )
    .await;
    assert_eq!(st.message, "custom");
    assert_eq!(st.detail_keys, vec!["k".to_string()]);
    assert_eq!(st.detail_values, vec!["v".to_string()]);
}

/// First failing check wins: an unknown method on a SHARED service that also
/// carries a session id reports UNIMPLEMENTED, because method routing runs
/// before session resolution.
#[tokio::test]
async fn checks_run_in_table_order() {
    let mut h = hdr("ErrorMapping", 1, "nope");
    h.session_id = 7;
    expect(Req::new(h), StatusCode::Unimplemented).await;
}
