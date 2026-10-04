#![cfg(feature = "rpc")]
//! A corrupt client-stream / bidi request frame *after the first* fails the
//! call with `INVALID_ARGUMENT`, exactly as a corrupt first frame does, and is
//! logged as a warning under `aster::rpc::dispatch` (R10).
//!
//! Before R10 a later frame that did not inflate silently ended the request
//! stream: the handler saw a normal end of stream and answered OK with
//! whatever it had read so far. And a later frame that did not *decode*
//! reached the client as `INTERNAL`.
//!
//! The client here writes raw frames over a plain `aster/1` stream, because
//! no well-behaved client produces a corrupt frame.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::rpc::codec::decode_rpc_status;
use aster::rpc::{
    async_trait, Call, PayloadRegistry, RequestStream, ResponseSink, RpcStatus, Server,
    ServiceDispatch, StatusCode, StreamHeader, WireField, RPC_ALPN,
};
use aster::{AsterConfig, Connection, Node, RelayMode, Result};
use aster_transport_core::framing::{
    encode_frame, FLAG_COMPRESSED, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER,
};
use fory_derive::ForyStruct;
use tokio::time::timeout;

// ── services ────────────────────────────────────────────────────────────────

/// What the raw handler saw when its request stream ended.
#[derive(Default)]
struct Seen {
    ends: Vec<(u32, bool)>, // (sum so far, call.is_cancelled())
}

/// Hand-written client-stream service on the raw `Call` API, under `name`.
struct RawSum(&'static str, Arc<Mutex<Seen>>);

#[async_trait]
impl ServiceDispatch for RawSum {
    fn name(&self) -> &str {
        self.0
    }
    fn version(&self) -> i32 {
        1
    }
    fn methods(&self) -> &[&str] {
        &["sum"]
    }
    async fn dispatch(&self, _method: &str, mut call: Call) {
        let mut total = 0u32;
        while let Some(req) = call.recv_request().await {
            total += req.iter().map(|b| *b as u32).sum::<u32>();
        }
        self.1
            .lock()
            .unwrap()
            .ends
            .push((total, call.is_cancelled()));
        // A handler that treats the end of the stream as success. After a
        // corrupt frame this must not reach the client.
        let _ = call
            .respond(total.to_le_bytes().to_vec(), &RpcStatus::ok())
            .await;
    }
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "corrupt/Count")]
struct Count {
    n: i32,
}

#[aster::service(name = "Typed", version = 1)]
trait Typed {
    #[rpc(client_stream)]
    async fn sum(&self, reqs: RequestStream<Count>) -> Result<Count>;

    #[rpc(bidi_stream)]
    async fn double(&self, reqs: RequestStream<Count>, out: ResponseSink<Count>) -> Result<()>;
}

struct TypedImpl;

#[async_trait]
impl Typed for TypedImpl {
    async fn sum(&self, mut reqs: RequestStream<Count>) -> Result<Count> {
        let mut total = 0;
        while let Some(c) = reqs.recv().await? {
            total += c.n;
        }
        Ok(Count { n: total })
    }

    async fn double(&self, mut reqs: RequestStream<Count>, out: ResponseSink<Count>) -> Result<()> {
        while let Some(c) = reqs.recv().await? {
            out.send(&Count { n: c.n * 2 }).await?;
        }
        Ok(())
    }
}

// ── harness ─────────────────────────────────────────────────────────────────

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    timeout(Duration::from_secs(15), f)
        .await
        .expect("operation timed out")
}

async fn wait_for_addr(n: &Node) {
    for _ in 0..50 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Fixture {
    server: Node,
    client: Node,
    conn: Connection,
    seen: Arc<Mutex<Seen>>,
    _handle: aster::rpc::ServerHandle,
}

async fn fixture() -> Fixture {
    capture();
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()])
        .await
        .unwrap();
    let client = Node::start(cfg()).await.unwrap();
    wait_for_addr(&server).await;
    wait_for_addr(&client).await;
    client.add_peer(&server).unwrap();
    server.add_peer(&client).unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let handle = Server::new(&server)
        .register(RawSum("RawSum", seen.clone()))
        // Only the logging test calls this one, so its events are its own.
        .register(RawSum("LoggedSum", Arc::default()))
        .register(TypedServer::new(TypedImpl))
        .serve();
    let conn = within(client.connect(&server.id(), RPC_ALPN))
        .await
        .unwrap();
    Fixture {
        server,
        client,
        conn,
        seen,
        _handle: handle,
    }
}

