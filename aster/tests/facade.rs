#![cfg(all(feature = "blobs", feature = "docs"))]
//! Cross-node integration: two in-memory nodes connect, round-trip a blob via
//! `download_hash` (from a specific peer) and sync a doc entry, observing the
//! live `InsertRemote { from }` event.

use aster::{
    AsterConfig, AuthorId, BlobFormat, Doc, DocEvent, DocEvents, Node, RelayMode, ShareMode,
};
use std::time::Duration;
use tokio::time::timeout;

fn test_config() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn wait_for_direct_addr(node: &Node) {
    for _ in 0..50 {
        if !node.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Two in-memory nodes that can reach each other directly (relay disabled,
/// addresses exchanged). Mirrors the Python `node_pair` fixture.
async fn node_pair() -> (Node, Node) {
    let a = Node::start(test_config()).await.expect("start a");
    let b = Node::start(test_config()).await.expect("start b");

    // Wait until each node has a direct address, then exchange.
    for n in [&a, &b] {
        wait_for_direct_addr(n).await;
    }
    a.add_peer(&b).unwrap();
    b.add_peer(&a).unwrap();
    (a, b)
}

async fn wait_for_exact(doc: &Doc, author: &AuthorId, key: &[u8], expected: Option<&[u8]>) {
    let key = key.to_vec();
    let expected = expected.map(Vec::from);
    timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(value) = doc.get_exact(author, key.clone()).await {
                if value == expected {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("doc exact value did not converge in time");
}

async fn wait_for_remote_insert_key(events: &DocEvents, from: &Node, key: &[u8]) {
    timeout(Duration::from_secs(20), async {
        loop {
            match events.recv().await.expect("doc event stream") {
                Some(DocEvent::InsertRemote {
                    from: event_from,
                    entry,
                }) if event_from == from.id() && entry.key == key => return,
                Some(_) => continue,
                None => panic!("doc event stream ended before remote insert"),
            }
        }
    })
    .await
    .expect("remote insert did not arrive in time");
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
async fn doc_prefix_delete_tombstone_syncs_to_current_and_late_peers() {
    let (a, b) = node_pair().await;

    let doc_a = a.docs().create().await.expect("create doc");
    let author_a = a.docs().default_author().await.expect("author");
    doc_a
        .set_bytes(&author_a, b"ops/1".to_vec(), b"one".to_vec())
        .await
        .expect("set ops/1");
    doc_a
        .set_bytes(&author_a, b"ops/2".to_vec(), b"two".to_vec())
        .await
        .expect("set ops/2");
    doc_a
        .set_bytes(&author_a, b"keep/1".to_vec(), b"keep".to_vec())
        .await
        .expect("set keep/1");

    let ticket = doc_a.share_with_addr(ShareMode::Read).await.expect("share");
    let (doc_b, events_b) = b
        .docs()
        .join_and_subscribe(ticket.clone())
        .await
        .expect("join b");

    // B must first prove it had the old entries locally. Otherwise a later
    // None could be a missing-sync false positive rather than a replicated tombstone.
    wait_for_exact(&doc_b, &author_a, b"ops/1", Some(b"one")).await;
    wait_for_exact(&doc_b, &author_a, b"ops/2", Some(b"two")).await;

    let removed = doc_a.del(&author_a, b"ops/".to_vec()).await.expect("del");
    assert_eq!(removed, 2);

    // The deletion marker itself replicates as an entry at the deleted prefix.
    wait_for_remote_insert_key(&events_b, &a, b"ops/").await;
    wait_for_exact(&doc_b, &author_a, b"ops/1", None).await;
    wait_for_exact(&doc_b, &author_a, b"ops/2", None).await;
    wait_for_exact(&doc_b, &author_a, b"keep/1", Some(b"keep")).await;

    // A later peer joining from the same ticket should learn the tombstone too,
    // not resurrect entries that predated the delete.
    let c = Node::start(test_config()).await.expect("start c");
    wait_for_direct_addr(&c).await;
    a.add_peer(&c).unwrap();
    c.add_peer(&a).unwrap();

    let (doc_c, events_c) = c.docs().join_and_subscribe(ticket).await.expect("join c");
    wait_for_remote_insert_key(&events_c, &a, b"ops/").await;
    wait_for_exact(&doc_c, &author_a, b"keep/1", Some(b"keep")).await;
    assert_eq!(
        doc_c.get_exact(&author_a, b"ops/1".to_vec()).await.unwrap(),
        None
    );
    assert_eq!(
        doc_c.get_exact(&author_a, b"ops/2".to_vec()).await.unwrap(),
        None
    );

    a.shutdown().await;
    b.shutdown().await;
    c.shutdown().await;
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
