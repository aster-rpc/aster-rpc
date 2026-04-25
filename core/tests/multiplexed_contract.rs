//! Tier-1 contract tests for the multiplexed-streams architecture.
//!
//! These run against real in-memory iroh endpoints (via `CoreNetClient`)
//! so they exercise the actual QUIC send/recv + reactor plumbing — they
//! are NOT unit tests on abstract mocks. Everything below the binding
//! layer is covered: pool accounting, stream reuse, dispatch error paths,
//! connection-lifecycle events.
//!
//! See `ffi_spec/Aster-multiplexed-streams.md` §3, §6, §7.5 for the
//! spec-level invariants these tests lock in.
//!
//! What's tested here:
//!
//! 1. `open_streaming_substream` bypasses the pool. Opening one or many
//!    streaming substreams never mutates the per-connection pool's
//!    `open_count` / `free_count` for any key (§3 line 65).
//!
//! 2. A single multiplexed stream carries multiple calls back-to-back
//!    (§6 "every stream is multiplexed"). The dispatcher loops on
//!    StreamHeader+request pairs until the peer closes the stream.
//!
//! 3. A stream whose first frame is NOT `FLAG_HEADER` produces a
//!    typed dispatch error without corrupting the reactor event
//!    channel — subsequent streams on the same connection still
//!    dispatch cleanly.
//!
//! 4. `ConnectionClosed` is emitted exactly once per connection after
//!    the peer closes, carrying the same `connection_id` as prior
//!    `Call` events on that connection. Bindings key their per-
//!    connection session map on this id (§7.5).
//!
//! These are the spec invariants that the §4.4 fix relies on. If a
//! future change regresses any of them, the binding-level tests in
//! Python / TS / Java would surface the symptom but not the root
//! cause; this file pins the cause at the core layer.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use proptest::prelude::*;
use tokio::sync::Mutex;
use tokio::time::timeout;

use aster_transport_core::framing::{
    encode_frame, FLAG_CALL, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER,
};
use aster_transport_core::pool::PoolKey;
use aster_transport_core::reactor::{create_reactor, IncomingCall, OutgoingFrame, ReactorEvent};
use aster_transport_core::{CoreConnection, CoreNetClient};

const TEST_ALPN: &[u8] = b"aster-test/contract";
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// Start a reactor that feeds on connections accepted by the given
/// server endpoint. Returns the reactor handle (for `next_event`) and
/// a task handle for the accept loop so tests can keep it alive.
async fn start_server_reactor(
    server: CoreNetClient,
) -> (
    aster_transport_core::reactor::ReactorHandle,
    tokio::task::JoinHandle<()>,
) {
    let (handle, feeder) = create_reactor(&tokio::runtime::Handle::current(), 256);
    let accept_task = tokio::spawn(async move {
        loop {
            match server.accept().await {
                Ok(conn) => feeder.feed(conn),
                Err(_) => return,
            }
        }
    });
    (handle, accept_task)
}

/// Stand up a server + client pair bound to the contract ALPN and
/// return the client-side `CoreConnection` plus the server reactor.
async fn setup_pair() -> Result<(
    CoreConnection,
    aster_transport_core::reactor::ReactorHandle,
    tokio::task::JoinHandle<()>,
    CoreNetClient, // keep the server alive — dropping it closes the endpoint
    CoreNetClient, // and the client too
)> {
    let server = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let client = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let server_id = server.endpoint_id();
    let (reactor, accept_task) = start_server_reactor(server.clone()).await;
    let conn = client.connect(server_id, TEST_ALPN.to_vec()).await?;
    Ok((conn, reactor, accept_task, server, client))
}