impl Fixture {
    async fn shutdown(self) {
        self.client.shutdown().await;
        self.server.shutdown().await;
    }
}

fn header(service: &str, method: &str) -> StreamHeader {
    StreamHeader {
        service: service.into(),
        method: method.into(),
        version: 1,
        ..Default::default()
    }
}

/// What came back on one stream: data payloads, then the trailer.
struct Outcome {
    data: Vec<Vec<u8>>,
    status: RpcStatus,
}

/// One call on its own stream: the header, then `frames` exactly as given
/// (payload, flags). Reads frames until the trailer.
async fn raw_call(conn: &Connection, h: &StreamHeader, frames: &[(&[u8], u8)]) -> Outcome {
    within(async {
        let (send, recv) = conn.open_bi().await.unwrap();
        let hdr = aster::rpc::encode_stream_header(h).unwrap();
        send.write_all(encode_frame(&hdr, FLAG_HEADER).unwrap())
            .await
            .unwrap();
        for (payload, flags) in frames {
            send.write_all(encode_frame(payload, *flags).unwrap())
                .await
                .unwrap();
        }
        let mut data = Vec::new();
        loop {
            let len = recv.read_exact(4).await.unwrap();
            let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
            let body = recv.read_exact(len).await.unwrap();
            let (flags, payload) = (body[0], &body[1..]);
            if flags & FLAG_TRAILER != 0 {
                return Outcome {
                    data,
                    status: decode_rpc_status(payload).unwrap(),
                };
            }
            assert_eq!(
                flags & FLAG_COMPRESSED,
                0,
                "small responses are not compressed"
            );
            data.push(payload.to_vec());
        }
    })
    .await
}

fn payload_fory() -> fory_core::Fory {
    let mut reg = PayloadRegistry::new();
    Count::register_payload(&mut reg);
    reg.into_fory()
}

fn count(n: i32) -> Vec<u8> {
    payload_fory().serialize(&Count { n }).unwrap()
}

/// Bytes that are not a zstd frame.
const NOT_ZSTD: &[u8] = b"definitely not a zstd frame";
/// Bytes that are not a Fory `Count`.
const NOT_FORY: &[u8] = &[0xff; 12];

