pub mod blob_custody;
pub use blob_custody::{CoreBlobCustody, CoreBlobCustodyError, CoreBlobCustodyLease};
#[cfg(feature = "attestation")]
pub mod attestation;
pub mod canonical;
pub mod contract;
pub mod datagram;
#[cfg(feature = "delegation")]
pub mod delegation;
pub mod dynamic;
#[cfg(feature = "enrollment")]
pub mod enrollment;
pub mod envelope;
pub mod framing;
pub mod grant_record;
pub mod grants;
pub mod hpke_envelope;
pub mod lease;
pub mod namespace;
pub mod pool;
pub mod reactor;
pub mod registry;
pub mod ring;
pub mod signing;
pub mod stream_io;
pub mod ticket;
pub mod topology;
pub mod trust;
pub mod tunnel;
pub mod xlang;

/// Exact Aster build version (`major.minor.commit-offset`) embedded at compile time.
pub const VERSION: &str = env!("ASTER_BUILD_VERSION");

pub use topology::cluster::{
    CoreClusterView, CoreSeparationVerdict, SharedView as CoreTopoSharedView, TopoConfig,
};
pub use topology::shared::{AdmittedFn, CoreTopoSwarm, CoreTopoSwarmConfig};
pub use topology::{CoreLadderLevel, CorePeerView};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{
    presets, AfterHandshakeOutcome, BeforeConnectOutcome, Closed, Connection, ConnectionError,
    Endpoint, EndpointHooks, IdleTimeout, PathEvent, PortmapperConfig, QuicTransportConfig,
    RelayMode, VarInt, WeakConnectionHandle,
};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{EndpointAddr, EndpointId, RelayMap, RelayUrl, SecretKey, TransportAddr};
use iroh_blobs::api::downloader::{
    DownloadOptions, DownloadProgressItem, Downloader, SplitStrategy,
};
use iroh_blobs::api::Store as BlobStore;
use iroh_blobs::format::collection::Collection;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::store::{gc_run_once as blobs_gc_run_once, GcConfig, ProtectCb, ProtectOutcome};
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{BlobFormat, BlobsProtocol, Hash, HashAndFormat, ALPN as BLOBS_ALPN};
use iroh_docs::api::protocol::{AddrInfoOptions, ShareMode};
use iroh_docs::api::Doc;
use iroh_docs::engine::{LiveEvent, ProtectCallbackHandler};
use iroh_docs::protocol::Docs;
use iroh_docs::store::{DownloadPolicy, FilterKind, Query};
use iroh_docs::{
    Author, AuthorId, Capability, CapabilityKind, DocTicket, NamespaceId, NamespaceSecret,
    RetireWriteAuthorityOutcome, ALPN as DOCS_ALPN,
};
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::Gossip;
use iroh_gossip::proto::TopicId;
use iroh_gossip::ALPN as GOSSIP_ALPN;
use iroh_mdns_address_lookup::MdnsAddressLookup;
use iroh_tickets::Ticket;
use std::sync::RwLock;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::framing::{encode_frame, FLAG_TUNNEL};
use crate::pool::{AcquireError, PoolConfig, PoolKey, StreamFactory, StreamHandle, StreamPool};
use crate::tunnel::{
    handle_http_relay, AdmissionState, AuthorizeTunnelError, LocalTargetRegistry,
    LocalTunnelAcceptor, TunnelConfig, TunnelRegistry, TunnelTarget, TunnelTicket,
    DEFAULT_MAX_STREAMS_PER_SERVICE, TUNNEL_SUBTYPE_HTTP_RELAY, TUNNEL_SUBTYPE_TICKET,
};
use tokio_stream::StreamExt;
use tracing::debug;
use url::Url;

// ============================================================================
// Core Types - FFI-safe wrappers
// ============================================================================

