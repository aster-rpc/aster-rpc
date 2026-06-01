#![cfg(all(feature = "blobs", feature = "docs"))]
//! Cross-node integration: two in-memory nodes connect, round-trip a blob via
//! `download_hash` (from a specific peer) and sync a doc entry, observing the
//! live `InsertRemote { from }` event.

use aster::{AsterConfig, BlobFormat, DocEvent, Node, RelayMode, ShareMode};
use std::time::Duration;
use tokio::time::timeout;

/// Two in-memory nodes that can reach each other directly (relay disabled,
/// addresses exchanged). Mirrors the Python `node_pair` fixture.
async fn node_pair() -> (Node, Node) {
    let cfg = || {
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .build()
    };
    let a = Node::start(cfg()).await.expect("start a");
    let b = Node::start(cfg()).await.expect("start b");

    // Wait until each node has a direct address, then exchange.
    for n in [&a, &b] {
        for _ in 0..50 {
            if !n.addr().direct_addresses.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    a.add_peer(&b).unwrap();
    b.add_peer(&a).unwrap();
    (a, b)
}

#[tokio::test]
async fn blob_round_trips_between_nodes() {
    let (a, b) = node_pair().await;

    let data = b"the quick brown fox".to_vec();
    let hash = a.blobs().add_bytes(data.clone()).await.expect("add");

    // B downloads the blob from A by hash + peer id.
    let got = timeout(
        Duration::from_secs(20),
        b.blobs().download_hash(&hash, &a.id(), BlobFormat::Raw),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    assert_eq!(got, data);
    assert!(b.blobs().has(&hash).await.unwrap());

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn doc_entry_syncs_with_insert_remote_event() {
    let (a, b) = node_pair().await;

    // A creates a doc and writes an entry.
    let doc_a = a.docs().create().await.expect("create doc");
    let author_a = a.docs().default_author().await.expect("author");
    doc_a
        .set_bytes(&author_a, b"k".to_vec(), b"v".to_vec())
        .await
        .expect("set");

    // B joins from a full ticket and subscribes to live events.
    let ticket = doc_a.share_with_addr(ShareMode::Read).await.expect("share");
    let (doc_b, events) = b.docs().join_and_subscribe(ticket).await.expect("join");

    // Expect to observe the remote insert from A.
    let saw_remote = timeout(Duration::from_secs(20), async {
        loop {
            match events.recv().await {
                Ok(Some(DocEvent::InsertRemote { from, entry })) => {
                    assert_eq!(from, a.id());
                    assert_eq!(entry.key, b"k");
                    return true;
                }
                Ok(Some(_)) => continue,
                Ok(None) => return false,
                Err(_) => return false,
            }
        }
    })
    .await
    .expect("did not observe InsertRemote in time");
    assert!(saw_remote);

    // And the value is readable on B. Entry metadata syncs before the content
    // blob finishes downloading, so retry the content read until it lands.
    let latest = doc_b.query_latest_exact(b"k".to_vec()).await.unwrap();
    let entry = latest.expect("entry present on B");
    let val = timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(v) = doc_b.read_entry_content(&entry.content_hash).await {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("content never became readable on B");
    assert_eq!(val, b"v");

    a.shutdown().await;
    b.shutdown().await;
}
