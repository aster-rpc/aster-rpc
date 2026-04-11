pub mod canonical;
pub mod contract;
pub mod framing;
pub mod signing;
pub mod ticket;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use iroh::address_lookup::memory::MemoryLookup;
use iroh::address_lookup::MdnsAddressLookup;
use iroh::endpoint::{
    presets, AfterHandshakeOutcome, BeforeConnectOutcome, Connection, ConnectionError,
    ConnectionInfo, Endpoint, EndpointHooks, PathInfo, PortmapperConfig, RelayMode, VarInt,
};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{EndpointAddr, EndpointId, RelayMap, RelayUrl, SecretKey, TransportAddr, Watcher};
use iroh_blobs::api::downloader::Downloader;
use iroh_blobs::api::Store as BlobStore;
use iroh_blobs::format::collection::Collection;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{BlobFormat, BlobsProtocol, Hash, HashAndFormat, ALPN as BLOBS_ALPN};
use iroh_docs::api::protocol::{AddrInfoOptions, ShareMode};
use iroh_docs::api::Doc;
use iroh_docs::engine::LiveEvent;
use iroh_docs::protocol::Docs;
use iroh_docs::store::{DownloadPolicy, FilterKind, Query};
use iroh_docs::{AuthorId, Capability, DocTicket, NamespaceId, ALPN as DOCS_ALPN};
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::Gossip;
use iroh_gossip::proto::TopicId;
use iroh_gossip::ALPN as GOSSIP_ALPN;
use iroh_tickets::Ticket;
use std::sync::RwLock;
use tokio::sync::{mpsc, oneshot, Mutex};
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
            bind_addr: None,
            clear_ip_transports: false,
            clear_relay_transports: false,
            portmapper_config: None,
            proxy_url: None,
            proxy_from_env: false,
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

/// Maximum number of entries returned per query.
/// Limits result sets to prevent unbounded reads when multiple authors write to the same key.
pub const QUERY_ENTRY_LIMIT: u64 = 3;

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
    fn update_from_path(&mut self, path: &PathInfo) {
        self.last_update = SystemTime::now();
        if path.is_ip() {
            self.ip_path = true;
        }
        if path.is_relay() {
            self.relay_path = true;
        }
        if let Some(stats) = path.stats() {
            self.rtt_min = self.rtt_min.min(stats.rtt);
            self.rtt_max = self.rtt_max.max(stats.rtt);
        }
    }
}

/// Internal entry for each remote endpoint
#[derive(Debug, Default)]
struct RemoteInfoEntry {
    aggregate: CoreRemoteAggregate,
    connections: HashMap<u64, ConnectionInfo>,
}

impl RemoteInfoEntry {
    fn is_active(&self) -> bool {
        !self.connections.is_empty()
    }

