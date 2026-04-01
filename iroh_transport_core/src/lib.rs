use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{
    presets, AfterHandshakeOutcome, BeforeConnectOutcome, Connection, ConnectionError,
    ConnectionInfo, EndpointHooks, Endpoint, PathInfo, RelayMode, VarInt,
};
use iroh::protocol::Router;
use iroh::{EndpointAddr, EndpointId, RelayUrl, SecretKey, TransportAddr, Watcher};
use iroh_blobs::api::downloader::Downloader;
use iroh_blobs::api::Store as BlobStore;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{BlobFormat, BlobsProtocol, Hash, ALPN as BLOBS_ALPN};
use iroh_docs::api::protocol::{AddrInfoOptions, ShareMode};
use iroh_docs::api::Doc;
use iroh_docs::protocol::Docs;
use iroh_docs::{AuthorId, DocTicket, ALPN as DOCS_ALPN};
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::Gossip;
use iroh_gossip::proto::TopicId;
use iroh_gossip::ALPN as GOSSIP_ALPN;
use iroh_tickets::Ticket;
use std::sync::RwLock;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::StreamExt;
use tracing::debug;

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
}

impl Default for CoreEndpointConfig {
    fn default() -> Self {
        Self {
            relay_mode: None,
            relay_urls: Vec::new(),
            alpns: Vec::new(),
            secret_key: None,
            enable_discovery: true,
            enable_monitoring: false,
            enable_hooks: false,
            hook_timeout_ms: 5000,
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
    async fn after_handshake<'a>(
        &'a self,
        conn: &'a ConnectionInfo,
    ) -> AfterHandshakeOutcome {
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

    async fn run(
        mut rx: mpsc::Receiver<ConnectionInfo>,
        map: RemoteMapInner,
    ) {
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
            let mut inner = map.write().expect("poisoned");
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
                    let mut inner = map.write().expect("poisoned");
                    let entry = inner.entry(remote_id).or_default();
                    entry.connections.remove(&conn_id);
                    entry.aggregate.last_update = SystemTime::now();
                    entry.aggregate.total_bytes_sent += stats.udp_tx.bytes;
                    entry.aggregate.total_bytes_received += stats.udp_rx.bytes;
                } else {
                    let mut inner = map.write().expect("poisoned");
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
                    let mut inner = map.write().expect("poisoned");
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
        let inner = self.map.read().expect("poisoned");
        inner.get(node_id).map(|entry| entry.to_core_remote_info(node_id))
    }

    /// Get all known remote endpoints.
    pub fn remote_info_iter(&self) -> Vec<CoreRemoteInfo> {
        let inner = self.map.read().expect("poisoned");
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
    before_connect_tx:
        mpsc::Sender<(CoreHookConnectInfo, oneshot::Sender<bool>)>,
    after_handshake_tx:
        mpsc::Sender<(CoreHookHandshakeInfo, oneshot::Sender<CoreAfterHandshakeDecision>)>,
    timeout: Duration,
}

/// Receiver side for hook events. Stored in `CoreNetClient`.
pub struct CoreHookReceiver {
    pub before_connect_rx:
        mpsc::Receiver<(CoreHookConnectInfo, oneshot::Sender<bool>)>,
    pub after_handshake_rx:
        mpsc::Receiver<(CoreHookHandshakeInfo, oneshot::Sender<CoreAfterHandshakeDecision>)>,
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

    async fn after_handshake<'a>(
        &'a self,
        conn: &'a ConnectionInfo,
    ) -> AfterHandshakeOutcome {
        let info = CoreHookHandshakeInfo {
            remote_endpoint_id: conn.remote_id().to_string(),
            alpn: conn.alpn().to_vec(),
            is_alive: conn.is_alive(),
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.after_handshake_tx.send((info, reply_tx)).await.is_err() {
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
            Ok(RelayMode::Default)
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

    Ok(builder)
}

// ============================================================================
// CoreNode - Full iroh node with all protocols
// ============================================================================

#[derive(Clone)]
pub struct CoreNode {
    inner: Arc<CoreNodeInner>,
}

struct CoreNodeInner {
    endpoint: Endpoint,
    #[allow(dead_code)]
    router: Router,
    #[allow(dead_code)]
    blobs: BlobsProtocol,
    docs: Docs,
    gossip: Gossip,
    store: BlobStore,
    secret_key_bytes: Vec<u8>,
}

impl CoreNode {
    pub async fn memory() -> Result<Self> {
        let endpoint = Endpoint::bind(presets::N0).await?;
        endpoint.online().await;
        let mem_store = MemStore::new();
        let store: BlobStore = (*mem_store).clone();
        let blobs = BlobsProtocol::new(&store, None);
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let docs = Docs::memory()
            .spawn(endpoint.clone(), store.clone(), gossip.clone())
            .await?;
        let router = Router::builder(endpoint.clone())
            .accept(BLOBS_ALPN, blobs.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(DOCS_ALPN, docs.clone())
            .spawn();

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
            }),
        })
    }

    pub async fn persistent(path: String) -> Result<Self> {
        let endpoint = Endpoint::bind(presets::N0).await?;
        endpoint.online().await;
        let fs_store = FsStore::load(path).await?;
        let store: BlobStore = fs_store.into();
        let blobs = BlobsProtocol::new(&store, None);
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let docs = Docs::memory()
            .spawn(endpoint.clone(), store.clone(), gossip.clone())
            .await?;
        let router = Router::builder(endpoint.clone())
            .accept(BLOBS_ALPN, blobs.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(DOCS_ALPN, docs.clone())
            .spawn();

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
        self.inner.endpoint.close().await;
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
        self.hook_receiver
            .as_ref()?
            .lock()
            .ok()?
            .take()
    }

    /// Returns whether hooks are enabled for this endpoint.
    pub fn has_hooks(&self) -> bool {
        self.hook_receiver.is_some()
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
            ConnectionError::ApplicationClosed(app) => (
                Some(app.error_code.into_inner()),
                Some(app.reason.to_vec()),
            ),
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
// CoreBlobsClient - Blob storage protocol
// ============================================================================

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

    pub async fn download_blob(&self, ticket_str: String) -> Result<Vec<u8>> {
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
        Ok(self.store.get_bytes(hash).await?.to_vec())
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