//! Ingress allocation properties, in their own test binary.
//!
//! These assert on what the reader DID -- resolver invocations and bytes
//! materialized -- rather than on whether a call was delivered. Non-delivery is
//! the weaker signal: an implementation that ignores the cap, reads 16 MiB and
//! then declines to dispatch also "delivers no call".
//!
//! They live apart from `multiplexed_contract` because the counters are
//! process-global: sharing a binary with tests that drive unrelated traffic
//! pollutes them, which is exactly how the first version of these tests failed
//! in a full run while passing in isolation.
#![cfg(feature = "ingress-metrics")]

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;

use aster_transport_core::framing::{encode_frame, FLAG_END_STREAM, FLAG_HEADER, FLAG_TRAILER};
use aster_transport_core::reactor::{create_reactor, IncomingCall, OutgoingFrame, ReactorEvent};
use aster_transport_core::{CoreConnection, CoreNetClient};

#[allow(dead_code)]
type AckBinding = (
    Arc<Mutex<Vec<IncomingCall>>>,
    Arc<Mutex<Vec<u64>>>,
    tokio::task::JoinHandle<()>,
);

const TEST_ALPN: &[u8] = b"aster-test/ingress";
#[allow(dead_code)]
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

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
                    let _ = call
                        .response_sender
                        .send(OutgoingFrame::Trailer(trailer))
                        .await;
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

