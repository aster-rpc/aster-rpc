#[cfg(feature = "attestation")]
pub mod attestation;
pub mod canonical;
pub mod contract;
pub mod framing;
pub mod hpke_envelope;
pub mod namespace;
pub mod pool;
pub mod reactor;
pub mod registry;
pub mod ring;
pub mod signing;
pub mod ticket;
pub mod trust;
pub mod tunnel;

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
use iroh_blobs::api::downloader::Downloader;
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
    Author, AuthorId, Capability, DocTicket, NamespaceId, NamespaceSecret, ALPN as DOCS_ALPN,
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
    AuthorizeTunnelError, TunnelConfig, TunnelRegistry, TunnelTarget, TunnelTicket,
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
    endpoint: Endpoint,
    router: Router,
    #[allow(dead_code)]
    blobs: BlobsProtocol,
    docs: Docs,
    gossip: Gossip,
    store: BlobStore,
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
    #[allow(dead_code)]
    monitor: Option<CoreMonitor>,
    /// Hooks receiver — present iff enable_hooks=true. Same shape as
    /// [`CoreNetClient::hook_receiver`]; takeable once via
    /// [`CoreNode::take_hook_receiver`].
    hook_receiver: Option<Arc<std::sync::Mutex<Option<CoreHookReceiver>>>>,
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
            hook_receiver,
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
            hook_receiver,
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
        hook_receiver: Option<Arc<std::sync::Mutex<Option<CoreHookReceiver>>>>,
        // The docs GC protection bridge, created together by
        // `ProtectCallbackHandler::new()`: the handler half drives the docs
        // engine, the callback half is stored for manual `gc_run_once` sweeps.
        gc_protect: (ProtectCallbackHandler, ProtectCb),
    ) -> Result<Self> {
        let (protect_handler, protect_cb) = gc_protect;
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
                endpoint,
                router,
                blobs,
                docs,
                gossip,
                store,
                protect_cb,
                secret_key_bytes,
                aster_rx: Mutex::new(aster_rx),
                monitor,
                hook_receiver,
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
            monitor: None,
            hook_receiver: None,
        }
    }

    /// Export all transport-level metrics in Prometheus text exposition format.
    ///
    /// Covers socket I/O, path counts, holepunching, relay, and net report
    /// counters from the iroh endpoint. Intended to be merged with Aster RPC
    /// metrics on a single `/metrics/prometheus` scrape endpoint.
    pub fn transport_metrics_prometheus(&self) -> String {
        let m = self.inner.endpoint.metrics();
        let s = &m.socket;
        let n = &m.net_report;

        format!(
            "# HELP iroh_send_ipv4 Bytes sent via IPv4\n\
             # TYPE iroh_send_ipv4 counter\n\
             iroh_send_ipv4 {}\n\
             # HELP iroh_send_ipv6 Bytes sent via IPv6\n\
             # TYPE iroh_send_ipv6 counter\n\
             iroh_send_ipv6 {}\n\
             # HELP iroh_send_relay Bytes sent via relay\n\
             # TYPE iroh_send_relay counter\n\
             iroh_send_relay {}\n\
             # HELP iroh_recv_data_ipv4 Bytes received via IPv4\n\
             # TYPE iroh_recv_data_ipv4 counter\n\
             iroh_recv_data_ipv4 {}\n\
             # HELP iroh_recv_data_ipv6 Bytes received via IPv6\n\
             # TYPE iroh_recv_data_ipv6 counter\n\
             iroh_recv_data_ipv6 {}\n\
             # HELP iroh_recv_data_relay Bytes received via relay\n\
             # TYPE iroh_recv_data_relay counter\n\
             iroh_recv_data_relay {}\n\
             # HELP iroh_recv_datagrams Total datagrams received\n\
             # TYPE iroh_recv_datagrams counter\n\
             iroh_recv_datagrams {}\n\
             # HELP iroh_conns_direct Direct connections\n\
             # TYPE iroh_conns_direct gauge\n\
             iroh_conns_direct {}\n\
             # HELP iroh_conns_opened Total connections opened\n\
             # TYPE iroh_conns_opened counter\n\
             iroh_conns_opened {}\n\
             # HELP iroh_conns_closed Total connections closed\n\
             # TYPE iroh_conns_closed counter\n\
             iroh_conns_closed {}\n\
             # HELP iroh_paths_direct Direct paths active\n\
             # TYPE iroh_paths_direct gauge\n\
             iroh_paths_direct {}\n\
             # HELP iroh_paths_relay Relay paths active\n\
             # TYPE iroh_paths_relay gauge\n\
             iroh_paths_relay {}\n\
             # HELP iroh_holepunch_attempts Holepunch attempts\n\
             # TYPE iroh_holepunch_attempts counter\n\
             iroh_holepunch_attempts {}\n\
             # HELP iroh_relay_home_change Relay home server changes\n\
             # TYPE iroh_relay_home_change counter\n\
             iroh_relay_home_change {}\n\
             # HELP iroh_net_reports Net reports completed\n\
             # TYPE iroh_net_reports counter\n\
             iroh_net_reports {}\n\
             # HELP iroh_net_reports_full Full net reports completed\n\
             # TYPE iroh_net_reports_full counter\n\
             iroh_net_reports_full {}\n",
            s.send_ipv4.get(),
            s.send_ipv6.get(),
            s.send_relay.get(),
            s.recv_data_ipv4.get(),
            s.recv_data_ipv6.get(),
            s.recv_data_relay.get(),
            s.recv_datagrams.get(),
            s.num_conns_direct.get(),
            s.num_conns_opened.get(),
            s.num_conns_closed.get(),
            s.paths_direct.get(),
            s.paths_relay.get(),
            s.holepunch_attempts.get(),
            s.relay_home_change.get(),
            n.reports.get(),
            n.reports_full.get(),
        )
    }
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
        }
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

            let (payload, flags) = match recv.read_one_frame().await {
                Ok(f) => f,
                Err(e) => {
                    debug!("accept_aster_bi: first-frame read error, dropping stream: {e}");
                    continue;
                }
            };

            if flags & FLAG_TUNNEL != 0 {
                let registry = self.tunnels.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        crate::tunnel::handle_tunnel_redeem(send, recv, payload, registry).await
                    {
                        debug!("tunnel redeem error: {e}");
                    }
                });
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
    /// Sends the `[FLAG_TUNNEL][ticket]` handshake frame internally,
    /// then returns the bidi stream pair. Bytes after the handshake
    /// are raw — the caller speaks the backend protocol directly
    /// (e.g. RFB for VNC).
    pub async fn open_tunnel(
        &self,
        ticket: TunnelTicket,
    ) -> Result<(CoreSendStream, CoreRecvStream)> {
        let (send, recv) = self.inner.open_bi().await?;
        let send = CoreSendStream::new(send);
        let recv = CoreRecvStream::new(recv);
        let frame = encode_frame(ticket.as_bytes(), FLAG_TUNNEL)?;
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
        let closed = self.inner.closed().await;
        let (code, reason) = match &closed {
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
        let paths = self.inner.paths();
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
}

// ============================================================================
// CoreRecvStream - QUIC recv stream
// ============================================================================

struct RecvInner {
    stream: iroh::endpoint::RecvStream,
    /// Bytes peeked off the wire ahead of the caller — populated by
    /// [`CoreConnection::accept_bi`] when it consumes the first frame
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
    /// them before going to the wire. Used by `CoreConnection::accept_bi`
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

    /// Read one length-prefixed frame: `[4B LE len][1B flags][payload]`.
    /// Used by `CoreConnection::accept_bi` for the `FLAG_TUNNEL` peek and
    /// by the reactor's per-stream frame reader.
    pub(crate) async fn read_one_frame(&self) -> Result<(Vec<u8>, u8)> {
        let mut len_bytes = [0u8; 4];
        self.read_exact_into(&mut len_bytes).await?;
        let frame_body_len = u32::from_le_bytes(len_bytes) as usize;
        if frame_body_len == 0 {
            return Err(anyhow!("zero-length frame"));
        }
        if frame_body_len > crate::framing::MAX_FRAME_SIZE as usize {
            return Err(anyhow!("frame too large: {}", frame_body_len));
        }
        let mut flags_buf = [0u8; 1];
        self.read_exact_into(&mut flags_buf).await?;
        let flags = flags_buf[0];
        let payload_len = frame_body_len - 1;
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
        Downloader::new(&self.store, &self.endpoint)
            .download(hash, vec![addr.id])
            .await?;

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

    /// Download a blob by hash from a specific node, bypassing ticket parsing.
    /// `format` should be "raw" or "hash_seq".
    pub async fn download_hash(
        &self,
        hash_hex: String,
        node_id_hex: String,
        format: String,
    ) -> Result<Vec<u8>> {
        let hash: Hash = hash_hex.parse()?;
        let node_id: EndpointId = node_id_hex.parse()?;
        let blob_format = if format == "hash_seq" {
            BlobFormat::HashSeq
        } else {
            BlobFormat::Raw
        };
        let haf = HashAndFormat {
            hash,
            format: blob_format,
        };
        Downloader::new(&self.store, &self.endpoint)
            .download(haf, vec![node_id])
            .await?;

        if blob_format == BlobFormat::HashSeq {
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
        let hash: Hash = hash_hex.parse()?;
        let node_id: EndpointId = node_id_hex.parse()?;
        let blob_format = if format == "hash_seq" {
            BlobFormat::HashSeq
        } else {
            BlobFormat::Raw
        };
        let haf = HashAndFormat {
            hash,
            format: blob_format,
        };
        Downloader::new(&self.store, &self.endpoint)
            .download(haf, vec![node_id])
            .await?;
        Ok(())
    }

    /// Download a collection by hash from a specific node, returning (name, data) pairs.
    pub async fn download_collection_hash(
        &self,
        hash_hex: String,
        node_id_hex: String,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let hash: Hash = hash_hex.parse()?;
        let node_id: EndpointId = node_id_hex.parse()?;
        let haf = HashAndFormat {
            hash,
            format: BlobFormat::HashSeq,
        };
        Downloader::new(&self.store, &self.endpoint)
            .download(haf, vec![node_id])
            .await?;

        let collection = Collection::load(hash, &self.store).await?;
        let mut files = Vec::new();
        for (name, blob_hash) in collection.iter() {
            let bytes = self.store.get_bytes(*blob_hash).await?;
            files.push((name.clone(), bytes.to_vec()));
        }
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
        Downloader::new(&self.store, &self.endpoint)
            .download(hash, vec![addr.id])
            .await?;

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
        let mut stream = self.inner.api().list().await?;
        while let Some(item) = stream.next().await {
            if item?.0 == ns_id {
                return Ok(true);
            }
        }
        Ok(false)
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
            doc: self.inner.api().import_namespace(ticket.capability).await?,
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

    /// Stop syncing this document.
    pub async fn leave(&self) -> Result<()> {
        self.doc.leave().await?;
        Ok(())
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