/// Drive one well-formed unary call on an already-acquired stream.
/// Writes `StreamHeader(FLAG_HEADER)`, `request(FLAG_END_STREAM)`,
/// then reads the trailer off the recv side. Used by the stream-reuse
/// test below.
async fn drive_unary_call(
    stream: &(
        aster_transport_core::CoreSendStream,
        aster_transport_core::CoreRecvStream,
    ),
    header: &[u8],
    request: &[u8],
) -> Result<()> {
    let (send, recv) = stream;
    let header_frame = encode_frame(header, FLAG_HEADER)?;
    send.write_all(header_frame).await?;
    let req_frame = encode_frame(request, FLAG_END_STREAM)?;
    send.write_all(req_frame).await?;
    // Drain frames until we see FLAG_TRAILER.
    loop {
        let mut len_bytes = [0u8; 4];
        recv.read_exact_into(&mut len_bytes).await?;
        let frame_body_len = u32::from_le_bytes(len_bytes) as usize;
        let mut flags_buf = [0u8; 1];
        recv.read_exact_into(&mut flags_buf).await?;
        let flags = flags_buf[0];
        let payload_len = frame_body_len - 1;
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            recv.read_exact_into(&mut payload).await?;
        }
        if flags & FLAG_TRAILER != 0 {
            return Ok(());
        }
    }
}

type AckBinding = (
    Arc<Mutex<Vec<IncomingCall>>>,
    Arc<Mutex<Vec<u64>>>, // connection_ids seen in ConnectionClosed
    tokio::task::JoinHandle<()>,
);

/// Spin up a "binding" task that reads calls off the reactor event
/// channel, logs them into the shared sink, and trivially acks each
/// with an empty OK trailer so the client-side stream drain returns.
/// Returns the sink so tests can assert on the calls received.
fn spawn_ack_binding(mut reactor: aster_transport_core::reactor::ReactorHandle) -> AckBinding {
    let calls = Arc::new(Mutex::new(Vec::<IncomingCall>::new()));
    let closed = Arc::new(Mutex::new(Vec::<u64>::new()));
    let calls_c = calls.clone();
    let closed_c = closed.clone();
    let task = tokio::spawn(async move {
        while let Some(event) = reactor.next_event().await {
            match event {
                ReactorEvent::Call(call) => {
                    // Synthesize an empty OK trailer so the client's
                    // drain returns. The binding contract says empty
                    // trailer payload == clean OK (see
                    // bindings/python/aster/transport/iroh.py
                    // `check_trailer`).
                    let trailer = encode_frame(&[], FLAG_TRAILER).unwrap();
                    let _ = call.response_sender.send(OutgoingFrame::Trailer(trailer));
                    calls_c.lock().await.push(call);
                }
                ReactorEvent::ConnectionClosed { connection_id, .. } => {
                    closed_c.lock().await.push(connection_id);
                }
            }
        }
    });
    (calls, closed, task)
}

// ============================================================================
// 1. Streaming substreams bypass the pool.
// ============================================================================