/// Start a reactor whose body-frame cap is resolved per call from the raw
/// header bytes. Header bytes here are `service|method`; `!` means undecodable.
async fn start_capped_reactor(
    server: CoreNetClient,
) -> (
    aster_transport_core::reactor::ReactorHandle,
    tokio::task::JoinHandle<()>,
) {
    use aster_transport_core::reactor::FrameLimit;
    let (handle, feeder) = create_reactor(&tokio::runtime::Handle::current(), 256);
    let feeder = feeder.with_frame_limits(std::sync::Arc::new(|h: &[u8]| {
        let s = match std::str::from_utf8(h) {
            Ok(s) => s,
            Err(_) => return FrameLimit::Reject,
        };
        if s.starts_with('!') {
            return FrameLimit::Reject;
        }
        match s {
            "small|m" => FrameLimit::Limit(4 * 1024),
            "large|m" => FrameLimit::Limit(1024 * 1024),
            _ => FrameLimit::Default,
        }
    }));
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

async fn setup_capped_pair() -> Result<(
    CoreConnection,
    aster_transport_core::reactor::ReactorHandle,
    tokio::task::JoinHandle<()>,
    CoreNetClient,
    CoreNetClient,
)> {
    let server = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let client = CoreNetClient::create(TEST_ALPN.to_vec()).await?;
    let server_id = server.endpoint_id();
    let (reactor, accept_task) = start_capped_reactor(server.clone()).await;
    let conn = client.connect(server_id, TEST_ALPN.to_vec()).await?;
    Ok((conn, reactor, accept_task, server, client))
}

/// These three assert on what the reader DID -- resolver calls and bytes
/// materialized -- not on whether a call arrived. Non-delivery is the weaker
/// signal: an implementation that ignores the cap, reads 16 MiB and then
/// declines to dispatch also "delivers no call".
#[cfg(feature = "ingress-metrics")]
mod ingress_properties {
    use super::*;
    use aster_transport_core::reactor::ingress_metrics;

    /// Serialize: the counters are process-global.
    static METRICS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn oversized_call_header_is_never_materialized() -> Result<()> {
        use aster_transport_core::framing::MAX_CALL_HEADER_SIZE;
        let _g = METRICS.lock().await;
        let (conn, reactor, _accept, _server, _client) = setup_capped_pair().await?;
        let (calls, _closed, _binding) = spawn_ack_binding(reactor);
        ingress_metrics::reset();

        let handle = conn.acquire_stream(None).await.unwrap();
        let (send, _recv) = handle.get();
        let declared = MAX_CALL_HEADER_SIZE + 1;
        let mut framed = Vec::new();
        framed.extend_from_slice(&declared.to_le_bytes());
        framed.push(FLAG_HEADER);
        send.write_all(framed).await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        let (resolves, header_bytes, body_bytes) = ingress_metrics::snapshot();
        // THE assertion. The counters below are downstream of a successful
        // read, so they would report zero even if `accept_aster_bi`'s peek --
        // which is where a stream's FIRST frame is read, before the reactor
        // exists -- had allocated 16 MiB and then blocked on a payload the peer
        // never sends. Only the allocation counter can see that.
        assert_eq!(
            ingress_metrics::allocated(),
            0,
            "an over-cap first frame must allocate nothing, in the peek or anywhere"
        );
        assert_eq!(
            header_bytes, 0,
            "an over-cap header must not be materialized"
        );
        assert_eq!(
            resolves, 0,
            "an over-cap header must never reach the resolver"
        );
        assert_eq!(body_bytes, 0);
        assert_eq!(calls.lock().await.len(), 0);
        Ok(())
    }

    /// The old bug could resolve, read the 512 KiB body, and only then reject.
    /// This fails if any body byte is read or the resolver is invoked.
    #[tokio::test]
    async fn non_header_first_frame_reaches_neither_resolver_nor_body() -> Result<()> {
        let _g = METRICS.lock().await;
        let (conn, reactor, _accept, _server, _client) = setup_capped_pair().await?;
        let (calls, _closed, _binding) = spawn_ack_binding(reactor);
        ingress_metrics::reset();

        let handle = conn.acquire_stream(None).await.unwrap();
        let (send, _recv) = handle.get();
        // Bytes that WOULD resolve to the 1 MiB cap -- but no FLAG_HEADER.
        send.write_all(encode_frame(b"large|m", 0).unwrap())
            .await
            .unwrap();
        send.write_all(encode_frame(&vec![0u8; 512 * 1024], FLAG_END_STREAM).unwrap())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(250)).await;
        let (resolves, _hdr, body_bytes) = ingress_metrics::snapshot();
        assert_eq!(resolves, 0, "a non-header frame must not be resolved");
        assert_eq!(
            body_bytes, 0,
            "no body may be read behind a non-header frame"
        );
        // The 7-byte non-header frame is legitimately allocated (twice: peek
        // then reader). The 512 KiB body behind it must not be.
        assert!(
            ingress_metrics::allocated() < 1024,
            "only the small non-header frame may be allocated, got {}",
            ingress_metrics::allocated()
        );
        assert_eq!(calls.lock().await.len(), 0);
        Ok(())
    }

    /// An over-cap BODY is refused without being materialized, while the call
    /// header that preceded it was read normally.
    #[tokio::test]
    async fn over_cap_body_is_refused_without_materializing() -> Result<()> {
        let _g = METRICS.lock().await;
        let (conn, reactor, _accept, _server, _client) = setup_capped_pair().await?;
        let (_calls, _closed, _binding) = spawn_ack_binding(reactor);
        ingress_metrics::reset();

        let handle = conn.acquire_stream(None).await.unwrap();
        let (send, _recv) = handle.get();
        // small|m caps bodies at 4 KiB; declare 64 KiB and send only the prefix.
        send.write_all(encode_frame(b"small|m", FLAG_HEADER).unwrap())
            .await
            .unwrap();
        let declared: u32 = 64 * 1024 + 1;
        let mut framed = Vec::new();
        framed.extend_from_slice(&declared.to_le_bytes());
        framed.push(FLAG_END_STREAM);
        send.write_all(framed).await.unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        let (resolves, header_bytes, body_bytes) = ingress_metrics::snapshot();
        assert_eq!(resolves, 1, "the header itself is legal and is resolved");
        assert_eq!(header_bytes, b"small|m".len() as u64);
        assert_eq!(body_bytes, 0, "the over-cap body must not be materialized");
        // The header is allocated TWICE -- once by `accept_aster_bi`'s peek and
        // again by the reader, because the peek re-prepends the frame onto the
        // stream. That is correct; what matters is that the 64 KiB body was
        // never allocated at all.
        let allocated = ingress_metrics::allocated();
        assert_eq!(
            allocated,
            2 * b"small|m".len() as u64,
            "expected the header twice (peek + reader) and no body"
        );
        assert!(
            allocated < 4 * 1024,
            "the over-cap body must never be allocated"
        );
        Ok(())
    }
}
