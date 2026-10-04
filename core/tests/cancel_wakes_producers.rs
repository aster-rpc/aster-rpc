//! §12.4: a real `FLAG_CANCEL` must wake a producer blocked on the bounded
//! response channel — over QUIC, with the connection left open.
//!
//! This is deliberately not the "abandoned reader" case. Closing the connection
//! makes the channel `Closed`, which every producer already notices. The gap
//! this covers is narrower and was real:
//!
//!   * a server-streaming call ends its request stream with the first frame, so
//!     the reactor's wire-read branch was gated off and a later `FLAG_CANCEL`
//!     was never read at all; and
//!   * even where a cancel *was* observed, it only set an atomic. A producer
//!     blocked awaiting capacity — or an FFI caller spinning on `QUEUE_FULL` —
//!     is waiting on the channel, and an atomic nobody polls never wakes it.
//!
//! So the test keeps the connection open, stops consuming responses until the
//! channel is certainly full, sends `FLAG_CANCEL`, and requires the producer to
//! stop promptly.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use aster_transport_core::framing::{
    encode_frame, FLAG_CANCEL, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER, RESET_CODE_CANCELLED,
    RESET_CODE_PROTOCOL_VIOLATION,
};
use aster_transport_core::reactor::{create_reactor, OutgoingFrame, ReactorEvent};
use aster_transport_core::{CoreConnection, CoreNetClient};

const TEST_ALPN: &[u8] = b"aster/cancel-test/1";

async fn setup() -> Result<(CoreConnection, CoreNetClient, CoreNetClient, ReactorPair)> {
    let server = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let client = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let server_id = server.endpoint_id();

    let (handle, feeder) = create_reactor(&tokio::runtime::Handle::current(), 256);
    let srv = server.clone();
    let accept = tokio::spawn(async move {
        while let Ok(conn) = srv.accept().await {
            feeder.feed(conn);
        }
    });

    let conn = client.connect(server_id, TEST_ALPN.to_vec()).await?;
    Ok((conn, server, client, ReactorPair { handle, accept }))
}

struct ReactorPair {
    handle: aster_transport_core::reactor::ReactorHandle,
    accept: tokio::task::JoinHandle<()>,
}

