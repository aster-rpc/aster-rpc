#![cfg(feature = "blobs")]
//! Ordered multi-provider blob fetch: provider preference, failover,
//! resumption, split-children fanout, and the report a caller ranks from.
//!
//! Ranking candidates is the caller's job (portal-sync does it from topology);
//! what is exercised here is that Aster consumes an ordered list *in order* and
//! reports truthfully what each provider did.

use aster::{AsterConfig, BlobFormat, Error, FetchStrategy, Node, NodeId, RelayMode};
use aster_transport_core::test_support::wait_budget;
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

#[tokio::test]
async fn interrupted_download_resumes_verified_partial_content() -> anyhow::Result<()> {
    use anyhow::{ensure, Context};
    use aster::native::{
        iroh::{address_lookup::memory::MemoryLookup, endpoint::presets, protocol::Router},
        iroh_blobs::{
            provider::events::{EventMask, EventSender, ProviderMessage, ThrottleMode},
            store::mem::MemStore,
            BlobFormat as NativeFormat, BlobsProtocol, HashAndFormat,
        },
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tokio::sync::{oneshot, Notify};

    // Four BAO chunk groups: no timing-sized payload or host stress.
    let payload: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    let store = MemStore::new();
    let tag = store.blobs().add_slice(&payload).await?;
    let native_hash = tag.hash;
    let hash = aster::Hash::from_hex(native_hash.to_hex().to_string());
    let provider = aster::native::iroh::Endpoint::builder(presets::N0)
        .relay_mode(aster::native::iroh::RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")?
        .bind()
        .await?;
    let provider_id = aster::NodeId::from_hex(provider.id().to_string());
    let release = Arc::new(Notify::new());
    let once = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = oneshot::channel();
    let (events, mut messages) = EventSender::channel(
        8,
        EventMask {
            throttle: ThrottleMode::Intercept,
            ..EventMask::DEFAULT
        },
    );
    let router = Router::builder(provider.clone())
        .accept(
            aster::native::iroh_blobs::ALPN,
            BlobsProtocol::new(&store, Some(events)),
        )
        .spawn();
    let release_worker = release.clone();
    let once_worker = once.clone();
    let event_worker = tokio::spawn(async move {
        let mut entered_tx = Some(entered_tx);
        while let Some(message) = messages.recv().await {
            if let ProviderMessage::Throttle(message) = message {
                // The provider has already emitted a verified 16 KiB group.
                if !once_worker.swap(true, Ordering::SeqCst) {
                    if let Some(tx) = entered_tx.take() {
                        let _ = tx.send(());
                    }
                    release_worker.notified().await;
                }
                let _ = message.tx.send(Ok(())).await;
            }
        }
    });
    let consumer = start_node().await;
    let blobs = consumer.blobs();
    let memory = MemoryLookup::new();
    memory.add_endpoint_info(provider.addr());
    blobs.native_endpoint().address_lookup()?.add(memory);
    let download_blobs = blobs.clone();
    let download_hash = hash.clone();
    let download_provider = provider_id.clone();
    let mut initial = Some(tokio::spawn(async move {
        download_blobs
            .download_hash_to_store_from(
                &download_hash,
                &[download_provider],
                BlobFormat::Raw,
                FetchStrategy::Whole,
            )
            .await
    }));
    // Return errors instead of asserting/panicking inside the controlled gate,
    // so every failure releases the provider and tears down owned tasks.
    let result: anyhow::Result<()> = async {
        timeout(wait_budget(), entered_rx).await.context("provider never reached throttle")??;
        let partial = timeout(wait_budget(), async {
            loop {
                let local = blobs.native_store().remote().local(HashAndFormat::new(native_hash, NativeFormat::Raw)).await?;
                if local.local_bytes() > 0 { return Ok::<_, anyhow::Error>(local); }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.context("no verified partial data reached store")??;
        let resident = partial.local_bytes();
        ensure!(!partial.is_complete() && resident < payload.len() as u64, "gated transfer falsely complete");
        ensure!(!initial.as_ref().context("missing initial task")?.is_finished(), "download finished before provider gate was released");
        let initial_task = initial.take().context("missing initial task")?;
        initial_task.abort();
        // Abort is observed by the downloader via its closed progress channel.
        // The provider remains gated while the public result is canceled.
        let cancelled = timeout(wait_budget(), initial_task).await.context("cancelled task failed to settle")?;
        ensure!(matches!(cancelled, Err(error) if error.is_cancelled()), "initial download did not cancel");
        ensure!(matches!(blobs.status(&hash).await?, aster::BlobStatus::Partial { .. }), "interrupted blob not Partial");
        release.notify_one();
        let (bytes, report) = timeout(wait_budget(), blobs.download_hash_from(
            &hash, &[provider_id], BlobFormat::Raw, FetchStrategy::Whole
        )).await.context("resume timed out")??;
        ensure!(bytes == payload, "resumed bytes differ");
        // Report counts transferred payload bytes, not Bao proof/framing bytes.
        ensure!(report.bytes_transferred() > 0 && report.bytes_transferred() < payload.len() as u64,
                "resume did not reuse verified resident data: {}", report.bytes_transferred());
        ensure!(matches!(blobs.status(&hash).await?, aster::BlobStatus::Complete { size } if size == payload.len() as u64), "resume incomplete");
        eprintln!("DATA01 partial_bytes={resident} resumed_payload_bytes={} total_bytes={}", report.bytes_transferred(), payload.len());
        Ok(())
    }.await;
    release.notify_one();
    if let Some(task) = initial {
        task.abort();
        let _ = timeout(wait_budget(), task).await;
    }
    let consumer_closed = timeout(wait_budget(), consumer.shutdown()).await;
    let provider_closed = timeout(wait_budget(), router.shutdown()).await;
    event_worker.abort();
    let _ = event_worker.await;
    result?;
    consumer_closed.context("consumer shutdown timed out")?;
    provider_closed.context("provider shutdown timed out")??;
    Ok(())
}

/// A blobs provider that serves the first chunk groups of `payload` and then
/// stops sending, with its connection still alive: it holds every throttle
/// request past the allowed ones, so the transfer sits open.
struct StallingProvider {
    id: NodeId,
    router: aster::native::iroh::protocol::Router,
    gate: tokio::task::JoinHandle<()>,
}

impl StallingProvider {
    /// Serves one chunk group, then stalls.
    async fn start(payload: &[u8], consumer: &Node) -> anyhow::Result<Self> {
        Self::start_allowing(payload, consumer, 0).await
    }

    /// Serves `allow` + 1 chunk groups, then stalls.
    async fn start_allowing(payload: &[u8], consumer: &Node, allow: usize) -> anyhow::Result<Self> {
        use aster::native::{
            iroh::{address_lookup::memory::MemoryLookup, endpoint::presets, protocol::Router},
            iroh_blobs::{
                provider::events::{EventMask, EventSender, ProviderMessage, ThrottleMode},
                store::mem::MemStore,
                BlobsProtocol,
            },
        };

        let store = MemStore::new();
        store.blobs().add_slice(payload).await?;
        let endpoint = aster::native::iroh::Endpoint::builder(presets::N0)
            .relay_mode(aster::native::iroh::RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")?
            .bind()
            .await?;
        let (events, mut messages) = EventSender::channel(
            8,
            EventMask {
                throttle: ThrottleMode::Intercept,
                ..EventMask::DEFAULT
            },
        );
        let router = Router::builder(endpoint.clone())
            .accept(
                aster::native::iroh_blobs::ALPN,
                BlobsProtocol::new(&store, Some(events)),
            )
            .spawn();
        let gate = tokio::spawn(async move {
            let mut held = Vec::new();
            let mut allowed = 0;
            while let Some(message) = messages.recv().await {
                if let ProviderMessage::Throttle(message) = message {
                    if allowed < allow {
                        allowed += 1;
                        message.tx.send(Ok(())).await.ok();
                    } else {
                        held.push(message);
                    }
                }
            }
        });
        let memory = MemoryLookup::new();
        memory.add_endpoint_info(endpoint.addr());
        consumer
            .blobs()
            .native_endpoint()
            .address_lookup()?
            .add(memory);
        Ok(Self {
            id: NodeId::from_hex(endpoint.id().to_string()),
            router,
            gate,
        })
    }

    async fn shutdown(self) -> anyhow::Result<()> {
        self.gate.abort();
        self.router.shutdown().await?;
        Ok(())
    }
}

fn stall_payload() -> Vec<u8> {
    (0..256 * 1024).map(|i| (i % 251) as u8).collect()
}

/// A provider that serves one chunk group and then stops sending, with its
/// connection still alive, is abandoned after the idle timeout and reported
/// as failed; the next candidate finishes the blob from where it stopped.
#[tokio::test]
async fn fails_over_from_a_provider_that_stalls_mid_transfer() -> anyhow::Result<()> {
    let payload = stall_payload();
    let holder = start_node().await;
    let hash = holder.blobs().add_bytes(payload.clone()).await?;
    let consumer = start_node().await;
    connect(&consumer, &holder);
    let stalled = StallingProvider::start(&payload, &consumer).await?;

    let (got, report) = timeout(
        FETCH_TIMEOUT,
        consumer
            .blobs()
            .with_provider_idle_timeout(Some(Duration::from_millis(500)))
            .download_hash_from(
                &hash,
                &[stalled.id.clone(), holder.id()],
                BlobFormat::Raw,
                FetchStrategy::Whole,
            ),
    )
    .await
    .expect("a stalled lead provider blocked the fetch")?;

    assert_eq!(got, payload);
    assert_eq!(
        ids(report.attempted()),
        vec![stalled.id.to_string(), holder.id().to_string()]
    );
    assert_eq!(ids(report.failed()), vec![stalled.id.to_string()]);
    assert_eq!(ids(report.served()), vec![holder.id().to_string()]);
    assert!(report.in_flight().next().is_none());
    assert_eq!(report.bytes_transferred(), payload.len() as u64);

    stalled.shutdown().await?;
    holder.shutdown().await;
    consumer.shutdown().await;
    Ok(())
}

/// When the caller's own timeout cancels a fetch, the tracker still reports
/// what happened, including the provider the fetch was waiting on.
#[tokio::test]
async fn tracker_reports_the_provider_a_cancelled_fetch_was_waiting_on() -> anyhow::Result<()> {
    let payload = stall_payload();
    let holder = start_node().await;
    let hash = holder.blobs().add_bytes(payload.clone()).await?;
    let consumer = start_node().await;
    connect(&consumer, &holder);
    let stalled = StallingProvider::start(&payload, &consumer).await?;

    let tracker = aster::FetchTracker::new();
    let blobs = consumer.blobs().with_provider_idle_timeout(None);
    let from = [stalled.id.clone(), holder.id()];
    let fetch = blobs.download_hash_to_store_from_tracked(
        &hash,
        &from,
        BlobFormat::Raw,
        FetchStrategy::Whole,
        &tracker,
    );
    assert!(
        // short-wait-ok: the fetch must still be waiting on the stalled provider
        timeout(Duration::from_secs(2), fetch).await.is_err(),
        "the fetch should still be waiting on the stalled provider"
    );

    let report = tracker.report();
    assert_eq!(ids(report.attempted()), vec![stalled.id.to_string()]);
    assert_eq!(ids(report.in_flight()), vec![stalled.id.to_string()]);
    assert!(report.failed().next().is_none());
    assert!(report.served().next().is_none());
    // The bytes the unfinished attempt received are in the report: the
    // provider served one chunk group before stalling.
    let stalled_outcome = &report.providers()[0];
    assert!(
        stalled_outcome.bytes_transferred() > 0
            && stalled_outcome.bytes_transferred() < payload.len() as u64,
        "{report:?}"
    );
    assert_eq!(
        stalled_outcome.bytes_transferred(),
        report.bytes_transferred()
    );
    // A plain-only provider: no compressed bytes.
    assert_eq!(stalled_outcome.wire_bytes(), None);

    stalled.shutdown().await?;
    holder.shutdown().await;
    consumer.shutdown().await;
    Ok(())
}

/// Portal's M3 acceptance: a failover fetch whose first provider dies after
/// delivering part of the blob credits bytes to both providers, and they add
/// up to the report's total.
#[tokio::test]
async fn failover_credits_bytes_to_a_provider_that_died_mid_transfer() -> anyhow::Result<()> {
    let payload = stall_payload();
    let holder = start_node().await;
    let hash = holder.blobs().add_bytes(payload.clone()).await?;
    let consumer = start_node().await;
    connect(&consumer, &holder);
    let dying = StallingProvider::start_allowing(&payload, &consumer, 3).await?;
    let dying_id = dying.id.clone();

    let blobs = consumer.blobs().with_provider_idle_timeout(None);
    let from = [dying_id.clone(), holder.id()];
    let fetch = blobs.download_hash_from(&hash, &from, BlobFormat::Raw, FetchStrategy::Whole);
    // Kill the first provider once part of the blob's content is stored.
    let native_hash: aster::native::iroh_blobs::Hash = hash.as_str().parse()?;
    let kill = async {
        let store = consumer.blobs().native_store();
        let content = aster::native::iroh_blobs::HashAndFormat::raw(native_hash);
        while store.remote().local(content).await?.local_bytes() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        dying.shutdown().await
    };
    let (fetched, killed) = timeout(FETCH_TIMEOUT, async { tokio::join!(fetch, kill) })
        .await
        .expect("the fetch did not fail over from the dead provider");
    killed?;
    let (got, report) = fetched?;

    assert_eq!(got, payload);
    assert_eq!(
        ids(report.attempted()),
        vec![dying_id.to_string(), holder.id().to_string()]
    );
    assert_eq!(ids(report.failed()), vec![dying_id.to_string()]);
    assert_eq!(ids(report.served()), vec![holder.id().to_string()]);
    let [from_dying, from_holder] = report.providers() else {
        panic!("two providers expected: {report:?}");
    };
    assert!(from_dying.bytes_transferred() > 0, "{report:?}");
    assert!(from_holder.bytes_transferred() > 0, "{report:?}");
    assert_eq!(
        from_dying.bytes_transferred() + from_holder.bytes_transferred(),
        report.bytes_transferred()
    );
    assert_eq!(report.bytes_transferred(), payload.len() as u64);
    // The dead provider served plain; the holder over zstd.
    assert_eq!(from_dying.wire_bytes(), None);
    assert!(
        from_holder.wire_bytes().is_some_and(|w| w > 0),
        "{report:?}"
    );

    holder.shutdown().await;
    consumer.shutdown().await;
    Ok(())
}