#[derive(Clone, Debug)]
pub struct CoreNodeAddr {
    pub endpoint_id: String,
    pub relay_url: Option<String>,
    pub direct_addresses: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CoreEndpointConfig {
    pub relay_mode: Option<String>,
    pub relay_urls: Vec<String>,
    pub alpns: Vec<Vec<u8>>,
    pub secret_key: Option<Vec<u8>>,
    pub enable_discovery: bool,
    /// Enable connection monitoring / remote-info tracking
    pub enable_monitoring: bool,
    /// Enable endpoint hooks (before_connect / after_handshake callbacks)
    pub enable_hooks: bool,
    /// Timeout in ms for hook replies (default 5000)
    pub hook_timeout_ms: u64,
    /// Post-handshake hook fallback when callback channel is closed or times out.
    /// `before_connect` falls open unless a low-level adapter is constructed
    /// with an explicit outbound failure mode.
    pub hook_failure_mode: trust::HookFailureMode,
    /// Bind address string e.g. "0.0.0.0:9000", "127.0.0.1:0", "`[::`]:0"
    pub bind_addr: Option<String>,
    /// Remove all direct IP (UDP/QUIC) transports; relay-only mode
    pub clear_ip_transports: bool,
    /// Remove all relay transports; direct IP-only mode
    pub clear_relay_transports: bool,
    /// Portmapper: "enabled" (default) or "disabled"
    pub portmapper_config: Option<String>,
    /// HTTP/SOCKS proxy URL for relay/HTTPS traffic e.g. "http://proxy:8080"
    pub proxy_url: Option<String>,
    /// Read proxy URL from HTTP_PROXY / HTTPS_PROXY environment variables
    pub proxy_from_env: bool,
    /// Maximum number of incoming bidirectional QUIC streams.
    pub transport_max_concurrent_bidi_streams: Option<u64>,
    /// Maximum number of incoming unidirectional QUIC streams.
    pub transport_max_concurrent_uni_streams: Option<u64>,
    /// Per-stream QUIC receive window in bytes.
    pub transport_stream_receive_window: Option<u64>,
    /// Per-connection QUIC receive window in bytes.
    pub transport_receive_window: Option<u64>,
    /// QUIC send window in bytes.
    pub transport_send_window: Option<u64>,
    /// QUIC idle timeout in milliseconds.
    pub transport_max_idle_timeout_ms: Option<u64>,
    /// QUIC keep-alive interval in milliseconds.
    pub transport_keep_alive_interval_ms: Option<u64>,
    /// Initial QUIC UDP payload size before MTU discovery.
    pub transport_initial_mtu: Option<u16>,
    /// Incoming QUIC application datagram buffer in bytes. Zero disables incoming datagrams.
    pub transport_datagram_receive_buffer_size: Option<usize>,
    /// Outgoing QUIC application datagram buffer in bytes.
    pub transport_datagram_send_buffer_size: Option<usize>,
    /// Whether to use fair scheduling across send streams at the same priority.
    pub transport_send_fairness: Option<bool>,
    /// Whether to use UDP generic segmentation offload when supported.
    pub transport_enable_segmentation_offload: Option<bool>,
    /// Node data directory for persistent state; empty = no persistent state
    pub data_dir: Option<String>,
}

impl Default for CoreEndpointConfig {
    fn default() -> Self {
        Self {
            relay_mode: None,
            relay_urls: Vec::new(),
            alpns: Vec::new(),
            secret_key: None,
            enable_discovery: false,
            enable_monitoring: false,
            enable_hooks: false,
            hook_timeout_ms: 5000,
            hook_failure_mode: trust::HookFailureMode::FailOpen,
            bind_addr: None,
            clear_ip_transports: false,
            clear_relay_transports: false,
            portmapper_config: None,
            proxy_url: None,
            proxy_from_env: false,
            transport_max_concurrent_bidi_streams: None,
            transport_max_concurrent_uni_streams: None,
            transport_stream_receive_window: None,
            transport_receive_window: None,
            transport_send_window: None,
            transport_max_idle_timeout_ms: None,
            transport_keep_alive_interval_ms: None,
            transport_initial_mtu: None,
            transport_datagram_receive_buffer_size: None,
            transport_datagram_send_buffer_size: None,
            transport_send_fairness: None,
            transport_enable_segmentation_offload: None,
            data_dir: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CoreClosedInfo {
    pub kind: String,
    pub code: Option<u64>,
    pub reason: Option<Vec<u8>>,
}

fn closed_info(closed: &ConnectionError) -> CoreClosedInfo {
    let (code, reason) = match closed {
        ConnectionError::ApplicationClosed(app) => {
            (Some(app.error_code.into_inner()), Some(app.reason.to_vec()))
        }
        _ => (None, Some(closed.to_string().into_bytes())),
    };
    CoreClosedInfo {
        kind: format!("{closed:?}"),
        code,
        reason,
    }
}

#[derive(Clone, Debug)]
pub struct CoreGossipEvent {
    pub event_type: String,
    pub data: Option<Vec<u8>>,
}

/// A document entry returned from queries, containing metadata about who wrote it and what they wrote.
#[derive(Clone, Debug)]
pub struct CoreDocEntry {
    /// The author who wrote this entry (hex string)
    pub author_id: String,
    /// The key this entry was written to
    pub key: Vec<u8>,
    /// The content hash of the value (hex string)
    pub content_hash: String,
    /// The content length in bytes
    pub content_len: u64,
    /// The timestamp when this entry was written (microseconds since epoch)
    pub timestamp: u64,
}

/// Download policy for a document. Controls which remote entries are automatically downloaded.
#[derive(Clone, Debug)]
pub enum CoreDownloadPolicy {
    /// Download all entries (default — maps to EverythingExcept with empty list).
    Everything,
    /// Download nothing except entries whose keys match one of the given byte prefixes.
    NothingExcept { prefixes: Vec<Vec<u8>> },
    /// Download everything except entries whose keys match one of the given byte prefixes.
    EverythingExcept { prefixes: Vec<Vec<u8>> },
}

fn core_policy_to_iroh(policy: CoreDownloadPolicy) -> DownloadPolicy {
    match policy {
        CoreDownloadPolicy::Everything => DownloadPolicy::EverythingExcept(vec![]),
        CoreDownloadPolicy::NothingExcept { prefixes } => DownloadPolicy::NothingExcept(
            prefixes
                .into_iter()
                .map(|p| FilterKind::Prefix(p.into()))
                .collect(),
        ),
        CoreDownloadPolicy::EverythingExcept { prefixes } => DownloadPolicy::EverythingExcept(
            prefixes
                .into_iter()
                .map(|p| FilterKind::Prefix(p.into()))
                .collect(),
        ),
    }
}

fn iroh_policy_to_core(policy: DownloadPolicy) -> CoreDownloadPolicy {
    match policy {
        DownloadPolicy::EverythingExcept(filters) if filters.is_empty() => {
            CoreDownloadPolicy::Everything
        }
        DownloadPolicy::EverythingExcept(filters) => CoreDownloadPolicy::EverythingExcept {
            prefixes: filters
                .into_iter()
                .map(|f| match f {
                    FilterKind::Prefix(b) => b.to_vec(),
                    FilterKind::Exact(b) => b.to_vec(),
                })
                .collect(),
        },
        DownloadPolicy::NothingExcept(filters) => CoreDownloadPolicy::NothingExcept {
            prefixes: filters
                .into_iter()
                .map(|f| match f {
                    FilterKind::Prefix(b) => b.to_vec(),
                    FilterKind::Exact(b) => b.to_vec(),
                })
                .collect(),
        },
    }
}

/// Snapshot of the local bitfield for a blob — how much data is locally available.
#[derive(Clone, Debug)]
pub struct CoreBlobObserveResult {
    /// Whether all chunks of the blob are present locally.
    pub is_complete: bool,
    /// Total blob size in bytes (0 if not yet known — header not fetched).
    pub size: u64,
}

/// Local availability info for a blob, from the Remote API.
#[derive(Clone, Debug)]
pub struct CoreBlobLocalInfo {
    /// Whether all requested data is present locally.
    pub is_complete: bool,
    /// Number of bytes we have locally for this blob.
    pub local_bytes: u64,
}

/// How a multi-provider fetch should decompose a HashSeq.
///
/// This is *not* about how many providers are used — ordered failover across
/// the provider list happens either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CoreFetchStrategy {
    /// Fetch the HashSeq as a single request.
    #[default]
    Whole,
    /// Fetch the root, then each child as its own concurrent request.
    SplitChildren,
}

/// What one provider did during a fetch.
#[derive(Clone, Debug)]
pub struct CoreProviderOutcome {
    /// The provider's endpoint id, hex-encoded.
    pub node_id: String,
    /// How many requests were attempted against this provider.
    pub attempts: u32,
    /// How many of those attempts failed to connect or transfer.
    pub failures: u32,
    /// How many requests this provider carried to completion.
    pub completed_requests: u32,
}

/// The outcome of a multi-provider fetch, for callers that rank providers.
#[derive(Clone, Debug, Default)]
pub struct CoreFetchReport {
    /// Providers in the order they were first attempted; repeated uses of one
    /// id are aggregated into a single entry.
    pub providers: Vec<CoreProviderOutcome>,
    /// Payload bytes pulled off the wire during this fetch: the sum of what
    /// each provider attempt successfully decoded. Excludes protocol overhead
    /// and data that was already resident locally. A provider that transferred
    /// part of a blob before failing is counted up to its last valid chunk and
    /// the next resumes from what is stored, so ordinary failover counts each
    /// byte once — but this is a transfer metric, not a content size: a range
    /// that was decoded and then failed to be stored is counted again when it
    /// is re-fetched.
    pub bytes_transferred: u64,
}

/// Folds a download's progress stream into a [`CoreFetchReport`].
///
/// Provider attribution is by request: `PartComplete` carries no provider id,
/// so it is credited to whichever provider that request last announced with
/// `TryProvider`. Requests are keyed by value (`GetRequest` is `Eq + Hash`),
/// not by allocation identity.
#[derive(Default)]
struct FetchAccumulator {
    order: Vec<EndpointId>,
    outcomes: HashMap<EndpointId, (u32, u32, u32)>,
    active: HashMap<Arc<iroh_blobs::protocol::GetRequest>, EndpointId>,
    bytes: u64,
    errors: Vec<String>,
    part_failed: bool,
}

impl FetchAccumulator {
    fn entry(&mut self, id: EndpointId) -> &mut (u32, u32, u32) {
        self.outcomes.entry(id).or_insert_with(|| {
            self.order.push(id);
            (0, 0, 0)
        })
    }

    fn observe(&mut self, item: DownloadProgressItem) {
        match item {
            DownloadProgressItem::TryProvider { id, request } => {
                self.entry(id).0 += 1;
                self.active.insert(request, id);
            }
            DownloadProgressItem::ProviderFailed { id, request } => {
                self.entry(id).1 += 1;
                self.active.remove(&request);
            }
            DownloadProgressItem::PartComplete { request } => {
                if let Some(id) = self.active.remove(&request) {
                    self.entry(id).2 += 1;
                }
            }
            DownloadProgressItem::BytesTransferred(n) => self.bytes += n,
            DownloadProgressItem::Error(err) => self.errors.push(err.to_string()),
            DownloadProgressItem::DownloadError => self.part_failed = true,
            DownloadProgressItem::Progress(_) => {}
        }
    }

    fn finish(self, hash: Hash) -> Result<CoreFetchReport> {
        let providers: Vec<CoreProviderOutcome> = self
            .order
            .iter()
            .map(|id| {
                let (attempts, failures, completed_requests) = self.outcomes[id];
                CoreProviderOutcome {
                    node_id: id.to_string(),
                    attempts,
                    failures,
                    completed_requests,
                }
            })
            .collect();
        if !self.errors.is_empty() || self.part_failed {
            let tried = providers
                .iter()
                .map(|p| p.node_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let failed = providers
                .iter()
                .filter(|p| p.failures > 0)
                .map(|p| p.node_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let detail = if self.errors.is_empty() {
                "one or more parts of the request could not be fetched".to_string()
            } else {
                self.errors.join("; ")
            };
            return Err(anyhow!(
                "download of {hash} failed: {detail} (tried: [{tried}], failed: [{failed}])"
            ));
        }
        Ok(CoreFetchReport {
            providers,
            bytes_transferred: self.bytes,
        })
    }
}

// ============================================================================
// Phase 1g: Transport Metrics (from iroh endpoint)
// ============================================================================

/// Snapshot of transport-layer metrics from the iroh endpoint.
///
/// These come from `endpoint.metrics()` which aggregates socket, net-report,
/// and portmapper metrics. All counters are monotonically increasing.
#[derive(Clone, Debug, Default)]
pub struct CoreTransportMetrics {
    // Socket layer
    pub send_ipv4: u64,
    pub send_ipv6: u64,
    pub send_relay: u64,
    pub recv_data_ipv4: u64,
    pub recv_data_ipv6: u64,
    pub recv_data_relay: u64,
    pub recv_datagrams: u64,
    pub num_conns_direct: u64,
    pub num_conns_opened: u64,
    pub num_conns_closed: u64,
    pub paths_direct: u64,
    pub paths_relay: u64,
    pub holepunch_attempts: u64,
    pub relay_home_change: u64,
    // Net report
    pub net_reports: u64,
    pub net_reports_full: u64,
}

// ============================================================================
// Phase 1b: Datagram Completion, Hooks & Monitoring Types
// ============================================================================

/// Information about a known remote endpoint
#[derive(Clone, Debug)]
pub struct CoreRemoteInfo {
    pub node_id: String,
    pub addr: Option<CoreNodeAddr>,
    pub relay_url: Option<String>,
    pub connection_type: ConnectionType,
    pub last_handshake_ns: Option<u64>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub is_connected: bool,
}

/// Type of connection to this peer
#[derive(Clone, Debug)]
pub enum ConnectionType {
    NotConnected,
    Connecting,
    Connected(ConnectionTypeDetail),
}

/// Detailed connection type
#[derive(Clone, Debug)]
pub enum ConnectionTypeDetail {
    UdpDirect,
    UdpRelay,
    Other(String),
}

/// Information about a connection (per-connection stats)
#[derive(Clone, Debug)]
pub struct CoreConnectionInfo {
    pub connection_type: ConnectionTypeDetail,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub rtt_ns: Option<u64>,
    pub alpn: Vec<u8>,
    pub is_connected: bool,
}

/// The remote address of a single network path.
#[derive(Clone, Debug)]
pub enum CorePathRemote {
    /// A direct UDP path to the peer's socket address.
    Direct(SocketAddr),
    /// A relay path via this relay URL. The peer's own IP is not visible.
    Relay(String),
    /// A custom or future transport; carries the debug representation.
    Other(String),
}

/// One network path of a connection at one instant. A live connection
/// can hold several paths simultaneously (typically a relay path plus a
/// direct path after holepunching succeeds).
#[derive(Clone, Debug)]
pub struct CorePathInfo {
    /// The remote address of this path.
    pub remote: CorePathRemote,
    /// Whether this path currently carries application data.
    pub is_selected: bool,
    /// RTT estimate for this path in microseconds. `None` if not yet measured.
    pub rtt_micros: Option<u64>,
}

/// Snapshot of the selected QUIC path for a connection at one instant.
/// Cheap to compute (clones a SmallVec entry) and intended for
/// per-call CallContext population. Exactly one of `peer_addr` /
/// `relay_url` is populated when a path is selected; both are `None`
/// only when the connection has no selected path (e.g. dropped).
#[derive(Clone, Debug, Default)]
pub struct CoreTransportSnapshot {
    /// Peer's UDP socket address when the selected path is direct.
    pub peer_addr: Option<SocketAddr>,
    /// Relay server URL when the selected path goes through a relay.
    /// The peer's own IP is not visible through a relay path.
    pub relay_url: Option<String>,
    /// RTT for the selected path, in microseconds. `None` if no path
    /// is selected or RTT has not yet been measured.
    pub rtt_micros: Option<u64>,
}

// ============================================================================
// Hooks Types (v0.97.0-compatible)
// ============================================================================

/// Information about a connect attempt, passed to hooks
#[derive(Clone, Debug)]
pub struct CoreHookConnectInfo {
    pub remote_endpoint_id: String,
    pub alpn: Vec<u8>,
}

/// Information about a completed handshake, passed to hooks
#[derive(Clone, Debug)]
pub struct CoreHookHandshakeInfo {
    pub remote_endpoint_id: String,
    pub alpn: Vec<u8>,
    pub is_alive: bool,
    /// Selected direct QUIC peer address, observed by the transport. Relayed,
    /// unknown and future paths do not supply an originating-peer address.
    #[cfg(feature = "enrollment")]
    pub observed_peer_addr: Option<SocketAddr>,
}

/// Decision for after_handshake hook
#[derive(Clone, Debug)]
pub enum CoreAfterHandshakeDecision {
    Accept,
    Reject { error_code: u32, reason: Vec<u8> },
}

/// Configuration for hook registration (kept for API compatibility)
#[derive(Clone, Debug, Default)]
pub struct CoreHookConfig {
    pub enable_before_connect: bool,
    pub enable_after_connect: bool,
    pub include_remote_info: bool,
    pub user_data: u64,
}

// ============================================================================
// Monitoring: RemoteMap (modeled after remote-info.rs example)
// ============================================================================

/// Aggregate information about a remote endpoint
#[derive(Clone, Debug)]
pub struct CoreRemoteAggregate {
    pub rtt_min: Duration,
    pub rtt_max: Duration,
    pub ip_path: bool,
    pub relay_path: bool,
    pub last_update: SystemTime,
    pub total_bytes_sent: u64,
    pub total_bytes_received: u64,
}

impl Default for CoreRemoteAggregate {
    fn default() -> Self {
        Self {
            rtt_min: Duration::MAX,
            rtt_max: Duration::ZERO,
            ip_path: false,
            relay_path: false,
            last_update: SystemTime::UNIX_EPOCH,
            total_bytes_sent: 0,
            total_bytes_received: 0,
        }
    }
}

impl CoreRemoteAggregate {
    fn update_from_path_stats(&mut self, addr: &TransportAddr, stats: &iroh::endpoint::PathStats) {
        self.last_update = SystemTime::now();
        if addr.is_ip() {
            self.ip_path = true;
        }
        if addr.is_relay() {
            self.relay_path = true;
        }
        if stats.rtt != Duration::ZERO {
            self.rtt_min = self.rtt_min.min(stats.rtt);
            self.rtt_max = self.rtt_max.max(stats.rtt);
        }
    }
}

/// Internal entry for each remote endpoint
#[derive(Debug, Default)]
struct RemoteInfoEntry {
    aggregate: CoreRemoteAggregate,
    connections: HashMap<u64, WeakConnectionHandle>,
    /// Derived topology-v1 state (ladder level, path quality, confidence),
    /// updated by the sampler spawned via [`CoreMonitor::spawn_sampler`].
    topo: topology::PeerTopoState,
}

impl RemoteInfoEntry {
    fn has_live_connection(&self) -> bool {
        self.connections.values().any(|h| h.upgrade().is_some())
    }
}

impl RemoteInfoEntry {
    fn to_core_remote_info(&self, node_id: &str) -> CoreRemoteInfo {
        // Upgrade weak handles to inspect current state.
        let live: Vec<Connection> = self
            .connections
            .values()
            .filter_map(WeakConnectionHandle::upgrade)
            .collect();

        let conn_type = if live.is_empty() {
            ConnectionType::NotConnected
        } else {
            let detail = live
                .iter()
                .find_map(|c| {
                    c.paths().iter().find(|p| p.is_selected()).map(|p| {
                        if p.is_relay() {
                            ConnectionTypeDetail::UdpRelay
                        } else if p.is_ip() {
                            ConnectionTypeDetail::UdpDirect
                        } else {
                            ConnectionTypeDetail::Other(format!("{:?}", p.remote_addr()))
                        }
                    })
                })
                .unwrap_or(ConnectionTypeDetail::Other("unknown".to_string()));
            ConnectionType::Connected(detail)
        };

        // Sum bytes from stats of currently-live connections.
        let (bytes_sent, bytes_received) = live.iter().fold((0u64, 0u64), |(s, r), c| {
            let stats = c.stats();
            (s + stats.udp_tx.bytes, r + stats.udp_rx.bytes)
        });

        CoreRemoteInfo {
            node_id: node_id.to_string(),
            addr: None, // Not tracked at this level
            relay_url: None,
            connection_type: conn_type,
            last_handshake_ns: Some(
                self.aggregate
                    .last_update
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .min(u64::MAX as u128) as u64,
            ),
            bytes_sent: bytes_sent.max(self.aggregate.total_bytes_sent),
            bytes_received: bytes_received.max(self.aggregate.total_bytes_received),
            is_connected: !live.is_empty(),
        }
    }
}

type RemoteMapInner = Arc<RwLock<HashMap<String, RemoteInfoEntry>>>;

/// Pairing of a connection's remote endpoint id (captured at handshake time)
/// with a weak handle to the connection. Modeled after the `remote-info.rs`
/// example in iroh 1.0.0-rc.0: storing a `WeakConnectionHandle` rather than a
/// strong `Connection` lets the monitor avoid keeping connections alive past
/// the application's last reference.
#[derive(Debug, Clone)]
struct TrackedConnection {
    remote_id: String,
    handle: WeakConnectionHandle,
}

/// Aborts the wrapped task when dropped.
#[derive(Debug)]
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Connection monitor that tracks remote endpoint information.
///
/// Implements `EndpointHooks` to capture a `WeakConnectionHandle` from
/// `after_handshake`, then spawns background tasks to watch path events and
/// connection close events.
#[derive(Clone, Debug)]
pub struct CoreMonitor {
    map: RemoteMapInner,
    #[allow(dead_code)]
    tx: mpsc::Sender<TrackedConnection>,
    _task: Arc<tokio::task::AbortHandle>,
    /// Sampler abort guard: the sampler task holds a strong `Endpoint`
    /// clone, so it must die with the last monitor handle or it would keep
    /// the endpoint alive after a plain drop (no explicit close()).
    sampler_guard: Arc<std::sync::Mutex<Option<AbortOnDrop>>>,
}

/// Hook portion of the monitor — installed on the endpoint builder.
#[derive(Debug)]
struct MonitorHook {
    tx: mpsc::Sender<TrackedConnection>,
}

impl EndpointHooks for MonitorHook {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let tracked = TrackedConnection {
            remote_id: conn.remote_id().to_string(),
            handle: conn.weak_handle(),
        };
        self.tx.send(tracked).await.ok();
        AfterHandshakeOutcome::Accept
    }
}

impl CoreMonitor {
    /// Create a new monitor. Returns `(hook, monitor)`.
    /// The hook must be installed on the endpoint builder.
    pub fn new() -> (impl EndpointHooks + 'static, Self) {
        let (tx, rx) = mpsc::channel(64);
        let map = RemoteMapInner::default();

        let task = tokio::spawn(Self::run(rx, map.clone()));
        let abort_handle = task.abort_handle();

        let hook = MonitorHook { tx: tx.clone() };
        let monitor = Self {
            map,
            tx,
            _task: Arc::new(abort_handle),
            sampler_guard: Arc::new(std::sync::Mutex::new(None)),
        };
        (hook, monitor)
    }

    async fn run(mut rx: mpsc::Receiver<TrackedConnection>, map: RemoteMapInner) {
        let mut conn_id: u64 = 0;
        let mut tasks = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                conn = rx.recv() => {
                    match conn {
                        Some(conn) => {
                            conn_id += 1;
                            Self::on_connection(&mut tasks, map.clone(), conn_id, conn);
                        }
                        None => break,
                    }
                }
                Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(e) = res {
                        if !e.is_cancelled() {
                            debug!("monitor task error: {e}");
                        }
                    }
                }
            }
        }

        // Drain remaining tasks
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                if !e.is_cancelled() {
                    debug!("monitor task error: {e}");
                }
            }
        }
    }

    fn on_connection(
        tasks: &mut tokio::task::JoinSet<()>,
        map: RemoteMapInner,
        conn_id: u64,
        tracked: TrackedConnection,
    ) {
        let remote_id = tracked.remote_id.clone();
        let handle = tracked.handle.clone();

        // Store the weak handle.
        {
            let mut inner = map.write().unwrap_or_else(|e| e.into_inner());
            let entry = inner.entry(remote_id.clone()).or_default();
            entry.connections.insert(conn_id, handle.clone());
            entry.aggregate.last_update = SystemTime::now();
        }

        // Track connection close. `WeakConnectionHandle::closed` does not keep
        // the connection alive but still resolves with final stats.
        tasks.spawn({
            let map = map.clone();
            let remote_id = remote_id.clone();
            let closed_fut = handle.closed();
            async move {
                let final_stats = closed_fut.await;
                let mut inner = map.write().unwrap_or_else(|e| e.into_inner());
                let entry = inner.entry(remote_id).or_default();
                entry.connections.remove(&conn_id);
                entry.aggregate.last_update = SystemTime::now();
                if let Some(Closed { stats, .. }) = final_stats {
                    entry.aggregate.total_bytes_sent += stats.udp_tx.bytes;
                    entry.aggregate.total_bytes_received += stats.udp_rx.bytes;
                }
            }
        });

        // Track path changes. We briefly upgrade to obtain a `PathEventStream`;
        // matching the upstream `remote-info.rs` example.
        if let Some(mut path_events) = handle.upgrade().map(|conn| conn.path_events()) {
            tasks.spawn({
                let map = map.clone();
                let handle = handle.clone();
                async move {
                    while let Some(event) = path_events.next().await {
                        let mut inner = map.write().unwrap_or_else(|e| e.into_inner());
                        let entry = inner.entry(remote_id.clone()).or_default();
                        match event {
                            PathEvent::Closed {
                                remote_addr,
                                last_stats,
                                ..
                            } => {
                                entry
                                    .aggregate
                                    .update_from_path_stats(&remote_addr, &last_stats);
                            }
                            _ => {
                                if let Some(conn) = handle.upgrade() {
                                    for path in conn.paths().iter() {
                                        let stats = path.stats();
                                        entry
                                            .aggregate
                                            .update_from_path_stats(path.remote_addr(), &stats);
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }
    }

    /// Query information about a specific remote endpoint.
    pub fn remote_info(&self, node_id: &str) -> Option<CoreRemoteInfo> {
        let inner = self.map.read().unwrap_or_else(|e| e.into_inner());
        inner
            .get(node_id)
            .map(|entry| entry.to_core_remote_info(node_id))
    }

    /// Get all known remote endpoints.
    pub fn remote_info_iter(&self) -> Vec<CoreRemoteInfo> {
        let inner = self.map.read().unwrap_or_else(|e| e.into_inner());
        inner
            .iter()
            .map(|(id, entry)| entry.to_core_remote_info(id))
            .collect()
    }

    /// Topology-v1 snapshot for one peer. `None` when the peer is unknown
    /// or has never been sampled.
    pub fn peer_view(&self, node_id: &str) -> Option<CorePeerView> {
        let inner = self.map.read().unwrap_or_else(|e| e.into_inner());
        let entry = inner.get(node_id)?;
        entry.topo.to_view(node_id, entry.has_live_connection())
    }

    /// Topology-v1 snapshots for every known peer (sampled at least once).
    pub fn peer_views(&self) -> Vec<CorePeerView> {
        let inner = self.map.read().unwrap_or_else(|e| e.into_inner());
        inner
            .iter()
            .filter_map(|(id, entry)| entry.topo.to_view(id, entry.has_live_connection()))
            .collect()
    }

    /// Start the topology sampler: every [`topology::SAMPLE_INTERVAL`] it
    /// folds live-connection stats into per-peer derived state. Called once
    /// per endpoint right after bind; the task exits when the endpoint
    /// closes, and is aborted when the last monitor handle drops (it holds
    /// a strong `Endpoint` clone, which must not outlive its owners).
    /// Purely passive — reads counters off connections the application
    /// already holds, never dials.
    pub(crate) fn spawn_sampler(&self, endpoint: Endpoint) {
        let map = self.map.clone();
        let task = tokio::spawn(async move {
            let closed_ep = endpoint.clone();
            let closed = closed_ep.closed();
            tokio::pin!(closed);
            let mut ticker = tokio::time::interval(topology::SAMPLE_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        let local_public: Vec<std::net::IpAddr> = endpoint
                            .addr()
                            .ip_addrs()
                            .map(|sa| sa.ip())
                            .filter(|ip| {
                                topology::classify_ip(*ip) == topology::AddrClass::Public
                            })
                            .collect();
                        Self::sample_all(&map, &local_public);
                    }
                    _ = &mut closed => break,
                }
            }
        });
        let guard = AbortOnDrop(task.abort_handle());
        *self.sampler_guard.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard);
    }

    /// One sampler tick: for every known peer, sum monotonic counters
    /// across *all* live connections (a peer often holds several — RPC,
    /// docs, gossip — and the busy one is what path quality should
    /// reflect), take the path shape from the best selected path (direct
    /// preferred, then lowest measured RTT), and fold the result into the
    /// peer's topology state.
    fn sample_all(map: &RemoteMapInner, local_public_ips: &[std::net::IpAddr]) {
        let mut inner = map.write().unwrap_or_else(|e| e.into_inner());
        for (_id, entry) in inner.iter_mut() {
            // Fingerprint of the live connection set: counter sums are only
            // comparable while the same set is alive.
            let mut conn_key = 0u64;
            let mut lost_packets = 0u64;
            let mut congestion_events = 0u64;
            let mut tx_datagrams = 0u64;
            let mut tx_bytes = 0u64;
            let mut rx_bytes = 0u64;
            // Representative selected path: direct beats relay, then lowest
            // measured RTT.
            let mut best: Option<(TransportAddr, Duration, u64)> = None;
            let mut any_live = false;

            for (conn_id, handle) in entry.connections.iter() {
                let Some(conn) = handle.upgrade() else {
                    continue;
                };
                any_live = true;
                conn_key ^= conn_id.wrapping_mul(0x9E37_79B9_7F4A_7C15);

                let conn_stats = conn.stats();
                lost_packets += conn_stats.lost_packets;
                tx_datagrams += conn_stats.udp_tx.datagrams;
                tx_bytes += conn_stats.udp_tx.bytes;
                rx_bytes += conn_stats.udp_rx.bytes;

                let paths = conn.paths();
                let Some(path) = paths.iter().find(|p| p.is_selected()) else {
                    continue;
                };
                let stats = path.stats();
                congestion_events += stats.congestion_events;

                let better = match &best {
                    None => true,
                    Some((cur_remote, cur_rtt, _)) => {
                        let cand_direct = path.remote_addr().is_ip();
                        let cur_direct = cur_remote.is_ip();
                        if cand_direct != cur_direct {
                            cand_direct
                        } else {
                            !stats.rtt.is_zero() && (cur_rtt.is_zero() || stats.rtt < *cur_rtt)
                        }
                    }
                };
                if better {
                    best = Some((path.remote_addr().clone(), stats.rtt, stats.cwnd));
                }
            }

            if !any_live {
                continue;
            }
            let Some((remote, rtt, cwnd)) = best else {
                continue;
            };
            entry.topo.apply_sample(topology::PeerSample {
                conn_key,
                remote,
                rtt,
                lost_packets,
                congestion_events,
                cwnd,
                tx_datagrams,
                tx_bytes,
                rx_bytes,
                local_public_ips: local_public_ips.to_vec(),
            });
        }
    }
}

// ============================================================================
// Hooks Adapter (v0.97.0-compatible builder-time hooks)
// ============================================================================

/// Adapter that implements `EndpointHooks` and forwards events through channels.
///
/// This bridges iroh's builder-time hooks to a channel-based model where
/// FFI/Python callers can receive hook events and respond asynchronously.
#[derive(Debug)]
pub struct CoreHooksAdapter {
    before_connect_tx: mpsc::Sender<(CoreHookConnectInfo, oneshot::Sender<bool>)>,
    after_handshake_tx: mpsc::Sender<(
        CoreHookHandshakeInfo,
        oneshot::Sender<CoreAfterHandshakeDecision>,
    )>,
    timeout: Duration,
    before_connect_failure_mode: trust::HookFailureMode,
    after_handshake_failure_mode: trust::HookFailureMode,
}

/// Receiver side for hook events. Stored in `CoreNetClient`.
pub struct CoreHookReceiver {
    pub before_connect_rx: mpsc::Receiver<(CoreHookConnectInfo, oneshot::Sender<bool>)>,
    pub after_handshake_rx: mpsc::Receiver<(
        CoreHookHandshakeInfo,
        oneshot::Sender<CoreAfterHandshakeDecision>,
    )>,
}

impl CoreHooksAdapter {
    /// Create a new hooks adapter. Returns `(adapter, receiver)`.
    /// The adapter is installed on the endpoint builder.
    /// The receiver is consumed by the FFI layer to forward events.
    pub fn new(timeout_ms: u64) -> (Self, CoreHookReceiver) {
        Self::new_with_failure_mode(timeout_ms, trust::HookFailureMode::FailOpen)
    }

    /// Create a new hooks adapter with explicit inbound fallback behavior.
    /// Outbound `before_connect` still falls open by default because common
    /// Gate-0 loops only drain `after_handshake`.
    pub fn new_with_failure_mode(
        timeout_ms: u64,
        failure_mode: trust::HookFailureMode,
    ) -> (Self, CoreHookReceiver) {
        Self::new_with_failure_modes(timeout_ms, trust::HookFailureMode::FailOpen, failure_mode)
    }

    /// Create a new hooks adapter with explicit fallback behavior for each hook.
    pub fn new_with_failure_modes(
        timeout_ms: u64,
        before_connect_failure_mode: trust::HookFailureMode,
        after_handshake_failure_mode: trust::HookFailureMode,
    ) -> (Self, CoreHookReceiver) {
        let (bc_tx, bc_rx) = mpsc::channel(16);
        let (ah_tx, ah_rx) = mpsc::channel(16);
        let adapter = Self {
            before_connect_tx: bc_tx,
            after_handshake_tx: ah_tx,
            timeout: Duration::from_millis(timeout_ms),
            before_connect_failure_mode,
            after_handshake_failure_mode,
        };
        let receiver = CoreHookReceiver {
            before_connect_rx: bc_rx,
            after_handshake_rx: ah_rx,
        };
        (adapter, receiver)
    }

    fn before_connect_fallback(&self) -> BeforeConnectOutcome {
        match self.before_connect_failure_mode {
            trust::HookFailureMode::FailOpen => BeforeConnectOutcome::Accept,
            trust::HookFailureMode::FailClosed => BeforeConnectOutcome::Reject,
        }
    }

    fn after_handshake_fallback(&self) -> AfterHandshakeOutcome {
        match self.after_handshake_failure_mode {
            trust::HookFailureMode::FailOpen => AfterHandshakeOutcome::Accept,
            trust::HookFailureMode::FailClosed => AfterHandshakeOutcome::Reject {
                error_code: VarInt::from_u32(403),
                reason: b"hook unavailable".to_vec(),
            },
        }
    }
}

fn selected_transport_snapshot(connection: &Connection) -> CoreTransportSnapshot {
    let paths = connection.paths();
    let Some(path) = paths.iter().find(|p| p.is_selected()) else {
        return CoreTransportSnapshot::default();
    };
    let (peer_addr, relay_url) = match path.remote_addr() {
        TransportAddr::Ip(socket) => (Some(*socket), None),
        TransportAddr::Relay(url) => (None, Some(url.to_string())),
        // Custom or future transports: surface RTT but no addr/url.
        _ => (None, None),
    };
    let rtt = path.rtt();
    let rtt_micros = if rtt == Duration::ZERO {
        None
    } else {
        Some(rtt.as_micros().min(u64::MAX as u128) as u64)
    };
    CoreTransportSnapshot {
        peer_addr,
        relay_url,
        rtt_micros,
    }
}

impl EndpointHooks for CoreHooksAdapter {
    async fn before_connect<'a>(
        &'a self,
        remote_addr: &'a EndpointAddr,
        alpn: &'a [u8],
    ) -> BeforeConnectOutcome {
        let info = CoreHookConnectInfo {
            remote_endpoint_id: remote_addr.id.to_string(),
            alpn: alpn.to_vec(),
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.before_connect_tx.send((info, reply_tx)).await.is_err() {
            return self.before_connect_fallback();
        }
        match tokio::time::timeout(self.timeout, reply_rx).await {
            Ok(Ok(true)) => BeforeConnectOutcome::Accept,
            Ok(Ok(false)) => BeforeConnectOutcome::Reject,
            _ => self.before_connect_fallback(),
        }
    }

    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let info = CoreHookHandshakeInfo {
            remote_endpoint_id: conn.remote_id().to_string(),
            alpn: conn.alpn().to_vec(),
            is_alive: conn.close_reason().is_none(),
            #[cfg(feature = "enrollment")]
            observed_peer_addr: selected_transport_snapshot(conn).peer_addr,
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .after_handshake_tx
            .send((info, reply_tx))
            .await
            .is_err()
        {
            return self.after_handshake_fallback();
        }
        match tokio::time::timeout(self.timeout, reply_rx).await {
            Ok(Ok(CoreAfterHandshakeDecision::Accept)) => AfterHandshakeOutcome::Accept,
            Ok(Ok(CoreAfterHandshakeDecision::Reject { error_code, reason })) => {
                AfterHandshakeOutcome::Reject {
                    error_code: VarInt::from_u32(error_code),
                    reason,
                }
            }
            _ => self.after_handshake_fallback(),
        }
    }
}

// ============================================================================
// Utility Functions
// ============================================================================

fn u64_to_varint(value: u64) -> Result<VarInt> {
    VarInt::try_from(value).map_err(Into::into)
}

pub fn endpoint_addr_to_core(addr: EndpointAddr) -> CoreNodeAddr {
    CoreNodeAddr {
        endpoint_id: addr.id.to_string(),
        relay_url: addr.relay_urls().next().map(|url| url.to_string()),
        direct_addresses: addr.ip_addrs().map(|addr| addr.to_string()).collect(),
    }
}

pub fn core_to_endpoint_addr(addr: &CoreNodeAddr) -> Result<EndpointAddr> {
    let id: EndpointId = addr.endpoint_id.parse()?;
    let mut addrs: Vec<TransportAddr> = addr
        .direct_addresses
        .iter()
        .map(|addr| addr.parse::<SocketAddr>().map(TransportAddr::Ip))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if let Some(relay_url) = &addr.relay_url {
        addrs.push(TransportAddr::Relay(relay_url.parse::<RelayUrl>()?));
    }
    Ok(EndpointAddr::from_parts(id, addrs))
}

fn relay_mode_from_config(config: &CoreEndpointConfig) -> Result<RelayMode> {
    match config.relay_mode.as_deref() {
        None | Some("default") => Ok(RelayMode::Default),
        Some("disabled") => Ok(RelayMode::Disabled),
        Some("staging") => Ok(RelayMode::Staging),
        Some("custom") if !config.relay_urls.is_empty() => {
            let urls: Vec<&str> = config.relay_urls.iter().map(|s| s.as_str()).collect();
            let relay_map =
                RelayMap::try_from_iter(urls).map_err(|e| anyhow!("invalid relay_urls: {e}"))?;
            Ok(RelayMode::Custom(relay_map))
        }
        Some("custom") if config.relay_urls.is_empty() => {
            Err(anyhow!("custom relay_mode requires at least one relay_url"))
        }
        Some(other) => Err(anyhow!("unsupported relay_mode: {other}")),
    }
}

fn u64_to_varint_field(field: &str, value: u64) -> Result<VarInt> {
    VarInt::try_from(value).map_err(|_| anyhow!("{field} must be less than 2^62"))
}

fn idle_timeout_from_millis(field: &str, value: u64) -> Result<IdleTimeout> {
    if value == 0 {
        return Err(anyhow!("{field} must be greater than zero"));
    }
    IdleTimeout::try_from(Duration::from_millis(value))
        .map_err(|_| anyhow!("{field} is too large for a QUIC idle timeout"))
}

fn positive_duration_from_millis(field: &str, value: u64) -> Result<Duration> {
    if value == 0 {
        return Err(anyhow!("{field} must be greater than zero"));
    }
    Ok(Duration::from_millis(value))
}

fn build_quic_transport_config(config: &CoreEndpointConfig) -> Result<Option<QuicTransportConfig>> {
    let mut builder = QuicTransportConfig::builder();
    let mut configured = false;

    if let Some(value) = config.transport_max_concurrent_bidi_streams {
        configured = true;
        builder = builder.max_concurrent_bidi_streams(u64_to_varint_field(
            "transport_max_concurrent_bidi_streams",
            value,
        )?);
    }

    if let Some(value) = config.transport_max_concurrent_uni_streams {
        configured = true;
        builder = builder.max_concurrent_uni_streams(u64_to_varint_field(
            "transport_max_concurrent_uni_streams",
            value,
        )?);
    }

    if let Some(value) = config.transport_stream_receive_window {
        configured = true;
        builder = builder.stream_receive_window(u64_to_varint_field(
            "transport_stream_receive_window",
            value,
        )?);
    }

    if let Some(value) = config.transport_receive_window {
        configured = true;
        builder = builder.receive_window(u64_to_varint_field("transport_receive_window", value)?);
    }

    if let Some(value) = config.transport_send_window {
        configured = true;
        builder = builder.send_window(value);
    }

    if let Some(value) = config.transport_max_idle_timeout_ms {
        configured = true;
        builder = builder.max_idle_timeout(Some(idle_timeout_from_millis(
            "transport_max_idle_timeout_ms",
            value,
        )?));
    }

    if let Some(value) = config.transport_keep_alive_interval_ms {
        configured = true;
        builder = builder.keep_alive_interval(positive_duration_from_millis(
            "transport_keep_alive_interval_ms",
            value,
        )?);
    }

    if let Some(value) = config.transport_initial_mtu {
        configured = true;
        if value < 1200 {
            return Err(anyhow!("transport_initial_mtu must be at least 1200"));
        }
        builder = builder.initial_mtu(value);
    }

    if let Some(value) = config.transport_datagram_receive_buffer_size {
        configured = true;
        builder = builder.datagram_receive_buffer_size(if value == 0 { None } else { Some(value) });
    }

    if let Some(value) = config.transport_datagram_send_buffer_size {
        configured = true;
        builder = builder.datagram_send_buffer_size(value);
    }

    if let Some(value) = config.transport_send_fairness {
        configured = true;
        builder = builder.send_fairness(value);
    }

    if let Some(value) = config.transport_enable_segmentation_offload {
        configured = true;
        builder = builder.enable_segmentation_offload(value);
    }

    Ok(configured.then(|| builder.build()))
}

fn build_endpoint_config(config: &CoreEndpointConfig) -> Result<iroh::endpoint::Builder> {
    let relay_mode = relay_mode_from_config(config)?;
    let mut builder = Endpoint::builder(presets::N0)
        .alpns(config.alpns.clone())
        .relay_mode(relay_mode);

    if let Some(transport_config) = build_quic_transport_config(config)? {
        builder = builder.transport_config(transport_config);
    }

    if let Some(ref secret_key) = config.secret_key {
        let bytes: [u8; 32] = secret_key
            .clone()
            .try_into()
            .map_err(|_| anyhow!("secret_key must be exactly 32 bytes"))?;
        builder = builder.secret_key(SecretKey::from_bytes(&bytes));
    }

    if config.clear_ip_transports {
        builder = builder.clear_ip_transports();
    }

    if config.clear_relay_transports {
        builder = builder.clear_relay_transports();
    }

    if let Some(ref addr) = config.bind_addr {
        builder = builder
            .bind_addr(addr.as_str())
            .map_err(|e| anyhow!("invalid bind_addr '{}': {}", addr, e))?;
    }

    match config.portmapper_config.as_deref() {
        None | Some("enabled") => {}
        Some("disabled") => {
            builder = builder.portmapper_config(PortmapperConfig::Disabled);
        }
        Some(other) => return Err(anyhow!("unsupported portmapper_config value: {other}")),
    }

    if let Some(ref url_str) = config.proxy_url {
        let url: Url = url_str
            .parse()
            .map_err(|e| anyhow!("invalid proxy_url '{}': {}", url_str, e))?;
        builder = builder.proxy_url(url);
    } else if config.proxy_from_env {
        builder = builder.proxy_from_env();
    }

    Ok(builder)
}

// ============================================================================
// CoreNode - Full iroh node with all protocols + custom Aster ALPN support
// ============================================================================

/// `ProtocolHandler` that forwards accepted connections on a given ALPN to a
/// shared bounded mpsc channel. One instance is registered with the iroh
/// `Router` per custom ALPN the node should listen on; they all share a
/// single sender so the consumer sees a unified stream of
/// `(alpn, Connection)` tuples via [`CoreNode::accept_aster`].
///
/// `iroh::endpoint::Connection` is `Arc`-backed `Clone`; `ProtocolHandler::accept`
/// drops its reference once the future returns, so the clone we place on the
/// channel is what keeps the connection alive until the consumer takes it.
#[derive(Debug, Clone)]
struct AsterQueueHandler {
    alpn: Vec<u8>,
    tx: mpsc::Sender<(Vec<u8>, Connection)>,
}

impl ProtocolHandler for AsterQueueHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        // Bounded channel — if full, back-pressure blocks this per-ALPN
        // handler task but leaves the Router's central accept loop and
        // other protocol handlers (blobs/docs/gossip) free.
        let _ = self.tx.send((self.alpn.clone(), conn)).await;
        Ok(())
    }
}

/// Internal capacity for the aster-accept queue. Generous for admission +
/// RPC; if an aster consumer wedges, back-pressure only hits the per-ALPN
/// Router task, not blobs/docs/gossip.
const ASTER_QUEUE_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct CoreNode {
    inner: Arc<CoreNodeInner>,
}

struct CoreNodeInner {
    #[cfg(feature = "enrollment")]
    enrollment_hook_failure_mode: trust::HookFailureMode,
    endpoint: Endpoint,
    router: Router,
    #[allow(dead_code)]
    blobs: BlobsProtocol,
    docs: Docs,
    gossip: Gossip,
    store: BlobStore,
    downloader: Downloader,
    /// GC protection callback bridging the docs engine to the blob store: when
    /// invoked it inserts every hash referenced by an open replica into the GC
    /// `live` set (aborting the sweep on any enumeration error). Wired into the
    /// periodic GC loop via `GcConfig.add_protected` *and* replayed by
    /// [`CoreBlobsClient::gc_run_once`], since the manual `gc_run_once` entry
    /// point does not invoke the callback itself. Without it, the first sweep
    /// would reclaim docs content (root policies, grants, manifests) — which
    /// iroh-docs stores as untagged blobs in this same shared store.
    protect_cb: ProtectCb,
    secret_key_bytes: Vec<u8>,
    /// Receiver half of the aster-ALPN queue. Wrapped in a tokio Mutex so
    /// `accept_aster(&self)` can pull from it across clones. Internal to
    /// Rust — never crosses the FFI boundary.
    aster_rx: Mutex<mpsc::Receiver<(Vec<u8>, Connection)>>,
    /// Connection monitor (populated if enable_monitoring=true on the
    /// endpoint config passed to a `_with_alpns` constructor).
    monitor: Option<CoreMonitor>,
    /// Hooks receiver — present iff enable_hooks=true. Same shape as
    /// [`CoreNetClient::hook_receiver`]; takeable once via
    /// [`CoreNode::take_hook_receiver`].
    hook_receiver: Option<Arc<std::sync::Mutex<Option<CoreHookReceiver>>>>,
    /// Joined topology swarm (v2), set via
    /// [`CoreNode::topology_join_swarm`]. The paired guard owns the
    /// publisher task: dropping this slot (close, re-join) aborts the
    /// publisher even while external `Arc`s to the swarm survive.
    topo_swarm: RwLock<TopologyRegistration>,
}

/// The close marker and registration share one lock, so an asynchronous join
/// cannot install a publisher after close has retired the previous publisher.
#[derive(Default)]
struct TopologyRegistration {
    closed: bool,
    swarm: Option<(Arc<CoreTopoSwarm>, AbortOnDrop)>,
}

impl TopologyRegistration {
    fn install(&mut self, swarm: Arc<CoreTopoSwarm>, guard: AbortOnDrop) -> Result<()> {
        if self.closed {
            // Dropping guard aborts a publisher whose startup raced close.
            return Err(anyhow!("cannot join topology on a closed node"));
        }
        self.swarm = Some((swarm, guard));
        Ok(())
    }

    fn close(&mut self) {
        self.closed = true;
        self.swarm = None;
    }
}

/// Build the iroh `Endpoint` for a `CoreNode`, optionally applying a
/// `CoreEndpointConfig` so `enable_hooks` / `enable_monitoring` /
/// `secret_key` / `relay_mode` / `bind_addr` etc. work on a full node the
/// same way they work on a bare `CoreNetClient`. Returns the bound
/// endpoint plus the monitor + hook-receiver channels the caller needs to
/// expose to the host. The `alpns` field on the config is ignored — iroh's
/// `Router::spawn()` overwrites `endpoint.set_alpns(...)` with the union of
/// all registered protocol ALPNs.
async fn build_node_endpoint(
    config: Option<CoreEndpointConfig>,
) -> Result<(
    Endpoint,
    Option<CoreMonitor>,
    Option<Arc<std::sync::Mutex<Option<CoreHookReceiver>>>>,
)> {
    match config {
        None => {
            let endpoint = Endpoint::bind(presets::N0).await?;
            endpoint.online().await;
            Ok((endpoint, None, None))
        }
        Some(config) => {
            let relay_mode = relay_mode_from_config(&config)?;
            let mut builder = build_endpoint_config(&config)?;

            let mut monitor = None;
            let mut hook_receiver = None;

            if config.enable_monitoring {
                let (hook, mon) = CoreMonitor::new();
                builder = builder.hooks(hook);
                monitor = Some(mon);
            }
            if config.enable_hooks {
                let (adapter, receiver) = CoreHooksAdapter::new_with_failure_mode(
                    config.hook_timeout_ms,
                    config.hook_failure_mode,
                );
                builder = builder.hooks(adapter);
                hook_receiver = Some(Arc::new(std::sync::Mutex::new(Some(receiver))));
            }

            let endpoint = builder.bind().await?;

            if let Some(mon) = &monitor {
                mon.spawn_sampler(endpoint.clone());
            }

            if config.enable_discovery {
                let mdns = MdnsAddressLookup::builder()
                    .build(endpoint.id())
                    .map_err(|e| anyhow!("mDNS init failed: {e}"))?;
                endpoint.address_lookup()?.add(mdns);
            }

            if !matches!(relay_mode, RelayMode::Disabled) {
                endpoint.online().await;
            }
            Ok((endpoint, monitor, hook_receiver))
        }
    }
}

/// A node's default docs [`Author`] — its own identity key, so `AuthorId == NodeId`.
///
/// Aster uses the node's endpoint key as the docs author: a doc entry's author is
/// exactly the node that wrote it, directly verifiable against the attestation
/// chain and the QUIC handshake, with no separate author↔node binding to publish.
///
/// Consequence: on a persistent node this writes the node secret into the docs
/// author store (`docs.redb`). Aster never surfaces author export (neither `core`
/// nor any binding wraps iroh-docs `author_export`), so the secret cannot be
/// extracted via the docs API; the only sanctioned way to obtain the node
/// identity is [`CoreNode::export_secret_key`].
fn node_docs_author(node_secret: &[u8; 32]) -> Author {
    Author::from_bytes(node_secret)
}

/// The default-docs `AuthorId` (hex) for a node with this secret — equals the
/// node id, since Aster uses the node identity key as the docs author.
pub fn default_docs_author_id(node_secret: &[u8; 32]) -> String {
    node_docs_author(node_secret).id().to_string()
}

/// Restrict a persistent node's data directory to the owner (`0700`).
///
/// The directory holds secret-bearing state — notably `docs.redb`, whose
/// authors table stores the node identity secret (see [`node_docs_author`]). A
/// `0700` directory blocks other local users from traversing into it regardless
/// of individual file modes, which is the primary protection for a node used as
/// a trust store.
///
/// No-op on non-Unix targets: Windows uses ACLs rather than mode bits, so a
/// caller needing private storage there must set directory ACLs externally.
#[cfg(unix)]
fn harden_dir_private(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn harden_dir_private(_dir: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Restrict an existing file to owner read/write (`0600`). Silently skips a file
/// that does not exist yet. No-op on non-Unix targets (see
/// [`harden_dir_private`]).
#[cfg(unix)]
fn harden_file_private(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn harden_file_private(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

impl CoreNode {
    pub async fn memory() -> Result<Self> {
        Self::memory_with_alpns(Vec::new(), None).await
    }

    /// In-memory node that serves blobs + docs + gossip AND accepts connections
    /// on each entry in `aster_alpns`. When provided, `endpoint_config`'s
    /// `enable_hooks` / `enable_monitoring` / `secret_key` / `relay_mode` /
    /// `bind_addr` / `clear_*_transports` / `portmapper_config` settings are
    /// applied to the endpoint builder (via the same path as
    /// [`CoreNetClient::create_with_config`]). The `alpns` field on the config
    /// is ignored here — the iroh `Router`'s `accept` registrations drive the
    /// endpoint's ALPN list on `spawn()` (see iroh-0.97.0 protocol.rs:429).
    pub async fn memory_with_alpns(
        aster_alpns: Vec<Vec<u8>>,
        endpoint_config: Option<CoreEndpointConfig>,
    ) -> Result<Self> {
        Self::memory_with_alpns_and_gc(aster_alpns, endpoint_config, None).await
    }

    /// Like [`Self::memory_with_alpns`], but opts the in-memory blob store into
    /// periodic garbage collection at `gc_interval`.
    ///
    /// `None` (the default taken by [`Self::memory_with_alpns`]) leaves the
    /// store grow-only: tags control retention but untagged blobs are never
    /// reclaimed. `Some(interval)` spawns the iroh-blobs GC loop, which sweeps
    /// every `interval`, collecting any blob no tag (or temp-tag) protects.
    /// Tags remain the sole retention root either way; see
    /// [`CoreBlobsClient::gc_run_once`] for a deterministic manual sweep.
    pub async fn memory_with_alpns_and_gc(
        aster_alpns: Vec<Vec<u8>>,
        endpoint_config: Option<CoreEndpointConfig>,
        gc_interval: Option<Duration>,
    ) -> Result<Self> {
        let hook_failure_mode = endpoint_config
            .as_ref()
            .map(|config| config.hook_failure_mode)
            .unwrap_or_default();
        let (endpoint, monitor, hook_receiver) = build_node_endpoint(endpoint_config).await?;
        // The docs engine stores entry content as untagged blobs in this shared
        // store, so GC must protect the docs live-content set or the first sweep
        // deletes synced policy/grant/manifest content. Create the handler/cb
        // pair unconditionally: the handler is wired into the docs engine in
        // `finalize`, and the cb is replayed by manual `gc_run_once` even when
        // the periodic loop is off.
        let (protect_handler, protect_cb) = ProtectCallbackHandler::new();
        let mem_store = match gc_interval {
            Some(interval) => MemStore::new_with_opts(iroh_blobs::store::mem::Options {
                gc_config: Some(GcConfig {
                    interval,
                    add_protected: Some(protect_cb.clone()),
                }),
            }),
            None => MemStore::new(),
        };
        let store: BlobStore = (*mem_store).clone();
        Self::finalize(
            endpoint,
            store,
            None,
            aster_alpns,
            monitor,
            (hook_receiver, hook_failure_mode),
            (protect_handler, protect_cb),
        )
        .await
    }

    pub async fn persistent(path: String) -> Result<Self> {
        Self::persistent_with_alpns(path, Vec::new(), None).await
    }

    /// Persistent (FsStore-backed) counterpart to [`Self::memory_with_alpns`].
    /// The FsStore is loaded from `path` exactly as [`Self::persistent`] does.
    pub async fn persistent_with_alpns(
        path: String,
        aster_alpns: Vec<Vec<u8>>,
        endpoint_config: Option<CoreEndpointConfig>,
    ) -> Result<Self> {
        Self::persistent_with_alpns_and_gc(path, aster_alpns, endpoint_config, None).await
    }

    /// Like [`Self::persistent_with_alpns`], but opts the FsStore into periodic
    /// garbage collection at `gc_interval`. See
    /// [`Self::memory_with_alpns_and_gc`] for the semantics; `None` preserves
    /// the grow-only default of [`Self::persistent_with_alpns`].
    pub async fn persistent_with_alpns_and_gc(
        path: String,
        aster_alpns: Vec<Vec<u8>>,
        endpoint_config: Option<CoreEndpointConfig>,
        gc_interval: Option<Duration>,
    ) -> Result<Self> {
        let hook_failure_mode = endpoint_config
            .as_ref()
            .map(|config| config.hook_failure_mode)
            .unwrap_or_default();
        let (endpoint, monitor, hook_receiver) = build_node_endpoint(endpoint_config).await?;
        let root = std::path::PathBuf::from(&path);
        // Create the data directory privately *before* the stores write secret
        // material (notably the node secret in `docs.redb`) into it.
        std::fs::create_dir_all(&root)?;
        harden_dir_private(&root)?;
        // See `memory_with_alpns_and_gc`: protect the docs live-content set so
        // GC never reclaims the untagged blobs backing docs entries.
        let (protect_handler, protect_cb) = ProtectCallbackHandler::new();
        let fs_store = match gc_interval {
            Some(interval) => {
                // Mirror `FsStore::load`'s db path, then overlay the GC option.
                let mut options = iroh_blobs::store::fs::options::Options::new(&root);
                options.gc = Some(GcConfig {
                    interval,
                    add_protected: Some(protect_cb.clone()),
                });
                FsStore::load_with_opts(root.join("blobs.db"), options).await?
            }
            None => FsStore::load(&root).await?,
        };
        let store: BlobStore = fs_store.into();
        Self::finalize(
            endpoint,
            store,
            Some(root),
            aster_alpns,
            monitor,
            (hook_receiver, hook_failure_mode),
            (protect_handler, protect_cb),
        )
        .await
    }

    /// Shared tail of both constructors: wire blobs/docs/gossip protocols +
    /// one `AsterQueueHandler` per entry in `aster_alpns` onto a Router, then
    /// assemble `CoreNodeInner`.
    ///
    /// `docs_root` mirrors the store backing: `Some(path)` for a persistent
    /// node uses `Docs::persistent(path)` so namespaces/authors/entries survive
    /// restart (`docs.redb` + `default-author` live alongside the blob
    /// `blobs.db` in the same directory); `None` for a memory node uses
    /// `Docs::memory()`.
    async fn finalize(
        endpoint: Endpoint,
        store: BlobStore,
        docs_root: Option<std::path::PathBuf>,
        aster_alpns: Vec<Vec<u8>>,
        monitor: Option<CoreMonitor>,
        hooks: (
            Option<Arc<std::sync::Mutex<Option<CoreHookReceiver>>>>,
            trust::HookFailureMode,
        ),
        // The docs GC protection bridge, created together by
        // `ProtectCallbackHandler::new()`: the handler half drives the docs
        // engine, the callback half is stored for manual `gc_run_once` sweeps.
        gc_protect: (ProtectCallbackHandler, ProtectCb),
    ) -> Result<Self> {
        let (hook_receiver, _hook_failure_mode) = hooks;
        let (protect_handler, protect_cb) = gc_protect;
        let downloader = Downloader::new(&store, &endpoint);
        let blobs = BlobsProtocol::new(&store, None);
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let docs_builder = match &docs_root {
            Some(path) => Docs::persistent(path.clone()),
            None => Docs::memory(),
        };
        // `protect_handler` is the docs-engine half of the GC protection bridge:
        // it answers the `protect_cb` with the engine's current live-content
        // hashes so a sweep never collects blobs still referenced by a doc.
        let docs = docs_builder
            .protect_handler(protect_handler)
            .spawn(endpoint.clone(), store.clone(), gossip.clone())
            .await?;

        // Default docs author = the node's own identity key, so `AuthorId == NodeId`
        // — a doc entry's author is exactly the node that wrote it (free, direct
        // attribution). Idempotent across restarts. See [`node_docs_author`] for
        // the on-disk-secret consequence and why author export is not surfaced.
        {
            let author = node_docs_author(&endpoint.secret_key().to_bytes());
            let author_id = author.id();
            docs.api().author_import(author).await?;
            docs.api().author_set_default(author_id).await?;
        }

        // The author import above persisted the node secret into `docs.redb`.
        // Restrict it (and the default-author pointer) to the owner as
        // defense-in-depth behind the `0700` data directory.
        if let Some(path) = &docs_root {
            harden_file_private(&path.join("docs.redb"))?;
            harden_file_private(&path.join("default-author"))?;
        }

        let (aster_tx, aster_rx) = mpsc::channel::<(Vec<u8>, Connection)>(ASTER_QUEUE_CAPACITY);

        let mut router_builder = Router::builder(endpoint.clone())
            .accept(BLOBS_ALPN, blobs.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(DOCS_ALPN, docs.clone());
        for alpn in &aster_alpns {
            router_builder = router_builder.accept(
                alpn.clone(),
                AsterQueueHandler {
                    alpn: alpn.clone(),
                    tx: aster_tx.clone(),
                },
            );
        }
        // Drop the extra sender we created via .clone() above; only the
        // handlers registered on the Router should keep the channel alive.
        drop(aster_tx);
        let router = router_builder.spawn();

        let secret_key_bytes = endpoint.secret_key().to_bytes().to_vec();

        Ok(Self {
            inner: Arc::new(CoreNodeInner {
                #[cfg(feature = "enrollment")]
                enrollment_hook_failure_mode: _hook_failure_mode,
                endpoint,
                router,
                blobs,
                docs,
                gossip,
                store,
                downloader,
                protect_cb,
                secret_key_bytes,
                aster_rx: Mutex::new(aster_rx),
                monitor,
                hook_receiver,
                topo_swarm: RwLock::new(TopologyRegistration::default()),
            }),
        })
    }

    pub fn node_id(&self) -> String {
        self.inner.endpoint.id().to_string()
    }
    pub fn node_addr_info(&self) -> CoreNodeAddr {
        endpoint_addr_to_core(self.inner.endpoint.addr())
    }
    pub fn node_addr_debug(&self) -> String {
        format!("{:?}", self.inner.endpoint.addr())
    }
    pub async fn close(&self) {
        // Drop the topology swarm first: its publisher task ticks for as
        // long as a swarm handle exists, and a caller that keeps this
        // CoreNode around after close() would otherwise leave it writing
        // against a shut-down docs engine.
        self.inner
            .topo_swarm
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .close();
        // Flush blob-store metadata to disk while the store actor is still
        // alive. `router.shutdown()` below shuts the store down (via
        // `BlobsProtocol::shutdown()` -> `store.shutdown()`), so a `sync_db()`
        // afterwards would race a dead actor and silently skip the flush. For a
        // persistent FsStore this guarantees durability of tags/entries added
        // before close; for a MemStore it is a harmless no-op.
        if let Err(err) = self.inner.store.sync_db().await {
            debug!("CoreNode::close: store.sync_db error: {err}");
        }
        // router.shutdown() drains protocol handlers (so in-flight
        // AsterQueueHandler::accept futures resolve), drops handlers (closing
        // the aster channel), stops the docs engine, shuts the blob store down,
        // then closes the endpoint.
        if let Err(err) = self.inner.router.shutdown().await {
            debug!("CoreNode::close: router.shutdown join error: {err}");
        }
    }

    /// Wait for the next incoming connection on any registered aster ALPN.
    /// Returns `(alpn_bytes, connection)`. Returns Err once the node is
    /// closed (all `AsterQueueHandler` senders dropped).
    pub async fn accept_aster(&self) -> Result<(Vec<u8>, CoreConnection)> {
        let mut rx = self.inner.aster_rx.lock().await;
        match rx.recv().await {
            Some((alpn, conn)) => Ok((alpn, CoreConnection::new(conn))),
            None => Err(anyhow!("aster accept channel closed")),
        }
    }

    /// Take the hooks receiver (one-shot). Returns `None` when the node was
    /// built without `enable_hooks=true` or the receiver was already taken.
    pub fn take_hook_receiver(&self) -> Option<CoreHookReceiver> {
        self.inner.hook_receiver.as_ref()?.lock().ok()?.take()
    }

    /// Whether this node has hooks wired (i.e. built via a constructor that
    /// was given an `endpoint_config` with `enable_hooks=true`).
    pub fn has_hooks(&self) -> bool {
        self.inner.hook_receiver.is_some()
    }

    pub fn export_secret_key(&self) -> Vec<u8> {
        self.inner.secret_key_bytes.clone()
    }

    pub fn add_node_addr(&self, other: &CoreNode) -> Result<()> {
        let addr = other.inner.endpoint.addr();
        let memory_lookup = MemoryLookup::new();
        memory_lookup.add_endpoint_info(addr);
        self.inner.endpoint.address_lookup()?.add(memory_lookup);
        Ok(())
    }

    /// Register address material for an arbitrary peer (id + relay + direct
    /// addresses) in this node's address lookup, so subsequent connects and
    /// docs joins addressed by node id can reach it without discovery. Unlike
    /// [`Self::add_node_addr`], the peer need not be a local `CoreNode`.
    pub fn add_node_addr_info(&self, addr: CoreNodeAddr) -> Result<()> {
        let endpoint_addr = core_to_endpoint_addr(&addr)?;
        let memory_lookup = MemoryLookup::new();
        memory_lookup.add_endpoint_info(endpoint_addr);
        self.inner.endpoint.address_lookup()?.add(memory_lookup);
        Ok(())
    }

    pub fn blobs_client(&self) -> CoreBlobsClient {
        CoreBlobsClient {
            store: self.inner.store.clone(),
            endpoint: self.inner.endpoint.clone(),
            downloader: self.inner.downloader.clone(),
            protect_cb: self.inner.protect_cb.clone(),
        }
    }
    pub fn docs_client(&self) -> CoreDocsClient {
        CoreDocsClient {
            inner: self.inner.docs.clone(),
            store: self.inner.store.clone(),
            endpoint: self.inner.endpoint.clone(),
        }
    }
    pub fn gossip_client(&self) -> CoreGossipClient {
        CoreGossipClient {
            inner: self.inner.gossip.clone(),
        }
    }
    pub fn net_client(&self) -> CoreNetClient {
        CoreNetClient {
            endpoint: self.inner.endpoint.clone(),
            secret_key_bytes: self.inner.secret_key_bytes.clone(),
            monitor: self.inner.monitor.clone(),
            hook_receiver: None,
        }
    }

    /// Whether this node was built with `enable_monitoring=true` (the
    /// prerequisite for the topology view).
    pub fn has_monitoring(&self) -> bool {
        self.inner.monitor.is_some()
    }

    /// Topology-v1 snapshot for one peer. `None` when monitoring is
    /// disabled, the peer is unknown, or it has never been sampled.
    pub fn peer_view(&self, node_id: &str) -> Option<CorePeerView> {
        self.inner.monitor.as_ref()?.peer_view(node_id)
    }

    /// Topology-v1 snapshots for every known peer. Empty when monitoring
    /// is disabled.
    pub fn peer_views(&self) -> Vec<CorePeerView> {
        self.inner
            .monitor
            .as_ref()
            .map(|m| m.peer_views())
            .unwrap_or_default()
    }

    /// Join a topology swarm (topology v2): import the shared namespace
    /// from its 32-byte secret, start publishing this node's measurements,
    /// and derive the swarm-wide cluster view. Requires monitoring
    /// (`enable_monitoring=true`) — the published edges come from the v1
    /// sampler. Idempotent-ish: joining again replaces the previous swarm
    /// handle.
    pub async fn topology_join_swarm(
        &self,
        secret: &[u8],
        config: CoreTopoSwarmConfig,
    ) -> Result<Arc<CoreTopoSwarm>> {
        if self
            .inner
            .topo_swarm
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .closed
        {
            return Err(anyhow!("cannot join topology on a closed node"));
        }
        let monitor = self
            .inner
            .monitor
            .clone()
            .ok_or_else(|| anyhow!("topology swarm requires monitoring (enable_monitoring)"))?;
        let namespace_id = topology::shared::namespace_id_for_secret(secret)?;
        let doc = self
            .docs_client()
            .import_write_namespace(secret.to_vec())
            .await?;
        let (swarm, publisher_guard) = CoreTopoSwarm::start(
            doc,
            namespace_id,
            self.node_id(),
            monitor,
            self.inner.endpoint.clone(),
            config,
        )
        .await?;
        self.inner
            .topo_swarm
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .install(swarm.clone(), publisher_guard)?;
        Ok(swarm)
    }

    /// The joined topology swarm, if [`Self::topology_join_swarm`] was
    /// called. The returned handle is query-only in lifetime terms: the
    /// publisher task belongs to the node's registration and stops on
    /// [`close`](Self::close) or re-join regardless of outstanding clones.
    pub fn topology_swarm(&self) -> Option<Arc<CoreTopoSwarm>> {
        self.inner
            .topo_swarm
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .swarm
            .as_ref()
            .map(|(swarm, _)| swarm.clone())
    }

    /// A snapshot of the iroh endpoint's byte and connection counters, for
    /// observable OTel instruments (`aster.transport.*`).
    pub fn transport_counters(&self) -> TransportCounters {
        let m = self.inner.endpoint.metrics();
        let s = &m.socket;
        TransportCounters {
            send_ipv4: s.send_ipv4.get(),
            send_ipv6: s.send_ipv6.get(),
            send_relay: s.send_relay.get(),
            recv_ipv4: s.recv_data_ipv4.get(),
            recv_ipv6: s.recv_data_ipv6.get(),
            recv_relay: s.recv_data_relay.get(),
            conns_opened: s.num_conns_opened.get(),
            conns_closed: s.num_conns_closed.get(),
        }
    }
}

/// The iroh endpoint's cumulative byte and connection counters
/// ([`CoreNode::transport_counters`]). Every field counts since the endpoint
/// started; connections count only completed handshakes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransportCounters {
    pub send_ipv4: u64,
    pub send_ipv6: u64,
    pub send_relay: u64,
    pub recv_ipv4: u64,
    pub recv_ipv6: u64,
    pub recv_relay: u64,
    pub conns_opened: u64,
    pub conns_closed: u64,
}

// ============================================================================
// CoreNetClient - QUIC endpoint client
// ============================================================================

#[derive(Clone)]
pub struct CoreNetClient {
    pub endpoint: Endpoint,
    secret_key_bytes: Vec<u8>,
    /// Connection monitor for remote-info tracking (populated if enable_monitoring=true)
    monitor: Option<CoreMonitor>,
    /// Hook receiver is NOT Clone — it's consumed by the FFI/Python layer.
    /// We store it wrapped in Arc<Mutex<Option<...>>> so it can be taken once.
    hook_receiver: Option<Arc<std::sync::Mutex<Option<CoreHookReceiver>>>>,
}

impl CoreNetClient {
    pub async fn create(alpn: Vec<u8>) -> Result<Self> {
        let endpoint = Endpoint::builder(presets::N0)
            .alpns(vec![alpn])
            .bind()
            .await?;
        endpoint.online().await;
        let secret_key_bytes = endpoint.secret_key().to_bytes().to_vec();
        Ok(Self {
            endpoint,
            secret_key_bytes,
            monitor: None,
            hook_receiver: None,
        })
    }

    pub async fn create_with_config(config: CoreEndpointConfig) -> Result<Self> {
        let relay_mode = relay_mode_from_config(&config)?;
        let mut builder = build_endpoint_config(&config)?;

        let mut monitor = None;
        let mut hook_receiver = None;

        // Install monitoring hook if requested
        if config.enable_monitoring {
            let (hook, mon) = CoreMonitor::new();
            builder = builder.hooks(hook);
            monitor = Some(mon);
        }

        // Install hooks adapter if requested
        if config.enable_hooks {
            let (adapter, receiver) = CoreHooksAdapter::new_with_failure_mode(
                config.hook_timeout_ms,
                config.hook_failure_mode,
            );
            builder = builder.hooks(adapter);
            hook_receiver = Some(Arc::new(std::sync::Mutex::new(Some(receiver))));
        }

        let endpoint = builder.bind().await?;

        if let Some(mon) = &monitor {
            mon.spawn_sampler(endpoint.clone());
        }

        if config.enable_discovery {
            let mdns = MdnsAddressLookup::builder()
                .build(endpoint.id())
                .map_err(|e| anyhow!("mDNS init failed: {e}"))?;
            endpoint.address_lookup()?.add(mdns);
        }

        if !matches!(relay_mode, RelayMode::Disabled) {
            endpoint.online().await;
        }
        let secret_key_bytes = endpoint.secret_key().to_bytes().to_vec();
        Ok(Self {
            endpoint,
            secret_key_bytes,
            monitor,
            hook_receiver,
        })
    }

    pub async fn connect(&self, node_id: String, alpn: Vec<u8>) -> Result<CoreConnection> {
        let id: EndpointId = node_id.parse()?;
        let conn = self.endpoint.connect(id, &alpn).await?;
        Ok(CoreConnection::new(conn))
    }

    pub async fn connect_node_addr(
        &self,
        addr: CoreNodeAddr,
        alpn: Vec<u8>,
    ) -> Result<CoreConnection> {
        let conn = self
            .endpoint
            .connect(core_to_endpoint_addr(&addr)?, &alpn)
            .await?;
        Ok(CoreConnection::new(conn))
    }

    pub async fn accept(&self) -> Result<CoreConnection> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| anyhow!("endpoint closed, no incoming connection"))?;
        let conn = incoming.accept()?.await?;
        Ok(CoreConnection::new(conn))
    }

    pub fn endpoint_id(&self) -> String {
        self.endpoint.id().to_string()
    }
    pub fn endpoint_addr_debug(&self) -> String {
        format!("{:?}", self.endpoint.addr())
    }
    pub fn endpoint_addr_info(&self) -> CoreNodeAddr {
        endpoint_addr_to_core(self.endpoint.addr())
    }
    pub async fn close(&self) {
        self.endpoint.close().await;
    }
    pub async fn closed(&self) {
        self.endpoint.closed().await;
    }

    pub fn export_secret_key(&self) -> Vec<u8> {
        self.secret_key_bytes.clone()
    }

    // ============================================================================
    // Phase 1b: Remote-Info & Monitoring (real implementation)
    // ============================================================================

    /// Query information about a specific known remote endpoint.
    ///
    /// Returns `Some(info)` if monitoring is enabled and the remote is known.
    /// Returns `None` if monitoring is disabled or the remote is unknown.
    pub fn remote_info(&self, node_id: &str) -> Option<CoreRemoteInfo> {
        self.monitor.as_ref()?.remote_info(node_id)
    }

    /// Get information about all known remote endpoints.
    ///
    /// Returns an empty vec if monitoring is disabled.
    pub fn remote_info_iter(&self) -> Vec<CoreRemoteInfo> {
        self.monitor
            .as_ref()
            .map(|m| m.remote_info_iter())
            .unwrap_or_default()
    }

    /// Returns whether monitoring is enabled for this endpoint.
    pub fn has_monitoring(&self) -> bool {
        self.monitor.is_some()
    }

    /// Topology-v1 snapshot for one peer. `None` when monitoring is
    /// disabled, the peer is unknown, or it has never been sampled.
    pub fn peer_view(&self, node_id: &str) -> Option<CorePeerView> {
        self.monitor.as_ref()?.peer_view(node_id)
    }

    /// Topology-v1 snapshots for every known peer. Empty when monitoring
    /// is disabled.
    pub fn peer_views(&self) -> Vec<CorePeerView> {
        self.monitor
            .as_ref()
            .map(|m| m.peer_views())
            .unwrap_or_default()
    }

    // ============================================================================
    // Phase 1b: Hooks
    // ============================================================================

    /// Take the hook receiver (can only be called once).
    /// Returns `None` if hooks are not enabled or the receiver was already taken.
    pub fn take_hook_receiver(&self) -> Option<CoreHookReceiver> {
        self.hook_receiver.as_ref()?.lock().ok()?.take()
    }

    /// Returns whether hooks are enabled for this endpoint.
    pub fn has_hooks(&self) -> bool {
        self.hook_receiver.is_some()
    }

    // ============================================================================
    // Phase 1g: Transport Metrics
    // ============================================================================

    /// Snapshot current transport metrics from the iroh endpoint.
    ///
    /// Reads counters from `endpoint.metrics()` which includes socket-level
    /// send/recv stats, path counts, holepunching, and net report metrics.
    pub fn transport_metrics(&self) -> CoreTransportMetrics {
        let m = self.endpoint.metrics();
        let s = &m.socket;
        let n = &m.net_report;
        CoreTransportMetrics {
            send_ipv4: s.send_ipv4.get(),
            send_ipv6: s.send_ipv6.get(),
            send_relay: s.send_relay.get(),
            recv_data_ipv4: s.recv_data_ipv4.get(),
            recv_data_ipv6: s.recv_data_ipv6.get(),
            recv_data_relay: s.recv_data_relay.get(),
            recv_datagrams: s.recv_datagrams.get(),
            num_conns_direct: s.num_conns_direct.get(),
            num_conns_opened: s.num_conns_opened.get(),
            num_conns_closed: s.num_conns_closed.get(),
            paths_direct: s.paths_direct.get(),
            paths_relay: s.paths_relay.get(),
            holepunch_attempts: s.holepunch_attempts.get(),
            relay_home_change: s.relay_home_change.get(),
            net_reports: n.reports.get(),
            net_reports_full: n.reports_full.get(),
        }
    }
}

// ============================================================================
// CoreConnection - QUIC connection
// ============================================================================

/// Pair of (send, recv) halves of a single QUIC bi-stream — the unit
/// of multiplexing in the per-connection stream pool.
pub type MultiplexedStream = (CoreSendStream, CoreRecvStream);

/// Per-connection stream pool over multiplexed bi-streams. See
/// `core::pool` for the underlying primitive and `Aster-multiplexed-streams.md`
/// §3 for design.
pub type MultiplexedStreamPool = StreamPool<MultiplexedStream>;

/// Handle returned by `CoreConnection::acquire_stream`. RAII; on drop
/// the underlying multiplexed stream is returned to the pool unless
/// `discard()` was called.
pub type MultiplexedStreamHandle = StreamHandle<MultiplexedStream>;

#[derive(Clone)]
pub struct CoreConnection {
    inner: Arc<Connection>,
    /// Per-connection multiplexed-stream pool. The pool's factory
    /// wraps `Connection::open_bi`. SHARED-pool (None) and
    /// per-session (Some(sessionId)) streams both live here.
    pool: MultiplexedStreamPool,
    /// Per-connection tunnel ticket registry. Ticket entries map to
    /// `TunnelTarget`s and never cross the wire — see
    /// `ffi_spec/Aster-tunneling.md`. Dropped with the connection,
    /// which mass-revokes any unredeemed tickets.
    tunnels: Arc<TunnelRegistry>,
    /// Node-shared map of `service_id -> L7 HTTP-object acceptor`
    /// (design doc §5.2). Shared into every connection so a single
    /// `register_local_target` exposes a service on all of them.
    local_targets: Arc<LocalTargetRegistry>,
    /// Per-connection L7 admission state: which services this connection
    /// is admitted for + the per-service concurrent-stream cap. Dropped
    /// with the connection (mass-revoke), like `tunnels`.
    admissions: Arc<AdmissionState>,
    /// Last client session id handed out on this connection
    /// (`next_session_id`). Shared by every clone, so a Rust client and a
    /// binding client on the same connection never pick the same id.
    session_ids: Arc<std::sync::atomic::AtomicU32>,
}

impl CoreConnection {
    fn new(conn: Connection) -> Self {
        Self::with_pool_config(conn, PoolConfig::default())
    }

    fn with_pool_config(conn: Connection, config: PoolConfig) -> Self {
        let inner = Arc::new(conn);
        let conn_for_factory = inner.clone();
        let factory: StreamFactory<MultiplexedStream> = Arc::new(move || {
            let conn = conn_for_factory.clone();
            Box::pin(async move {
                let (send, recv) = conn.open_bi().await?;
                Ok((CoreSendStream::new(send), CoreRecvStream::new(recv)))
            })
        });
        let pool = StreamPool::new(config, factory);
        Self {
            inner,
            pool,
            tunnels: Arc::new(TunnelRegistry::new(TunnelConfig::default())),
            // Per-connection by default. A node that exposes services shares
            // one registry across its connections via `set_local_targets`
            // (wired by the `aster-expose` facade); the admission state stays
            // per-connection regardless.
            local_targets: Arc::new(LocalTargetRegistry::new()),
            admissions: Arc::new(AdmissionState::new(DEFAULT_MAX_STREAMS_PER_SERVICE)),
            session_ids: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    /// Allocate a client session id for this connection: 1, 2, 3, ... in
    /// order, never reused. The counter is per connection and shared by every
    /// clone of it, so every client on the connection (Rust or a binding) draws
    /// from one sequence -- which the server needs, since it treats an id at
    /// or below the highest it has seen as a closed session (spec
    /// Aster-session-scoped-services.md, the graveyard rule).
    ///
    /// The wire carries the id as an `i32`, so ids stop at `i32::MAX`; after
    /// that this returns `None`.
    pub fn next_session_id(&self) -> Option<u32> {
        use std::sync::atomic::Ordering;
        self.session_ids
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |last| {
                (last < i32::MAX as u32).then_some(last + 1)
            })
            .ok()
            .map(|last| last + 1)
    }

    /// Replace this connection's L7 acceptor map with a node-shared one, so a
    /// single `register_local_target` exposes a service across every
    /// connection the node accepts. Called by the exposing facade right after
    /// the connection is created, before any stream is accepted.
    pub fn set_local_targets(&mut self, registry: Arc<LocalTargetRegistry>) {
        self.local_targets = registry;
    }

    /// Register an L7 HTTP-object acceptor for `service_id` on this
    /// connection's acceptor map (design doc §5.2). If the map is shared
    /// (`set_local_targets`), the registration is visible to every connection
    /// sharing it.
    pub fn register_local_target(&self, service_id: &str, acceptor: Arc<dyn LocalTunnelAcceptor>) {
        self.local_targets.register(service_id, acceptor);
    }

    /// Borrow the per-connection multiplexed-stream pool. Callers can
    /// query stats, apply the connect-time QUIC ceiling clamp, or
    /// acquire streams directly.
    pub fn pool(&self) -> &MultiplexedStreamPool {
        &self.pool
    }

    /// Acquire a multiplexed stream from the pool. `key` is `None`
    /// for SHARED unary calls or `Some(sessionId)` for session-bound
    /// calls. The returned handle returns the stream to the pool on
    /// drop unless `discard()` is called.
    pub async fn acquire_stream(
        &self,
        key: PoolKey,
    ) -> Result<MultiplexedStreamHandle, AcquireError> {
        self.pool.acquire(key).await
    }

    /// Open a dedicated multiplexed substream that **bypasses the
    /// per-connection pool**. Intended for streaming calls
    /// (server-stream, client-stream, bidi) per
    /// `ffi_spec/Aster-multiplexed-streams.md` §3: "streaming
    /// substreams don't count against any pool." They're per-call,
    /// lifetime-bounded by the call's iterator/future, and closed on
    /// drop — no return-to-pool path. The only ceiling is the QUIC
    /// `max_concurrent_streams` negotiated with the peer (spec §9).
    ///
    /// Callers MUST NOT use this for unary calls — those go through
    /// `acquire_stream` so they share pooled streams and observe the
    /// configured pool bounds.
    pub async fn open_streaming_substream(&self) -> Result<MultiplexedStream> {
        let (send, recv) = self.inner.open_bi().await?;
        Ok((CoreSendStream::new(send), CoreRecvStream::new(recv)))
    }

    pub async fn open_bi(&self) -> Result<(CoreSendStream, CoreRecvStream)> {
        let (send, recv) = self.inner.open_bi().await?;
        Ok((CoreSendStream::new(send), CoreRecvStream::new(recv)))
    }

    /// Accept the next inbound bidi stream verbatim. **Does not** peek
    /// for `FLAG_TUNNEL` — the caller gets whatever bytes the peer
    /// wrote, in the protocol's native format. Use this for non-Aster
    /// protocols (trust admission, etc.).
    pub async fn accept_bi(&self) -> Result<(CoreSendStream, CoreRecvStream)> {
        let (send, recv) = self.inner.accept_bi().await?;
        Ok((CoreSendStream::new(send), CoreRecvStream::new(recv)))
    }

    /// Accept the next inbound bidi stream that is **not** a tunnel
    /// redeem, on a connection that speaks the Aster RPC framing
    /// protocol. Tunnel-redeem streams (those whose first frame carries
    /// `FLAG_TUNNEL`) are dispatched transparently to the per-connection
    /// tunnel registry inside this method — the caller never sees them.
    /// See `ffi_spec/Aster-tunneling.md` §6 ("Dispatch site").
    ///
    /// For non-tunnel streams the peeked first frame is re-prepended
    /// onto the returned `CoreRecvStream`, so the caller observes a
    /// normal length-prefixed stream starting from the StreamHeader.
    ///
    /// Only call this when you know the connection's ALPN means every
    /// inbound stream's first frame uses Aster framing
    /// (`[4B le len][1B flags][payload]`). For non-Aster ALPNs (trust
    /// admission, custom protocols), use [`accept_bi`] instead.
    pub async fn accept_aster_bi(&self) -> Result<(CoreSendStream, CoreRecvStream)> {
        loop {
            let (send, recv) = self.inner.accept_bi().await?;
            let send = CoreSendStream::new(send);
            let recv = CoreRecvStream::new(recv);

            // The peeked frame is either a tunnel-redeem marker or a call
            // header, and both are small. Bounding it here matters because this
            // read happens before the reactor -- and therefore before any
            // per-service limit -- exists.
            let (payload, flags) = match recv
                .read_one_frame_capped(crate::framing::MAX_CALL_HEADER_SIZE)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    debug!("accept_aster_bi: first-frame read error, dropping stream: {e}");
                    continue;
                }
            };

            if flags & FLAG_TUNNEL != 0 {
                // First payload byte is the subtype discriminator (§5.2).
                match payload.first().copied() {
                    Some(TUNNEL_SUBTYPE_TICKET) => {
                        let ticket = payload[1..].to_vec();
                        let registry = self.tunnels.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                crate::tunnel::handle_tunnel_redeem(send, recv, ticket, registry)
                                    .await
                            {
                                debug!("tunnel redeem error: {e}");
                            }
                        });
                    }
                    Some(TUNNEL_SUBTYPE_HTTP_RELAY) => {
                        let rest = payload[1..].to_vec();
                        let peer = self.remote_id();
                        let conn = self.clone();
                        let registry = self.local_targets.clone();
                        let admissions = self.admissions.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_http_relay(
                                conn, send, recv, rest, peer, registry, admissions,
                            )
                            .await
                            {
                                debug!("http-relay error: {e}");
                            }
                        });
                    }
                    other => {
                        debug!("unknown FLAG_TUNNEL subtype {other:?}, dropping stream");
                    }
                }
                continue;
            }

            // Non-tunnel: re-encode the frame and prepend so the caller
            // reads it as the first frame of a normal RPC stream.
            let frame = encode_frame(&payload, flags)?;
            recv.prepend(frame).await;
            return Ok((send, recv));
        }
    }

    /// Mint a tunnel capability covering one or more targets. Called by
    /// an RPC handler that has already validated the peer's request —
    /// `core` performs no policy checks. `targets` is an ordered
    /// preference list; at redeem the acceptor tries each in order and
    /// splices the first one that connects. The returned 32 bytes go
    /// into the RPC response; the peer redeems with [`open_tunnel`].
    /// See `ffi_spec/Aster-tunneling.md`.
    pub fn authorize_tunnel(
        &self,
        targets: Vec<TunnelTarget>,
        ttl: Duration,
    ) -> Result<TunnelTicket, AuthorizeTunnelError> {
        self.tunnels.authorize(targets, ttl)
    }

    /// Open a tunnel by redeeming a ticket received from the peer.
    /// Sends the `[FLAG_TUNNEL][subtype 0][ticket]` handshake frame
    /// internally, then returns the bidi stream pair. Bytes after the
    /// handshake are raw — the caller speaks the backend protocol
    /// directly (e.g. RFB for VNC).
    pub async fn open_tunnel(
        &self,
        ticket: TunnelTicket,
    ) -> Result<(CoreSendStream, CoreRecvStream)> {
        let (send, recv) = self.inner.open_bi().await?;
        let send = CoreSendStream::new(send);
        let recv = CoreRecvStream::new(recv);
        // Subtype 0 = ticket-redeem (§5.2). Flag-day wire change.
        let mut payload = Vec::with_capacity(33);
        payload.push(TUNNEL_SUBTYPE_TICKET);
        payload.extend_from_slice(ticket.as_bytes());
        let frame = encode_frame(&payload, FLAG_TUNNEL)?;
        send.write_all(frame).await?;
        Ok((send, recv))
    }

    /// Open an L7 HTTP-object relay stream for `service_id` (design doc
    /// §5.2). Writes the `[FLAG_TUNNEL][subtype 1][u16 len][service_id][u16
    /// mlen][metadata]` first frame, then returns the bidi stream pair.
    /// `metadata` is an opaque blob the remote's admission policy inspects
    /// (token / capability / app identity / …); pass `&[]` for none. The caller
    /// then writes the H3-object request and reads the response (the
    /// `aster-expose` companion crate owns that codec). One stream per request.
    pub async fn open_http_relay(
        &self,
        service_id: &str,
        metadata: &[u8],
    ) -> Result<(CoreSendStream, CoreRecvStream)> {
        let (send, recv) = self.inner.open_bi().await?;
        let send = CoreSendStream::new(send);
        let recv = CoreRecvStream::new(recv);
        let id = service_id.as_bytes();
        let len = u16::try_from(id.len())
            .map_err(|_| anyhow!("service_id too long ({} bytes, max 65535)", id.len()))?;
        let mlen = u16::try_from(metadata.len())
            .map_err(|_| anyhow!("metadata too long ({} bytes, max 65535)", metadata.len()))?;
        let mut payload = Vec::with_capacity(5 + id.len() + metadata.len());
        payload.push(TUNNEL_SUBTYPE_HTTP_RELAY);
        payload.extend_from_slice(&len.to_le_bytes());
        payload.extend_from_slice(id);
        payload.extend_from_slice(&mlen.to_le_bytes());
        payload.extend_from_slice(metadata);
        let frame = encode_frame(&payload, FLAG_TUNNEL)?;
        send.write_all(frame).await?;
        Ok((send, recv))
    }

    pub async fn open_uni(&self) -> Result<CoreSendStream> {
        Ok(CoreSendStream::new(self.inner.open_uni().await?))
    }

    pub async fn accept_uni(&self) -> Result<CoreRecvStream> {
        Ok(CoreRecvStream::new(self.inner.accept_uni().await?))
    }

    pub fn send_datagram(&self, data: Vec<u8>) -> Result<()> {
        self.inner.send_datagram(Bytes::from(data))?;
        Ok(())
    }

    pub async fn read_datagram(&self) -> Result<Vec<u8>> {
        Ok(self.inner.read_datagram().await?.to_vec())
    }

    pub fn remote_id(&self) -> String {
        self.inner.remote_id().to_string()
    }

    pub fn close(&self, code: u64, reason: Vec<u8>) -> Result<()> {
        self.inner.close(u64_to_varint(code)?, &reason);
        Ok(())
    }

    pub async fn closed(&self) -> CoreClosedInfo {
        closed_info(&self.inner.closed().await)
    }

    /// How the connection closed, without waiting; `None` while it is open.
    pub fn close_info(&self) -> Option<CoreClosedInfo> {
        self.inner.close_reason().map(|closed| closed_info(&closed))
    }

    // ============================================================================
    // Phase 1b: Datagram Completion
    // ============================================================================

    pub fn max_datagram_size(&self) -> Option<usize> {
        self.inner.max_datagram_size()
    }

    pub fn datagram_send_buffer_space(&self) -> usize {
        self.inner.datagram_send_buffer_space()
    }

    // ============================================================================
    // Phase 1b: Connection Info
    // ============================================================================

    /// Snapshot of the currently selected network path. Cheap; safe to
    /// call once per RPC dispatch. Returns all-`None` fields if the
    /// connection has been dropped or no path is selected yet.
    pub fn transport_snapshot(&self) -> CoreTransportSnapshot {
        selected_transport_snapshot(&self.inner)
    }

    /// Owned snapshot of every currently-open network path. Unlike
    /// [`transport_snapshot`](Self::transport_snapshot) (selected path
    /// only), this returns all paths so callers can see e.g. a relay
    /// path and a direct path coexisting during holepunching. The
    /// returned `Vec` is owned and can cross task boundaries, unlike
    /// iroh's borrow-bound `PathList`.
    pub fn paths(&self) -> Vec<CorePathInfo> {
        self.inner
            .paths()
            .iter()
            .map(|p| {
                let remote = match p.remote_addr() {
                    TransportAddr::Ip(socket) => CorePathRemote::Direct(*socket),
                    TransportAddr::Relay(url) => CorePathRemote::Relay(url.to_string()),
                    other => CorePathRemote::Other(format!("{other:?}")),
                };
                let rtt = p.rtt();
                let rtt_micros =
                    (rtt != Duration::ZERO).then(|| rtt.as_micros().min(u64::MAX as u128) as u64);
                CorePathInfo {
                    remote,
                    is_selected: p.is_selected(),
                    rtt_micros,
                }
            })
            .collect()
    }

    pub fn connection_info(&self) -> CoreConnectionInfo {
        let stats = self.inner.stats();
        let paths = self.inner.paths();
        let selected_path = paths.iter().find(|p| p.is_selected());

        let connection_type = match selected_path.as_ref() {
            Some(path) if path.is_relay() => ConnectionTypeDetail::UdpRelay,
            Some(path) if path.is_ip() => ConnectionTypeDetail::UdpDirect,
            Some(path) => ConnectionTypeDetail::Other(format!("{:?}", path.remote_addr())),
            None => ConnectionTypeDetail::Other("unknown".to_string()),
        };

        let rtt_ns = selected_path.as_ref().and_then(|p| {
            let rtt = p.rtt();
            (rtt != Duration::ZERO).then(|| rtt.as_nanos().min(u64::MAX as u128) as u64)
        });

        CoreConnectionInfo {
            connection_type,
            bytes_sent: stats.udp_tx.bytes,
            bytes_received: stats.udp_rx.bytes,
            rtt_ns,
            alpn: self.inner.alpn().to_vec(),
            is_connected: self.inner.close_reason().is_none(),
        }
    }

    // ============================================================================
    // Per-connection metrics (for routing / HA)
    // ============================================================================

    /// Current round-trip time in milliseconds for the selected path.
    /// Returns 0.0 if no path is selected or RTT is not yet measured.
    pub fn rtt_ms(&self) -> f64 {
        self.inner
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| p.rtt().as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }

    /// Total bytes sent on the selected path (UDP layer).
    pub fn bytes_sent(&self) -> u64 {
        self.inner
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| p.stats().udp_tx.bytes)
            .unwrap_or(0)
    }

    /// Total bytes received on the selected path (UDP layer).
    pub fn bytes_recv(&self) -> u64 {
        self.inner
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| p.stats().udp_rx.bytes)
            .unwrap_or(0)
    }

    /// Current congestion window size in bytes for the selected path.
    /// Returns 0 if not available.
    pub fn congestion_window(&self) -> u64 {
        self.inner
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| p.stats().cwnd)
            .unwrap_or(0)
    }

    /// Number of lost packets on the selected path.
    pub fn lost_packets(&self) -> u64 {
        self.inner
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| p.stats().lost_packets)
            .unwrap_or(0)
    }

    /// Number of congestion events on the selected path.
    pub fn congestion_events(&self) -> u64 {
        self.inner
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| p.stats().congestion_events)
            .unwrap_or(0)
    }

    /// Current path MTU in bytes.
    pub fn current_mtu(&self) -> u16 {
        self.inner
            .paths()
            .iter()
            .find(|p| p.is_selected())
            .map(|p| p.stats().current_mtu)
            .unwrap_or(0)
    }

    // ========================================================================
    // Transactional RPC methods (v0.3 — collapse FFI crossings)
    // ========================================================================

    /// Execute a complete unary RPC in one async call. Collapses 8 FFI
    /// crossings (open_bi, write_all, finish, read_exact×4, finish) into 1.
    ///
    /// The caller pre-encodes the header frame and request frame using
    /// whatever codec the stream uses. Core treats them as opaque bytes,
    /// handles the QUIC IO, and returns the raw response + trailer bytes
    /// for the caller to decode.
    pub async fn unary_call(
        &self,
        header_frame: &[u8],
        request_frame: &[u8],
    ) -> Result<UnaryCallResult> {
        use crate::framing::{FLAG_TRAILER, MAX_FRAME_SIZE};

        let (send, recv) = self.inner.open_bi().await?;
        let mut send = send;
        let mut recv = recv;

        // Write header + request frames and finish send side
        send.write_all(header_frame).await?;
        send.write_all(request_frame).await?;
        send.finish()?;

        // Read frames until we get the trailer
        let mut response_payload: Option<Vec<u8>> = None;
        let mut response_flags: u8 = 0;

        loop {
            // Read 4-byte length prefix
            let mut len_buf = [0u8; 4];
            match recv.read_exact(&mut len_buf).await {
                Ok(()) => {}
                Err(e) => {
                    if response_payload.is_some() {
                        // Stream ended after response but before trailer —
                        // some protocols omit trailer on success (session unary).
                        // Return what we have with empty trailer.
                        return Ok(UnaryCallResult {
                            response_payload: response_payload.unwrap_or_default(),
                            response_flags,
                            trailer_payload: Vec::new(),
                            trailer_flags: FLAG_TRAILER,
                        });
                    }
                    return Err(anyhow!("stream ended before response: {}", e));
                }
            }

            let frame_body_len = u32::from_le_bytes(len_buf) as usize;
            if frame_body_len == 0 {
                return Err(anyhow!("received zero-length frame"));
            }
            if frame_body_len > MAX_FRAME_SIZE as usize {
                return Err(anyhow!(
                    "frame size {} exceeds maximum {}",
                    frame_body_len,
                    MAX_FRAME_SIZE
                ));
            }

            // Read flags + payload
            let mut body = vec![0u8; frame_body_len];
            recv.read_exact(&mut body).await?;

            let flags = body[0];
            let payload = body[1..].to_vec();

            if flags & FLAG_TRAILER != 0 {
                return Ok(UnaryCallResult {
                    response_payload: response_payload.unwrap_or_default(),
                    response_flags,
                    trailer_payload: payload,
                    trailer_flags: flags,
                });
            }

            // Data frame — should be the response
            if response_payload.is_some() {
                return Err(anyhow!("received multiple data frames in unary call"));
            }
            response_payload = Some(payload);
            response_flags = flags;
        }
    }

    /// Read incoming request header + payload from an accepted stream.
    /// Collapses 4 FFI crossings (read_exact×4) into 1.
    ///
    /// Returns the raw header bytes and request bytes for the caller to
    /// decode with its codec.
    pub async fn read_request(recv: &mut iroh::endpoint::RecvStream) -> Result<IncomingRequest> {
        use crate::framing::FLAG_HEADER;

        // Read header frame
        let (header_payload, header_flags) = read_one_frame(recv).await?;
        if header_flags & FLAG_HEADER == 0 {
            return Err(anyhow!("first frame missing HEADER flag"));
        }

        // Read request frame
        let (request_payload, request_flags) = read_one_frame(recv).await?;

        Ok(IncomingRequest {
            header_payload,
            header_flags,
            request_payload,
            request_flags,
        })
    }

    /// Write response + trailer and finish the send side.
    /// Collapses 3 FFI crossings (write_all×2, finish) into 1.
    pub async fn write_response(
        send: &mut iroh::endpoint::SendStream,
        response_frame: &[u8],
        trailer_frame: &[u8],
    ) -> Result<()> {
        send.write_all(response_frame).await?;
        send.write_all(trailer_frame).await?;
        send.finish()?;
        Ok(())
    }
}

