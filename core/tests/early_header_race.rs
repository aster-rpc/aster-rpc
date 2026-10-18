//! A reused stream's next call must be served even when it arrives before the
//! previous call's writer task has completed.
//!
//! The pooled unary path (every binding's fast path, and `RpcConnection::unary`)
//! reads call N's trailer, puts the stream back in the pool and the next call
//! writes N+1's header at once. The reactor's response writer hands N's trailer
//! to QUIC inside `write_all`; the connection driver can transmit it before the
//! writer task's poll returns. So N+1's header can reach the dispatch loop while
//! `writer.is_finished()` is still false, and the dispatch loop used to reset
//! the stream with `RESET_CODE_PROTOCOL_VIOLATION` ("StreamHeader before
//! previous call's response completed"). Under load that was about one pooled
//! call in two million (perf-baseline-r3 §5.3).
//!
//! `test_hooks::set_writer_exit_pause` makes the descheduled writer
//! deterministic: it blocks the writer's thread after the terminal write.
//! Needs feature `test-hooks`:
//!
//! ```text
//! cargo test -p aster_transport_core --features test-hooks --test early_header_race
//! ```
#![cfg(feature = "test-hooks")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use aster_transport_core::framing::{encode_frame, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER};
use aster_transport_core::reactor::{create_reactor, test_hooks, OutgoingFrame, ReactorEvent};
use aster_transport_core::{CoreNetClient, CoreRecvStream};

const TEST_ALPN: &[u8] = b"aster/early-header-test/1";

async fn read_to_trailer(recv: &CoreRecvStream) -> Result<()> {
    loop {
        let mut head = [0u8; 5];
        recv.read_exact_into(&mut head).await?;
        let body_len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if body_len > 1 {
            let mut payload = vec![0u8; body_len - 1];
            recv.read_exact_into(&mut payload).await?;
        }
        if head[4] & FLAG_TRAILER != 0 {
            return Ok(());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_call_may_arrive_before_the_writer_task_completes() -> Result<()> {
    const ROUNDS: usize = 20;

    let server = CoreNetClient::create_loopback(TEST_ALPN.to_vec()).await?;
    let client = CoreNetClient::create_loopback(TEST_ALPN.to_vec()).await?;
    let (handle, feeder) = create_reactor(&tokio::runtime::Handle::current(), 256);
    let srv = server.clone();
    let accept = tokio::spawn(async move {
        while let Ok(conn) = srv.accept().await {
            feeder.feed(conn);
        }
    });

    let served = Arc::new(AtomicUsize::new(0));
    let served_c = served.clone();
    let server_task = tokio::spawn(async move {
        let mut reactor = handle;
        while let Some(event) = reactor.next_event().await {
            if let ReactorEvent::Call(call) = event {
                served_c.fetch_add(1, Ordering::SeqCst);
                let mut out = encode_frame(b"resp", 0).expect("frame");
                out.extend(encode_frame(&[], FLAG_TRAILER).expect("trailer"));
                let _ = call
                    .response_sender
                    .send(OutgoingFrame::CompleteUnary(out))
                    .await;
            }
        }
    });

    let conn = client
        .connect_node_addr(server.loopback_addr()?, TEST_ALPN.to_vec())
        .await?;
    let (send, recv) = conn.open_streaming_substream().await?;

    // Every writer now lingers after its trailer, long enough for the client to
    // read it and send the next header -- but well inside the grace period.
    test_hooks::set_writer_exit_pause(Duration::from_millis(20));

    for round in 0..ROUNDS {
        let mut pair = encode_frame(b"hdr", FLAG_HEADER)?;
        pair.extend(encode_frame(b"req", FLAG_END_STREAM)?);
        send.write_all(pair).await?;
        tokio::time::timeout(Duration::from_secs(10), read_to_trailer(&recv))
            .await
            .map_err(|_| anyhow::anyhow!("round {round}: no trailer"))?
            .map_err(|e| {
                anyhow::anyhow!(
                    "round {round}: the reused stream was reset -- the next header \
                     raced the previous call's writer task: {e:#}"
                )
            })?;
    }
    test_hooks::set_writer_exit_pause(Duration::ZERO);

    assert_eq!(served.load(Ordering::SeqCst), ROUNDS);
    server_task.abort();
    accept.abort();
    Ok(())
}