    fn to_core_remote_info(&self, node_id: &str) -> CoreRemoteInfo {
        // Determine connection type from active connections
        let conn_type = if self.connections.is_empty() {
            ConnectionType::NotConnected
        } else {
            // Check selected path of any active connection
            let detail = self
                .connections
                .values()
                .find_map(|c| {
                    c.selected_path().map(|p| {
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

        // Sum bytes from stats of active connections
        let (bytes_sent, bytes_received) = self
            .connections
            .values()
            .filter_map(|c| c.stats())
            .fold((0u64, 0u64), |(s, r), stats| {
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
            is_connected: self.is_active(),
        }
    }
}

type RemoteMapInner = Arc<RwLock<HashMap<String, RemoteInfoEntry>>>;

/// Connection monitor that tracks remote endpoint information.
///
/// Implements `EndpointHooks` to capture `ConnectionInfo` from `after_handshake`,
/// then spawns background tasks to track path changes and connection close events.
/// This is modeled after the `remote-info.rs` example in iroh 0.97.0.
#[derive(Clone, Debug)]
pub struct CoreMonitor {
    map: RemoteMapInner,
    #[allow(dead_code)]
    tx: mpsc::Sender<ConnectionInfo>,
    _task: Arc<tokio::task::AbortHandle>,
}

/// Hook portion of the monitor — installed on the endpoint builder.
#[derive(Debug)]
struct MonitorHook {
    tx: mpsc::Sender<ConnectionInfo>,
}

impl EndpointHooks for MonitorHook {
    async fn after_handshake<'a>(&'a self, conn: &'a ConnectionInfo) -> AfterHandshakeOutcome {
        self.tx.send(conn.clone()).await.ok();
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

    async fn run(mut rx: mpsc::Receiver<ConnectionInfo>, map: RemoteMapInner) {
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
        conn: ConnectionInfo,
    ) {
        let remote_id = conn.remote_id().to_string();

        // Store connection info
        {
            let mut inner = map.write().unwrap_or_else(|e| e.into_inner());
            let entry = inner.entry(remote_id.clone()).or_default();
            entry.connections.insert(conn_id, conn.clone());
            entry.aggregate.last_update = SystemTime::now();
        }

        // Track connection close
        tasks.spawn({
            let conn = conn.clone();
            let map = map.clone();
            let remote_id = remote_id.clone();
            async move {
                if let Some((_, stats)) = conn.closed().await {
                    let mut inner = map.write().unwrap_or_else(|e| e.into_inner());
                    let entry = inner.entry(remote_id).or_default();
                    entry.connections.remove(&conn_id);
                    entry.aggregate.last_update = SystemTime::now();
                    entry.aggregate.total_bytes_sent += stats.udp_tx.bytes;
                    entry.aggregate.total_bytes_received += stats.udp_rx.bytes;
                } else {
                    let mut inner = map.write().unwrap_or_else(|e| e.into_inner());
                    let entry = inner.entry(remote_id).or_default();
                    entry.connections.remove(&conn_id);
                    entry.aggregate.last_update = SystemTime::now();
                }
            }
        });

        // Track path changes
        tasks.spawn({
            let map = map.clone();
            async move {
                let mut path_updates = conn.paths().stream();
                while let Some(paths) = path_updates.next().await {
                    let mut inner = map.write().unwrap_or_else(|e| e.into_inner());
                    let entry = inner.entry(remote_id.clone()).or_default();
                    for path in paths {
                        entry.aggregate.update_from_path(&path);
                    }
                }
            }
        });
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
        let (bc_tx, bc_rx) = mpsc::channel(16);
        let (ah_tx, ah_rx) = mpsc::channel(16);
        let adapter = Self {
            before_connect_tx: bc_tx,
            after_handshake_tx: ah_tx,
            timeout: Duration::from_millis(timeout_ms),
        };
        let receiver = CoreHookReceiver {
            before_connect_rx: bc_rx,
            after_handshake_rx: ah_rx,
        };
        (adapter, receiver)
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
            // Channel closed → allow by default
            return BeforeConnectOutcome::Accept;
        }
        match tokio::time::timeout(self.timeout, reply_rx).await {
            Ok(Ok(true)) => BeforeConnectOutcome::Accept,
            Ok(Ok(false)) => BeforeConnectOutcome::Reject,
            _ => {
                // Timeout or channel error → allow by default
                BeforeConnectOutcome::Accept
            }
        }
    }

    async fn after_handshake<'a>(&'a self, conn: &'a ConnectionInfo) -> AfterHandshakeOutcome {
        let info = CoreHookHandshakeInfo {
            remote_endpoint_id: conn.remote_id().to_string(),
            alpn: conn.alpn().to_vec(),
            is_alive: conn.is_alive(),
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .after_handshake_tx
            .send((info, reply_tx))
            .await
            .is_err()
        {
            return AfterHandshakeOutcome::Accept;
        }
        match tokio::time::timeout(self.timeout, reply_rx).await {
            Ok(Ok(CoreAfterHandshakeDecision::Accept)) => AfterHandshakeOutcome::Accept,
            Ok(Ok(CoreAfterHandshakeDecision::Reject { error_code, reason })) => {
                AfterHandshakeOutcome::Reject {
                    error_code: VarInt::from_u32(error_code),
                    reason,
                }
            }
            _ => AfterHandshakeOutcome::Accept,
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

fn build_endpoint_config(config: &CoreEndpointConfig) -> Result<iroh::endpoint::Builder> {
    let relay_mode = relay_mode_from_config(config)?;
    let mut builder = Endpoint::builder(presets::N0)
        .alpns(config.alpns.clone())
        .relay_mode(relay_mode);

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
                let (adapter, receiver) = CoreHooksAdapter::new(config.hook_timeout_ms);
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
        let (endpoint, monitor, hook_receiver) = build_node_endpoint(endpoint_config).await?;
        let mem_store = MemStore::new();
        let store: BlobStore = (*mem_store).clone();
        Self::finalize(endpoint, store, aster_alpns, monitor, hook_receiver).await
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
        let (endpoint, monitor, hook_receiver) = build_node_endpoint(endpoint_config).await?;
        let fs_store = FsStore::load(path).await?;
        let store: BlobStore = fs_store.into();
        Self::finalize(endpoint, store, aster_alpns, monitor, hook_receiver).await
    }

    /// Shared tail of both constructors: wire blobs/docs/gossip protocols +
    /// one `AsterQueueHandler` per entry in `aster_alpns` onto a Router, then
    /// assemble `CoreNodeInner`.
    async fn finalize(
        endpoint: Endpoint,
        store: BlobStore,
        aster_alpns: Vec<Vec<u8>>,
        monitor: Option<CoreMonitor>,
        hook_receiver: Option<Arc<std::sync::Mutex<Option<CoreHookReceiver>>>>,
    ) -> Result<Self> {
        let blobs = BlobsProtocol::new(&store, None);
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let docs = Docs::memory()
            .spawn(endpoint.clone(), store.clone(), gossip.clone())
            .await?;

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
        // router.shutdown() drains protocol handlers (so in-flight
        // AsterQueueHandler::accept futures resolve), drops handlers (closing
        // the aster channel), then closes the endpoint. See iroh-0.97.0
        // protocol.rs:490-495.
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

    pub fn blobs_client(&self) -> CoreBlobsClient {
        CoreBlobsClient {
            store: self.inner.store.clone(),
            endpoint: self.inner.endpoint.clone(),
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
            let (adapter, receiver) = CoreHooksAdapter::new(config.hook_timeout_ms);
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

#[derive(Clone)]
pub struct CoreConnection {
    inner: Arc<Connection>,
}

impl CoreConnection {
    fn new(conn: Connection) -> Self {
        Self {
            inner: Arc::new(conn),
        }
    }

    pub async fn open_bi(&self) -> Result<(CoreSendStream, CoreRecvStream)> {
        let (send, recv) = self.inner.open_bi().await?;
        Ok((CoreSendStream::new(send), CoreRecvStream::new(recv)))
    }

    pub async fn accept_bi(&self) -> Result<(CoreSendStream, CoreRecvStream)> {
        let (send, recv) = self.inner.accept_bi().await?;
        Ok((CoreSendStream::new(send), CoreRecvStream::new(recv)))
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

    pub fn connection_info(&self) -> CoreConnectionInfo {
        let info = self.inner.to_info();
        let stats = info.stats();
        let selected_path = info.selected_path();

        let connection_type = match selected_path.as_ref() {
            Some(path) if path.is_relay() => ConnectionTypeDetail::UdpRelay,
            Some(path) if path.is_ip() => ConnectionTypeDetail::UdpDirect,
            Some(path) => ConnectionTypeDetail::Other(format!("{:?}", path.remote_addr())),
            None => ConnectionTypeDetail::Other("unknown".to_string()),
        };

        CoreConnectionInfo {
            connection_type,
            bytes_sent: stats.as_ref().map(|s| s.udp_tx.bytes).unwrap_or(0),
            bytes_received: stats.as_ref().map(|s| s.udp_rx.bytes).unwrap_or(0),
            rtt_ns: selected_path
                .and_then(|p| p.rtt())
                .map(|d| d.as_nanos().min(u64::MAX as u128) as u64),
            alpn: info.alpn().to_vec(),
            is_connected: info.is_alive(),
        }
    }

    // ============================================================================
    // Per-connection metrics (for routing / HA)
    // ============================================================================

    /// Current round-trip time in milliseconds for the selected path.
    /// Returns 0.0 if no path is selected or RTT is not yet measured.
    pub fn rtt_ms(&self) -> f64 {
        self.inner
            .to_info()
            .selected_path()
            .and_then(|p| p.rtt())
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }

    /// Total bytes sent on the selected path (UDP layer).
    pub fn bytes_sent(&self) -> u64 {
        self.inner
            .to_info()
            .selected_path()
            .and_then(|p| p.stats())
            .map(|s| s.udp_tx.bytes)
            .unwrap_or(0)
    }

    /// Total bytes received on the selected path (UDP layer).
    pub fn bytes_recv(&self) -> u64 {
        self.inner
            .to_info()
            .selected_path()
            .and_then(|p| p.stats())
            .map(|s| s.udp_rx.bytes)
            .unwrap_or(0)
    }

    /// Current congestion window size in bytes for the selected path.
    /// Returns 0 if not available.
    pub fn congestion_window(&self) -> u64 {
        self.inner
            .to_info()
            .selected_path()
            .and_then(|p| p.stats())
            .map(|s| s.cwnd)
            .unwrap_or(0)
    }

    /// Number of lost packets on the selected path.
    pub fn lost_packets(&self) -> u64 {
        self.inner
            .to_info()
            .selected_path()
            .and_then(|p| p.stats())
            .map(|s| s.lost_packets)
            .unwrap_or(0)
    }

    /// Number of congestion events on the selected path.
    pub fn congestion_events(&self) -> u64 {
        self.inner
            .to_info()
            .selected_path()
            .and_then(|p| p.stats())
            .map(|s| s.congestion_events)
            .unwrap_or(0)
    }

    /// Current path MTU in bytes.
    pub fn current_mtu(&self) -> u16 {
        self.inner
            .to_info()
            .selected_path()
            .and_then(|p| p.stats())
            .map(|s| s.current_mtu)
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

#[derive(Clone)]
pub struct CoreRecvStream {
    inner: Arc<Mutex<iroh::endpoint::RecvStream>>,
}

impl CoreRecvStream {
    fn new(stream: iroh::endpoint::RecvStream) -> Self {
        Self {
            inner: Arc::new(Mutex::new(stream)),
        }
    }

    pub async fn read(&self, max_len: usize) -> Result<Option<Vec<u8>>> {
        let mut s = self.inner.lock().await;
        Ok(s.read_chunk(max_len).await?.map(|c| c.bytes.to_vec()))
    }

    pub async fn read_exact(&self, n: usize) -> Result<Vec<u8>> {
        let mut s = self.inner.lock().await;
        let mut buf = vec![0u8; n];
        s.read_exact(&mut buf).await?;
        Ok(buf)
    }

    pub async fn read_to_end(&self, max_size: usize) -> Result<Vec<u8>> {
        let mut s = self.inner.lock().await;
        Ok(s.read_to_end(max_size).await?.to_vec())
    }

    pub fn stop(&self, code: u64) -> Result<()> {
        let mut s = self
            .inner
            .try_lock()
            .map_err(|_| anyhow!("recv stream is busy"))?;
        s.stop(u64_to_varint(code)?)?;
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
}

impl CoreBlobsClient {
    pub async fn add_bytes(&self, data: Vec<u8>) -> Result<String> {
        Ok(self.store.add_slice(&data).await?.hash.to_string())
    }

    pub async fn read_to_bytes(&self, hash_hex: String) -> Result<Vec<u8>> {
        Ok(self
            .store
            .get_bytes(hash_hex.parse::<Hash>()?)
            .await?
            .to_vec())
    }

    pub fn create_ticket(&self, hash_hex: String) -> Result<String> {
        Ok(BlobTicket::new(
            self.endpoint.addr(),
            hash_hex.parse::<Hash>()?,
            BlobFormat::Raw,
        )
        .serialize())
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
        .serialize())
    }

    pub async fn download_blob(&self, ticket_str: String) -> Result<Vec<u8>> {
        let ticket = BlobTicket::deserialize(&ticket_str)?;
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
        let ticket = BlobTicket::deserialize(&ticket_str)?;
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

    pub async fn join(&self, ticket_str: String) -> Result<CoreDoc> {
        let ticket = DocTicket::deserialize(&ticket_str)?;
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
        let ticket = DocTicket::deserialize(&ticket_str)?;
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

    /// Query all entries for an exact key, across all authors.
    /// Returns a list of CoreDocEntry with metadata (author, content hash, timestamp, etc.)
    pub async fn query_key_exact(&self, key: Vec<u8>) -> Result<Vec<CoreDocEntry>> {
        let query = Query::key_exact(key).limit(QUERY_ENTRY_LIMIT).build();
        let mut entries_stream = Box::pin(self.doc.get_many(query).await?);
        let mut results = Vec::new();
        while let Some(entry) = entries_stream.next().await {
            let entry = entry?;
            results.push(CoreDocEntry {
                author_id: entry.author().to_string(),
                key: entry.key().to_vec(),
                content_hash: entry.content_hash().to_hex().to_string(),
                content_len: entry.content_len(),
                timestamp: entry.timestamp(),
            });
        }
        Ok(results)
    }

    /// Query all entries matching a key prefix, across all authors.
    /// Returns a list of CoreDocEntry with metadata (author, content hash, timestamp, etc.)
    pub async fn query_key_prefix(&self, prefix: Vec<u8>) -> Result<Vec<CoreDocEntry>> {
        let query = Query::key_prefix(prefix).limit(QUERY_ENTRY_LIMIT).build();
        let mut entries_stream = Box::pin(self.doc.get_many(query).await?);
        let mut results = Vec::new();
        while let Some(entry) = entries_stream.next().await {
            let entry = entry?;
            results.push(CoreDocEntry {
                author_id: entry.author().to_string(),
                key: entry.key().to_vec(),
                content_hash: entry.content_hash().to_hex().to_string(),
                content_len: entry.content_len(),
                timestamp: entry.timestamp(),
            });
        }
        Ok(results)
    }

    /// Read the content bytes for a given content hash.
    /// This can be used after querying entries to fetch the actual value.
    pub async fn read_entry_content(&self, content_hash_hex: String) -> Result<Vec<u8>> {
        let hash: Hash = content_hash_hex.parse()?;
        Ok(self.store.get_bytes(hash).await?.to_vec())
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
            .serialize())
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
            .serialize())
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
