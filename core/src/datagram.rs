//! Datagram demux — many logical flows over one connection's unreliable QUIC
//! datagrams (design doc §5.4, Stage B). Each datagram is tagged
//! `[flow-id varint][payload]`; inbound datagrams route to the registered
//! flow's **bounded drop-queue** (tail-drop on full — shed load, never
//! bufferbloat, per §3.4). Built for WebTransport session relay but
//! transport-agnostic and reusable.
//!
//! The transport is abstracted behind [`DatagramTransport`] (implemented by
//! `CoreConnection`) so the router is unit-testable without a live connection,
//! mirroring the `ChunkRead`/`ChunkWrite` seam under [`crate::stream_io`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use tokio::sync::mpsc;

use crate::stream_io::BoxFut;

/// Per-flow inbound queue depth — lifted from iroh-relay's
/// `PER_CLIENT_SEND_QUEUE_DEPTH` (design doc Appendix A).
pub const DEFAULT_FLOW_QUEUE_DEPTH: usize = 512;

/// The unreliable-datagram transport a [`DatagramRouter`] runs over.
/// Implemented by `CoreConnection`; mocked in tests.
pub trait DatagramTransport: Send + Sync + 'static {
    /// Send one datagram (already framed). Unreliable — may be dropped.
    fn send(&self, data: Vec<u8>) -> Result<()>;
    /// Await the next inbound datagram. `Err` ends the receive loop (e.g. the
    /// connection closed).
    fn recv(&self) -> BoxFut<Result<Vec<u8>>>;
    /// Max datagram size negotiated for the connection, if any. Used to reject
    /// oversize sends before they're silently dropped by QUIC.
    fn max_size(&self) -> Option<usize>;
}

// ── varint (unsigned LEB128) ────────────────────────────────────────────────

/// Append `v` as an unsigned LEB128 varint.
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Decode an unsigned LEB128 varint, returning `(value, bytes_consumed)`.
pub fn get_varint(buf: &[u8]) -> Result<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
        shift += 7;
    }
    bail!("varint truncated or longer than 10 bytes")
}

// ── router ──────────────────────────────────────────────────────────────────

struct Inner<T: DatagramTransport> {
    transport: T,
    flows: Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>,
    drops: AtomicU64,
    queue_depth: usize,
}

/// Demultiplexes one connection's datagrams across many flows by a leading
/// varint flow-id. Send tags outbound datagrams; a spawned receive loop routes
/// inbound ones to the matching flow's bounded queue.
pub struct DatagramRouter<T: DatagramTransport> {
    inner: Arc<Inner<T>>,
}

impl<T: DatagramTransport> Clone for DatagramRouter<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: DatagramTransport> DatagramRouter<T> {
    pub fn new(transport: T) -> Self {
        Self::with_queue_depth(transport, DEFAULT_FLOW_QUEUE_DEPTH)
    }

    pub fn with_queue_depth(transport: T, queue_depth: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                transport,
                flows: Mutex::new(HashMap::new()),
                drops: AtomicU64::new(0),
                queue_depth,
            }),
        }
    }

    /// Register `flow_id`, returning the receiver for its inbound datagram
    /// payloads. Re-registering replaces the queue.
    pub fn register(&self, flow_id: u64) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel(self.inner.queue_depth);
        self.inner
            .flows
            .lock()
            .expect("flows poisoned")
            .insert(flow_id, tx);
        rx
    }

    pub fn unregister(&self, flow_id: u64) {
        self.inner
            .flows
            .lock()
            .expect("flows poisoned")
            .remove(&flow_id);
    }

    /// Send `payload` on `flow_id`: prepends the varint flow-id and rejects the
    /// datagram if it would exceed the connection's max datagram size (the edge
    /// is expected to clamp the browser-advertised `maxDatagramSize` so this
    /// never trips in steady state — see §3.4).
    pub fn send(&self, flow_id: u64, payload: &[u8]) -> Result<()> {
        let mut framed = Vec::with_capacity(10 + payload.len());
        put_varint(&mut framed, flow_id);
        framed.extend_from_slice(payload);
        if let Some(max) = self.inner.transport.max_size() {
            if framed.len() > max {
                bail!(
                    "datagram {} bytes exceeds connection max {} (clamp maxDatagramSize)",
                    framed.len(),
                    max
                );
            }
        }
        self.inner.transport.send(framed)
    }

    /// Spawn the inbound demux loop. Reads datagrams, parses the flow-id, and
    /// routes the payload to the registered flow's queue (tail-drop on full or
    /// unregistered). Ends when the transport errors (connection closed).
    pub fn spawn_recv_loop(&self) -> tokio::task::JoinHandle<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            loop {
                let datagram = match inner.transport.recv().await {
                    Ok(d) => d,
                    Err(_) => break, // connection closed
                };
                let (flow_id, consumed) = match get_varint(&datagram) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        inner.drops.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                let sender = inner
                    .flows
                    .lock()
                    .expect("flows poisoned")
                    .get(&flow_id)
                    .cloned();
                let routed = match sender {
                    Some(tx) => tx.try_send(datagram[consumed..].to_vec()).is_ok(),
                    None => false,
                };
                if !routed {
                    // Full, closed, or unregistered → tail-drop (no bufferbloat).
                    inner.drops.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    }

    /// Total datagrams dropped (full queue, unknown flow, or malformed).
    pub fn dropped(&self) -> u64 {
        self.inner.drops.load(Ordering::Relaxed)
    }
}