/// Open N streaming substreams and assert the per-connection pool's
/// counters stay at zero for every key. This is the core-level gate
/// for spec §3 line 65 ("streaming substreams don't count against
/// any pool").
///
/// Pre-fix regression: if a future change routes
/// `open_streaming_substream` through `acquire_stream`, the assertions
/// below fire with `open_count > 0`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_substream_does_not_mutate_pool_stats() -> Result<()> {
    let (conn, reactor, _accept, _server, _client) = setup_pair().await?;
    let (calls, closed, _binding) = spawn_ack_binding(reactor);

    // Fresh pool — open_count is 0 for every key before we do anything.
    assert_eq!(conn.pool().open_count(&None), 0);
    assert_eq!(conn.pool().open_count(&Some(vec![1, 0, 0, 0])), 0);

    // PART A: drive a real unary call through a streaming substream.
    // This proves `open_streaming_substream` returns a WORKING stream,
    // not a dummy — a regression that returned a non-functional stub
    // would pass the "counters are zero" check but fail the round
    // trip. Also verifies the dispatch path on the server side
    // doesn't care whether the stream came from the pool or
    // directly.
    let driven = conn.open_streaming_substream().await?;
    timeout(
        STEP_TIMEOUT,
        drive_unary_call(&driven, b"streaming-drive", b"request-body"),
    )
    .await??;

    // Counters still zero after a completed streaming call.
    assert_eq!(
        conn.pool().open_count(&None),
        0,
        "SHARED pool should be untouched even after a streaming call completes"
    );

    // PART B: open 4 more streaming substreams but DON'T use them.
    // Counters must remain zero. This is the original shape of the
    // test — retained as the structural bypass check.
    let mut streams = vec![driven];
    for _ in 0..4 {
        let s = conn.open_streaming_substream().await?;
        streams.push(s);
    }

    // Core invariant: the pool counters for every key are still 0.
    // `open_streaming_substream` must not touch the pool.
    assert_eq!(
        conn.pool().open_count(&None),
        0,
        "SHARED pool should be untouched by streaming substreams"
    );
    assert_eq!(
        conn.pool().free_count(&None),
        0,
        "SHARED free list should be untouched"
    );
    for slot in 1u32..=4 {
        let key: PoolKey = Some(slot.to_le_bytes().to_vec());
        assert_eq!(
            conn.pool().open_count(&key),
            0,
            "session pool {slot:?} should be untouched by streaming substreams"
        );
    }

    // Negative space: the binding saw exactly ONE call (from part A)
    // and zero ConnectionClosed events. Opening (but not using) the
    // other 4 substreams should produce no IncomingCall events.
    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let calls = calls.lock().await;
        assert_eq!(
            calls.len(),
            1,
            "expected exactly one call (from the driven streaming substream), got {}",
            calls.len()
        );
        assert_eq!(calls[0].header_payload, b"streaming-drive");
    }
    assert_eq!(
        closed.lock().await.len(),
        0,
        "no ConnectionClosed should have fired during this test"
    );

    // Clean up: drop streaming substreams. Still no pool churn.
    drop(streams);
    assert_eq!(conn.pool().open_count(&None), 0);
    Ok(())
}

/// Mixed workload: interleave pooled acquires with streaming substream
/// opens and verify the two accounting paths don't cross-contaminate.
/// Specifically, acquiring pool streams should grow the pool but not
/// the streaming count, and opening streaming substreams should do the
/// inverse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pooled_and_streaming_acquires_are_independent() -> Result<()> {
    let (conn, reactor, _accept, _server, _client) = setup_pair().await?;
    let (_calls, _closed, _binding) = spawn_ack_binding(reactor);

    // 1. Three pooled SHARED stream acquires → open_count=3.
    let p1 = conn.acquire_stream(None).await.unwrap();
    let p2 = conn.acquire_stream(None).await.unwrap();
    let p3 = conn.acquire_stream(None).await.unwrap();
    assert_eq!(conn.pool().open_count(&None), 3);

    // 2. Two streaming substreams → pool still 3.
    let _s1 = conn.open_streaming_substream().await?;
    let _s2 = conn.open_streaming_substream().await?;
    assert_eq!(
        conn.pool().open_count(&None),
        3,
        "streaming substreams must not be counted against the pool"
    );

    // 3. Release one pooled stream → still 3 open (returned to free),
    //    2 in-flight.
    drop(p1);
    assert_eq!(conn.pool().open_count(&None), 3);
    assert_eq!(conn.pool().free_count(&None), 1);

    // 4. Discard another → open_count drops to 2.
    p2.discard();
    assert_eq!(conn.pool().open_count(&None), 2);

    // Streaming substreams are still irrelevant to pool accounting at
    // every step.
    drop(p3);
    Ok(())
}

// ============================================================================
// 2. Stream reuse — one multiplexed stream carries two calls.
// ============================================================================

