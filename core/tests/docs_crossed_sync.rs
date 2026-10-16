//! Both replicas of a document dial each other at once. iroh-docs settles the
//! crossing by a tie-break and reports the losing connect as a failed
//! `SyncFinished` ("Remote peer aborted sync: AlreadySyncing") before the
//! other direction completes. A caller waiting for a completed sync must not
//! fail on that abort.

use std::time::Duration;

use aster_transport_core::{CoreEndpointConfig, CoreNode};

async fn node(seed: u8) -> CoreNode {
    CoreNode::memory_with_alpns_and_gc(
        Vec::new(),
        Some(CoreEndpointConfig {
            relay_mode: Some("disabled".into()),
            secret_key: Some(vec![seed; 32]),
            ..Default::default()
        }),
        Some(Duration::from_secs(3600)),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn simultaneous_syncs_in_both_directions_both_complete() {
    let a = node(51).await;
    let b = node(52).await;
    a.add_node_addr(&b).unwrap();
    b.add_node_addr(&a).unwrap();
    let doc_a = a.docs_client().create().await.unwrap();
    let author = a.docs_client().create_author().await.unwrap();
    let doc_b = b
        .docs_client()
        .join(doc_a.share_with_addr("write".into()).await.unwrap())
        .await
        .unwrap();
    let a_id = a.node_id();
    let b_id = b.node_id();
    // Repeat so the two dials overlap in at least some rounds; every round
    // must succeed on both sides.
    for round in 0..10u8 {
        doc_a
            .set_bytes(author.clone(), vec![b'k', round], vec![round])
            .await
            .unwrap();
        let (from_a, from_b) = tokio::join!(
            doc_a.sync_with_timeout(vec![b_id.clone()], Duration::from_secs(20)),
            doc_b.sync_with_timeout(vec![a_id.clone()], Duration::from_secs(20)),
        );
        from_a.unwrap_or_else(|error| panic!("round {round}, a -> b: {error:#}"));
        from_b.unwrap_or_else(|error| panic!("round {round}, b -> a: {error:#}"));
    }
    a.close().await;
    b.close().await;
}

/// `join` imports the ticket *and* starts syncing with its peers (TM-6): the
/// entry arrives without any `start_sync` call.
#[tokio::test]
async fn join_starts_syncing_with_the_ticket_peers() {
    let a = node(53).await;
    let b = node(54).await;
    let doc_a = a.docs_client().create().await.unwrap();
    let author = a.docs_client().create_author().await.unwrap();
    doc_a
        .set_bytes(author.clone(), b"k".to_vec(), b"v".to_vec())
        .await
        .unwrap();
    let doc_b = b
        .docs_client()
        .join(doc_a.share_with_addr("write".into()).await.unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !matches!(
            doc_b.entry_exists(author.clone(), b"k".to_vec()).await,
            Ok(true)
        ) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("join alone synced the entry");
    a.close().await;
    b.close().await;
}