fn assert_invalid_argument(o: &Outcome, prefix: &str) {
    assert_eq!(
        StatusCode::from_i32(o.status.code),
        StatusCode::InvalidArgument,
        "{:?}",
        o.status
    );
    assert!(
        o.status.message.starts_with(prefix),
        "message {:?} should start with {prefix:?}",
        o.status.message
    );
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn raw_handler_corrupt_later_frame_fails_the_call() {
    let f = fixture().await;
    let o = raw_call(
        &f.conn,
        &header("RawSum", "sum"),
        &[(&[1, 2], 0), (&[3], 0), (NOT_ZSTD, FLAG_COMPRESSED)],
    )
    .await;
    assert_invalid_argument(&o, "request payload:");
    assert!(o.data.is_empty(), "the handler's OK answer never went out");

    // The handler saw the stream end -- flagged as over -- after the two good
    // frames.
    for _ in 0..100 {
        if !f.seen.lock().unwrap().ends.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(f.seen.lock().unwrap().ends, vec![(6, true)]);

    // A good call on the same connection still works.
    let o = raw_call(
        &f.conn,
        &header("RawSum", "sum"),
        &[(&[1, 2], 0), (&[3], FLAG_END_STREAM)],
    )
    .await;
    assert_eq!(o.status.code, StatusCode::Ok.as_i32(), "{:?}", o.status);
    assert_eq!(o.data, vec![6u32.to_le_bytes().to_vec()]);
    f.shutdown().await;
}

#[tokio::test]
async fn raw_handler_corrupt_first_frame_is_unchanged() {
    let f = fixture().await;
    let o = raw_call(
        &f.conn,
        &header("RawSum", "sum"),
        &[(NOT_ZSTD, FLAG_COMPRESSED | FLAG_END_STREAM)],
    )
    .await;
    assert_invalid_argument(&o, "request payload:");
    assert!(f.seen.lock().unwrap().ends.is_empty(), "never dispatched");
    f.shutdown().await;
}

#[tokio::test]
async fn typed_client_stream_corrupt_later_frame_fails_the_call() {
    let f = fixture().await;
    let one = count(1);
    let o = raw_call(
        &f.conn,
        &header("Typed", "sum"),
        &[(&one, 0), (NOT_ZSTD, FLAG_COMPRESSED)],
    )
    .await;
    assert_invalid_argument(&o, "request payload:");
    assert!(o.data.is_empty());
    f.shutdown().await;
}

#[tokio::test]
async fn typed_client_stream_undecodable_frame_is_invalid_argument() {
    let f = fixture().await;
    let one = count(1);
    // Later frame.
    let o = raw_call(
        &f.conn,
        &header("Typed", "sum"),
        &[(&one, 0), (NOT_FORY, FLAG_END_STREAM)],
    )
    .await;
    assert_invalid_argument(&o, "request decode:");
    // First frame: was INTERNAL (the handler's error), now the same status.
    let o = raw_call(
        &f.conn,
        &header("Typed", "sum"),
        &[(NOT_FORY, FLAG_END_STREAM)],
    )
    .await;
    assert_invalid_argument(&o, "request decode:");
    f.shutdown().await;
}

#[tokio::test]
async fn typed_bidi_corrupt_later_frame_fails_after_earlier_responses() {
    let f = fixture().await;
    let two = count(2);
    let o = raw_call(
        &f.conn,
        &header("Typed", "double"),
        &[(&two, 0), (NOT_ZSTD, FLAG_COMPRESSED)],
    )
    .await;
    assert_invalid_argument(&o, "request payload:");
    let fory = payload_fory();
    let got: Vec<Count> = o
        .data
        .iter()
        .map(|b| fory.deserialize::<Count>(b).unwrap())
        .collect();
    assert_eq!(
        got,
        vec![Count { n: 4 }],
        "responses before the bad frame stand"
    );
    f.shutdown().await;
}

// ── the warning ─────────────────────────────────────────────────────────────

/// Collects `aster*` events: (target, level, rendered fields).
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<(String, tracing::Level, String)>>>);

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={:?} ", field.name(), value));
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target().starts_with("aster")
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push((
            event.metadata().target().to_string(),
            *event.metadata().level(),
            fields.0,
        ));
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

/// The process-wide subscriber, installed by the first test that asks. A
/// thread-scoped one would be simpler, but tracing caches per-callsite
/// interest across threads, so tests running in parallel without it could
/// switch its callsites off. Every test in this file runs with it, which also
/// exercises the enabled logging paths.
fn capture() -> Capture {
    static CAPTURE: std::sync::OnceLock<Capture> = std::sync::OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let capture = Capture::default();
            tracing::subscriber::set_global_default(capture.clone())
                .expect("no other global subscriber in this test binary");
            capture
        })
        .clone()
}

#[tokio::test]
async fn corrupt_later_frame_is_logged_as_a_warning() {
    let capture = capture();

    let f = fixture().await;
    let o = raw_call(
        &f.conn,
        &header("LoggedSum", "sum"),
        &[(&[1], 0), (NOT_ZSTD, FLAG_COMPRESSED)],
    )
    .await;
    assert_invalid_argument(&o, "request payload:");
    // A good call, for the per-call completion event.
    let o = raw_call(
        &f.conn,
        &header("LoggedSum", "sum"),
        &[(&[1], FLAG_END_STREAM)],
    )
    .await;
    assert_eq!(o.status.code, 0);
    f.shutdown().await;

    let events: Vec<_> = capture
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, _, m)| m.contains("service=\"LoggedSum\""))
        .cloned()
        .collect();
    let warning = events
        .iter()
        .find(|(t, l, _)| t == "aster::rpc::dispatch" && *l == tracing::Level::WARN)
        .unwrap_or_else(|| panic!("no dispatch warning in {events:#?}"));
    for want in [
        "service=\"LoggedSum\"",
        "method=\"sum\"",
        "frame=1",
        "corrupt request frame",
    ] {
        assert!(
            warning.2.contains(want),
            "{want} missing from {}",
            warning.2
        );
    }
    // Every call's completion is reported at debug with its final status.
    let completions: Vec<_> = events
        .iter()
        .filter(|(t, l, m)| {
            t == "aster::rpc::dispatch"
                && *l == tracing::Level::DEBUG
                && m.contains("rpc completed")
        })
        .collect();
    assert_eq!(completions.len(), 2, "{completions:#?}");
    assert!(completions[0].2.contains("status=InvalidArgument"));
    assert!(completions[1].2.contains("status=Ok"));
}