/// Spec §6 guarantee: a multiplexed stream is reused for multiple
/// calls. Each call is a StreamHeader + request(END_STREAM) pair; the
/// dispatcher loops on them until the peer closes the stream. This
/// test opens one bi-stream, drives two back-to-back unary calls over
/// it, and asserts the reactor surfaced two distinct `IncomingCall`
/// events on the same `stream_id`.
///
/// Pre-fix regression: if a future change causes the dispatcher to
/// exit after the first trailer (instead of looping back to the next
/// StreamHeader), the second call times out and the assertion fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiplexed_stream_carries_multiple_sequential_calls() -> Result<()> {
    let (conn, reactor, _accept, _server, _client) = setup_pair().await?;
    let (calls, closed, _binding) = spawn_ack_binding(reactor);

    // Acquire a first pooled stream and drive two calls on it back-to-back.
    let handle1 = conn.acquire_stream(None).await.unwrap();
    let stream1 = handle1.get();
    timeout(
        STEP_TIMEOUT,
        drive_unary_call(stream1, b"header-one", b"request-one"),
    )
    .await??;
    timeout(
        STEP_TIMEOUT,
        drive_unary_call(stream1, b"header-two", b"request-two"),
    )
    .await??;

    // Now acquire a SECOND pooled stream (concurrently held with the
    // first so the pool grows to 2 — LIFO reuse can't short-circuit
    // this) and drive one call on it. This gives us a reference
    // point for the `stream_id` identity check: same-stream calls
    // must carry the same id AND different-stream calls must carry a
    // different id. Without the second data point the first
    // assertion is reflexive and meaningless — a bug that assigned
    // every stream the same id would pass.
    let handle2 = conn.acquire_stream(None).await.unwrap();
    let stream2 = handle2.get();
    timeout(
        STEP_TIMEOUT,
        drive_unary_call(stream2, b"header-three", b"request-three"),
    )
    .await??;

    // Drain: the binding task sees each call asynchronously.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let calls = calls.lock().await;
    assert_eq!(
        calls.len(),
        3,
        "expected three total calls (two on stream1, one on stream2)"
    );
    assert_eq!(
        calls[0].stream_id, calls[1].stream_id,
        "calls multiplexed on one bi-stream should share a stream_id"
    );
    assert_ne!(
        calls[0].stream_id, calls[2].stream_id,
        "calls on distinct bi-streams should have distinct stream_ids — \
         the multiplexing invariant is meaningful only if different streams \
         genuinely get different ids"
    );
    assert_eq!(calls[0].header_payload, b"header-one");
    assert_eq!(calls[1].header_payload, b"header-two");
    assert_eq!(calls[2].header_payload, b"header-three");

    // Negative space: no ConnectionClosed during the test. Both
    // streams are still open (the handles are held below), so no
    // close event should have fired as a side effect.
    assert_eq!(
        closed.lock().await.len(),
        0,
        "ConnectionClosed fired unexpectedly during stream-reuse test"
    );
    drop(handle1);
    drop(handle2);
    Ok(())
}

// ============================================================================
// 3. Malformed first frame is a terminal dispatch error.
// ============================================================================

/// Spec §6 requires the first frame on every stream to carry
/// `FLAG_HEADER`. A misbehaving client that opens a stream and
/// immediately sends a `FLAG_CALL` frame (or any non-header first
/// frame) must cause the dispatcher to terminate *that stream* with a
/// typed error, without corrupting the reactor event channel — the
/// next well-behaved stream on the same connection still gets
/// dispatched normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_first_frame_does_not_corrupt_reactor() -> Result<()> {
    let (conn, reactor, _accept, _server, _client) = setup_pair().await?;
    let (calls, closed, _binding) = spawn_ack_binding(reactor);

    // Open a streaming substream and deliberately send a non-header
    // first frame. Any non-FLAG_HEADER value triggers the guard at
    // reactor.rs:368.
    let (bad_send, _bad_recv) = conn.open_streaming_substream().await?;
    let junk_frame = encode_frame(b"junk", FLAG_CALL)?;
    bad_send.write_all(junk_frame).await?;
    // Drop the stream so the reactor's reader task sees EOF and
    // surfaces the error via the dispatch loop's guard.
    drop(bad_send);

    // Give the reactor a moment to process and reject the bad stream
    // BEFORE we open the good one. This makes the "the malformed
    // stream produced zero events" assertion meaningful — without
    // the settle window, the bad stream might not have been
    // dispatched yet when we check.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        calls.lock().await.len(),
        0,
        "malformed stream should not have produced any IncomingCall events"
    );

    // Now drive a well-formed call over a fresh stream on the same
    // connection. It must succeed — the malformed stream's error
    // must not have poisoned the reactor.
    let handle = conn.acquire_stream(None).await.unwrap();
    timeout(
        STEP_TIMEOUT,
        drive_unary_call(handle.get(), b"well-formed", b"payload"),
    )
    .await??;

    tokio::time::sleep(Duration::from_millis(50)).await;
    let calls = calls.lock().await;
    assert_eq!(
        calls.len(),
        1,
        "only the well-formed call should have been surfaced"
    );
    assert_eq!(calls[0].header_payload, b"well-formed");
    // Negative space: no ConnectionClosed fired as a side effect of
    // the malformed stream. Rejecting a bad stream must NOT tear
    // down the parent connection.
    assert_eq!(
        closed.lock().await.len(),
        0,
        "ConnectionClosed fired unexpectedly after malformed stream — \
         rejecting a bad stream must not tear down the parent connection"
    );
    Ok(())
}

