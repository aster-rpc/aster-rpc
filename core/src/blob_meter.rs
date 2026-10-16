//! Blob transfer accounting: every blob transfer this node fetches or serves,
//! plain or zstd, node-wide and per peer.
//!
//! Three sources feed it, none of which needs a change to iroh-blobs:
//!
//! - **Fetch payload** comes from the downloader's progress stream. Each
//!   provider attempt reports `TryProvider`, an optional `PlainFallback`, one
//!   `BytesTransferred` (payload decoded by that attempt) and then
//!   `PartComplete` or `ProviderFailed`. The downloader emits one attempt's
//!   items contiguously, also when it splits a HashSeq into concurrent child
//!   requests (it drains each child's channel in turn), so `BytesTransferred`
//!   belongs to the provider of the latest `TryProvider`.
//! - **Serve payload** comes from provider events on both blob handlers:
//!   `ClientConnected` maps the connection to its peer, and each request's
//!   `TransferCompleted` / `TransferAborted` carries its payload bytes.
//! - **zstd wire bytes** come from the transforms' counters. The fetching side
//!   shares one transform across the downloaders, so its wire bytes are
//!   node-wide only. The serving side gets one transform per connection, so
//!   its wire bytes are also known per peer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::EndpointId;
use iroh_blobs::api::downloader::DownloadProgressItem;
use iroh_blobs::api::Store as BlobStore;
use iroh_blobs::provider::events::{
    ConnectMode, EventMask, EventSender, ProviderMessage, RequestMode, RequestUpdate,
};
use iroh_blobs::transport::zstd::{ZstdStats, ZstdStatsSnapshot, ZstdTransform};

use crate::CoreBlobCompression;

/// How a blob transfer crossed the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CoreBlobEncoding {
    /// The plain blobs ALPN.
    Plain,
    /// The zstd blobs ALPN ([`crate::BLOBS_ZSTD_ALPN`]).
    Zstd,
}

impl CoreBlobEncoding {
    /// Both encodings, plain first.
    pub const ALL: [Self; 2] = [Self::Plain, Self::Zstd];

    /// `"plain"` or `"zstd"`, the metric label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Zstd => "zstd",
        }
    }
}

/// Finished blob transfers by encoding and result.
///
/// A fetch transfer is one provider attempt (it ends in `PartComplete` or
/// `ProviderFailed`); a serve transfer is one request a peer made of this
/// node (it completes or is aborted).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreBlobTransferCounts {
    pub plain_ok: u64,
    pub plain_failed: u64,
    pub zstd_ok: u64,
    pub zstd_failed: u64,
}

impl CoreBlobTransferCounts {
    /// Transfers with `encoding` that succeeded (`ok`) or failed.
    pub fn get(&self, encoding: CoreBlobEncoding, ok: bool) -> u64 {
        match (encoding, ok) {
            (CoreBlobEncoding::Plain, true) => self.plain_ok,
            (CoreBlobEncoding::Plain, false) => self.plain_failed,
            (CoreBlobEncoding::Zstd, true) => self.zstd_ok,
            (CoreBlobEncoding::Zstd, false) => self.zstd_failed,
        }
    }

    fn bump(&mut self, encoding: CoreBlobEncoding, ok: bool) {
        let slot = match (encoding, ok) {
            (CoreBlobEncoding::Plain, true) => &mut self.plain_ok,
            (CoreBlobEncoding::Plain, false) => &mut self.plain_failed,
            (CoreBlobEncoding::Zstd, true) => &mut self.zstd_ok,
            (CoreBlobEncoding::Zstd, false) => &mut self.zstd_failed,
        };
        *slot += 1;
    }
}