/// Execute a unary call within a session on existing streams. Writes
/// call_header + request, reads response frame(s) + optional trailer.
/// One FFI crossing instead of 4.
pub async fn session_unary_call(
    send: &CoreSendStream,
    recv: &CoreRecvStream,
    call_header_frame: &[u8],
    request_frame: &[u8],
) -> Result<UnaryCallResult> {
    use crate::framing::{FLAG_TRAILER, MAX_FRAME_SIZE};

    // Write call header + request in one write
    let mut buf = Vec::with_capacity(call_header_frame.len() + request_frame.len());
    buf.extend_from_slice(call_header_frame);
    buf.extend_from_slice(request_frame);
    send.write_all(buf).await?;

    // Read one response frame. Per spec §4.6, session unary calls do NOT
    // require a trailer on success — the response data frame alone is the
    // complete reply. If the server sends a trailer instead (error case),
    // we return it as the trailer.
    let len_bytes = recv.read_exact(4).await?;
    let frame_body_len =
        u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
    if frame_body_len == 0 || frame_body_len > MAX_FRAME_SIZE as usize {
        return Err(anyhow!("invalid frame length: {}", frame_body_len));
    }

    let body = recv.read_exact(frame_body_len).await?;
    let flags = body[0];
    let payload = body[1..].to_vec();

    if flags & FLAG_TRAILER != 0 {
        // Error case: server sent a trailer instead of a response
        Ok(UnaryCallResult {
            response_payload: Vec::new(),
            response_flags: 0,
            trailer_payload: payload,
            trailer_flags: flags,
        })
    } else {
        // Success: data frame is the response, no trailer expected
        Ok(UnaryCallResult {
            response_payload: payload,
            response_flags: flags,
            trailer_payload: Vec::new(),
            trailer_flags: 0,
        })
    }
}