// ============================================================================
// 4. ConnectionClosed is emitted exactly once after peer close.
// ============================================================================

/// Spec §7.5 guarantee: every connection produces at most one
/// `ConnectionClosed` event, fired after the peer has closed the
/// connection. The `connection_id` on the event matches the id on
/// prior `Call` events so the binding can reap per-connection state
/// (session map, graveyard). This test makes one call over a fresh
/// connection, then drops the connection, and asserts on the event
/// ordering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_closed_emitted_once_per_connection() -> Result<()> {
    let (conn, reactor, _accept, _server, _client) = setup_pair().await?;
    let (calls, closed, _binding) = spawn_ack_binding(reactor);

    // Drive one call so we know a Call event preceded the close.
    let handle = conn.acquire_stream(None).await.unwrap();
    timeout(STEP_TIMEOUT, drive_unary_call(handle.get(), b"h", b"r")).await??;
    drop(handle);

    // Tear the connection down. Dropping `CoreConnection` does NOT
    // force-close the underlying QUIC connection — we have to call
    // `close` explicitly so the server's `accept_bi` loop exits and
    // the reactor emits `ConnectionClosed`.
    let _ = conn.close(0, b"bye".to_vec());

    // Wait (with timeout) for the ConnectionClosed event to land.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if !closed.lock().await.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for ConnectionClosed event");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Give the reactor a bit of extra time in case duplicates would
    // arrive late, then assert on the final state.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let calls = calls.lock().await;
    let closed = closed.lock().await;
    assert_eq!(closed.len(), 1, "expected exactly one ConnectionClosed");
    assert!(!calls.is_empty(), "expected at least one prior call");
    assert_eq!(
        calls[0].connection_id, closed[0],
        "ConnectionClosed.connection_id must match prior Call.connection_id"
    );
    Ok(())
}

/// The previous test closes a connection after its in-flight call has
/// cleanly drained (trailer received). This variant closes a
/// connection with a call STILL mid-dispatch: the client writes the
/// StreamHeader but never sends the request, so the reactor's
/// dispatch loop is parked on `frame_rx.recv().await` waiting for
/// the first request frame when the connection goes away.
///
/// Spec §7.5 invariant: the reactor must STILL emit exactly one
/// `ConnectionClosed` event even when calls were mid-dispatch at
/// close time. The binding relies on this to reap per-connection
/// state without caring about per-call cleanup races.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_closed_emitted_with_in_flight_calls() -> Result<()> {
    let (conn, reactor, _accept, _server, _client) = setup_pair().await?;
    let (_calls, closed, _binding) = spawn_ack_binding(reactor);

    // Open a streaming substream, write ONLY the stream header, and
    // NOT send the request frame. The reactor's dispatch loop now
    // sits in `frame_rx.recv().await` waiting for frame 2.
    let (send, _recv) = conn.open_streaming_substream().await?;
    let header_frame = encode_frame(b"stuck-in-flight", FLAG_HEADER)?;
    send.write_all(header_frame).await?;
    // Keep `send` alive so QUIC doesn't see an EOF. We want the
    // connection to be closed out from under the reactor, not the
    // stream to be closed cleanly.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Tear down the connection. The reactor's per-stream reader task
    // should surface a read error, dispatch should exit, and the
    // connection loop should emit exactly one `ConnectionClosed`.
    let _ = conn.close(0, b"bye-in-flight".to_vec());

    // Wait up to 10s for ConnectionClosed. Then verify exactly one
    // fired — duplicates or missing events are both regressions.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if !closed.lock().await.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("ConnectionClosed never fired after closing a connection with a call in flight");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Settle window for any spurious late events.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        closed.lock().await.len(),
        1,
        "expected exactly one ConnectionClosed even with an in-flight call"
    );
    Ok(())
}

