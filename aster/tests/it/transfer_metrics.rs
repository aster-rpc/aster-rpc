#![cfg(feature = "docs")]
//! Transfer metrics: blob transfers counted plain and zstd, node-wide and per
//! peer, and QUIC bytes per peer by protocol in `PeerView` (Portal's request
//! M1/M2, `portal-sync/docs/external_requests/2026-10-16-aster-transfer-metrics.md`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aster::topology::PeerView;
use aster::{
    AsterConfig, BlobFormat, BlobTransportCompression, FetchStrategy, Node, NodeId, RelayMode,
    ShareMode,
};
use tokio::time::timeout;

const MIB: u64 = 1024 * 1024;
const SIZE: u64 = 10 * MIB;
const WAIT: Duration = Duration::from_secs(60);

/// `None` keeps the default (zstd on, level 3); `Some(0)` turns it off.
async fn start(zstd_level: Option<u8>, alpns: Vec<Vec<u8>>) -> Node {
    let mut builder = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .monitoring(true);
    if let Some(level) = zstd_level {
        builder = builder.blob_zstd_level(level);
    }
    let node = Node::start_with_alpns(builder.build(), alpns)
        .await
        .expect("start node");
    for _ in 0..50 {
        if !node.addr().direct_addresses.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node
}

fn connect(a: &Node, b: &Node) {
    a.add_peer(b).unwrap();
    b.add_peer(a).unwrap();
}

/// 10 MiB of numbered text lines: compresses well.
fn compressible() -> Vec<u8> {
    let mut out = Vec::with_capacity(SIZE as usize);
    let mut i = 0u64;
    while (out.len() as u64) < SIZE {
        out.extend_from_slice(format!("line {i}: the quick brown fox jumps over\n").as_bytes());
        i += 1;
    }
    out.truncate(SIZE as usize);
    out
}

/// 10 MiB of xorshift output: does not compress.
fn incompressible() -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut out = Vec::with_capacity(SIZE as usize);
    while (out.len() as u64) < SIZE {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Poll `node`'s view of `peer` until `done` holds.
async fn view_until(
    node: &Node,
    peer: &NodeId,
    what: &str,
    done: impl Fn(&PeerView) -> bool,
) -> PeerView {
    let mut last = None;
    let found = timeout(WAIT, async {
        loop {
            if let Some(view) = node.topology().peer(peer) {
                if done(&view) {
                    return view;
                }
                last = Some(view);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    found.unwrap_or_else(|_| panic!("timed out waiting for {what}: {last:#?}"))
}

async fn fetch(consumer: &Node, provider: &Node, hash: &aster::Hash) -> aster::FetchReport {
    timeout(
        WAIT,
        consumer.blobs().download_hash_to_store_from(
            hash,
            &[provider.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await
    .expect("fetch in time")
    .expect("fetch")
}

/// Portal's acceptance: A fetches a compressible blob from B over zstd, then
/// an incompressible one from C, which does not serve zstd.
#[tokio::test(flavor = "multi_thread")]
async fn blob_bytes_by_peer_and_encoding() {
    let a = start(None, vec![]).await;
    let b = start(None, vec![]).await;
    let c = start(Some(0), vec![]).await;
    connect(&a, &b);
    connect(&a, &c);
    let text = compressible();
    let noise = incompressible();
    let text_hash = b.blobs().add_bytes(text.clone()).await.unwrap();
    let noise_hash = c.blobs().add_bytes(noise.clone()).await.unwrap();
    assert_eq!(
        a.blobs().transport_compression(),
        BlobTransportCompression::PreferZstd
    );

    let from_b_report = fetch(&a, &b, &text_hash).await;
    let from_c_report = fetch(&a, &c, &noise_hash).await;
    assert_eq!(a.blobs().read_to_bytes(&text_hash).await.unwrap(), text);
    assert_eq!(a.blobs().read_to_bytes(&noise_hash).await.unwrap(), noise);

    // A's view of B: blob payload over zstd, fewer QUIC bytes than payload.
    let from_b = view_until(&a, &b.id(), "A's view of B", |v| {
        v.blob_transfer.fetched_payload_bytes == SIZE
    })
    .await;
    assert_eq!(from_b.blob_transfer.fetched_zstd_payload_bytes, SIZE);
    assert_eq!(from_b.blob_transfer.plain_fallbacks, 0);
    let zstd_rx = from_b.quic_traffic.blobs_zstd.quic_bytes_received;
    assert!(
        zstd_rx > 0 && zstd_rx < SIZE / 2,
        "compressed blob traffic {zstd_rx} B for {SIZE} B payload"
    );
    assert_eq!(from_b.quic_traffic.blobs.quic_bytes_received, 0);
    // Compressed bytes fetched from B, attributed to B.
    let fetched_wire = from_b.blob_transfer.fetched_wire_bytes;
    assert!(
        fetched_wire > 0 && fetched_wire < SIZE / 2,
        "fetched {fetched_wire} wire bytes for {SIZE} B payload"
    );
    assert!(from_b.blob_transfer.fetched_plain_bytes > SIZE);
    assert!(fetched_wire <= zstd_rx);

    // Each fetch's report names its provider with its payload, and the zstd
    // one its compressed bytes (the same figure as the per-peer one).
    let [b_outcome] = from_b_report.providers() else {
        panic!("one provider expected: {from_b_report:?}");
    };
    assert_eq!(b_outcome.node_id(), &b.id());
    assert_eq!(b_outcome.bytes_transferred(), SIZE);
    assert_eq!(b_outcome.wire_bytes(), Some(fetched_wire));
    assert_eq!(from_b_report.bytes_transferred(), SIZE);
    let [c_outcome] = from_c_report.providers() else {
        panic!("one provider expected: {from_c_report:?}");
    };
    assert_eq!(c_outcome.node_id(), &c.id());
    assert_eq!(c_outcome.bytes_transferred(), SIZE);
    assert_eq!(c_outcome.wire_bytes(), None);

    // A's view of C: plain payload, no zstd, QUIC bytes at least the payload.
    let from_c = view_until(&a, &c.id(), "A's view of C", |v| {
        v.blob_transfer.fetched_payload_bytes == SIZE
    })
    .await;
    assert_eq!(from_c.blob_transfer.fetched_zstd_payload_bytes, 0);
    assert_eq!(from_c.blob_transfer.plain_fallbacks, 1);
    assert_eq!(from_c.blob_transfer.fetched_wire_bytes, 0);
    assert_eq!(from_c.blob_transfer.fetched_plain_bytes, 0);
    assert!(from_c.quic_traffic.blobs.quic_bytes_received >= SIZE);
    assert_eq!(from_c.quic_traffic.blobs_zstd, Default::default());

    // B's view of A: the matching bytes sent, exact wire bytes per peer.
    let to_a = view_until(&b, &a.id(), "B's view of A", |v| {
        v.blob_transfer.served_payload_bytes == SIZE
    })
    .await;
    assert_eq!(to_a.blob_transfer.served_zstd_payload_bytes, SIZE);
    let a_stats = a.blobs().transfer_stats();
    assert_eq!(
        to_a.blob_transfer.served_wire_bytes,
        a_stats.fetched_wire_bytes
    );
    assert_eq!(to_a.blob_transfer.served_wire_bytes, fetched_wire);
    assert_eq!(
        to_a.blob_transfer.served_plain_bytes,
        from_b.blob_transfer.fetched_plain_bytes
    );
    assert_eq!(
        to_a.blob_transfer.served_plain_bytes,
        a_stats.fetched_plain_bytes
    );
    assert!(to_a.quic_traffic.blobs_zstd.quic_bytes_sent >= to_a.blob_transfer.served_wire_bytes);

    // A node-wide: both fetches in payload, only B's in the compressed
    // subset, one plain fallback.
    assert_eq!(a_stats.fetched_payload_bytes, 2 * SIZE);
    assert_eq!(a_stats.fetched_zstd_payload_bytes, SIZE);
    assert!(a_stats.fetched_wire_bytes > 0 && a_stats.fetched_wire_bytes < SIZE / 2);
    assert!(a_stats.fetched_plain_bytes > SIZE);
    assert_eq!(a_stats.plain_fallbacks, 1);
    assert_eq!(a_stats.fetch_transfers.zstd_ok, 1);
    assert_eq!(a_stats.fetch_transfers.plain_ok, 1);
    assert_eq!(a_stats.fetch_decode_errors, 0);

    // The servers: B over zstd, C plain.
    let b_stats = b.blobs().transfer_stats();
    assert_eq!(b_stats.served_payload_bytes, SIZE);
    assert_eq!(b_stats.served_zstd_payload_bytes, SIZE);
    assert_eq!(b_stats.serve_transfers.zstd_ok, 1);
    assert_eq!(b_stats.plain_serves, 0);
    let c_stats = view_until(&c, &a.id(), "C's view of A", |v| {
        v.blob_transfer.served_payload_bytes == SIZE
    })
    .await;
    assert_eq!(c_stats.blob_transfer.served_zstd_payload_bytes, 0);
    assert_eq!(c.blobs().transfer_stats().plain_serves, 1);

    a.shutdown().await;
    b.shutdown().await;
    c.shutdown().await;
}

/// Docs sync and gossip land under their own protocol.
#[tokio::test(flavor = "multi_thread")]
async fn docs_and_gossip_under_their_own_protocol() {
    let a = start(None, vec![]).await;
    let b = start(None, vec![]).await;
    connect(&a, &b);

    let topic = [7u8; 32];
    // Subscribing waits for a neighbour, so both sides join at once.
    let (ga, gb) = (a.gossip(), b.gossip());
    let (a_id, b_id) = (a.id(), b.id());
    let (ta, tb) = timeout(WAIT, async {
        tokio::join!(
            ga.subscribe(topic, std::slice::from_ref(&b_id)),
            gb.subscribe(topic, std::slice::from_ref(&a_id))
        )
    })
    .await
    .expect("gossip joined");
    let (ta, tb) = (ta.unwrap(), tb.unwrap());
    timeout(WAIT, async {
        loop {
            ta.broadcast(b"hello".to_vec()).await.unwrap();
            if let Ok(Ok(aster::GossipEvent::Received { .. })) =
                timeout(Duration::from_millis(500), tb.recv()).await
            {
                return;
            }
        }
    })
    .await
    .expect("gossip message delivered");

    let doc = a.docs().create().await.unwrap();
    let author = a.docs().default_author().await.unwrap();
    doc.set_bytes(&author, b"k".to_vec(), b"v".to_vec())
        .await
        .unwrap();
    let ticket = doc.share_with_addr(ShareMode::Read).await.unwrap();
    let doc_b = b.docs().join(ticket).await.unwrap();
    timeout(WAIT, async {
        while doc_b
            .get_exact(&author, b"k".to_vec())
            .await
            .unwrap()
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("doc entry synced");

    let view = view_until(&a, &b.id(), "docs and gossip traffic", |v| {
        v.quic_traffic.docs.quic_bytes_sent > 0 && v.quic_traffic.gossip.quic_bytes_sent > 0
    })
    .await;
    assert!(view.quic_traffic.docs.quic_bytes_received > 0);
    assert!(view.quic_traffic.gossip.quic_bytes_received > 0);
    assert_eq!(view.quic_traffic.rpc, Default::default());

    a.shutdown().await;
    b.shutdown().await;
}

/// A peer's QUIC totals never decrease while its connections churn: each
/// closed connection's final bytes move into the total as it leaves.
#[tokio::test(flavor = "multi_thread")]
async fn quic_bytes_survive_reconnects_monotonically() {
    const ALPN: &[u8] = b"aster-test/churn";
    const CHUNK: usize = 64 * 1024;
    const ROUNDS: usize = 6;
    let a = start(None, vec![]).await;
    let b = start(None, vec![ALPN.to_vec()]).await;
    connect(&a, &b);

    let server = b.clone();
    tokio::spawn(async move {
        while let Ok((_, conn)) = server.accept().await {
            tokio::spawn(async move {
                if let Ok(recv) = conn.accept_uni().await {
                    let _ = recv.read_to_end(CHUNK * 2).await;
                }
                conn.closed().await;
            });
        }
    });

    // Hold one connection until the peer has been sampled into a view.
    let first = a.connect(&b.id(), ALPN).await.unwrap();
    view_until(&a, &b.id(), "a sampled peer view", |_| true).await;

    let stop = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (a, b_id, stop) = (a.clone(), b.id(), stop.clone());
        tokio::spawn(async move {
            let mut prev = (0, 0);
            let mut reads = 0u64;
            while !stop.load(Ordering::Relaxed) {
                if let Some(v) = a.topology().peer(&b_id) {
                    let total = v.quic_traffic.total();
                    let now = (total.quic_bytes_sent, total.quic_bytes_received);
                    assert!(
                        now.0 >= prev.0 && now.1 >= prev.1,
                        "QUIC totals went back: {prev:?} -> {now:?}"
                    );
                    prev = now;
                    reads += 1;
                }
                tokio::task::yield_now().await;
            }
            reads
        })
    };

    first.close(0, b"done".to_vec()).unwrap();
    for _ in 0..ROUNDS {
        let conn = a.connect(&b.id(), ALPN).await.unwrap();
        let send = conn.open_uni().await.unwrap();
        send.write_all(vec![0u8; CHUNK]).await.unwrap();
        send.finish().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        conn.close(0, b"done".to_vec()).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let view = view_until(&a, &b.id(), "every connection's bytes", |v| {
        v.quic_traffic.other.quic_bytes_sent >= (ROUNDS * CHUNK) as u64
    })
    .await;
    stop.store(true, Ordering::Relaxed);
    let reads = watcher.await.expect("totals stayed monotonic");
    assert!(reads > 0);
    assert_eq!(view.quic_traffic.blobs, Default::default());

    a.shutdown().await;
    b.shutdown().await;
}