/// Result of a unary RPC call. All fields are raw bytes — the caller
/// decodes them with its codec.
pub struct UnaryCallResult {
    pub response_payload: Vec<u8>,
    pub response_flags: u8,
    pub trailer_payload: Vec<u8>,
    pub trailer_flags: u8,
}

/// An incoming request read from an accepted stream.
pub struct IncomingRequest {
    pub header_payload: Vec<u8>,
    pub header_flags: u8,
    pub request_payload: Vec<u8>,
    pub request_flags: u8,
}

/// Read one length-prefixed frame from a recv stream.
async fn read_one_frame(recv: &mut iroh::endpoint::RecvStream) -> Result<(Vec<u8>, u8)> {
    use crate::framing::MAX_FRAME_SIZE;

    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;

    let frame_body_len = u32::from_le_bytes(len_buf) as usize;
    if frame_body_len == 0 {
        return Err(anyhow!("received zero-length frame"));
    }
    if frame_body_len > MAX_FRAME_SIZE as usize {
        return Err(anyhow!(
            "frame size {} exceeds maximum {}",
            frame_body_len,
            MAX_FRAME_SIZE
        ));
    }

    let mut body = vec![0u8; frame_body_len];
    recv.read_exact(&mut body).await?;

    let flags = body[0];
    let payload = body[1..].to_vec();
    Ok((payload, flags))
}