// ============================================================================
// 5. Adversarial byte-stream fuzz (property-based).
// ============================================================================
//
// The integration tests above each pin one specific scenario. This
// section runs a property-based fuzz against the reactor: proptest
// generates arbitrary byte sequences (pure random, random framed, or
// mixed), writes them to a fresh streaming substream, and then asserts
// three liveness invariants:
//
//   (a) The reactor does not panic or deadlock -- a well-formed call
//       on a FRESH pooled stream must still dispatch cleanly after
//       the adversarial bytes.
//   (b) No spurious `ConnectionClosed` event fired (the connection
//       should still be alive).
//   (c) The pool is quiescent (`open_count == free_count`) -- no
//       handles leaked across the adversarial path.
//
// A single endpoint pair + reactor is shared across all property cases
// via `OnceLock` so the test runs in a couple of seconds end-to-end
// instead of paying the endpoint-setup cost per case.

struct AdversarialHarness {
    conn: CoreConnection,
    closed: Arc<Mutex<Vec<u64>>>,
    // Keep the supporting tasks + endpoints alive for the whole
    // process. These are `_` because the test body doesn't reference
    // them directly; dropping them would tear down the connection.
    _calls: Arc<Mutex<Vec<IncomingCall>>>,
    _server: CoreNetClient,
    _client: CoreNetClient,
    _accept: tokio::task::JoinHandle<()>,
    _binding: tokio::task::JoinHandle<()>,
}

fn shared_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
}

fn shared_harness() -> &'static AdversarialHarness {
    static H: OnceLock<AdversarialHarness> = OnceLock::new();
    H.get_or_init(|| {
        shared_runtime().block_on(async {
            let (conn, reactor, accept, server, client) = setup_pair().await.unwrap();
            let (calls, closed, binding) = spawn_ack_binding(reactor);
            AdversarialHarness {
                conn,
                closed,
                _calls: calls,
                _server: server,
                _client: client,
                _accept: accept,
                _binding: binding,
            }
        })
    })
}

/// Generate one synthetic framed byte blob: a 4-byte LE length prefix,
/// a random 1-byte flags field, and a random payload. Lengths are
/// bounded to stay under the reactor's `MAX_FRAME_SIZE` by a wide
/// margin so the test covers the "frame decodes, router processes it"
/// branch as well as the "length-prefix rejected" branch.
fn any_framed_blob() -> impl Strategy<Value = Vec<u8>> {
    (any::<u8>(), proptest::collection::vec(any::<u8>(), 0..256)).prop_map(|(flags, payload)| {
        let body_len = (payload.len() + 1) as u32;
        let mut out = Vec::with_capacity(4 + payload.len() + 1);
        out.extend_from_slice(&body_len.to_le_bytes());
        out.push(flags);
        out.extend_from_slice(&payload);
        out
    })
}

/// Generate adversarial wire bytes. Three shapes:
/// - Pure random bytes up to 512 (mostly rejected at length-prefix
///   validation, exercises the early-error branch).
/// - Random sequence of well-framed blobs with random flags (exercises
///   the dispatch loop and flag-validation paths).
/// - Concatenation of the two (frame + garbage, garbage + frame).
fn any_adversarial_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..512),
        proptest::collection::vec(any_framed_blob(), 0..6)
            .prop_map(|frames| frames.into_iter().flatten().collect::<Vec<u8>>()),
        (
            proptest::collection::vec(any::<u8>(), 0..128),
            proptest::collection::vec(any_framed_blob(), 0..4)
        )
            .prop_map(|(garbage, frames)| {
                let mut out = garbage;
                for f in frames {
                    out.extend_from_slice(&f);
                }
                out
            }),
    ]
}