/// Blob transfer counters for a whole node, since it started. Monotonic.
///
/// Payload is blob content as decoded, whatever the encoding: the coverage of
/// compression is `*_zstd_payload_bytes / *_payload_bytes`, and its saving is
/// `*_plain_bytes - *_wire_bytes` over the compressed subset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreBlobTransferStats {
    /// Payload bytes this node fetched, plain and zstd.
    pub fetched_payload_bytes: u64,
    /// The part of `fetched_payload_bytes` that came over zstd.
    pub fetched_zstd_payload_bytes: u64,
    /// Compressed bytes received while fetching over zstd.
    pub fetched_wire_bytes: u64,
    /// Protocol bytes those decompressed to (zstd payload plus a small
    /// protocol overhead).
    pub fetched_plain_bytes: u64,
    /// Payload bytes this node served, plain and zstd.
    pub served_payload_bytes: u64,
    /// The part of `served_payload_bytes` that went out over zstd.
    pub served_zstd_payload_bytes: u64,
    /// Compressed bytes sent while serving over zstd.
    pub served_wire_bytes: u64,
    /// Protocol bytes those were compressed from.
    pub served_plain_bytes: u64,
    /// Fetch attempts that went over plain transport because the provider does
    /// not serve zstd ([`CoreBlobCompression::PreferZstd`]).
    pub plain_fallbacks: u64,
    /// Requests this node served over plain transport.
    pub plain_serves: u64,
    /// Fetch attempts by encoding and result.
    pub fetch_transfers: CoreBlobTransferCounts,
    /// Served requests by encoding and result.
    pub serve_transfers: CoreBlobTransferCounts,
    /// Fetched zstd streams that failed on malformed or truncated zstd data
    /// (not on transport errors or content verification).
    pub fetch_decode_errors: u64,
    /// Served zstd streams that failed the same way (the peer's requests).
    pub serve_decode_errors: u64,
}

/// Blob transfer counters for one peer, since this node started and across
/// reconnects. Monotonic.
///
/// The zstd wire bytes of a fetch are not attributed to a provider: the
/// fetching side shares one zstd transform across all providers. The peer's
/// QUIC bytes on the `blobs-zstd` protocol are the closest per-peer figure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CorePeerBlobStats {
    /// Payload bytes fetched from this peer, plain and zstd.
    pub fetched_payload_bytes: u64,
    /// The part of `fetched_payload_bytes` that came over zstd.
    pub fetched_zstd_payload_bytes: u64,
    /// Payload bytes served to this peer, plain and zstd.
    pub served_payload_bytes: u64,
    /// The part of `served_payload_bytes` that went out over zstd.
    pub served_zstd_payload_bytes: u64,
    /// Compressed bytes sent to this peer while serving over zstd.
    pub served_wire_bytes: u64,
    /// Protocol bytes those were compressed from.
    pub served_plain_bytes: u64,
    /// Fetch attempts against this peer that fell back to plain transport.
    pub plain_fallbacks: u64,
}

fn add_snapshot(sum: &mut ZstdStatsSnapshot, s: &ZstdStatsSnapshot) {
    sum.wire_bytes_written += s.wire_bytes_written;
    sum.plain_bytes_written += s.plain_bytes_written;
    sum.wire_bytes_read += s.wire_bytes_read;
    sum.plain_bytes_read += s.plain_bytes_read;
    sum.decode_errors += s.decode_errors;
}

/// Index of `encoding` in the per-encoding arrays.
fn slot(encoding: CoreBlobEncoding) -> usize {
    match encoding {
        CoreBlobEncoding::Plain => 0,
        CoreBlobEncoding::Zstd => 1,
    }
}

#[derive(Debug, Default)]
struct PeerState {
    /// Payload bytes by encoding.
    fetched: [u64; 2],
    served: [u64; 2],
    plain_fallbacks: u64,
    /// zstd serve counters of this peer's connections that have finished.
    serve_zstd_closed: ZstdStatsSnapshot,
}

#[derive(Debug, Default)]
struct MeterState {
    fetched: [u64; 2],
    served: [u64; 2],
    plain_fallbacks: u64,
    fetch_transfers: CoreBlobTransferCounts,
    serve_transfers: CoreBlobTransferCounts,
    /// zstd serve counters of finished connections, node-wide.
    serve_zstd_closed: ZstdStatsSnapshot,
    /// zstd serve counters of connections some stream may still write to:
    /// while anything but this list holds the `Arc`, the counters can move.
    serve_zstd_live: Vec<(EndpointId, Arc<ZstdStats>)>,
    peers: HashMap<EndpointId, PeerState>,
}