// ============================================================================
// CoreSendStream - QUIC send stream
// ============================================================================

#[derive(Clone)]
pub struct CoreSendStream {
    inner: Arc<Mutex<iroh::endpoint::SendStream>>,
}

impl CoreSendStream {
    fn new(stream: iroh::endpoint::SendStream) -> Self {
        Self {
            inner: Arc::new(Mutex::new(stream)),
        }
    }

    pub async fn write_all(&self, data: Vec<u8>) -> Result<()> {
        let mut s = self.inner.lock().await;
        s.write_all(&data).await?;
        Ok(())
    }

    pub async fn finish(&self) -> Result<()> {
        let mut s = self.inner.lock().await;
        s.finish()?;
        Ok(())
    }

    pub async fn stopped(&self) -> Result<Option<u64>> {
        let s = &mut *self.inner.lock().await;
        Ok(s.stopped().await?.map(|v| v.into_inner()))
    }

    /// Abruptly terminate this send direction with an application error code.
    ///
    /// Used to **poison** a stream after `FLAG_CANCEL` (§6): a cancelled call
    /// may have had a response frame partially written when flow control
    /// blocked, so the byte stream is no longer at a frame boundary and nothing
    /// may be written after it. Reset makes that explicit to the peer instead
    /// of leaving a half-frame that the next call's bytes would silently
    /// extend.
    ///
    /// Unlike `finish`, this does not wait for the peer and does not deliver
    /// buffered data.
    ///
    /// **Awaits the lock and propagates failure.** An earlier version used
    /// `try_lock` and returned `Ok(())` when the lock was held, on the theory
    /// that a blocked writer would be dropped anyway -- so a reset that never
    /// happened reported success. Callers must abort and await the writer
    /// first, which releases the guard at its suspension point; by the time
    /// this runs the lock is free, and blocking here would be a bug worth
    /// seeing rather than hiding.
    pub async fn reset(&self, code: u32) -> Result<()> {
        let mut s = self.inner.lock().await;
        s.reset(code.into())?;
        Ok(())
    }

    /// Set this stream's send priority. Quinn/iroh semantics: a higher value is
    /// scheduled first when several send streams on the same connection compete
    /// for the wire; default is `0`. Errors only if the stream is already closed.
    pub async fn set_priority(&self, priority: i32) -> Result<()> {
        let s = self.inner.lock().await;
        s.set_priority(priority)?;
        Ok(())
    }

    /// This stream's current send priority.
    pub async fn priority(&self) -> Result<i32> {
        let s = self.inner.lock().await;
        Ok(s.priority()?)
    }
}

// ============================================================================
// CoreRecvStream - QUIC recv stream
// ============================================================================

struct RecvInner {
    stream: iroh::endpoint::RecvStream,
    /// Bytes peeked off the wire ahead of the caller — populated by
    /// [`CoreConnection::accept_aster_bi`] when it consumes the first frame
    /// for `FLAG_TUNNEL` dispatch and the stream turns out to be a
    /// regular RPC stream. Reads drain this buffer before going to
    /// QUIC so the caller sees an unmodified length-prefixed stream.
    front: Vec<u8>,
}

impl RecvInner {
    fn drain_front(&mut self, buf: &mut [u8]) -> usize {
        if self.front.is_empty() || buf.is_empty() {
            return 0;
        }
        let take = self.front.len().min(buf.len());
        buf[..take].copy_from_slice(&self.front[..take]);
        self.front.drain(..take);
        take
    }
}

#[derive(Clone)]
pub struct CoreRecvStream {
    inner: Arc<Mutex<RecvInner>>,
}

impl CoreRecvStream {
    fn new(stream: iroh::endpoint::RecvStream) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RecvInner {
                stream,
                front: Vec::new(),
            })),
        }
    }

    /// Stash `bytes` at the front of the stream so the next read(s) drain
    /// them before going to the wire. Used by `CoreConnection::accept_aster_bi`
    /// to put back the first frame after peeking it for `FLAG_TUNNEL`.
    /// Caller must ensure the stream has not been read from yet — this
    /// is enforced in debug builds.
    pub(crate) async fn prepend(&self, bytes: Vec<u8>) {
        let mut s = self.inner.lock().await;
        debug_assert!(s.front.is_empty(), "CoreRecvStream::prepend called twice");
        s.front = bytes;
    }

    pub async fn read(&self, max_len: usize) -> Result<Option<Vec<u8>>> {
        let mut s = self.inner.lock().await;
        if !s.front.is_empty() {
            let take = s.front.len().min(max_len);
            let chunk: Vec<u8> = s.front.drain(..take).collect();
            return Ok(Some(chunk));
        }
        Ok(s.stream.read_chunk(max_len).await?.map(|b| b.to_vec()))
    }

    pub async fn read_exact(&self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.read_exact_into(&mut buf).await?;
        Ok(buf)
    }

    /// Zero-copy read of exactly `buf.len()` bytes into the caller-provided buffer.
    ///
    /// Uses noq's `read_into` (from aster-rpc/noq) which bypasses the `Bytes`
    /// intermediate allocation inside the assembler. This is the fast path for
    /// the reactor frame reader and any FFI consumer that already has a buffer
    /// ready to receive data.
    pub async fn read_exact_into(&self, buf: &mut [u8]) -> Result<()> {
        let mut s = self.inner.lock().await;
        let mut filled = s.drain_front(buf);
        while filled < buf.len() {
            match s.stream.read_into(&mut buf[filled..]).await? {
                Some(0) | None => {
                    return Err(anyhow!(
                        "stream ended with {} bytes remaining",
                        buf.len() - filled
                    ))
                }
                Some(n) => filled += n,
            }
        }
        Ok(())
    }

    pub async fn read_to_end(&self, max_size: usize) -> Result<Vec<u8>> {
        let mut s = self.inner.lock().await;
        let mut out = std::mem::take(&mut s.front);
        if out.len() > max_size {
            return Err(anyhow!(
                "prepended buffer ({} bytes) exceeds max_size ({})",
                out.len(),
                max_size
            ));
        }
        let remaining = max_size - out.len();
        let tail = s.stream.read_to_end(remaining).await?;
        out.extend_from_slice(&tail);
        Ok(out)
    }

    /// Read one length-prefixed frame: `[4B LE len][1B flags][payload]`,
    /// refusing a frame whose *declared* body length exceeds `max` before
    /// allocating for it.
    ///
    /// There is deliberately no uncapped variant. Every caller states the limit
    /// in force -- `CoreConnection::accept_aster_bi`'s `FLAG_TUNNEL` peek and the
    /// reactor's per-stream reader both do -- so the cap cannot be forgotten by
    /// reaching for a more convenient method.
    ///
    /// This is the only place a caller-declared length becomes an allocation,
    /// so it is the only place a per-method cap can be enforced. Checking after
    /// the read -- in the dispatcher, say -- is checking after the harm.
    pub(crate) async fn read_one_frame_capped(&self, max: u32) -> Result<(Vec<u8>, u8)> {
        let mut len_bytes = [0u8; 4];
        self.read_exact_into(&mut len_bytes).await?;
        let frame_body_len = u32::from_le_bytes(len_bytes) as usize;
        if frame_body_len == 0 {
            return Err(anyhow!("zero-length frame"));
        }
        let cap = max.min(crate::framing::MAX_FRAME_SIZE) as usize;
        if frame_body_len > cap {
            return Err(anyhow!(
                "frame too large: {} exceeds cap {}",
                frame_body_len,
                cap
            ));
        }
        let mut flags_buf = [0u8; 1];
        self.read_exact_into(&mut flags_buf).await?;
        let flags = flags_buf[0];
        let payload_len = frame_body_len - 1;
        // Counted HERE, at the allocation itself, so the metric covers every
        // caller -- including `accept_aster_bi`'s peek, which happens before
        // the reactor exists. Counting in the reactor only would report zero
        // for a peek that allocated 16 MiB and then blocked.
        #[cfg(feature = "ingress-metrics")]
        crate::reactor::ingress_metrics::FRAME_BYTES_ALLOCATED
            .fetch_add(payload_len as u64, std::sync::atomic::Ordering::Relaxed);
        let mut payload = vec![0u8; payload_len];
        self.read_exact_into(&mut payload).await?;
        Ok((payload, flags))
    }

    pub fn stop(&self, code: u64) -> Result<()> {
        let mut s = self
            .inner
            .try_lock()
            .map_err(|_| anyhow!("recv stream is busy"))?;
        s.stream.stop(u64_to_varint(code)?)?;
        Ok(())
    }
}

