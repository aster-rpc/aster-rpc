#![cfg(feature = "blobs")]
//! Ordered multi-provider blob fetch: provider preference, failover,
//! resumption, split-children fanout, and the report a caller ranks from.
//!
//! Ranking candidates is the caller's job (portal-sync does it from topology);
//! what is exercised here is that Aster consumes an ordered list *in order* and
//! reports truthfully what each provider did.

use aster::{AsterConfig, BlobFormat, Error, FetchStrategy, Node, NodeId, RelayMode};
use std::time::Duration;
use tokio::time::timeout;

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

fn test_config() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn start_node() -> Node {
    let node = Node::start(test_config()).await.expect("start node");
    for _ in 0..50 {
        if !node.addr().direct_addresses.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node
}

/// Teach each node the other's address (relay is disabled, so without this
/// there is nothing to dial).
fn connect(a: &Node, b: &Node) {
    a.add_peer(b).unwrap();
    b.add_peer(a).unwrap();
}

fn ids<'a>(it: impl Iterator<Item = &'a NodeId>) -> Vec<String> {
    it.map(|id| id.to_string()).collect()
}

fn collection() -> Vec<(String, Vec<u8>)> {
    vec![
        ("big".to_string(), vec![1u8; 512 * 1024]),
        ("small".to_string(), vec![2u8; 4096]),
    ]
}