impl MeterState {
    fn peer(&mut self, id: EndpointId) -> &mut PeerState {
        self.peers.entry(id).or_default()
    }

    /// Fold the counters of connections no stream holds any more into the
    /// closed totals. The sums stay the same, so readers never see a dip.
    fn fold_finished(&mut self) {
        let mut i = 0;
        while i < self.serve_zstd_live.len() {
            if Arc::strong_count(&self.serve_zstd_live[i].1) > 1 {
                i += 1;
                continue;
            }
            let (peer, stats) = self.serve_zstd_live.swap_remove(i);
            let snap = stats.snapshot();
            add_snapshot(&mut self.serve_zstd_closed, &snap);
            add_snapshot(&mut self.peer(peer).serve_zstd_closed, &snap);
        }
    }
}

/// Which provider (and encoding) the fetch's current attempt is using.
pub(crate) struct FetchAttribution {
    /// The downloader offers zstd first.
    zstd: bool,
    current: Option<(EndpointId, CoreBlobEncoding)>,
}

impl FetchAttribution {
    pub(crate) fn new(mode: CoreBlobCompression) -> Self {
        Self {
            zstd: mode != CoreBlobCompression::Off,
            current: None,
        }
    }
}

/// The node's blob transfer counters. One per node, shared by its blob
/// clients and both serving handlers.
#[derive(Debug)]
pub(crate) struct BlobMeter {
    /// The fetching side's zstd counters (one transform for all downloaders).
    fetch_zstd: Arc<ZstdStats>,
    state: Mutex<MeterState>,
}