// ============================================================================
// CoreTagInfo - Tag information
// ============================================================================

/// Information about a named tag in the blob store.
#[derive(Clone, Debug)]
pub struct CoreTagInfo {
    /// Tag name (UTF-8)
    pub name: String,
    /// Content hash (hex string)
    pub hash: String,
    /// Format: "raw" or "hash_seq"
    pub format: String,
}

impl CoreTagInfo {
    fn from_upstream(ti: iroh_blobs::api::tags::TagInfo) -> Self {
        Self {
            name: String::from_utf8_lossy(&ti.name.0).into_owned(),
            hash: ti.hash.to_hex().to_string(),
            format: match ti.format {
                BlobFormat::HashSeq => "hash_seq".to_string(),
                BlobFormat::Raw => "raw".to_string(),
            },
        }
    }
}

// ============================================================================
// CoreBlobsClient - Blob storage protocol
// ============================================================================

/// Status of a blob in the local store.
#[derive(Debug, Clone)]
pub enum CoreBlobStatus {
    /// Blob is not present.
    NotFound,
    /// Blob is partially present. `size` is the number of bytes currently stored (0 if unknown).
    Partial { size: u64 },
    /// Blob is fully present.
    Complete { size: u64 },
}

#[derive(Clone)]
pub struct CoreBlobsClient {
    pub store: BlobStore,
    pub endpoint: Endpoint,
    downloader: Downloader,
    /// Docs GC protection callback (see [`CoreNodeInner::protect_cb`]). Replayed
    /// by [`Self::gc_run_once`] to seed the `live` set before a manual sweep, so
    /// a deterministic sweep protects docs content exactly as the periodic loop
    /// does.
    pub protect_cb: ProtectCb,
}

impl CoreBlobsClient {
    pub async fn add_bytes(&self, data: Vec<u8>) -> Result<String> {
        Ok(self.store.add_slice(&data).await?.hash.to_string())
    }

    /// Run exactly one GC mark+sweep pass now and return when it completes.
    ///
    /// Reclaims every blob not protected by a persistent tag, a live temp-tag,
    /// or the store's protect callback. Unlike the periodic GC loop (enabled
    /// via the `gc_interval` constructors), this is synchronous and
    /// deterministic, so test harnesses can assert reclamation without waiting
    /// on the timed interval. Safe to call whether or not periodic GC is
    /// configured. GC is **node-wide** over the one shared blob store: it
    /// collects any untagged blob regardless of which tree added it.
    ///
    /// Before sweeping, this replays the docs protect callback to seed `live`
    /// with the docs engine's current content hashes — `blobs_gc_run_once` does
    /// not invoke the callback itself (only the periodic `run_gc` loop does), so
    /// without this a manual sweep would reclaim the untagged blobs backing docs
    /// entries. Matching the periodic loop's fail-safe, an `Abort` from the
    /// callback (e.g. the docs engine failed to enumerate its hashes) skips the
    /// sweep entirely rather than risk deleting live content.
    pub async fn gc_run_once(&self) -> Result<()> {
        let mut live = std::collections::HashSet::new();
        if let ProtectOutcome::Abort = (self.protect_cb)(&mut live).await {
            tracing::warn!("gc_run_once: protect callback aborted; skipping sweep");
            return Ok(());
        }
        blobs_gc_run_once(&self.store, &mut live).await?;
        Ok(())
    }

    /// Import a file from `path` into the blob store and return its hash (hex).
    ///
    /// Uses `ImportMode::Copy` (the reflink-or-copy import path, never
    /// `TryReference`), so the imported data is owned by the store and cannot be
    /// corrupted by later edits to the source file. Like [`Self::add_bytes`], an
    /// auto-created tag keeps the blob alive; callers that need a deterministic,
    /// GC-controllable tag should use [`Self::add_path_with_named_tag`].
    pub async fn add_path(&self, path: String) -> Result<String> {
        let tag = self
            .store
            .blobs()
            .add_path(std::path::PathBuf::from(path))
            .await?;
        Ok(tag.hash.to_string())
    }

    /// Import a file from `path` and set a persistent named tag in one step,
    /// returning the blob hash (hex).
    ///
    /// Same `ImportMode::Copy` import as [`Self::add_path`], but the blob is
    /// protected by the caller-chosen `tag_name` instead of an anonymous
    /// auto-tag. portal-sync keys the tag name by `(tree_id, blob_hash)` so it
    /// can GC blobs deterministically by deleting the tag.
    pub async fn add_path_with_named_tag(&self, path: String, tag_name: String) -> Result<String> {
        let haf = self
            .store
            .blobs()
            .add_path(std::path::PathBuf::from(path))
            .with_named_tag(tag_name.as_bytes())
            .await?;
        Ok(haf.hash.to_string())
    }

    pub async fn read_to_bytes(&self, hash_hex: String) -> Result<Vec<u8>> {
        Ok(self
            .store
            .get_bytes(hash_hex.parse::<Hash>()?)
            .await?
            .to_vec())
    }

    pub async fn read_range(&self, hash_hex: String, offset: u64, len: u64) -> Result<Vec<u8>> {
        let end = offset.saturating_add(len);
        Ok(self
            .store
            .blobs()
            .export_ranges(hash_hex.parse::<Hash>()?, offset..end)
            .concatenate()
            .await?)
    }

    pub fn create_ticket(&self, hash_hex: String) -> Result<String> {
        Ok(BlobTicket::new(
            self.endpoint.addr(),
            hash_hex.parse::<Hash>()?,
            BlobFormat::Raw,
        )
        .encode_string())
    }

    /// Store bytes as a single-file Collection (HashSeq), compatible with sendme.
    ///
    /// This wraps the data in a Collection with the given filename, matching
    /// what `sendme send` does. Returns the collection hash (hex).
    ///
    /// A persistent named tag `"aster-python/{name}"` is set so the GC keeps the
    /// collection alive. Call `tag_delete("aster-python/{name}")` to unpublish.
    pub async fn add_bytes_as_collection(&self, name: String, data: Vec<u8>) -> Result<String> {
        // Store the raw blob (TempTag keeps it alive until we set a persistent tag)
        let temp_blob = self.store.add_slice(&data).await?;
        let blob_hash = temp_blob.hash;

        // Build a Collection containing just this one file
        let collection: Collection = vec![(name.clone(), blob_hash)].into_iter().collect();

        // Store the collection itself (produces a HashSeq blob)
        let temp_collection = collection.store(&self.store).await?;
        let collection_hash = temp_collection.hash();
        let hash_str = collection_hash.to_string();

        // Set a persistent named tag so the collection (and its children) survive GC.
        // This replaces the previous std::mem::forget approach.
        let tag_name = format!("aster-python/{name}");
        self.store
            .tags()
            .set(
                tag_name.as_bytes(),
                HashAndFormat {
                    hash: collection_hash,
                    format: BlobFormat::HashSeq,
                },
            )
            .await?;

        // TempTags drop here — the persistent named tag now protects the data.
        drop(temp_blob);
        drop(temp_collection);

        Ok(hash_str)
    }

    /// Store a multi-file collection (HashSeq).
    ///
    /// Each `(name, data)` pair is stored as a raw blob, then a `Collection`
    /// is built from the `(name, hash)` pairs and stored as a HashSeq blob.
    /// A persistent tag is set with `BlobFormat::HashSeq` so the collection
    /// (and all its children) are protected from GC.
    ///
    /// Returns the collection hash (hex).
    pub async fn add_collection(&self, entries: Vec<(String, Vec<u8>)>) -> Result<String> {
        // Store each entry as a raw blob, keeping TempTags alive
        let mut temp_tags = Vec::with_capacity(entries.len());
        let mut name_hash_pairs: Vec<(String, Hash)> = Vec::with_capacity(entries.len());
        for (name, data) in &entries {
            let temp = self.store.add_slice(data).await?;
            let hash = temp.hash;
            name_hash_pairs.push((name.clone(), hash));
            temp_tags.push(temp);
        }

        // Build a Collection from the name/hash pairs
        let collection: Collection = name_hash_pairs.into_iter().collect();

        // Store the collection itself (produces a HashSeq blob)
        let temp_collection = collection.store(&self.store).await?;
        let collection_hash = temp_collection.hash();
        let hash_str = collection_hash.to_string();

        // Set a persistent named tag so the collection survives GC
        let tag_name = format!("aster-collection/{}", &hash_str[..16]);
        self.store
            .tags()
            .set(
                tag_name.as_bytes(),
                HashAndFormat {
                    hash: collection_hash,
                    format: BlobFormat::HashSeq,
                },
            )
            .await?;

        // TempTags drop here — the persistent named tag now protects all data.
        drop(temp_tags);
        drop(temp_collection);

        Ok(hash_str)
    }

    /// List entries from a stored collection.
    ///
    /// Loads the `Collection` by hash, reads the size of each child blob,
    /// and returns `Vec<(name, hash_hex, size)>`.
    pub async fn list_collection(&self, hash_hex: String) -> Result<Vec<(String, String, u64)>> {
        let hash = hash_hex.parse::<Hash>()?;
        let collection = Collection::load(hash, &self.store).await?;
        let mut result = Vec::new();
        for (name, blob_hash) in collection.iter() {
            let size = match self.store.blobs().status(*blob_hash).await? {
                iroh_blobs::api::proto::BlobStatus::Complete { size } => size,
                iroh_blobs::api::proto::BlobStatus::Partial { size } => size.unwrap_or(0),
                iroh_blobs::api::proto::BlobStatus::NotFound => 0,
            };
            result.push((name.clone(), blob_hash.to_string(), size));
        }
        Ok(result)
    }

    // ── Tag methods ──────────────────────────────────────────────────────────

    /// Set a named tag. `format` is "raw" or "hash_seq".
    pub async fn tag_set(&self, name: String, hash_hex: String, format: String) -> Result<()> {
        let hash: Hash = hash_hex.parse()?;
        let fmt = if format == "hash_seq" {
            BlobFormat::HashSeq
        } else {
            BlobFormat::Raw
        };
        self.store
            .tags()
            .set(name.as_bytes(), HashAndFormat { hash, format: fmt })
            .await?;
        Ok(())
    }

    /// Get a tag by name. Returns `None` if not found.
    pub async fn tag_get(&self, name: String) -> Result<Option<CoreTagInfo>> {
        let result = self.store.tags().get(name.as_bytes()).await?;
        Ok(result.map(CoreTagInfo::from_upstream))
    }

    /// Delete a tag by name. Returns the number of tags removed (0 or 1).
    pub async fn tag_delete(&self, name: String) -> Result<u64> {
        Ok(self.store.tags().delete(name.as_bytes()).await?)
    }

    /// Delete all tags matching a prefix. Returns count removed.
    pub async fn tag_delete_prefix(&self, prefix: String) -> Result<u64> {
        Ok(self.store.tags().delete_prefix(prefix.as_bytes()).await?)
    }

    /// List all tags.
    pub async fn tag_list(&self) -> Result<Vec<CoreTagInfo>> {
        let stream = self.store.tags().list().await?;
        let mut results = Vec::new();
        let mut pinned = Box::pin(stream);
        while let Some(item) = pinned.next().await {
            results.push(CoreTagInfo::from_upstream(item?));
        }
        Ok(results)
    }

    /// List tags matching a prefix.
    pub async fn tag_list_prefix(&self, prefix: String) -> Result<Vec<CoreTagInfo>> {
        let stream = self.store.tags().list_prefix(prefix.as_bytes()).await?;
        let mut results = Vec::new();
        let mut pinned = Box::pin(stream);
        while let Some(item) = pinned.next().await {
            results.push(CoreTagInfo::from_upstream(item?));
        }
        Ok(results)
    }

    /// List only HashSeq-format tags (collections).
    pub async fn tag_list_hash_seq(&self) -> Result<Vec<CoreTagInfo>> {
        let stream = self.store.tags().list_hash_seq().await?;
        let mut results = Vec::new();
        let mut pinned = Box::pin(stream);
        while let Some(item) = pinned.next().await {
            results.push(CoreTagInfo::from_upstream(item?));
        }
        Ok(results)
    }

    /// Return the status of a blob in the local store.
    pub async fn blob_status(&self, hash_hex: String) -> Result<CoreBlobStatus> {
        let hash = hash_hex.parse::<Hash>()?;
        let status = self.store.blobs().status(hash).await?;
        Ok(match status {
            iroh_blobs::api::proto::BlobStatus::NotFound => CoreBlobStatus::NotFound,
            iroh_blobs::api::proto::BlobStatus::Partial { size } => CoreBlobStatus::Partial {
                size: size.unwrap_or(0),
            },
            iroh_blobs::api::proto::BlobStatus::Complete { size } => {
                CoreBlobStatus::Complete { size }
            }
        })
    }

    /// Return true if the blob is fully stored locally.
    pub async fn blob_has(&self, hash_hex: String) -> Result<bool> {
        let hash = hash_hex.parse::<Hash>()?;
        Ok(self.store.blobs().has(hash).await?)
    }

    /// Snapshot of the current bitfield for a blob — is it complete, and what is its size?
    /// Returns the first bitfield emitted by `store.blobs().observe(hash)`.
    pub async fn blob_observe_snapshot(&self, hash_hex: String) -> Result<CoreBlobObserveResult> {
        let hash = hash_hex.parse::<Hash>()?;
        let bitfield = self.store.blobs().observe(hash).await?;
        Ok(CoreBlobObserveResult {
            is_complete: bitfield.is_complete(),
            size: bitfield.size(),
        })
    }

    /// Wait until the blob is fully downloaded locally.
    /// Resolves immediately if the blob is already complete.
    pub async fn blob_observe_complete(&self, hash_hex: String) -> Result<()> {
        let hash = hash_hex.parse::<Hash>()?;
        self.store.blobs().observe(hash).await_completion().await?;
        Ok(())
    }

    /// Check how many bytes of the blob we already have locally, and whether it is complete.
    /// Uses the Remote API which accounts for partial downloads.
    pub async fn blob_local_info(&self, hash_hex: String) -> Result<CoreBlobLocalInfo> {
        let hash = hash_hex.parse::<Hash>()?;
        let info = self.store.remote().local(HashAndFormat::raw(hash)).await?;
        Ok(CoreBlobLocalInfo {
            is_complete: info.is_complete(),
            local_bytes: info.local_bytes(),
        })
    }

    /// Create a ticket for a Collection (HashSeq format), compatible with sendme.
    pub fn create_collection_ticket(&self, hash_hex: String) -> Result<String> {
        Ok(BlobTicket::new(
            self.endpoint.addr(),
            hash_hex.parse::<Hash>()?,
            BlobFormat::HashSeq,
        )
        .encode_string())
    }

    pub async fn download_blob(&self, ticket_str: String) -> Result<Vec<u8>> {
        let ticket = BlobTicket::decode_string(&ticket_str)?;
        let hash = ticket.hash();
        let format = ticket.format();
        let (addr, _, _) = ticket.into_parts();
        if let Ok(lookup) = self.endpoint.address_lookup() {
            let mem = MemoryLookup::new();
            mem.add_endpoint_info(addr.clone());
            lookup.add(mem);
        }
        self.downloader.download(hash, vec![addr.id]).await?;

        // If it's a HashSeq (Collection), extract the file contents
        if format == BlobFormat::HashSeq {
            let collection = Collection::load(hash, &self.store).await?;
            // Concatenate all file contents (typically just one file for sendme)
            let mut result = Vec::new();
            for (_name, blob_hash) in collection.iter() {
                let bytes = self.store.get_bytes(*blob_hash).await?;
                result.extend_from_slice(&bytes);
            }
            Ok(result)
        } else {
            Ok(self.store.get_bytes(hash).await?.to_vec())
        }
    }

    /// Parse the legacy `format` string used across the bindings. Only the
    /// literal `"hash_seq"` selects HashSeq; anything else stays Raw.
    fn parse_blob_format(format: &str) -> BlobFormat {
        if format == "hash_seq" {
            BlobFormat::HashSeq
        } else {
            BlobFormat::Raw
        }
    }

    fn parse_providers(node_ids_hex: &[String]) -> Result<Vec<EndpointId>> {
        node_ids_hex
            .iter()
            .map(|id| id.parse::<EndpointId>().map_err(Into::into))
            .collect()
    }

    /// Read a blob back out of the store once it is resident.
    async fn read_resident(&self, hash: Hash, format: BlobFormat) -> Result<Vec<u8>> {
        if format == BlobFormat::HashSeq {
            let collection = Collection::load(hash, &self.store).await?;
            let mut result = Vec::new();
            for (_name, blob_hash) in collection.iter() {
                let bytes = self.store.get_bytes(*blob_hash).await?;
                result.extend_from_slice(&bytes);
            }
            Ok(result)
        } else {
            Ok(self.store.get_bytes(hash).await?.to_vec())
        }
    }

    /// Fetch `haf` from `providers` **in the given order**, reporting what each
    /// provider did.
    ///
    /// Ordered failover, resumption (each attempt asks only for what is still
    /// missing) and child splitting are the downloader's own behaviour; this
    /// consumes its progress stream to build the report rather than awaiting
    /// the future and discarding it.
    async fn run_download(
        &self,
        haf: HashAndFormat,
        providers: Vec<EndpointId>,
        strategy: CoreFetchStrategy,
    ) -> Result<CoreFetchReport> {
        if providers.is_empty() {
            return Err(anyhow!(
                "no providers supplied for download of {}",
                haf.hash
            ));
        }
        if strategy == CoreFetchStrategy::SplitChildren && haf.format != BlobFormat::HashSeq {
            return Err(anyhow!(
                "split-children fetch needs a hash_seq blob, got raw for {}",
                haf.hash
            ));
        }
        let split = match strategy {
            CoreFetchStrategy::Whole => SplitStrategy::None,
            CoreFetchStrategy::SplitChildren => SplitStrategy::Split,
        };
        let mut stream = self
            .downloader
            .download_with_opts(DownloadOptions::new(haf, providers, split))
            .stream()
            .await?;
        let mut acc = FetchAccumulator::default();
        while let Some(item) = futures_lite::StreamExt::next(&mut stream).await {
            acc.observe(item);
        }
        acc.finish(haf.hash)
    }

    /// Download a blob by hash from an **ordered** list of providers, returning
    /// the bytes and a report of what each provider did. Providers are tried in
    /// order; each attempt asks only for the ranges still missing.
    pub async fn download_hash_multi(
        &self,
        hash_hex: String,
        node_ids_hex: Vec<String>,
        format: String,
        strategy: CoreFetchStrategy,
    ) -> Result<(Vec<u8>, CoreFetchReport)> {
        let hash: Hash = hash_hex.parse()?;
        let blob_format = Self::parse_blob_format(&format);
        let providers = Self::parse_providers(&node_ids_hex)?;
        let report = self
            .run_download(
                HashAndFormat {
                    hash,
                    format: blob_format,
                },
                providers,
                strategy,
            )
            .await?;
        Ok((self.read_resident(hash, blob_format).await?, report))
    }

    /// As [`Self::download_hash_multi`], but leaves the blob in the store
    /// without reading it back — see [`Self::download_hash_to_store`] for why
    /// that matters.
    pub async fn download_hash_to_store_multi(
        &self,
        hash_hex: String,
        node_ids_hex: Vec<String>,
        format: String,
        strategy: CoreFetchStrategy,
    ) -> Result<CoreFetchReport> {
        let hash: Hash = hash_hex.parse()?;
        let providers = Self::parse_providers(&node_ids_hex)?;
        self.run_download(
            HashAndFormat {
                hash,
                format: Self::parse_blob_format(&format),
            },
            providers,
            strategy,
        )
        .await
    }

    /// As [`Self::download_hash_multi`], for a collection: returns the
    /// (name, data) pairs plus the fetch report.
    pub async fn download_collection_hash_multi(
        &self,
        hash_hex: String,
        node_ids_hex: Vec<String>,
        strategy: CoreFetchStrategy,
    ) -> Result<(Vec<(String, Vec<u8>)>, CoreFetchReport)> {
        let hash: Hash = hash_hex.parse()?;
        let providers = Self::parse_providers(&node_ids_hex)?;
        let report = self
            .run_download(HashAndFormat::hash_seq(hash), providers, strategy)
            .await?;
        Ok((self.load_collection(hash).await?, report))
    }

    async fn load_collection(&self, hash: Hash) -> Result<Vec<(String, Vec<u8>)>> {
        let collection = Collection::load(hash, &self.store).await?;
        let mut files = Vec::new();
        for (name, blob_hash) in collection.iter() {
            let bytes = self.store.get_bytes(*blob_hash).await?;
            files.push((name.clone(), bytes.to_vec()));
        }
        Ok(files)
    }

    /// Download a blob by hash from a specific node, bypassing ticket parsing.
    /// `format` should be "raw" or "hash_seq".
    pub async fn download_hash(
        &self,
        hash_hex: String,
        node_id_hex: String,
        format: String,
    ) -> Result<Vec<u8>> {
        let (bytes, _report) = self
            .download_hash_multi(
                hash_hex,
                vec![node_id_hex],
                format,
                CoreFetchStrategy::Whole,
            )
            .await?;
        Ok(bytes)
    }

    /// Download a blob by hash from a specific node **into the local store
    /// without reading it back into memory**. Use this when the caller only
    /// needs the blob resident locally (e.g. to serve it later via ranged
    /// reads) and would otherwise discard the returned bytes: unlike
    /// [`Self::download_hash`], this skips the trailing
    /// `store.get_bytes(hash).to_vec()` round-trip, which materializes the
    /// whole blob twice in memory (the `Bytes` plus its `Vec` clone) — a ~2×
    /// transient RSS spike on large blobs. The `Downloader` streams the blob
    /// to the on-disk store, so this returns only once the blob is resident.
    pub async fn download_hash_to_store(
        &self,
        hash_hex: String,
        node_id_hex: String,
        format: String,
    ) -> Result<()> {
        self.download_hash_to_store_multi(
            hash_hex,
            vec![node_id_hex],
            format,
            CoreFetchStrategy::Whole,
        )
        .await?;
        Ok(())
    }

