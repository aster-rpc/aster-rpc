#![cfg(feature = "blobs")]
//! zstd blob transport: compressed fetches between compressing nodes, plain
//! fallback to nodes that do not compress, ordered fail-over and tracking over
//! the compressed ALPN, and Gate-0 admission of the compressed ALPN exactly as
//! the plain one.

use aster::{alpns, Gate0};
use aster::{
    AsterConfig, BlobFormat, BlobTransportCompression, FetchStrategy, FetchTracker, Node, NodeId,
    RelayMode,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

fn config(zstd_level: Option<u8>, hooks: bool) -> AsterConfig {
    let mut builder = AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .hooks(hooks);
    if let Some(level) = zstd_level {
        builder = builder.blob_zstd_level(level);
    }
    builder.build()
}

async fn start(zstd_level: Option<u8>, hooks: bool) -> Node {
    let node = Node::start(config(zstd_level, hooks))
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

fn ids<'a>(it: impl Iterator<Item = &'a NodeId>) -> Vec<String> {
    it.map(|id| id.to_string()).collect()
}

/// Text-heavy content: Aster's own sources, each included once.
fn text() -> Vec<u8> {
    [
        include_str!("../../src/config.rs"),
        include_str!("../../src/blobs.rs"),
        include_str!("../../src/docs.rs"),
        include_str!("../../src/admission.rs"),
    ]
    .concat()
    .into_bytes()
}

/// Two compressing nodes fetch over zstd by default: the same content under
/// the same hash, the same payload accounting, fewer bytes on the wire.
#[tokio::test]
async fn compressing_nodes_fetch_over_zstd() -> anyhow::Result<()> {
    let holder = start(Some(3), false).await;
    let consumer = start(Some(3), false).await;
    connect(&consumer, &holder);
    let data = text();
    let hash = holder.blobs().add_bytes(data.clone()).await?;
    let blobs = consumer.blobs();
    assert_eq!(
        blobs.transport_compression(),
        BlobTransportCompression::PreferZstd
    );

    let report = timeout(
        FETCH_TIMEOUT,
        blobs.download_hash_to_store_from(
            &hash,
            &[holder.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await??;

    assert_eq!(consumer.blobs().read_to_bytes(&hash).await?, data);
    assert_eq!(ids(report.served()), vec![holder.id().to_string()]);
    assert!(report.plain_only().next().is_none());
    assert_eq!(report.bytes_transferred(), data.len() as u64);
    let fetched = blobs.transport_compression_stats();
    let served = holder.blobs().transport_compression_stats();
    assert_eq!(fetched.fetched_wire_bytes, served.served_wire_bytes);
    assert_eq!(fetched.fetched_plain_bytes, served.served_plain_bytes);
    assert!(fetched.fetched_wire_bytes * 2 < fetched.fetched_plain_bytes);
    assert_eq!(fetched.decode_errors, 0);
    println!(
        "aster zstd level 3: payload {} B, wire {} B, ratio {:.2}x",
        data.len(),
        fetched.fetched_wire_bytes,
        fetched.fetched_plain_bytes as f64 / fetched.fetched_wire_bytes as f64
    );

    holder.shutdown().await;
    consumer.shutdown().await;
    Ok(())
}

/// A provider that does not serve zstd is fetched from plain and reported as
/// plain-only, not as failed. Requiring zstd fails it instead.
#[tokio::test]
async fn falls_back_to_a_plain_provider() -> anyhow::Result<()> {
    let plain = start(None, false).await;
    let consumer = start(Some(3), false).await;
    connect(&consumer, &plain);
    let data = text();
    let hash = plain.blobs().add_bytes(data.clone()).await?;
    let other = plain.blobs().add_bytes(b"another".to_vec()).await?;

    let required = timeout(
        FETCH_TIMEOUT,
        consumer
            .blobs()
            .with_transport_compression(BlobTransportCompression::RequireZstd)
            .download_hash_to_store_from(
                &hash,
                &[plain.id()],
                BlobFormat::Raw,
                FetchStrategy::Whole,
            ),
    )
    .await?;
    assert!(required.is_err(), "zstd was required: {required:?}");
    assert!(!consumer.blobs().has(&hash).await?);

    let report = timeout(
        FETCH_TIMEOUT,
        consumer.blobs().download_hash_to_store_from(
            &hash,
            &[plain.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await??;
    assert_eq!(consumer.blobs().read_to_bytes(&hash).await?, data);
    assert_eq!(ids(report.plain_only()), vec![plain.id().to_string()]);
    assert!(report.failed().next().is_none());
    assert_eq!(ids(report.served()), vec![plain.id().to_string()]);
    assert_eq!(report.providers()[0].plain_fallbacks(), 1);

    // The refusal is remembered: the next fetch goes plain straight away and
    // is still reported as plain-only.
    let report = timeout(
        FETCH_TIMEOUT,
        consumer.blobs().download_hash_to_store_from(
            &other,
            &[plain.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await??;
    assert_eq!(ids(report.plain_only()), vec![plain.id().to_string()]);
    assert_eq!(
        consumer
            .blobs()
            .transport_compression_stats()
            .fetched_wire_bytes,
        0
    );

    // Off fetches plain without offering zstd at all.
    let off = consumer
        .blobs()
        .with_transport_compression(BlobTransportCompression::Off);
    let third = plain.blobs().add_bytes(b"third".to_vec()).await?;
    let report = timeout(
        FETCH_TIMEOUT,
        off.download_hash_to_store_from(
            &third,
            &[plain.id()],
            BlobFormat::Raw,
            FetchStrategy::Whole,
        ),
    )
    .await??;
    assert!(report.plain_only().next().is_none());

    plain.shutdown().await;
    consumer.shutdown().await;
    Ok(())
}

/// A zstd provider that accepts a request and stalls with its connection
/// alive is abandoned after the idle timeout; the next zstd provider serves.
/// The tracker reports the same as the returned report.
#[tokio::test]
async fn ordered_fetch_fails_over_from_a_stalled_zstd_provider() -> anyhow::Result<()> {
    let data = text();
    let holder = start(Some(3), false).await;
    let hash = holder.blobs().add_bytes(data.clone()).await?;
    let consumer = start(Some(3), false).await;
    connect(&consumer, &holder);
    let stalled = StallingZstdProvider::start(&data, &consumer).await?;

    let tracker = FetchTracker::new();
    let report = timeout(
        FETCH_TIMEOUT,
        consumer
            .blobs()
            .with_provider_idle_timeout(Some(Duration::from_millis(500)))
            .download_hash_to_store_from_tracked(
                &hash,
                &[stalled.id.clone(), holder.id()],
                BlobFormat::Raw,
                FetchStrategy::Whole,
                &tracker,
            ),
    )
    .await
    .expect("a stalled lead provider blocked the fetch")?;

    assert_eq!(consumer.blobs().read_to_bytes(&hash).await?, data);
    let expected_attempted = vec![stalled.id.to_string(), holder.id().to_string()];
    assert_eq!(ids(report.attempted()), expected_attempted);
    assert_eq!(ids(report.failed()), vec![stalled.id.to_string()]);
    assert_eq!(ids(report.served()), vec![holder.id().to_string()]);
    assert!(report.plain_only().next().is_none());
    assert!(
        stalled.accepted_zstd(),
        "the stalled provider was dialed over zstd"
    );
    assert!(
        consumer
            .blobs()
            .transport_compression_stats()
            .fetched_wire_bytes
            > 0
    );

    let tracked = tracker.report();
    assert_eq!(ids(tracked.attempted()), expected_attempted);
    assert_eq!(ids(tracked.failed()), vec![stalled.id.to_string()]);
    assert_eq!(ids(tracked.served()), vec![holder.id().to_string()]);
    assert_eq!(tracked.bytes_transferred(), report.bytes_transferred());

    stalled.shutdown().await?;
    holder.shutdown().await;
    consumer.shutdown().await;
    Ok(())
}

/// A zstd blobs provider whose transfers never progress: every throttle
/// request is held, with the connection alive. Serves the zstd ALPN only.
struct StallingZstdProvider {
    id: NodeId,
    router: aster::native::iroh::protocol::Router,
    gate: tokio::task::JoinHandle<()>,
    accepted: Arc<Mutex<bool>>,
}

impl StallingZstdProvider {
    async fn start(payload: &[u8], consumer: &Node) -> anyhow::Result<Self> {
        use aster::native::{
            iroh::{address_lookup::memory::MemoryLookup, endpoint::presets, protocol::Router},
            iroh_blobs::{
                provider::events::{EventMask, EventSender, ProviderMessage, ThrottleMode},
                store::mem::MemStore,
                transport::{zstd::ZstdTransform, TransformedBlobsProtocol},
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
                alpns::BLOBS_ZSTD,
                TransformedBlobsProtocol::new(
                    &store,
                    Some(events),
                    Arc::new(ZstdTransform::default()),
                ),
            )
            .spawn();
        let accepted = Arc::new(Mutex::new(false));
        let seen = accepted.clone();
        let gate = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Some(message) = messages.recv().await {
                *seen.lock().unwrap() = true;
                if let ProviderMessage::Throttle(message) = message {
                    held.push(message);
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
            accepted,
        })
    }

    fn accepted_zstd(&self) -> bool {
        *self.accepted.lock().unwrap()
    }

    async fn shutdown(self) -> anyhow::Result<()> {
        self.gate.abort();
        self.router.shutdown().await?;
        Ok(())
    }
}

// ── Gate 0 ──────────────────────────────────────────────────────────────────

/// The compressed ALPN is a data-plane ALPN: not in the admission allowlist,
/// so a protected gate refuses it to an unadmitted peer, as it does the plain
/// blobs ALPN.
#[test]
fn compressed_alpn_is_not_an_admission_alpn() {
    use aster_transport_core::trust::{default_admission_alpns, GateDecision, GatePolicy};
    assert_ne!(alpns::BLOBS_ZSTD, alpns::BLOBS);
    assert!(!default_admission_alpns()
        .iter()
        .any(|alpn| alpn.as_slice() == alpns::BLOBS_ZSTD));
    let peer = "ab".repeat(32);
    let policy = GatePolicy::protected();
    for alpn in [alpns::BLOBS, alpns::BLOBS_ZSTD] {
        assert!(matches!(
            policy.should_allow(&peer, alpn),
            GateDecision::Reject { .. }
        ));
    }
    let gate = Gate0::new();
    let peer = NodeId::from_hex(peer);
    assert!(!gate.should_allow(&peer, alpns::BLOBS_ZSTD));
    gate.admit(peer.clone());
    assert!(gate.should_allow(&peer, alpns::BLOBS_ZSTD));
}

/// A server whose post-handshake hook applies a protected [`Gate0`], and
/// records every ALPN it decided on.
async fn gated_server() -> (Node, Gate0, Arc<Mutex<Vec<(Vec<u8>, bool)>>>) {
    let server = start(Some(3), true).await;
    let gate = Gate0::new();
    let decisions = Arc::new(Mutex::new(Vec::new()));
    let mut admission = server.take_admission().expect("hooks enabled");
    let (g, d) = (gate.clone(), decisions.clone());
    tokio::spawn(async move {
        while let Some(req) = admission.next_handshake().await {
            let allow = g.should_allow(&req.peer, &req.alpn);
            d.lock().unwrap().push((req.alpn.clone(), allow));
            if allow {
                req.accept();
            } else {
                req.reject(403, b"not admitted".to_vec());
            }
        }
    });
    (server, gate, decisions)
}

/// An unadmitted peer is refused on the compressed ALPN exactly as on the
/// plain one, and the refusal is not mistaken for "provider does not serve
/// zstd": no plain fallback is reported. Once admitted, it fetches over zstd.
#[tokio::test]
async fn gate0_refuses_an_unadmitted_peer_on_the_compressed_alpn() -> anyhow::Result<()> {
    let (server, gate, decisions) = gated_server().await;
    let data = text();
    let hash = server.blobs().add_bytes(data.clone()).await?;
    let client = start(Some(3), false).await;
    connect(&client, &server);

    for mode in [
        BlobTransportCompression::RequireZstd,
        BlobTransportCompression::PreferZstd,
        BlobTransportCompression::Off,
    ] {
        let tracker = FetchTracker::new();
        let res = timeout(
            FETCH_TIMEOUT,
            client
                .blobs()
                .with_transport_compression(mode)
                .download_hash_to_store_from_tracked(
                    &hash,
                    &[server.id()],
                    BlobFormat::Raw,
                    FetchStrategy::Whole,
                    &tracker,
                ),
        )
        .await?;
        assert!(res.is_err(), "{mode:?}: unadmitted peer fetched: {res:?}");
        let report = tracker.report();
        assert_eq!(
            ids(report.failed()),
            vec![server.id().to_string()],
            "{mode:?}"
        );
        assert!(report.plain_only().next().is_none(), "{mode:?}");
    }
    assert!(!client.blobs().has(&hash).await?);
    {
        let seen = decisions.lock().unwrap();
        assert!(
            seen.contains(&(alpns::BLOBS_ZSTD.to_vec(), false)),
            "{seen:?}"
        );
        assert!(seen.contains(&(alpns::BLOBS.to_vec(), false)), "{seen:?}");
        assert!(seen.iter().all(|(_, allowed)| !allowed), "{seen:?}");
    }

    gate.admit(client.id());
    let report = timeout(
        FETCH_TIMEOUT,
        client
            .blobs()
            .with_transport_compression(BlobTransportCompression::RequireZstd)
            .download_hash_to_store_from(
                &hash,
                &[server.id()],
                BlobFormat::Raw,
                FetchStrategy::Whole,
            ),
    )
    .await??;
    assert_eq!(ids(report.served()), vec![server.id().to_string()]);
    assert_eq!(client.blobs().read_to_bytes(&hash).await?, data);
    assert!(decisions
        .lock()
        .unwrap()
        .contains(&(alpns::BLOBS_ZSTD.to_vec(), true)));

    client.shutdown().await;
    server.shutdown().await;
    Ok(())
}

/// Mirrors `admission::rejected_peer_cannot_download` on the compressed ALPN.
#[tokio::test]
async fn rejected_peer_cannot_download_compressed() {
    let server = start(Some(3), true).await;
    let mut adm = server.take_admission().expect("hooks enabled");
    tokio::spawn(async move {
        while let Some(req) = adm.next_handshake().await {
            req.reject(403, b"denied".to_vec());
        }
    });
    let hash = server
        .blobs()
        .add_bytes(b"secret payload".to_vec())
        .await
        .unwrap();
    let client = start(Some(3), false).await;
    connect(&client, &server);

    let res = timeout(
        Duration::from_secs(10),
        client
            .blobs()
            .with_transport_compression(BlobTransportCompression::RequireZstd)
            .download_hash(&hash, &server.id(), BlobFormat::Raw),
    )
    .await;
    if let Ok(Ok(bytes)) = res {
        panic!("rejected peer should not have downloaded: {bytes:?}");
    }

    client.shutdown().await;
    server.shutdown().await;
}