/// A producer that streams until the channel refuses it, then reports how it
/// ended. This mirrors what every binding does: keep sending until the sink
/// says stop.
/// The producer must wake promptly. Before the writer task existed, flow
/// control blocked the dispatch loop inside `write_all`, so it was never in the
/// `select!` to poll the cancel at all.
#[tokio::test]
async fn flag_cancel_wakes_a_producer_blocked_on_a_full_response_channel() -> Result<()> {
    let (conn, _server, _client, pair) = setup().await?;

    let sent = Arc::new(AtomicUsize::new(0));
    let ended = Arc::new(AtomicUsize::new(0));
    let sent_c = sent.clone();
    let ended_c = ended.clone();

    let ReactorPair { handle, accept } = pair;
    let producer = tokio::spawn(async move {
        let mut reactor = handle;
        if let Some(ReactorEvent::Call(call)) = reactor.next_event().await {
            // Stream without limit. The peer will stop reading, so the bounded
            // channel fills and this `send` blocks — which is the state the
            // cancellation has to break.
            loop {
                let frame = encode_frame(&[7u8; 64], 0).expect("frame");
                if call
                    .response_sender
                    .send(OutgoingFrame::Frame(frame))
                    .await
                    .is_err()
                {
                    // Closed channel: the cancellation woke us.
                    ended_c.store(1, Ordering::SeqCst);
                    return;
                }
                sent_c.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    // Open one call and stop reading its responses.
    let (send, _recv) = conn.open_streaming_substream().await?;
    send.write_all(encode_frame(b"hdr", FLAG_HEADER)?).await?;
    // FLAG_END_STREAM on the first request frame: this is the server-streaming
    // shape, and precisely the one whose wire reads used to be gated off.
    send.write_all(encode_frame(b"req", FLAG_END_STREAM)?)
        .await?;

    // Let the producer fill the channel. Once it stops advancing, it is blocked
    // on capacity rather than merely slow.
    let mut last = 0;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let now = sent.load(Ordering::SeqCst);
        if now > 0 && now == last {
            break;
        }
        last = now;
    }
    assert!(
        sent.load(Ordering::SeqCst) > 0,
        "the producer never got going"
    );
    assert_eq!(
        ended.load(Ordering::SeqCst),
        0,
        "the producer ended before it was cancelled"
    );

    // Cancel, leaving the connection OPEN. Nothing about this closes the QUIC
    // stream, so only the reactor observing FLAG_CANCEL can end the producer.
    let started = Instant::now();
    send.write_all(encode_frame(&[], FLAG_CANCEL)?).await?;

    // Prompt termination.
    let deadline = Duration::from_secs(10);
    while ended.load(Ordering::SeqCst) == 0 && started.elapsed() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ended.load(Ordering::SeqCst),
        1,
        "producer still blocked {:?} after FLAG_CANCEL; the atomic flag alone \
         does not wake a sender awaiting capacity",
        started.elapsed()
    );

    producer.abort();
    accept.abort();
    Ok(())
}

/// §6 v1 cancellation contract: `FLAG_CANCEL` **poisons the physical stream**,
/// but the connection and any connection-scoped session survive. The client
/// discards that stream and acquires another on the same connection.
///
/// Same-stream reuse is deliberately not promised: a response frame may have
/// been partially written when flow control blocked, so the byte stream is no
/// longer at a frame boundary and anything written after it would silently
/// extend a half-frame. The reactor resets the send direction to make that
/// explicit rather than leaving the peer to discover it.
#[tokio::test]
async fn cancel_poisons_the_stream_but_the_connection_survives() -> Result<()> {
    let (conn, _server, _client, pair) = setup().await?;

    let ReactorPair { handle, accept } = pair;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_c = calls.clone();
    let server_task = tokio::spawn(async move {
        let mut reactor = handle;
        while let Some(event) = reactor.next_event().await {
            if let ReactorEvent::Call(call) = event {
                calls_c.fetch_add(1, Ordering::SeqCst);
                // Stream indefinitely so the first call is genuinely in flight
                // when the cancel arrives -- a handler that answered
                // immediately would be testing a late cancel after completion.
                loop {
                    let frame = encode_frame(&[3u8; 64], 0).expect("frame");
                    if call
                        .response_sender
                        .send(OutgoingFrame::Frame(frame))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    // Stream 1: start a call, do NOT read the responses, then cancel it.
    let (send1, recv1) = conn.open_streaming_substream().await?;
    send1.write_all(encode_frame(b"hdr", FLAG_HEADER)?).await?;
    send1
        .write_all(encode_frame(b"req", FLAG_END_STREAM)?)
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send1.write_all(encode_frame(&[], FLAG_CANCEL)?).await?;

    // The poisoned stream must NOT be reused. Writing to it after the reset is
    // expected to fail; that failure is the contract, not a defect.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let reuse = send1.write_all(encode_frame(b"hdr2", FLAG_HEADER)?).await;
    assert!(
        reuse.is_err(),
        "a cancelled stream must be poisoned, not silently reusable"
    );

    // Prove the SERVER'S send direction was reset, and with the defined code.
    // The write failure above only shows the client's own send side stopped,
    // which a plain connection fault would also produce. Reading the response
    // direction is what distinguishes "the server deliberately retired this
    // stream" from "something broke": the peer sees RESET_CODE_CANCELLED.
    let mut buf = [0u8; 4];
    let read_err = recv1
        .read_exact_into(&mut buf)
        .await
        .expect_err("the server's send direction must be reset, not left open");
    let rendered = format!("{read_err:#}");
    let code = RESET_CODE_CANCELLED;
    assert!(
        rendered.contains(&code.to_string()) || rendered.contains(&format!("{code:#x}")),
        "expected reset code {code} ({code:#x}) from the peer, got: {rendered}"
    );

    // Stream 2 on the SAME connection: this is what a client does after a
    // cancellation, and it must work.
    let (send2, _recv2) = conn.open_streaming_substream().await?;
    send2.write_all(encode_frame(b"hdr2", FLAG_HEADER)?).await?;
    send2
        .write_all(encode_frame(b"req2", FLAG_END_STREAM)?)
        .await?;

    let started = Instant::now();
    while calls.load(Ordering::SeqCst) < 2 && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the connection must survive a cancelled stream: a fresh stream on it \
         never dispatched"
    );

    server_task.abort();
    accept.abort();
    Ok(())
}

/// Regression: call N+1 sent **immediately** after N's trailer must be served.
///
/// The reader starts on N+1 as soon as N's request stream ends, so N+1's header
/// and N's writer completion can both be ready at once. With a `biased` select
/// putting the wire first, whichever won used to decide whether a perfectly
/// legal reused stream was accepted or rejected as "mid-call" -- a race, so it
/// failed intermittently rather than never.
///
/// Repeated deliberately: one round would pass by luck.
#[tokio::test]
async fn pipelined_calls_on_a_reused_stream_are_all_served() -> Result<()> {
    const ROUNDS: usize = 40;

    let (conn, _server, _client, pair) = setup().await?;
    let ReactorPair { handle, accept } = pair;

    let served = Arc::new(AtomicUsize::new(0));
    let served_c = served.clone();
    let server_task = tokio::spawn(async move {
        let mut reactor = handle;
        while let Some(event) = reactor.next_event().await {
            if let ReactorEvent::Call(call) = event {
                served_c.fetch_add(1, Ordering::SeqCst);
                let trailer = encode_frame(&[], FLAG_TRAILER).expect("trailer");
                let _ = call
                    .response_sender
                    .send(OutgoingFrame::Trailer(trailer))
                    .await;
            }
        }
    });

    let (send, recv) = conn.open_streaming_substream().await?;
    for _ in 0..ROUNDS {
        send.write_all(encode_frame(b"hdr", FLAG_HEADER)?).await?;
        send.write_all(encode_frame(b"req", FLAG_END_STREAM)?)
            .await?;
        // Wait for THIS call's trailer, then immediately send the next header.
        // That is the interleaving the race needs: the reader picks up N+1
        // while N's writer is completing.
        loop {
            let mut len_bytes = [0u8; 4];
            recv.read_exact_into(&mut len_bytes).await?;
            let body_len = u32::from_le_bytes(len_bytes) as usize;
            let mut flags_buf = [0u8; 1];
            recv.read_exact_into(&mut flags_buf).await?;
            let payload_len = body_len - 1;
            if payload_len > 0 {
                let mut payload = vec![0u8; payload_len];
                recv.read_exact_into(&mut payload).await?;
            }
            if flags_buf[0] & FLAG_TRAILER != 0 {
                break;
            }
        }
    }

    let started = Instant::now();
    while served.load(Ordering::SeqCst) < ROUNDS && started.elapsed() < Duration::from_secs(20) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        served.load(Ordering::SeqCst),
        ROUNDS,
        "a reused multiplexed stream dropped a call: the next header raced the \
         previous call's completion"
    );

    server_task.abort();
    accept.abort();
    Ok(())
}

/// The attack the pipelining handoff must not admit.
///
/// Calls are strictly sequential: N+1 may begin only after N's *response*
/// finishes, not merely its request. An earlier fix accepted any header once
/// the request stream ended, stashed it, and stopped reading the wire -- so a
/// peer could send
///
///   N END_STREAM -> premature N+1 header -> CANCEL
///
/// and the cancel would never be read, leaving the blocked producer to stream
/// forever.
///
/// Deterministic, unlike the forty-round reuse test: the producer here never
/// finishes, so the writer is *guaranteed* unfinished when the header arrives.
/// That pins which branch ran, rather than hoping a race lands.
#[tokio::test]
async fn an_early_header_cannot_mask_a_following_cancel() -> Result<()> {
    let (conn, _server, _client, pair) = setup().await?;
    let ReactorPair { handle, accept } = pair;

    let ended = Arc::new(AtomicUsize::new(0));
    let ended_c = ended.clone();
    let producer = tokio::spawn(async move {
        let mut reactor = handle;
        if let Some(ReactorEvent::Call(call)) = reactor.next_event().await {
            // Never completes: the writer is unfinished for the whole test.
            loop {
                let frame = encode_frame(&[5u8; 64], 0).expect("frame");
                if call
                    .response_sender
                    .send(OutgoingFrame::Frame(frame))
                    .await
                    .is_err()
                {
                    ended_c.store(1, Ordering::SeqCst);
                    return;
                }
            }
        }
    });

    let (send, _recv) = conn.open_streaming_substream().await?;
    send.write_all(encode_frame(b"hdr", FLAG_HEADER)?).await?;
    send.write_all(encode_frame(b"req", FLAG_END_STREAM)?)
        .await?;

    // Let the producer fill the bounded channel and block.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        ended.load(Ordering::SeqCst),
        0,
        "the producer ended before the test began"
    );

    // The premature header, then the cancel.
    send.write_all(encode_frame(b"hdr2", FLAG_HEADER)?).await?;
    send.write_all(encode_frame(&[], FLAG_CANCEL)?).await?;

    // Either the reactor rejected the early header (killing the stream) or it
    // read the cancel -- both wake the producer. What must NOT happen is the
    // header being stashed and the cancel going unread.
    let started = Instant::now();
    while ended.load(Ordering::SeqCst) == 0 && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ended.load(Ordering::SeqCst),
        1,
        "producer still blocked {:?} after an early header followed by CANCEL: \
         the header disabled wire reads and masked the cancel",
        started.elapsed()
    );

    producer.abort();
    accept.abort();
    Ok(())
}

/// A framing violation must be distinguishable from an honoured cancellation,
/// and must not take the connection with it.
///
/// Both are stream resets, so the CODE is the only thing telling them apart. If
/// a violation reused `RESET_CODE_CANCELLED`, a client that sent a bad header
/// would log a clean cancellation for a bug it needs to fix.
#[tokio::test]
async fn a_framing_violation_resets_with_its_own_code_and_spares_the_connection() -> Result<()> {
    let (conn, _server, _client, pair) = setup().await?;
    let ReactorPair { handle, accept } = pair;

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_c = calls.clone();
    let server_task = tokio::spawn(async move {
        let mut reactor = handle;
        while let Some(event) = reactor.next_event().await {
            if let ReactorEvent::Call(call) = event {
                calls_c.fetch_add(1, Ordering::SeqCst);
                // Never finishes, so the writer is unfinished when the early
                // header arrives -- the violation branch, deterministically.
                loop {
                    let frame = encode_frame(&[8u8; 64], 0).expect("frame");
                    if call
                        .response_sender
                        .send(OutgoingFrame::Frame(frame))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let (send, recv) = conn.open_streaming_substream().await?;
    send.write_all(encode_frame(b"hdr", FLAG_HEADER)?).await?;
    send.write_all(encode_frame(b"req", FLAG_END_STREAM)?)
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Call N+1 before N's response finished: forbidden.
    send.write_all(encode_frame(b"hdr2", FLAG_HEADER)?).await?;

    // Drain first: the producer streamed frames before the violation, so the
    // client has buffered bytes to read through before the reset surfaces.
    let mut buf = [0u8; 4096];
    let rendered = loop {
        match recv.read_exact_into(&mut buf).await {
            Ok(()) => continue,
            Err(e) => break format!("{e:#}"),
        }
    };
    let violation = RESET_CODE_PROTOCOL_VIOLATION;
    let cancelled = RESET_CODE_CANCELLED;
    assert!(
        rendered.contains(&violation.to_string()),
        "expected the protocol-violation code {violation}, got: {rendered}"
    );
    assert!(
        !rendered.contains(&cancelled.to_string()),
        "a violation must NOT report the cancellation code -- a client would \
         read its own cancel as honoured: {rendered}"
    );

    // The CONNECTION survives: only the offending stream is poisoned.
    let (send2, _recv2) = conn.open_streaming_substream().await?;
    send2.write_all(encode_frame(b"hdr3", FLAG_HEADER)?).await?;
    send2
        .write_all(encode_frame(b"req3", FLAG_END_STREAM)?)
        .await?;

    let started = Instant::now();
    while calls.load(Ordering::SeqCst) < 2 && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a framing violation must poison only its stream, not the connection"
    );

    server_task.abort();
    accept.abort();
    Ok(())
}

/// A client that resets its send side has abandoned the call: that is a
/// cancel, even when no `FLAG_CANCEL` frame arrives.
///
/// Clients cancel by writing `FLAG_CANCEL` and then resetting, and a reset lets
/// QUIC discard data the server has not read yet -- the cancel frame included.
/// The reader used to treat the reset like a clean EOF, so a handler that was
/// not producing (here: a unary call that never answers) was never told.
#[tokio::test]
async fn a_request_side_reset_cancels_the_call_in_flight() -> Result<()> {
    let (conn, _server, _client, pair) = setup().await?;
    let ReactorPair { handle, accept } = pair;

    let observed = Arc::new(AtomicUsize::new(0));
    let observed_c = observed.clone();
    let server_task = tokio::spawn(async move {
        let mut reactor = handle;
        if let Some(ReactorEvent::Call(call)) = reactor.next_event().await {
            // Never answers; waits for the cancel.
            let notified = call.cancel_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !call.cancelled.load(Ordering::Acquire) {
                notified.await;
            }
            observed_c.store(1, Ordering::SeqCst);
            // Keep the call alive until the test is done with it.
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    let (send, _recv) = conn.open_streaming_substream().await?;
    let mut pair = encode_frame(b"hdr", FLAG_HEADER)?;
    pair.extend(encode_frame(b"req", FLAG_END_STREAM)?);
    send.write_all(pair).await?;
    // Let the call reach the handler, then abandon it with a bare reset.
    tokio::time::sleep(Duration::from_millis(200)).await;
    send.reset(RESET_CODE_CANCELLED).await?;

    let started = Instant::now();
    while observed.load(Ordering::SeqCst) == 0 && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        observed.load(Ordering::SeqCst),
        1,
        "the handler was never told its caller reset the stream"
    );

    server_task.abort();
    accept.abort();
    Ok(())
}