    /// Download a collection by hash from a specific node, returning (name, data) pairs.
    pub async fn download_collection_hash(
        &self,
        hash_hex: String,
        node_id_hex: String,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let (files, _report) = self
            .download_collection_hash_multi(hash_hex, vec![node_id_hex], CoreFetchStrategy::Whole)
            .await?;
        Ok(files)
    }

    /// Download a collection and return list of (name, data) pairs.
    pub async fn download_collection(&self, ticket_str: String) -> Result<Vec<(String, Vec<u8>)>> {
        let ticket = BlobTicket::decode_string(&ticket_str)?;
        let hash = ticket.hash();
        let (addr, _, _) = ticket.into_parts();
        if let Ok(lookup) = self.endpoint.address_lookup() {
            let mem = MemoryLookup::new();
            mem.add_endpoint_info(addr.clone());
            lookup.add(mem);
        }
        self.downloader.download(hash, vec![addr.id]).await?;

        let collection = Collection::load(hash, &self.store).await?;
        let mut files = Vec::new();
        for (name, blob_hash) in collection.iter() {
            let bytes = self.store.get_bytes(*blob_hash).await?;
            files.push((name.clone(), bytes.to_vec()));
        }
        Ok(files)
    }
}

// ============================================================================
// CoreDocsClient - Document sync protocol
// ============================================================================

/// Release a document's two references under one absolute deadline.
///
/// Split out from [`CoreDoc::shutdown_for_role_swap`] so the sequencing can be
/// tested with failures injected: `leave` cannot be made to fail through the
/// public API — it is idempotent and succeeds even when nothing is syncing — so
/// the "close runs anyway" property would otherwise be untestable, and an
/// untested property in cleanup code is one that quietly stops holding.
///
/// One deadline, not one per step: a caller narrowing a role has a transition
/// budget, and two sequential budgets would silently double it.
async fn release_both_references(
    budget: Duration,
    leave: impl std::future::Future<Output = anyhow::Result<()>>,
    close: impl std::future::Future<Output = anyhow::Result<()>>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;

    let left = match tokio::time::timeout_at(deadline, leave).await {
        Ok(Ok(())) => Ok(()),
        // `context` rather than a formatted string: the original error keeps
        // its `source()` chain, so a caller can still walk to the root cause
        // instead of receiving a flattened top line.
        Ok(Err(e)) => Err(e.context("leave failed")),
        Err(_) => Err(anyhow!("leave timed out")),
    };
    // Deliberately not short-circuited on `left`. A failed leave is precisely
    // when the handle most needs releasing — abandoning the close there would
    // leave the reference the whole operation exists to drop.
    let closed = match tokio::time::timeout_at(deadline, close).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.context("close failed")),
        Err(_) => Err(anyhow!("close timed out")),
    };

    match (left, closed) {
        (Ok(()), Ok(())) => Ok(()),
        // One failure: returned as-is, so its `source()` chain survives intact.
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
        // Both: `anyhow::Error` carries one chain, and dropping either failure
        // would hide a fault — a cleanup failure is a different and worse one
        // than what preceded it. So the close failure keeps its structural
        // chain and the leave failure is rendered with `{:#}`, which prints its
        // full chain rather than only the top line. Both causes are readable;
        // only one is walkable.
        (Err(l), Err(c)) => Err(c.context(format!("after leave failed: {l:#}"))),
    }
}

#[derive(Clone)]
pub struct CoreDocsClient {
    pub inner: Docs,
    pub store: BlobStore,
    pub endpoint: Endpoint,
}

impl CoreDocsClient {
    pub async fn create(&self) -> Result<CoreDoc> {
        Ok(CoreDoc {
            doc: self.inner.api().create().await?,
            store: self.store.clone(),
        })
    }

    pub async fn create_author(&self) -> Result<String> {
        Ok(self.inner.api().author_create().await?.to_string())
    }

    /// Return the node-wide default author (hex). On a persistent node this
    /// author is created on first start and its key is saved in the data
    /// directory, so it is stable across restarts — the recommended "current
    /// node author" for callers that don't manage their own authors.
    pub async fn default_author(&self) -> Result<String> {
        Ok(self.inner.api().author_default().await?.to_string())
    }

    /// Open-or-import a **writable** namespace from a deterministic 32-byte
    /// secret (e.g. a BLAKE3 seed). The namespace id is derived from the secret
    /// (`doc.id() == NamespaceSecret::id()`), so the same secret resolves to the
    /// same namespace across peers and restarts.
    ///
    /// Idempotent and capability-upgrading: if the namespace is absent it is
    /// imported with write capability; if it is already present **read-only**
    /// (a prior [`Self::import_read_namespace`]) the stored capability is
    /// upgraded to write and a writable [`CoreDoc`] is returned; if already
    /// writable it simply reattaches. Backed by iroh-docs `import_namespace`,
    /// which merges capabilities in-place.
    pub async fn import_write_namespace(&self, secret_bytes: Vec<u8>) -> Result<CoreDoc> {
        if secret_bytes.len() != 32 {
            return Err(anyhow!("namespace secret must be 32 bytes"));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&secret_bytes);
        let capability = Capability::Write(NamespaceSecret::from_bytes(&arr));
        Ok(CoreDoc {
            doc: self.inner.api().import_namespace(capability).await?,
            store: self.store.clone(),
        })
    }

    /// Open-or-import a **read-only** namespace from its public 32-byte id.
    /// Idempotent; never downgrades an existing write capability to read.
    pub async fn import_read_namespace(&self, namespace_id_bytes: Vec<u8>) -> Result<CoreDoc> {
        if namespace_id_bytes.len() != 32 {
            return Err(anyhow!("namespace id must be 32 bytes"));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&namespace_id_bytes);
        let capability = Capability::Read(NamespaceId::from(arr));
        Ok(CoreDoc {
            doc: self.inner.api().import_namespace(capability).await?,
            store: self.store.clone(),
        })
    }

    /// Retire this node's **local write authority** over a namespace, keeping
    /// every entry.
    ///
    /// Local only: it revokes nothing cryptographically and reaches no other
    /// node — a peer holding the namespace secret still writes. What it retires
    /// is this node's own ability to sign.
    ///
    /// **Release every reference first.** A live-sync reference and each caller
    /// handle both count; until all are gone this reports
    /// [`CoreRetireOutcome::Busy`] with the number remaining rather than
    /// proceeding. [`CoreDoc::shutdown_for_role_swap`] releases both under one
    /// deadline.
    ///
    /// Returns the access now **stored**, so a caller verifies the outcome
    /// rather than trusting that its request was applied. Idempotent, and a
    /// later [`Self::import_write_namespace`] widens again.
    pub async fn retire_local_write_authority(
        &self,
        namespace_id_bytes: Vec<u8>,
    ) -> Result<CoreRetireOutcome> {
        if namespace_id_bytes.len() != 32 {
            return Err(anyhow!("namespace id must be 32 bytes"));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&namespace_id_bytes);
        Ok(self
            .inner
            .api()
            .retire_write_authority(NamespaceId::from(arr))
            .await?
            .into())
    }