impl DatagramTransport for crate::CoreConnection {
    fn send(&self, data: Vec<u8>) -> Result<()> {
        self.send_datagram(data)
    }

    fn recv(&self) -> BoxFut<Result<Vec<u8>>> {
        let conn = self.clone();
        Box::pin(async move { conn.read_datagram().await })
    }

    fn max_size(&self) -> Option<usize> {
        self.max_datagram_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Shared buffer capturing datagrams the router handed to the transport.
    type Sent = Arc<StdMutex<Vec<Vec<u8>>>>;

    struct MockTransport {
        sent: Sent,
        inbound: Arc<tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>>,
        max: Option<usize>,
    }

    fn mock(max: Option<usize>) -> (MockTransport, Sent, mpsc::Sender<Vec<u8>>) {
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel(64);
        let t = MockTransport {
            sent: sent.clone(),
            inbound: Arc::new(tokio::sync::Mutex::new(rx)),
            max,
        };
        (t, sent, tx)
    }

    impl DatagramTransport for MockTransport {
        fn send(&self, data: Vec<u8>) -> Result<()> {
            self.sent.lock().unwrap().push(data);
            Ok(())
        }
        fn recv(&self) -> BoxFut<Result<Vec<u8>>> {
            let inbound = self.inbound.clone();
            Box::pin(async move {
                inbound
                    .lock()
                    .await
                    .recv()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("transport closed"))
            })
        }
        fn max_size(&self) -> Option<usize> {
            self.max
        }
    }

    #[test]
    fn varint_round_trips() {
        for v in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            let (got, n) = get_varint(&buf).unwrap();
            assert_eq!(got, v);
            assert_eq!(n, buf.len());
        }
        // A trailing payload after the varint is not consumed.
        let mut buf = Vec::new();
        put_varint(&mut buf, 300);
        buf.extend_from_slice(b"payload");
        let (got, n) = get_varint(&buf).unwrap();
        assert_eq!(got, 300);
        assert_eq!(&buf[n..], b"payload");
    }

    #[tokio::test]
    async fn send_prepends_flow_id() {
        let (t, sent, _tx) = mock(Some(1200));
        let router = DatagramRouter::new(t);
        router.send(300, b"hi").unwrap();
        let mut expected = Vec::new();
        put_varint(&mut expected, 300);
        expected.extend_from_slice(b"hi");
        assert_eq!(sent.lock().unwrap()[0], expected);
    }

    #[tokio::test]
    async fn send_clamp_rejects_oversize() {
        let (t, sent, _tx) = mock(Some(4));
        let router = DatagramRouter::new(t);
        assert!(router.send(1, b"way too long").is_err());
        assert!(sent.lock().unwrap().is_empty(), "oversize must not be sent");
    }

    #[tokio::test]
    async fn recv_routes_and_drops_unknown() {
        let (t, _sent, tx) = mock(None);
        let router = DatagramRouter::new(t);
        let mut flow = router.register(7);
        let _loop = router.spawn_recv_loop();

        // Unregistered flow 9 first (dropped), then registered flow 7.
        let mut dg9 = Vec::new();
        put_varint(&mut dg9, 9);
        dg9.extend_from_slice(b"gone");
        tx.send(dg9).await.unwrap();

        let mut dg7 = Vec::new();
        put_varint(&mut dg7, 7);
        dg7.extend_from_slice(b"hello");
        tx.send(dg7).await.unwrap();

        let got = flow.recv().await.unwrap();
        assert_eq!(got, b"hello");
        // The ordered loop processed dg9 before dg7, so the drop is recorded.
        assert!(
            router.dropped() >= 1,
            "unknown-flow datagram should be dropped"
        );
    }
}