/// The first-listed provider serves, and the rest are never touched.
#[tokio::test]
async fn prefers_the_first_listed_provider() {
    let first = start_node().await;
    let second = start_node().await;
    let consumer = start_node().await;
    connect(&consumer, &first);
    connect(&consumer, &second);

    let data = vec![7u8; 256 * 1024];
    let hash = first.blobs().add_bytes(data.clone()).await.unwrap();
    let same = second.blobs().add_bytes(data.clone()).await.unwrap();
    assert_eq!(hash, same, "both providers hold the same blob");

    let (got, report) = timeout(
        FETCH_TIMEOUT,
        consumer.blobs().download_hash_from(
            &hash,
            &[first.id(), second.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    assert_eq!(got, data);
    assert_eq!(ids(report.attempted()), vec![first.id().to_string()]);
    assert_eq!(ids(report.served()), vec![first.id().to_string()]);
    assert!(ids(report.failed()).is_empty());
    assert_eq!(report.bytes_transferred(), data.len() as u64);

    first.shutdown().await;
    second.shutdown().await;
    consumer.shutdown().await;
}

/// A candidate that cannot be reached is reported as failed, and the next one
/// in the list completes the fetch.
#[tokio::test]
async fn falls_over_from_an_unreachable_candidate() {
    let dead = start_node().await;
    let holder = start_node().await;
    let consumer = start_node().await;
    // The consumer learns the dead node's address before it goes away, so this
    // is a genuine offline peer rather than an unaddressable id.
    connect(&consumer, &dead);
    connect(&consumer, &holder);

    let data = vec![3u8; 128 * 1024];
    let hash = holder.blobs().add_bytes(data.clone()).await.unwrap();
    let dead_node_id = dead.id();
    let dead_id = dead_node_id.to_string();
    dead.shutdown().await;

    let (got, report) = timeout(
        FETCH_TIMEOUT,
        consumer.blobs().download_hash_from(
            &hash,
            &[dead_node_id, holder.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    assert_eq!(got, data);
    assert_eq!(
        ids(report.attempted()),
        vec![dead_id.clone(), holder.id().to_string()],
        "the dead candidate is tried first, then the holder"
    );
    assert_eq!(ids(report.failed()), vec![dead_id]);
    assert_eq!(ids(report.served()), vec![holder.id().to_string()]);
    assert_eq!(report.bytes_transferred(), data.len() as u64);

    holder.shutdown().await;
    consumer.shutdown().await;
}

/// A valid id that the endpoint has no address for fails fast and
/// deterministically — the complement to the offline-peer case above.
#[tokio::test]
async fn falls_over_from_an_unaddressable_candidate() {
    let stranger = start_node().await;
    let holder = start_node().await;
    let consumer = start_node().await;
    connect(&consumer, &holder);

    let data = vec![4u8; 4096];
    let hash = holder.blobs().add_bytes(data.clone()).await.unwrap();

    let (got, report) = timeout(
        FETCH_TIMEOUT,
        consumer.blobs().download_hash_from(
            &hash,
            &[stranger.id(), holder.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    assert_eq!(got, data);
    assert_eq!(ids(report.failed()), vec![stranger.id().to_string()]);
    assert_eq!(ids(report.served()), vec![holder.id().to_string()]);

    stranger.shutdown().await;
    holder.shutdown().await;
    consumer.shutdown().await;
}

/// Content already resident is not re-fetched: each attempt asks only for what
/// is missing.
#[tokio::test]
async fn resumes_without_refetching_resident_content() {
    let provider = start_node().await;
    let entries = collection();
    let root = provider
        .blobs()
        .add_collection(entries.clone())
        .await
        .unwrap();
    let root_bytes = provider.blobs().read_to_bytes(&root).await.unwrap();

    // Control: what a consumer holding nothing has to transfer.
    let total = {
        let fresh = start_node().await;
        connect(&fresh, &provider);
        let report = timeout(
            FETCH_TIMEOUT,
            fresh.blobs().download_hash_to_store_from(
                &root,
                &[provider.id()],
                BlobFormat::HashSeq,
                FetchStrategy::Whole,
            ),
        )
        .await
        .expect("control download timed out")
        .expect("control download failed");
        let total = report.bytes_transferred();
        fresh.shutdown().await;
        total
    };

    // This consumer already holds the HashSeq root and the big child. Seeding
    // the root matters: without it the local side cannot enumerate children,
    // and there is nothing to skip.
    let consumer = start_node().await;
    connect(&consumer, &provider);
    consumer
        .blobs()
        .add_bytes(root_bytes.clone())
        .await
        .unwrap();
    consumer
        .blobs()
        .add_bytes(entries[0].1.clone())
        .await
        .unwrap();

    let (files, report) = timeout(
        FETCH_TIMEOUT,
        consumer
            .blobs()
            .download_collection_from(&root, &[provider.id()], FetchStrategy::Whole),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    assert_eq!(files, entries, "every entry is resident and intact");
    assert_eq!(
        report.bytes_transferred(),
        total - root_bytes.len() as u64 - entries[0].1.len() as u64,
        "the resident root and big child were not transferred again"
    );

    provider.shutdown().await;
    consumer.shutdown().await;
}

/// Split fetch across two partial holders: each child walks the list from the
/// top, so the first provider serves what it has and the second covers the
/// rest.
#[tokio::test]
async fn splits_children_across_partial_holders() {
    let full = start_node().await;
    let partial = start_node().await;
    let consumer = start_node().await;
    connect(&consumer, &full);
    connect(&consumer, &partial);

    let entries = collection();
    let root = full.blobs().add_collection(entries.clone()).await.unwrap();
    let root_bytes = full.blobs().read_to_bytes(&root).await.unwrap();
    // `partial` holds the HashSeq root and the first child only.
    partial.blobs().add_bytes(root_bytes).await.unwrap();
    partial
        .blobs()
        .add_bytes(entries[0].1.clone())
        .await
        .unwrap();

    let (files, report) = timeout(
        FETCH_TIMEOUT,
        consumer.blobs().download_collection_from(
            &root,
            &[partial.id(), full.id()],
            FetchStrategy::SplitChildren,
        ),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    assert_eq!(files, entries, "every child ended up resident and intact");
    assert_eq!(
        ids(report.attempted()),
        vec![partial.id().to_string(), full.id().to_string()],
        "the partial holder is tried first for every child"
    );
    assert_eq!(
        ids(report.served()),
        vec![partial.id().to_string(), full.id().to_string()],
        "both providers carried at least one request"
    );
    assert_eq!(
        ids(report.failed()),
        vec![partial.id().to_string()],
        "the partial holder failed the children it does not have"
    );

    full.shutdown().await;
    partial.shutdown().await;
    consumer.shutdown().await;
}

/// A resident child in split mode costs neither a provider attempt nor a byte.
#[tokio::test]
async fn split_skips_resident_children() {
    let provider = start_node().await;
    let entries = collection();
    let root = provider
        .blobs()
        .add_collection(entries.clone())
        .await
        .unwrap();
    let root_bytes = provider.blobs().read_to_bytes(&root).await.unwrap();

    let total = {
        let fresh = start_node().await;
        connect(&fresh, &provider);
        let report = timeout(
            FETCH_TIMEOUT,
            fresh.blobs().download_hash_to_store_from(
                &root,
                &[provider.id()],
                BlobFormat::HashSeq,
                FetchStrategy::SplitChildren,
            ),
        )
        .await
        .expect("control download timed out")
        .expect("control download failed");
        let total = report.bytes_transferred();
        fresh.shutdown().await;
        total
    };

    let consumer = start_node().await;
    connect(&consumer, &provider);
    consumer
        .blobs()
        .add_bytes(root_bytes.clone())
        .await
        .unwrap();
    consumer
        .blobs()
        .add_bytes(entries[0].1.clone())
        .await
        .unwrap();

    let (files, report) = timeout(
        FETCH_TIMEOUT,
        consumer.blobs().download_collection_from(
            &root,
            &[provider.id()],
            FetchStrategy::SplitChildren,
        ),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    assert_eq!(files, entries);
    let outcome = &report.providers()[0];
    assert_eq!(report.providers().len(), 1);
    // The resident root and big child are never dialled for. What remains is
    // the small child plus the collection's own metadata child.
    assert_eq!(outcome.attempts(), 2);
    assert_eq!(outcome.completed_requests(), 2);
    assert_eq!(outcome.failures(), 0);
    assert_eq!(
        report.bytes_transferred(),
        total - root_bytes.len() as u64 - entries[0].1.len() as u64
    );

    provider.shutdown().await;
    consumer.shutdown().await;
}

/// When every candidate fails, the error names who was tried.
#[tokio::test]
async fn reports_tried_providers_when_all_candidates_fail() {
    let holder = start_node().await;
    let first = start_node().await;
    let second = start_node().await;
    let consumer = start_node().await;
    connect(&consumer, &first);
    connect(&consumer, &second);

    // Only `holder` has the blob, and it is not a candidate.
    let hash = holder.blobs().add_bytes(vec![5u8; 4096]).await.unwrap();

    let err = timeout(
        FETCH_TIMEOUT,
        consumer.blobs().download_hash_from(
            &hash,
            &[first.id(), second.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await
    .expect("download timed out")
    .expect_err("download should have failed");

    let message = err.to_string();
    assert!(
        message.contains(first.id().as_str()) && message.contains(second.id().as_str()),
        "error should name both tried providers, got: {message}"
    );

    holder.shutdown().await;
    first.shutdown().await;
    second.shutdown().await;
    consumer.shutdown().await;
}

/// An empty candidate list is rejected before any network work, with a clear
/// error rather than the downloader's generic "unable to download".
#[tokio::test]
async fn empty_provider_list_is_an_invalid_argument() {
    let node = start_node().await;
    let hash = node.blobs().add_bytes(vec![6u8; 32]).await.unwrap();

    let err = node
        .blobs()
        .download_hash_from(&hash, &[], BlobFormat::Raw, FetchStrategy::Whole)
        .await
        .expect_err("empty provider list should be rejected");
    assert!(matches!(err, Error::InvalidArgument(_)), "got {err:?}");

    let err = node
        .blobs()
        .download_hash_to_store_from(&hash, &[], BlobFormat::Raw, FetchStrategy::Whole)
        .await
        .expect_err("empty provider list should be rejected");
    assert!(matches!(err, Error::InvalidArgument(_)), "got {err:?}");

    node.shutdown().await;
}

/// Splitting a raw blob is meaningless (it would treat the blob's own bytes as
/// a HashSeq), so it is rejected up front.
#[tokio::test]
async fn split_children_requires_a_hash_seq() {
    let provider = start_node().await;
    let consumer = start_node().await;
    connect(&consumer, &provider);
    let hash = provider.blobs().add_bytes(vec![8u8; 4096]).await.unwrap();

    let err = consumer
        .blobs()
        .download_hash_from(
            &hash,
            &[provider.id()],
            BlobFormat::Raw,
            FetchStrategy::SplitChildren,
        )
        .await
        .expect_err("raw + split should be rejected");
    assert!(matches!(err, Error::InvalidArgument(_)), "got {err:?}");

    provider.shutdown().await;
    consumer.shutdown().await;
}

/// One candidate behaves exactly like the single-provider API it delegates to.
#[tokio::test]
async fn one_candidate_matches_the_single_provider_api() {
    let provider = start_node().await;
    let data = vec![9u8; 64 * 1024];
    let hash = provider.blobs().add_bytes(data.clone()).await.unwrap();

    // Separate consumers, so the second call is not a local-cache no-op.
    let old_consumer = start_node().await;
    connect(&old_consumer, &provider);
    let old = timeout(
        FETCH_TIMEOUT,
        old_consumer
            .blobs()
            .download_hash(&hash, &provider.id(), BlobFormat::Raw),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    let new_consumer = start_node().await;
    connect(&new_consumer, &provider);
    let (new, report) = timeout(
        FETCH_TIMEOUT,
        new_consumer.blobs().download_hash_from(
            &hash,
            &[provider.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    assert_eq!(old, data);
    assert_eq!(new, data);
    assert_eq!(ids(report.served()), vec![provider.id().to_string()]);

    provider.shutdown().await;
    old_consumer.shutdown().await;
    new_consumer.shutdown().await;
}