    /// Open an existing local namespace by its ID (hex), without a ticket or a
    /// peer to sync from. Returns `None` if the namespace is not present in the
    /// local replica store.
    ///
    /// This is the access path for a persistent node after restart: the
    /// namespace/authors/entries written before [`CoreNode::close`] are reloaded
    /// from `docs.redb`, and `open` hands back a usable [`CoreDoc`].
    pub async fn open(&self, namespace_id_hex: String) -> Result<Option<CoreDoc>> {
        let ns_bytes = hex::decode(&namespace_id_hex)?;
        if ns_bytes.len() != 32 {
            return Err(anyhow!("namespace_id must be 32 bytes (64 hex chars)"));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&ns_bytes);
        let ns_id = NamespaceId::from(arr);
        // Upstream `DocsApi::open` propagates a "Replica not found" error for an
        // unknown namespace instead of returning `Ok(None)` (its `Option` is
        // vestigial — it only ever yields `Some`). To honour our documented
        // `None`-on-missing contract, an error is reconciled against `list()`:
        // a genuinely absent namespace maps to `Ok(None)`, any other failure is
        // re-raised. The happy path stays a single `open()` call (no scan).
        match self.inner.api().open(ns_id).await {
            Ok(opt) => Ok(opt.map(|doc| CoreDoc {
                doc,
                store: self.store.clone(),
            })),
            Err(err) => {
                if self.namespace_exists(ns_id).await? {
                    Err(err)
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Whether `ns_id` is present in the local replica store. Used to
    /// disambiguate a "not found" open from a real error (see [`Self::open`]).
    async fn namespace_exists(&self, ns_id: NamespaceId) -> Result<bool> {
        Ok(self.stored_access(ns_id).await?.is_some())
    }

    /// The access **stored** for `ns_id`, or `None` if the namespace is not in
    /// the local replica store.
    ///
    /// This is the one place that reads authority back out of the store, and it
    /// is deliberately not parameterised by what the caller asked for: the whole
    /// point is to report what the store holds after any import/merge, not to
    /// restate a request. `list()` is the only read-only capability
    /// introspection the document layer exposes, so it costs one pass over the
    /// local namespaces; a targeted lookup would need a change in that
    /// dependency and is not worth one until measurement says so.
    async fn stored_access(&self, ns_id: NamespaceId) -> Result<Option<CoreLocalAccess>> {
        let mut stream = self.inner.api().list().await?;
        while let Some(item) = stream.next().await {
            let (id, kind) = item?;
            if id == ns_id {
                return Ok(Some(kind.into()));
            }
        }
        Ok(None)
    }

    /// Report the authority this node actually holds over `namespace_id_bytes`.
    ///
    /// Fallible on purpose, and absent-means-error rather than
    /// `Ok(None)`: a caller asks this to decide whether it may serve a
    /// namespace, so "cannot prove" and "proved read-only" must not arrive in
    /// the same shape. Returning the requested mode, or an echo of the caller's
    /// own capability bytes, would answer a different question than the one
    /// being asked.
    pub async fn authority_report(
        &self,
        namespace_id_bytes: Vec<u8>,
    ) -> Result<CoreAuthorityReport> {
        if namespace_id_bytes.len() != 32 {
            return Err(anyhow!("namespace id must be 32 bytes"));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&namespace_id_bytes);
        let ns_id = NamespaceId::from(arr);
        match self.stored_access(ns_id).await? {
            Some(access) => Ok(CoreAuthorityReport {
                namespace: arr,
                access,
            }),
            None => Err(anyhow!(
                "namespace {ns_id} is not in the local replica store, so its \
                 authority cannot be reported"
            )),
        }
    }

    pub async fn join(&self, ticket_str: String) -> Result<CoreDoc> {
        let ticket = DocTicket::decode_string(&ticket_str)?;
        if let Ok(lookup) = self.endpoint.address_lookup() {
            for node_addr in &ticket.nodes {
                let mem = MemoryLookup::new();
                mem.add_endpoint_info(node_addr.clone());
                lookup.add(mem);
            }
        }
        Ok(CoreDoc {
            // The ticket carries replica addresses; importing only its
            // capability creates an empty local document without syncing.
            doc: self.inner.api().import(ticket).await?,
            store: self.store.clone(),
        })
    }

    /// Join a document and subscribe to live events in one atomic step.
    /// Returns a `(CoreDoc, CoreDocEventReceiver)` pair.
    pub async fn join_and_subscribe(
        &self,
        ticket_str: String,
    ) -> Result<(CoreDoc, CoreDocEventReceiver)> {
        let ticket = DocTicket::decode_string(&ticket_str)?;
        if let Ok(lookup) = self.endpoint.address_lookup() {
            for node_addr in &ticket.nodes {
                let mem = MemoryLookup::new();
                mem.add_endpoint_info(node_addr.clone());
                lookup.add(mem);
            }
        }
        let (doc, stream) = self.inner.api().import_and_subscribe(ticket).await?;
        let core_doc = CoreDoc {
            doc,
            store: self.store.clone(),
        };
        let receiver = CoreDocEventReceiver {
            inner: Arc::new(Mutex::new(Box::pin(stream))),
        };
        Ok((core_doc, receiver))
    }

    /// Join a doc by namespace ID (hex) without a full DocTicket.
    ///
    /// `peer_node_id_hex` is the endpoint ID of the peer to sync from.
    /// Constructs a DocTicket internally from the namespace ID + peer address,
    /// avoiding the DocTicket serialization/deserialization round-trip on the wire.
    pub async fn join_and_subscribe_namespace(
        &self,
        namespace_id_hex: String,
        peer_node_id_hex: String,
    ) -> Result<(CoreDoc, CoreDocEventReceiver)> {
        let ns_bytes = hex::decode(&namespace_id_hex)?;
        if ns_bytes.len() != 32 {
            return Err(anyhow!("namespace_id must be 32 bytes (64 hex chars)"));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&ns_bytes);
        let ns_id = NamespaceId::from(arr);
        let capability = Capability::Read(ns_id);

        // Look up the peer's full address info from the endpoint.
        let peer_id: EndpointId = peer_node_id_hex.parse()?;

        // Construct a DocTicket with the peer's address so the docs
        // protocol can find and sync from it.
        let peer_addr = if let Some(info) = self.endpoint.remote_info(peer_id).await {
            let addr =
                EndpointAddr::from_parts(info.id(), info.into_addrs().map(|a| a.into_addr()));
            if let Ok(lookup) = self.endpoint.address_lookup() {
                let mem = MemoryLookup::new();
                mem.add_endpoint_info(addr.clone());
                lookup.add(mem);
            }
            addr
        } else {
            // Fallback: use the peer's ID with our relay URL so the docs
            // protocol can at least try reaching the peer via relay.
            let our_addr = self.endpoint.addr();
            let mut addrs: Vec<TransportAddr> = Vec::new();
            for url in our_addr.relay_urls() {
                addrs.push(TransportAddr::Relay(url.clone()));
            }
            EndpointAddr::from_parts(peer_id, addrs)
        };

        let ticket = DocTicket::new(capability, vec![peer_addr]);
        let (doc, stream) = self.inner.api().import_and_subscribe(ticket).await?;

        let core_doc = CoreDoc {
            doc,
            store: self.store.clone(),
        };
        let receiver = CoreDocEventReceiver {
            inner: Arc::new(Mutex::new(Box::pin(stream))),
        };
        Ok((core_doc, receiver))
    }
}

// ============================================================================
// CoreDoc - Single document instance
// ============================================================================

/// What access a namespace grants this node locally.
///
/// Aster's own type. The underlying document layer has an equivalent, but
/// leaking it would put a dependency's enum in Aster's public API — and callers
/// would then have to track that crate's versioning to match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreLocalAccess {
    /// This node can write to the namespace.
    Write,
    /// This node can read it and nothing more.
    Read,
}

impl From<CapabilityKind> for CoreLocalAccess {
    fn from(kind: CapabilityKind) -> Self {
        match kind {
            CapabilityKind::Write => CoreLocalAccess::Write,
            CapabilityKind::Read => CoreLocalAccess::Read,
        }
    }
}

impl CoreLocalAccess {
    /// The byte this access contributes to a fingerprint.
    ///
    /// Declared here rather than reusing the document layer's discriminants, so
    /// a renumbering upstream cannot silently change a value Aster has already
    /// published as stable.
    const fn fingerprint_byte(self) -> u8 {
        match self {
            CoreLocalAccess::Write => 1,
            CoreLocalAccess::Read => 2,
        }
    }
}

/// The domain separator for [`CoreAuthorityFingerprint`].
///
/// Versioned in the string itself: a future capability model that makes the
/// fingerprint carry more than `(namespace, access)` gets `/v2`, and an old
/// value can never collide with a new one.
const AUTHORITY_FINGERPRINT_DOMAIN: &[u8] = b"aster-docs/authority-fingerprint/v1\0";

/// A stable, opaque correlation value over `(namespace, stored access)`.
///
/// **This is a correlation token, not evidence.** Under the current capability
/// model a namespace ID *is* the namespace public key, and a write capability
/// is the single secret behind it — so `(namespace, access)` already identifies
/// every capability a node can hold for that namespace, and this hash adds no
/// information to comparing those two fields directly. It exists because a
/// caller comparing a grant record with an opened runtime wants one value to
/// compare rather than two, and because a future capability type could carry
/// material that this pair does not determine. It is emphatically not a proof
/// over secret material: Aster cannot read the stored secret back, and a
/// fingerprint that claimed to cover it would be a lie a caller might rely on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CoreAuthorityFingerprint([u8; 32]);

impl CoreAuthorityFingerprint {
    /// Fingerprint a namespace and access pair.
    ///
    /// Takes raw namespace bytes rather than the document layer's id type, so
    /// no caller has to name that dependency to derive the same value from its
    /// own grant record.
    pub fn of(namespace: [u8; 32], access: CoreLocalAccess) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(AUTHORITY_FINGERPRINT_DOMAIN);
        hasher.update(&namespace);
        hasher.update(&[access.fingerprint_byte()]);
        Self(*hasher.finalize().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// What a node actually holds over a namespace, read back from the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreAuthorityReport {
    namespace: [u8; 32],
    access: CoreLocalAccess,
}

impl CoreAuthorityReport {
    /// The namespace actually opened, as raw bytes — the document layer's id
    /// type stays out of this crate's public surface.
    pub fn namespace(&self) -> [u8; 32] {
        self.namespace
    }

    /// The access **stored** for it — never the access that was requested.
    ///
    /// Importing a read capability over an existing write one does not narrow
    /// it, so a read request can legitimately report [`CoreLocalAccess::Write`].
    pub fn access(&self) -> CoreLocalAccess {
        self.access
    }

    /// This report's correlation fingerprint. See [`CoreAuthorityFingerprint`]
    /// for what it does and does not prove.
    pub fn fingerprint(&self) -> CoreAuthorityFingerprint {
        CoreAuthorityFingerprint::of(self.namespace, self.access)
    }
}

/// The result of retiring local write authority.
///
/// `#[must_use]` because `Busy` is an `Ok` value: `retire(..).await?;` would
/// otherwise discard a fail-closed outcome and read as success.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreRetireOutcome {
    /// Retired; this is the access now **stored**, not the access requested.
    Retired(CoreLocalAccess),
    /// Refused: references to the document are still open.
    ///
    /// Both a live-sync reference and each caller handle count. Unexpected
    /// handles usually mean the caller does not own what it thought it owned,
    /// so this is normally a fail-closed condition rather than one to retry
    /// through.
    Busy {
        /// How many references remain.
        handles: u64,
    },
}

impl From<RetireWriteAuthorityOutcome> for CoreRetireOutcome {
    fn from(outcome: RetireWriteAuthorityOutcome) -> Self {
        match outcome {
            RetireWriteAuthorityOutcome::Retired(kind) => CoreRetireOutcome::Retired(kind.into()),
            RetireWriteAuthorityOutcome::Busy { handles } => CoreRetireOutcome::Busy { handles },
        }
    }
}

#[derive(Clone)]
pub struct CoreDoc {
    pub doc: Doc,
    pub store: BlobStore,
}

impl CoreDoc {
    pub fn doc_id(&self) -> String {
        self.doc.id().to_string()
    }

    pub async fn set_bytes(
        &self,
        author_hex: String,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<String> {
        let author_id: AuthorId = author_hex.parse()?;
        Ok(self
            .doc
            .set_bytes(author_id, Bytes::from(key), Bytes::from(value))
            .await?
            .to_hex()
            .to_string())
    }

    /// Write a key/value only after its blob metadata is durable.
    ///
    /// Unlike [`Self::set_bytes`], this orders the blob-store commit before
    /// iroh-docs inserts the record that names it. A hard process kill after
    /// this call returns therefore cannot leave a durable document record
    /// whose locally-authored value reopens as missing.
    pub async fn set_bytes_durable(
        &self,
        author_hex: String,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<String> {
        let author_id: AuthorId = author_hex.parse()?;
        Ok(self
            .doc
            .set_bytes_durable(author_id, Bytes::from(key), Bytes::from(value))
            .await?
            .to_hex()
            .to_string())
    }

    /// Delete all entries for `author_hex` whose key starts with `prefix`.
    ///
    /// iroh-docs implements this by writing a deletion marker at `prefix`.
    /// Current-value queries omit entries hidden by that marker.
    pub async fn del(&self, author_hex: String, prefix: Vec<u8>) -> Result<u64> {
        let author_id: AuthorId = author_hex.parse()?;
        Ok(self.doc.del(author_id, Bytes::from(prefix)).await? as u64)
    }

    /// Query all entries for an exact key, across all authors.
    /// Returns a list of CoreDocEntry with metadata (author, content hash, timestamp, etc.)
    ///
    /// iroh-docs stores one entry per (author, key) and orders the result by author id
    /// (ascending), NOT by timestamp — so a `limit` truncates by author id, which can
    /// exclude the most-recent writer. Use this only when you genuinely need every
    /// author's entry (e.g. to apply a trust filter before picking a winner); pass
    /// `None` for an unbounded read. To read the single latest value of a key, use
    /// [`CoreDoc::query_latest_exact`] instead.
    pub async fn query_key_exact(
        &self,
        key: Vec<u8>,
        limit: Option<u64>,
    ) -> Result<Vec<CoreDocEntry>> {
        let mut builder = Query::key_exact(key);
        if let Some(limit) = limit {
            builder = builder.limit(limit);
        }
        let query = builder.build();
        Ok(self
            .doc
            .get_many_vec(query)
            .await?
            .into_iter()
            .map(|entry| CoreDocEntry {
                author_id: entry.author().to_string(),
                key: entry.key().to_vec(),
                content_hash: entry.content_hash().to_hex().to_string(),
                content_len: entry.content_len(),
                timestamp: entry.timestamp(),
            })
            .collect())
    }

    /// Read the single latest entry for an exact key, across all authors.
    ///
    /// Asks the store to collapse the per-author entries to the one with the highest
    /// timestamp (iroh-docs' `single_latest_per_key`), so the result is independent of
    /// how many authors have written the key — the most-recent writer always wins.
    /// Returns `None` if the key was never written, or if the latest write was a
    /// deletion (tombstone). This is the correct primitive for reading the current
    /// value of a key; [`CoreDoc::query_key_exact`] is for enumerating all authors.
    pub async fn query_latest_exact(&self, key: Vec<u8>) -> Result<Option<CoreDocEntry>> {
        let query = Query::single_latest_per_key().key_exact(key).build();
        Ok(self
            .doc
            .get_many_vec(query)
            .await?
            .into_iter()
            .next()
            .map(|entry| CoreDocEntry {
                author_id: entry.author().to_string(),
                key: entry.key().to_vec(),
                content_hash: entry.content_hash().to_hex().to_string(),
                content_len: entry.content_len(),
                timestamp: entry.timestamp(),
            }))
    }

    /// Query all entries matching a key prefix, across all authors.
    /// Returns a list of CoreDocEntry with metadata (author, content hash, timestamp, etc.)
    ///
    /// A prefix query is a listing: every matching key is distinct, so there is no
    /// natural small bound. `limit` is the caller's responsibility — pass `Some(n)` to
    /// cap the result set (e.g. for pagination), or `None` for an unbounded listing.
    pub async fn query_key_prefix(
        &self,
        prefix: Vec<u8>,
        limit: Option<u64>,
    ) -> Result<Vec<CoreDocEntry>> {
        let mut builder = Query::key_prefix(prefix);
        if let Some(limit) = limit {
            builder = builder.limit(limit);
        }
        let query = builder.build();
        Ok(self
            .doc
            .get_many_vec(query)
            .await?
            .into_iter()
            .map(|entry| CoreDocEntry {
                author_id: entry.author().to_string(),
                key: entry.key().to_vec(),
                content_hash: entry.content_hash().to_hex().to_string(),
                content_len: entry.content_len(),
                timestamp: entry.timestamp(),
            })
            .collect())
    }

    /// List the latest entry for each distinct key under a prefix, across all authors.
    ///
    /// This is the listing counterpart to [`CoreDoc::query_latest_exact`]: where
    /// [`query_key_prefix`](CoreDoc::query_key_prefix) returns one row per (author, key)
    /// — so a key written by N authors appears N times — this collapses each key to its
    /// single highest-timestamp entry (iroh-docs' `single_latest_per_key`). Keys whose
    /// latest write was a deletion are omitted. Use it for directory-style listings,
    /// where duplicate keys would be a bug. `limit` caps the number of distinct keys
    /// returned; pass `None` for an unbounded listing.
    ///
    /// Note: this collapses by document timestamp. Do NOT use it where freshness is
    /// tracked by an in-payload sequence number (e.g. endpoint leases); enumerate with
    /// [`query_key_prefix`](CoreDoc::query_key_prefix) and dedupe on that field instead.
    pub async fn query_latest_prefix(
        &self,
        prefix: Vec<u8>,
        limit: Option<u64>,
    ) -> Result<Vec<CoreDocEntry>> {
        let mut builder = Query::single_latest_per_key().key_prefix(prefix);
        if let Some(limit) = limit {
            builder = builder.limit(limit);
        }
        let query = builder.build();
        Ok(self
            .doc
            .get_many_vec(query)
            .await?
            .into_iter()
            .map(|entry| CoreDocEntry {
                author_id: entry.author().to_string(),
                key: entry.key().to_vec(),
                content_hash: entry.content_hash().to_hex().to_string(),
                content_len: entry.content_len(),
                timestamp: entry.timestamp(),
            })
            .collect())
    }

    /// Read the content bytes for a given content hash.
    /// This can be used after querying entries to fetch the actual value.
    pub async fn read_entry_content(&self, content_hash_hex: String) -> Result<Vec<u8>> {
        let hash: Hash = content_hash_hex.parse()?;
        Ok(self.store.get_bytes(hash).await?.to_vec())
    }

    /// Read multiple document entry value blobs that are complete locally.
    ///
    /// Output length/order match the inputs, including duplicates. Missing or
    /// partial blobs at the local completeness check return `None`; a complete
    /// empty blob returns `Some(Vec::new())`. Empty input returns an empty list.
    /// Every hash must contain exactly 64 ASCII hexadecimal characters and is
    /// validated before any store read; malformed input fails the
    /// entire call. No network fetch, tag creation or retention is performed.
    ///
    /// Complete inline values use the store's batched read transaction. Larger
    /// values fall back to validated export. This is not a global snapshot: GC
    /// or local availability may change between check and export. Export,
    /// integrity and storage errors fail the entire call, rather than becoming
    /// `None` or partial results. Use blob custody when stable retention is
    /// needed across reads and host verification.
    pub async fn read_entry_contents_if_complete(
        &self,
        content_hash_hexes: Vec<String>,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let mut hashes = Vec::with_capacity(content_hash_hexes.len());
        for hash_hex in content_hash_hexes {
            // The upstream parser assumes a correctly sized decode buffer for
            // non-hex input. Validate the public hex contract before calling it.
            anyhow::ensure!(
                hash_hex.len() == 64 && hash_hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "content hash must contain exactly 64 ASCII hexadecimal characters"
            );
            hashes.push(hash_hex.to_ascii_lowercase().parse::<Hash>()?);
        }

        let results = self
            .store
            .blobs()
            .get_bytes_many_if_complete(hashes)
            .await?;
        let mut out = Vec::with_capacity(results.len());
        for result in results {
            out.push(result?.map(|bytes| bytes.to_vec()));
        }
        Ok(out)
    }

    /// Return the local storage status for a document entry value blob.
    pub async fn entry_content_status(&self, content_hash_hex: String) -> Result<CoreBlobStatus> {
        let hash: Hash = content_hash_hex.parse()?;
        let status = self.store.blobs().status(hash).await?;
        Ok(match status {
            iroh_blobs::api::proto::BlobStatus::NotFound => CoreBlobStatus::NotFound,
            iroh_blobs::api::proto::BlobStatus::Partial { size } => CoreBlobStatus::Partial {
                size: size.unwrap_or(0),
            },
            iroh_blobs::api::proto::BlobStatus::Complete { size } => {
                CoreBlobStatus::Complete { size }
            }
        })
    }

    pub async fn get_exact(&self, author_hex: String, key: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let author_id: AuthorId = author_hex.parse()?;
        match self.doc.get_exact(author_id, key, false).await? {
            Some(entry) => Ok(Some(
                self.store.get_bytes(entry.content_hash()).await?.to_vec(),
            )),
            None => Ok(None),
        }
    }

    /// Whether an entry exists for an exact `(author, key)`, INCLUDING a deletion tombstone
    /// (an empty entry left by [`del`](Self::del)). Unlike [`get_exact`](Self::get_exact) — which
    /// passes `include_empty = false` and so reports a tombstone as absent — this reads with
    /// `include_empty = true` and never fetches content. It exists for insert-if-absent callers
    /// that must treat a revoked key as permanently taken, so a tombstoned id is never resurrected.
    pub async fn entry_exists(&self, author_hex: String, key: Vec<u8>) -> Result<bool> {
        let author_id: AuthorId = author_hex.parse()?;
        Ok(self.doc.get_exact(author_id, key, true).await?.is_some())
    }

    pub async fn share(&self, mode: String) -> Result<String> {
        let share_mode = match mode.as_str() {
            "read" | "Read" => ShareMode::Read,
            "write" | "Write" => ShareMode::Write,
            _ => return Err(anyhow!("mode must be 'read' or 'write'")),
        };
        Ok(self
            .doc
            .share(share_mode, AddrInfoOptions::Id)
            .await?
            .encode_string())
    }

    /// Start syncing this document with the given peers (by endpoint ID hex string).
    pub async fn start_sync(&self, peers: Vec<String>) -> Result<()> {
        let endpoint_addrs: Vec<EndpointAddr> = peers
            .iter()
            .map(|s| {
                let id: EndpointId = s.parse()?;
                Ok(EndpointAddr::from_parts(id, std::iter::empty()))
            })
            .collect::<Result<Vec<_>>>()?;
        self.doc.start_sync(endpoint_addrs).await?;
        Ok(())
    }

    /// Synchronize with every selected replica before trusting local policy state.
    /// Failed syncs and a closed event stream fail closed. This waits for metadata
    /// reconciliation; callers must still require complete content when reading.
    pub async fn sync_with_timeout(&self, peers: Vec<String>, budget: Duration) -> Result<()> {
        let mut pending = peers
            .into_iter()
            .map(|peer| peer.parse::<EndpointId>())
            .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
        if pending.is_empty() {
            return Err(anyhow!("policy publication requires at least one replica"));
        }
        tokio::time::timeout(budget, async {
            let events = self.doc.subscribe().await?;
            futures_lite::pin!(events);
            self.start_sync(pending.iter().map(ToString::to_string).collect())
                .await?;
            while !pending.is_empty() {
                match futures_lite::StreamExt::next(&mut events).await {
                    Some(Ok(LiveEvent::SyncFinished(event))) if pending.contains(&event.peer) => {
                        event
                            .result
                            .map_err(|error| anyhow!("policy replica sync failed: {error}"))?;
                        pending.remove(&event.peer);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error),
                    None => return Err(anyhow!("policy replica sync stream ended")),
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| anyhow!("policy replica sync deadline exceeded; publication refused"))?
    }

    /// Stop syncing this document.
    pub async fn leave(&self) -> Result<()> {
        self.doc.leave().await?;
        Ok(())
    }

    /// Release **both** of this document's references, so its namespace can
    /// change role.
    ///
    /// Consuming, because it invalidates the handle and every clone of it: the
    /// underlying close sets a shared flag, so returning `self` would hand back
    /// something permanently unusable. A caller that still holds a clone will
    /// find it dead too, which is the point — role swap must not leave a live
    /// reference behind.
    ///
    /// `leave` releases the live-sync reference, `close` releases this handle's
    /// own. Both are needed: retirement counts references, not intentions, and
    /// releasing one leaves the other holding the namespace open.
    ///
    /// **One absolute deadline covers both steps**, not one budget each — a
    /// caller narrowing a role has a transition budget, and two sequential
    /// budgets would silently double it.
    ///
    /// Close is attempted even when `leave` fails, because a failed leave is
    /// precisely when the handle most needs releasing. If both fail, both are
    /// reported: the close failure keeps its structural `source()` chain and
    /// the leave failure is rendered with its full chain into the message.
    pub async fn shutdown_for_role_swap(self, budget: Duration) -> Result<()> {
        release_both_references(budget, self.doc.leave(), self.doc.close()).await
    }

    /// Subscribe to live document events.
    pub async fn subscribe(&self) -> Result<CoreDocEventReceiver> {
        let stream = self.doc.subscribe().await?;
        Ok(CoreDocEventReceiver {
            inner: Arc::new(Mutex::new(Box::pin(stream))),
        })
    }

    /// Set the download policy for this document.
    pub async fn set_download_policy(&self, policy: CoreDownloadPolicy) -> Result<()> {
        self.doc
            .set_download_policy(core_policy_to_iroh(policy))
            .await?;
        Ok(())
    }

    /// Get the current download policy for this document.
    pub async fn get_download_policy(&self) -> Result<CoreDownloadPolicy> {
        let policy = self.doc.get_download_policy().await?;
        Ok(iroh_policy_to_core(policy))
    }

    /// Share this document with full relay+address info, returning a ticket string.
    /// mode: "read" or "write"
    pub async fn share_with_addr(&self, mode: String) -> Result<String> {
        let share_mode = match mode.as_str() {
            "read" | "Read" => ShareMode::Read,
            "write" | "Write" => ShareMode::Write,
            _ => return Err(anyhow!("mode must be 'read' or 'write'")),
        };
        Ok(self
            .doc
            .share(share_mode, AddrInfoOptions::RelayAndAddresses)
            .await?
            .encode_string())
    }
}

// ============================================================================
// CoreDocEvent / CoreDocEventReceiver - Live document event subscription
// ============================================================================

/// A live document event, emitted when the doc's content changes or peers connect.
#[derive(Debug, Clone)]
pub enum CoreDocEvent {
    /// A local insertion: this node wrote an entry.
    InsertLocal { entry: CoreDocEntry },
    /// A remote insertion: a peer sent us an entry.
    InsertRemote { from: String, entry: CoreDocEntry },
    /// The content for an entry is now available locally.
    ContentReady { hash: String },
    /// All pending content downloads from the last sync run completed (or failed).
    PendingContentReady,
    /// A new peer joined the swarm for this document.
    NeighborUp { peer: String },
    /// A peer left the swarm for this document.
    NeighborDown { peer: String },
    /// A set-reconciliation sync with a peer finished.
    SyncFinished { peer: String },
}

fn live_event_to_core(ev: LiveEvent) -> CoreDocEvent {
    match ev {
        LiveEvent::InsertLocal { entry } => CoreDocEvent::InsertLocal {
            entry: CoreDocEntry {
                author_id: entry.author().to_string(),
                key: entry.key().to_vec(),
                content_hash: entry.content_hash().to_hex().to_string(),
                content_len: entry.content_len(),
                timestamp: entry.timestamp(),
            },
        },
        LiveEvent::InsertRemote { from, entry, .. } => CoreDocEvent::InsertRemote {
            from: from.to_string(),
            entry: CoreDocEntry {
                author_id: entry.author().to_string(),
                key: entry.key().to_vec(),
                content_hash: entry.content_hash().to_hex().to_string(),
                content_len: entry.content_len(),
                timestamp: entry.timestamp(),
            },
        },
        LiveEvent::ContentReady { hash } => CoreDocEvent::ContentReady {
            hash: hash.to_string(),
        },
        LiveEvent::PendingContentReady => CoreDocEvent::PendingContentReady,
        LiveEvent::NeighborUp(peer) => CoreDocEvent::NeighborUp {
            peer: peer.to_string(),
        },
        LiveEvent::NeighborDown(peer) => CoreDocEvent::NeighborDown {
            peer: peer.to_string(),
        },
        LiveEvent::SyncFinished(ev) => CoreDocEvent::SyncFinished {
            peer: ev.peer.to_string(),
        },
    }
}

type DocEventStream = std::pin::Pin<Box<dyn futures_lite::Stream<Item = Result<LiveEvent>> + Send>>;

/// Receiver for live document events. Clone-safe via Arc<Mutex>.
#[derive(Clone)]
pub struct CoreDocEventReceiver {
    inner: Arc<Mutex<DocEventStream>>,
}

impl CoreDocEventReceiver {
    /// Receive the next event. Returns None when the subscription ends.
    pub async fn recv(&self) -> Result<Option<CoreDocEvent>> {
        let mut stream = self.inner.lock().await;
        match futures_lite::StreamExt::next(&mut *stream).await {
            None => Ok(None),
            Some(Ok(ev)) => Ok(Some(live_event_to_core(ev))),
            Some(Err(e)) => Err(e),
        }
    }
}

// ============================================================================
// CoreGossipClient - Gossip protocol client
// ============================================================================

#[derive(Clone)]
pub struct CoreGossipClient {
    pub inner: Gossip,
}

impl CoreGossipClient {
    pub async fn subscribe(
        &self,
        topic_bytes: Vec<u8>,
        bootstrap_peers: Vec<String>,
    ) -> Result<CoreGossipTopic> {
        let topic_arr: [u8; 32] = topic_bytes
            .try_into()
            .map_err(|_| anyhow!("topic_bytes must be exactly 32 bytes"))?;
        let peers: Vec<EndpointId> = bootstrap_peers
            .iter()
            .map(|s| s.parse::<EndpointId>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let topic = self
            .inner
            .subscribe_and_join(TopicId::from_bytes(topic_arr), peers)
            .await?;
        let (sender, receiver) = topic.split();
        Ok(CoreGossipTopic {
            sender,
            receiver: Arc::new(Mutex::new(receiver)),
        })
    }
}

// ============================================================================
// CoreGossipTopic - Gossip topic subscription
// ============================================================================

#[derive(Clone)]
pub struct CoreGossipTopic {
    pub sender: GossipSender,
    pub receiver: Arc<Mutex<GossipReceiver>>,
}

impl CoreGossipTopic {
    pub async fn broadcast(&self, data: Vec<u8>) -> Result<()> {
        self.sender.broadcast(Bytes::from(data)).await?;
        Ok(())
    }

    pub async fn recv(&self) -> Result<CoreGossipEvent> {
        let mut rx = self.receiver.lock().await;
        let event = futures_lite::StreamExt::next(&mut *rx)
            .await
            .ok_or_else(|| anyhow!("gossip topic closed"))??;
        Ok(match event {
            Event::Received(msg) => CoreGossipEvent {
                event_type: "received".into(),
                data: Some(msg.content.to_vec()),
            },
            Event::NeighborUp(id) => CoreGossipEvent {
                event_type: "neighbor_up".into(),
                data: Some(id.to_string().into_bytes()),
            },
            Event::NeighborDown(id) => CoreGossipEvent {
                event_type: "neighbor_down".into(),
                data: Some(id.to_string().into_bytes()),
            },
            Event::Lagged => CoreGossipEvent {
                event_type: "lagged".into(),
                data: None,
            },
        })
    }
}

#[cfg(test)]
mod role_swap_shutdown_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test scaffolding: a failed unwrap/panic is the test failing"
    )]

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::release_both_references;

    const BUDGET: Duration = Duration::from_millis(500);

    /// Leave must **complete** before close is even polled.
    ///
    /// Load-bearing, not stylistic: close invalidates the handle, and a leave
    /// issued against an already-closed handle is rejected. Reversing the order
    /// would leave the live-sync reference held while reporting success, and
    /// retirement would then report `Busy` with nothing left able to fix it.
    ///
    /// Recorded rather than timed: the close future's body cannot run until it
    /// is polled, so the log order *is* the poll order.
    #[tokio::test]
    async fn leave_completes_before_close_is_polled() {
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let leave_log = Arc::clone(&log);
        let close_log = Arc::clone(&log);

        release_both_references(
            BUDGET,
            async move {
                leave_log.lock().unwrap().push("leave:start");
                // Several yields: if close were polled concurrently or first,
                // its entry would land inside this gap.
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                }
                leave_log.lock().unwrap().push("leave:done");
                Ok(())
            },
            async move {
                close_log.lock().unwrap().push("close:polled");
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec!["leave:start", "leave:done", "close:polled"],
            "close must not be polled until leave has finished",
        );
    }

    /// Nested causes survive on both paths.
    #[tokio::test]
    async fn error_chains_are_preserved() {
        let root = || anyhow::anyhow!("socket closed").context("transport lost");

        // Single failure: the chain is walkable through `source()`.
        let err = release_both_references(BUDGET, async { Err(root()) }, async { Ok(()) })
            .await
            .expect_err("leave failed");
        let chain: Vec<String> = err.chain().map(|c| c.to_string()).collect();
        assert_eq!(
            chain,
            vec!["leave failed", "transport lost", "socket closed"],
            "a formatted top line would have lost the causes",
        );

        // Both failed: close keeps its walkable chain, leave's is rendered in
        // full into the message.
        let err = release_both_references(BUDGET, async { Err(root()) }, async {
            Err(anyhow::anyhow!("disk gone").context("store unavailable"))
        })
        .await
        .expect_err("both failed");
        let chain: Vec<String> = err.chain().map(|c| c.to_string()).collect();
        assert!(
            chain.iter().any(|c| c == "store unavailable")
                && chain.iter().any(|c| c == "disk gone"),
            "the close chain must stay walkable: {chain:?}",
        );
        let text = format!("{err:#}");
        assert!(
            text.contains("transport lost") && text.contains("socket closed"),
            "the leave chain must be readable in full, not just its top line: {text}",
        );
    }

    /// Close runs even when leave fails promptly, and both failures survive.
    ///
    /// A failed leave is exactly when the handle most needs releasing:
    /// abandoning the close there would leave behind the reference the whole
    /// operation exists to drop, and retirement would then report `Busy`
    /// forever with no indication why.
    #[tokio::test]
    async fn close_runs_after_a_prompt_leave_failure() {
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);

        let err = release_both_references(
            BUDGET,
            async { Err(anyhow::anyhow!("no route to peer")) },
            async move {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect_err("a failed leave must be reported");

        assert!(
            closed.load(Ordering::SeqCst),
            "close must be attempted regardless of how leave went",
        );
        // `{:#}` walks the chain; the bare form prints only the top context,
        // which is the whole point of keeping the cause attached.
        assert!(
            format!("{err:#}").contains("no route to peer"),
            "the leave failure must survive: {err:#}",
        );
    }

    /// Both failures are preserved, not just the first.
    #[tokio::test]
    async fn both_failures_are_preserved() {
        let err = release_both_references(
            BUDGET,
            async { Err(anyhow::anyhow!("leave broke")) },
            async { Err(anyhow::anyhow!("close broke")) },
        )
        .await
        .expect_err("two failures must be reported");

        let text = format!("{err:#}");
        assert!(text.contains("leave broke"), "{text}");
        assert!(
            text.contains("close broke"),
            "a cleanup failure is a different and worse fault than the one \
             before it, and diagnosing needs both: {text}",
        );
    }

    /// One absolute deadline covers both steps, not one budget each.
    ///
    /// Per-step budgets look bounded and are not: a slow leave gets the whole
    /// budget and the close then gets a fresh one, doubling what the caller
    /// allowed for a transition it is holding a lock across.
    ///
    /// Paused time, so the assertion is exact rather than a threshold. With
    /// real time and two steps that hang, a loaded machine could push a
    /// correct implementation over any margin — and two immediate steps would
    /// satisfy any margin regardless of how many budgets were used, which is
    /// the failure a wall-clock version of this test actually had.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_is_shared_across_both_steps() {
        let budget = Duration::from_millis(300);
        let started = tokio::time::Instant::now();

        let err = release_both_references(
            budget,
            async {
                std::future::pending::<()>().await;
                Ok(())
            },
            async {
                std::future::pending::<()>().await;
                Ok(())
            },
        )
        .await
        .expect_err("both steps hang, so both must time out");
        let elapsed = started.elapsed();

        assert_eq!(
            elapsed, budget,
            "one budget covers both steps; a budget each would take exactly \
             twice as long",
        );
        let text = format!("{err:#}");
        assert!(text.contains("leave timed out"), "{text}");
        assert!(text.contains("close timed out"), "{text}");
    }
}

#[cfg(test)]
mod topology_registration_tests {
    use super::*;

    #[tokio::test]
    async fn late_join_registration_after_close_aborts_its_publisher() {
        let node = CoreNode::memory_with_alpns_and_gc(
            Vec::new(),
            Some(CoreEndpointConfig {
                relay_mode: Some("disabled".into()),
                enable_monitoring: true,
                ..Default::default()
            }),
            None,
        )
        .await
        .unwrap();
        let swarm = node
            .topology_join_swarm(&[17; 32], CoreTopoSwarmConfig::default())
            .await
            .unwrap();
        // Hold a completed startup just before its registration commit. Close
        // must win this ordering, including when a caller retains the query Arc.
        let (_, publisher) = node.inner.topo_swarm.write().unwrap().swarm.take().unwrap();
        let abort = publisher.0.clone();
        node.close().await;
        assert!(node
            .inner
            .topo_swarm
            .write()
            .unwrap()
            .install(swarm, publisher)
            .is_err());
        assert!(node.topology_swarm().is_none());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !abort.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("rejected registration must abort its publisher");
        assert!(node
            .topology_join_swarm(&[17; 32], CoreTopoSwarmConfig::default())
            .await
            .is_err());
    }
}