proptest! {
    #![proptest_config(proptest::test_runner::Config {
        // 48 cases covers the strategy space without blowing out
        // wall-clock; each case opens a fresh QUIC substream on the
        // shared connection (fast, a few ms apiece).
        cases: 48,
        .. proptest::test_runner::Config::default()
    })]

    /// **The tier-1 adversarial property**: no matter what bytes the
    /// client sends on a multiplexed stream, the reactor (a) does not
    /// panic, (b) does not deadlock, and (c) is still responsive to
    /// well-formed calls on fresh streams afterwards.
    ///
    /// This is the single most valuable invariant in the tier-1
    /// suite: it covers the entire adversarial wire surface (random
    /// flags, truncated length prefixes, oversized frames that hit
    /// `MAX_FRAME_SIZE`, `FLAG_TRAILER`-first, `FLAG_HEADER` twice in
    /// a row, and every other pathological combination) in one
    /// property instead of requiring a hand-written case per shape.
    ///
    /// Liveness is asserted by driving a well-formed unary call on a
    /// FRESH pooled stream after the adversarial write. If the
    /// reactor hung, the call times out via `drive_unary_call`'s
    /// inner read and the property fails.
    #[test]
    fn reactor_survives_arbitrary_stream_bytes(bytes in any_adversarial_bytes()) {
        let rt = shared_runtime();
        let h = shared_harness();

        // Snapshot the `closed` counter before the adversarial write
        // so the negative-space assertion can detect any spurious
        // ConnectionClosed event that fires as a side effect.
        let closed_before = rt.block_on(async { h.closed.lock().await.len() });

        // Phase 1: write adversarial bytes onto a fresh streaming
        // substream and drop it. A write failure is NOT a test
        // failure -- the reactor rejecting bytes at the QUIC layer
        // is a legitimate outcome we're trying to survive.
        rt.block_on(async {
            let (send, _recv) = h.conn.open_streaming_substream().await.unwrap();
            let _ = send.write_all(bytes.clone()).await;
            drop(send);
            // Brief settle so the per-stream reader task can drain
            // whatever it can parse and emit any resulting event.
            tokio::time::sleep(Duration::from_millis(20)).await;
        });

        // Phase 2 (LIVENESS): a well-formed call on a fresh pooled
        // stream must still dispatch. If it times out, the reactor
        // is hung and the whole property fails.
        let live_result: Result<Result<(), anyhow::Error>, tokio::time::error::Elapsed> =
            rt.block_on(async {
                timeout(Duration::from_secs(5), async {
                    let handle = h.conn.acquire_stream(None).await?;
                    drive_unary_call(handle.get(), b"live", b"check").await
                })
                .await
            });
        proptest::prop_assert!(
            live_result.is_ok(),
            "reactor deadlocked after adversarial bytes (len={}): {:?}",
            bytes.len(),
            live_result.err()
        );
        proptest::prop_assert!(
            live_result.unwrap().is_ok(),
            "reactor alive but liveness call failed after adversarial bytes"
        );

        // Negative space (a): connection not closed as a side effect.
        let closed_after = rt.block_on(async { h.closed.lock().await.len() });
        proptest::prop_assert_eq!(
            closed_before,
            closed_after,
            "ConnectionClosed fired unexpectedly after adversarial bytes"
        );

        // Negative space (b): the SHARED pool is quiescent -- the
        // liveness call's handle has been dropped so open_count
        // should equal free_count. If the pool leaks on any
        // adversarial shape, this is where it surfaces.
        let shared_open = h.conn.pool().open_count(&None);
        let shared_free = h.conn.pool().free_count(&None);
        proptest::prop_assert_eq!(
            shared_open,
            shared_free,
            "pool leaked a handle during adversarial test"
        );
    }
}