impl BlobMeter {
    pub(crate) fn new(fetch_zstd: Arc<ZstdStats>) -> Arc<Self> {
        Arc::new(Self {
            fetch_zstd,
            state: Mutex::new(MeterState::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, MeterState> {
        // Plain counters: a panic mid-update cannot leave them unreadable.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fold one item of a download's progress stream into the counters.
    pub(crate) fn observe_fetch(&self, attr: &mut FetchAttribution, item: &DownloadProgressItem) {
        match item {
            DownloadProgressItem::TryProvider { id, .. } => {
                let encoding = if attr.zstd {
                    CoreBlobEncoding::Zstd
                } else {
                    CoreBlobEncoding::Plain
                };
                attr.current = Some((*id, encoding));
            }
            DownloadProgressItem::PlainFallback { id, .. } => {
                if let Some((current, encoding)) = &mut attr.current {
                    if current == id {
                        *encoding = CoreBlobEncoding::Plain;
                    }
                }
                let mut s = self.lock();
                s.plain_fallbacks += 1;
                s.peer(*id).plain_fallbacks += 1;
            }
            DownloadProgressItem::BytesTransferred(n) => {
                let Some((id, encoding)) = attr.current else {
                    return;
                };
                let mut s = self.lock();
                s.fetched[slot(encoding)] += n;
                s.peer(id).fetched[slot(encoding)] += n;
            }
            DownloadProgressItem::PartComplete { .. } => {
                if let Some((_, encoding)) = attr.current.take() {
                    self.lock().fetch_transfers.bump(encoding, true);
                }
            }
            DownloadProgressItem::ProviderFailed { id, .. } => {
                let encoding = match attr.current.take() {
                    Some((current, encoding)) if current == *id => encoding,
                    _ if attr.zstd => CoreBlobEncoding::Zstd,
                    _ => CoreBlobEncoding::Plain,
                };
                self.lock().fetch_transfers.bump(encoding, false);
            }
            DownloadProgressItem::Progress(_)
            | DownloadProgressItem::Error(_)
            | DownloadProgressItem::DownloadError => {}
        }
    }

    fn on_serve_update(
        &self,
        peer: Option<EndpointId>,
        encoding: CoreBlobEncoding,
        update: RequestUpdate,
    ) {
        let (stats, ok) = match update {
            RequestUpdate::Completed(c) => (c.stats, true),
            RequestUpdate::Aborted(a) => (a.stats, false),
            RequestUpdate::Started(_) | RequestUpdate::Progress(_) => return,
        };
        let n = stats.payload_bytes_sent;
        let mut s = self.lock();
        s.served[slot(encoding)] += n;
        s.serve_transfers.bump(encoding, ok);
        if let Some(peer) = peer {
            s.peer(peer).served[slot(encoding)] += n;
        }
    }

    /// Provider events for one serving handler: connection → peer, and each
    /// request's payload once it completes or aborts.
    ///
    /// Only what that needs is subscribed: connect notifications, and request
    /// logs (the only mode that reports transfer completion). Nothing is
    /// intercepted, so no request waits on this task.
    pub(crate) fn serve_events(self: &Arc<Self>, encoding: CoreBlobEncoding) -> EventSender {
        let mask = EventMask {
            connected: ConnectMode::Notify,
            get: RequestMode::NotifyLog,
            get_many: RequestMode::NotifyLog,
            ..EventMask::DEFAULT
        };
        let (sender, mut rx) = EventSender::channel(64, mask);
        let meter = self.clone();
        tokio::spawn(async move {
            let mut peers: HashMap<u64, EndpointId> = HashMap::new();
            // A request's updates arrive on their own channel, which must be
            // drained until the request ends: the provider waits for room to
            // report its start and its end.
            macro_rules! drain {
                ($connection_id:expr, $rx:expr) => {{
                    let peer = peers.get(&$connection_id).copied();
                    let meter = meter.clone();
                    let mut updates = $rx;
                    tokio::spawn(async move {
                        while let Ok(Some(update)) = updates.recv().await {
                            meter.on_serve_update(peer, encoding, update);
                        }
                    });
                }};
            }
            while let Some(msg) = rx.recv().await {
                match msg {
                    ProviderMessage::ClientConnectedNotify(msg) => {
                        if let Some(id) = msg.inner.endpoint_id {
                            peers.insert(msg.inner.connection_id, id);
                        }
                    }
                    ProviderMessage::ClientConnected(msg) => {
                        if let Some(id) = msg.inner.endpoint_id {
                            peers.insert(msg.inner.connection_id, id);
                        }
                        msg.tx.send(Ok(())).await.ok();
                    }
                    ProviderMessage::ConnectionClosed(msg) => {
                        peers.remove(&msg.inner.connection_id);
                    }
                    ProviderMessage::GetRequestReceivedNotify(msg) => {
                        drain!(msg.inner.connection_id, msg.rx)
                    }
                    ProviderMessage::GetManyRequestReceivedNotify(msg) => {
                        drain!(msg.inner.connection_id, msg.rx)
                    }
                    ProviderMessage::PushRequestReceivedNotify(msg) => {
                        drain!(msg.inner.connection_id, msg.rx)
                    }
                    ProviderMessage::ObserveRequestReceivedNotify(msg) => {
                        drain!(msg.inner.connection_id, msg.rx)
                    }
                    // Intercept variants are not subscribed; answer them as
                    // allowed should a future mask produce one.
                    ProviderMessage::GetRequestReceived(msg) => {
                        msg.tx.send(Ok(())).await.ok();
                        drain!(msg.inner.connection_id, msg.rx)
                    }
                    ProviderMessage::GetManyRequestReceived(msg) => {
                        msg.tx.send(Ok(())).await.ok();
                        drain!(msg.inner.connection_id, msg.rx)
                    }
                    ProviderMessage::PushRequestReceived(msg) => {
                        msg.tx.send(Ok(())).await.ok();
                        drain!(msg.inner.connection_id, msg.rx)
                    }
                    ProviderMessage::ObserveRequestReceived(msg) => {
                        msg.tx.send(Ok(())).await.ok();
                        drain!(msg.inner.connection_id, msg.rx)
                    }
                    ProviderMessage::Throttle(msg) => {
                        msg.tx.send(Ok(())).await.ok();
                    }
                }
            }
        });
        sender
    }

    /// Register a serving connection's zstd counters under its peer.
    fn track_serve_zstd(&self, peer: EndpointId, stats: Arc<ZstdStats>) {
        let mut s = self.lock();
        s.fold_finished();
        s.serve_zstd_live.push((peer, stats));
    }

    /// Node-wide counters.
    pub(crate) fn stats(&self) -> CoreBlobTransferStats {
        let fetch = self.fetch_zstd.snapshot();
        let mut s = self.lock();
        s.fold_finished();
        let mut serve = s.serve_zstd_closed;
        for (_, live) in &s.serve_zstd_live {
            add_snapshot(&mut serve, &live.snapshot());
        }
        let plain = slot(CoreBlobEncoding::Plain);
        let zstd = slot(CoreBlobEncoding::Zstd);
        CoreBlobTransferStats {
            fetched_payload_bytes: s.fetched[plain] + s.fetched[zstd],
            fetched_zstd_payload_bytes: s.fetched[zstd],
            fetched_wire_bytes: fetch.wire_bytes_read,
            fetched_plain_bytes: fetch.plain_bytes_read,
            served_payload_bytes: s.served[plain] + s.served[zstd],
            served_zstd_payload_bytes: s.served[zstd],
            served_wire_bytes: serve.wire_bytes_written,
            served_plain_bytes: serve.plain_bytes_written,
            plain_fallbacks: s.plain_fallbacks,
            plain_serves: s.serve_transfers.plain_ok + s.serve_transfers.plain_failed,
            fetch_transfers: s.fetch_transfers,
            serve_transfers: s.serve_transfers,
            fetch_decode_errors: fetch.decode_errors,
            serve_decode_errors: serve.decode_errors,
        }
    }

    /// One peer's counters; zero for a peer that never exchanged blobs.
    pub(crate) fn peer(&self, id: &EndpointId) -> CorePeerBlobStats {
        let s = self.lock();
        let mut serve = ZstdStatsSnapshot::default();
        for (peer, live) in &s.serve_zstd_live {
            if peer == id {
                add_snapshot(&mut serve, &live.snapshot());
            }
        }
        let Some(p) = s.peers.get(id) else {
            return CorePeerBlobStats {
                served_wire_bytes: serve.wire_bytes_written,
                served_plain_bytes: serve.plain_bytes_written,
                ..Default::default()
            };
        };
        add_snapshot(&mut serve, &p.serve_zstd_closed);
        let plain = slot(CoreBlobEncoding::Plain);
        let zstd = slot(CoreBlobEncoding::Zstd);
        CorePeerBlobStats {
            fetched_payload_bytes: p.fetched[plain] + p.fetched[zstd],
            fetched_zstd_payload_bytes: p.fetched[zstd],
            served_payload_bytes: p.served[plain] + p.served[zstd],
            served_zstd_payload_bytes: p.served[zstd],
            served_wire_bytes: serve.wire_bytes_written,
            served_plain_bytes: serve.plain_bytes_written,
            plain_fallbacks: p.plain_fallbacks,
        }
    }
}

/// Serves the blobs protocol over zstd with one transform per connection, so
/// the compressed bytes are counted per peer. Otherwise identical to
/// `TransformedBlobsProtocol`: same provider code, same events, and shutting it
/// down leaves the store to the plain handler.
#[derive(Debug, Clone)]
pub(crate) struct MeteredZstdBlobs {
    pub(crate) store: BlobStore,
    pub(crate) events: EventSender,
    pub(crate) level: i32,
    pub(crate) meter: Arc<BlobMeter>,
}

impl ProtocolHandler for MeteredZstdBlobs {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let transform = ZstdTransform::new(self.level);
        self.meter
            .track_serve_zstd(conn.remote_id(), transform.stats().clone());
        iroh_blobs::provider::handle_connection_with_transform(
            conn,
            self.store.clone(),
            self.events.clone(),
            Arc::new(transform),
        )
        .await;
        Ok(())
    }
}
