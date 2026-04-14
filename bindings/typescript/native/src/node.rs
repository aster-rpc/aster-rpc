//! IrohNode — wraps CoreNode from aster_transport_core.
//!
//! Mirrors bindings/python/rust/src/node.rs but uses NAPI-RS instead of PyO3.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::{CoreEndpointConfig, CoreNode};

use crate::blobs::BlobsClient;
use crate::docs::DocsClient;
use crate::error::to_napi_err;
use crate::gossip::GossipClient;
use crate::hooks::NodeHookReceiver;
use crate::net::IrohConnection;

/// Endpoint configuration for `IrohNode.memoryWithAlpns` / `persistentWithAlpns`.
///
/// Mirrors `bindings/python/rust/src/net.rs::EndpointConfig` so the same
/// surface is available across PyO3 and NAPI bindings. All fields are
/// optional with sensible defaults; pass `undefined` to use core defaults.
#[napi(object)]
#[derive(Clone, Default)]
pub struct EndpointConfig {
    /// Relay mode: `"default"`, `"disabled"`, or `"staging"`.
    pub relay_mode: Option<String>,
    /// Optional 32-byte secret key for the endpoint identity.
    pub secret_key: Option<Buffer>,
    /// Enable connection monitoring / remote-info tracking (Phase 1b).
    pub enable_monitoring: Option<bool>,
    /// Enable endpoint hooks (before_connect / after_handshake callbacks).
    pub enable_hooks: Option<bool>,
    /// Timeout in ms for hook replies (default 5000).
    pub hook_timeout_ms: Option<u32>,
    /// Bind address e.g. `"0.0.0.0:9000"`, `"127.0.0.1:0"`, `"[::]:0"`.
    pub bind_addr: Option<String>,
    /// Relay-only mode: disable all direct IP (UDP/QUIC) transports.
    pub clear_ip_transports: Option<bool>,
    /// Direct IP-only mode: disable all relay transports.
    pub clear_relay_transports: Option<bool>,
    /// Portmapper (UPnP/NAT-PMP): `"enabled"` (default) or `"disabled"`.
    pub portmapper_config: Option<String>,
    /// HTTP/SOCKS proxy URL for relay/HTTPS traffic e.g. `"http://proxy:8080"`.
    pub proxy_url: Option<String>,
    /// Read proxy from `HTTP_PROXY` / `HTTPS_PROXY` environment variables.
    pub proxy_from_env: Option<bool>,
    /// Enable mDNS local network discovery (default: false).
    pub enable_local_discovery: Option<bool>,
    /// Node data directory for persistent state; empty = no persistent state.
    pub data_dir: Option<String>,
}

impl From<EndpointConfig> for CoreEndpointConfig {
    fn from(c: EndpointConfig) -> Self {
        CoreEndpointConfig {
            relay_mode: c.relay_mode,
            relay_urls: Vec::new(),
            alpns: Vec::new(),
            secret_key: c.secret_key.map(|b| b.to_vec()),
            enable_discovery: c.enable_local_discovery.unwrap_or(false),
            enable_monitoring: c.enable_monitoring.unwrap_or(false),
            enable_hooks: c.enable_hooks.unwrap_or(false),
            hook_timeout_ms: c.hook_timeout_ms.unwrap_or(5000) as u64,
            bind_addr: c.bind_addr,
            clear_ip_transports: c.clear_ip_transports.unwrap_or(false),
            clear_relay_transports: c.clear_relay_transports.unwrap_or(false),
            portmapper_config: c.portmapper_config,
            proxy_url: c.proxy_url,
            proxy_from_env: c.proxy_from_env.unwrap_or(false),
            data_dir: c.data_dir,
        }
    }
}

/// IrohNode — a peer-to-peer Iroh node with all protocols enabled.
///
/// Create via `IrohNode.memory()` or `IrohNode.persistent(path)`.
#[napi]
pub struct IrohNode {
    inner: CoreNode,
}

impl IrohNode {
    /// Borrow a clone of the underlying `CoreNode` for sibling modules
    /// (reactor) that need to start the accept loop on this node.
    pub(crate) fn core_clone(&self) -> CoreNode {
        self.inner.clone()
    }
}

#[napi]
impl IrohNode {
    /// Create an in-memory Iroh node with all protocols.
    #[napi(factory)]
    pub async fn memory() -> Result<IrohNode> {
        let inner = CoreNode::memory().await.map_err(to_napi_err)?;
        Ok(IrohNode { inner })
    }

    /// Create a persistent Iroh node backed by FsStore at the given path.
    #[napi(factory)]
    pub async fn persistent(path: String) -> Result<IrohNode> {
        let inner = CoreNode::persistent(path).await.map_err(to_napi_err)?;
        Ok(IrohNode { inner })
    }

    /// Return this node's EndpointId as a hex string.
    #[napi]
    pub fn node_id(&self) -> String {
        self.inner.node_id()
    }

    /// Return the node's address info (debug format).
    #[napi]
    pub fn node_addr(&self) -> String {
        format!("{:?}", self.inner.node_addr_info())
    }

    /// Export the node's secret key as 32 bytes.
    #[napi]
    pub fn export_secret_key(&self) -> Vec<u8> {
        self.inner.export_secret_key()
    }

    /// Create an in-memory node with custom ALPNs.
    ///
    /// `config` is optional; pass `undefined` for core defaults. To enable
    /// Gate 0 hooks (required for `allow_all_consumers=false` admission),
    /// set `enableHooks: true`.
    #[napi(factory)]
    pub async fn memory_with_alpns(
        alpns: Vec<Buffer>,
        config: Option<EndpointConfig>,
    ) -> Result<IrohNode> {
        let alpn_vecs: Vec<Vec<u8>> = alpns.into_iter().map(|b| b.to_vec()).collect();
        let core_cfg: Option<CoreEndpointConfig> = config.map(Into::into);
        let inner = CoreNode::memory_with_alpns(alpn_vecs, core_cfg)
            .await
            .map_err(to_napi_err)?;
        Ok(IrohNode { inner })
    }

    /// Create a persistent node with custom ALPNs.
    ///
    /// `config` is optional; pass `undefined` for core defaults.
    #[napi(factory)]
    pub async fn persistent_with_alpns(
        path: String,
        alpns: Vec<Buffer>,
        config: Option<EndpointConfig>,
    ) -> Result<IrohNode> {
        let alpn_vecs: Vec<Vec<u8>> = alpns.into_iter().map(|b| b.to_vec()).collect();
        let core_cfg: Option<CoreEndpointConfig> = config.map(Into::into);
        let inner = CoreNode::persistent_with_alpns(path, alpn_vecs, core_cfg)
            .await
            .map_err(to_napi_err)?;
        Ok(IrohNode { inner })
    }

    /// Whether this node was built with hooks enabled.
    #[napi]
    pub fn has_hooks(&self) -> bool {
        self.inner.has_hooks()
    }

    /// Take the hook receiver (one-shot). Returns null if hooks not enabled.
    #[napi]
    pub fn take_hook_receiver(&self) -> Option<NodeHookReceiver> {
        self.inner
            .take_hook_receiver()
            .map(NodeHookReceiver::from_core)
    }

    /// Accept the next incoming aster-ALPN connection.
    /// The ALPN tag is stored on the returned connection (call `conn.alpn()` to read it).
    #[napi]
    pub async fn accept_aster(&self) -> Result<IrohConnection> {
        let (alpn, conn) = self
            .inner
            .clone()
            .accept_aster()
            .await
            .map_err(to_napi_err)?;
        let mut iroh_conn = IrohConnection::from(conn);
        iroh_conn.alpn_tag = Some(String::from_utf8_lossy(&alpn).to_string());
        Ok(iroh_conn)
    }

    /// Get the blobs client for this node.
    #[napi]
    pub fn blobs_client(&self) -> BlobsClient {
        BlobsClient::from(self.inner.blobs_client())
    }

    /// Get the docs client for this node.
    #[napi]
    pub fn docs_client(&self) -> DocsClient {
        DocsClient::from(self.inner.docs_client())
    }

    /// Get the gossip client for this node.
    #[napi]
    pub fn gossip_client(&self) -> GossipClient {
        GossipClient::from(self.inner.gossip_client())
    }

    /// Add another node's address info for peer discovery.
    #[napi]
    pub fn add_node_addr(&self, other: &IrohNode) -> Result<()> {
        self.inner.add_node_addr(&other.inner).map_err(to_napi_err)
    }

    /// Connect to a remote node by its endpoint ID (hex) and ALPN.
    /// Returns an IrohConnection for sending/receiving streams.
    #[napi]
    pub async fn connect(&self, node_id: String, alpn: Buffer) -> Result<IrohConnection> {
        let net = self.inner.net_client();
        let conn = net
            .connect(node_id, alpn.to_vec())
            .await
            .map_err(to_napi_err)?;
        Ok(IrohConnection::from(conn))
    }

    /// Connect to a remote node using full address info (endpoint ID + relay + direct addrs).
    /// This is needed when nodes can't discover each other via relay (e.g. local connections).
    #[napi]
    pub async fn connect_node_addr(
        &self,
        endpoint_id: String,
        alpn: Buffer,
        direct_addrs: Option<Vec<String>>,
        relay_url: Option<String>,
    ) -> Result<IrohConnection> {
        let net = self.inner.net_client();
        let addr = aster_transport_core::CoreNodeAddr {
            endpoint_id,
            relay_url,
            direct_addresses: direct_addrs.unwrap_or_default(),
        };
        let conn = net
            .connect_node_addr(addr, alpn.to_vec())
            .await
            .map_err(to_napi_err)?;
        Ok(IrohConnection::from(conn))
    }

    /// Gracefully shut down the node.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        self.inner.clone().close().await;
        Ok(())
    }

    /// Export all transport-level metrics in Prometheus text exposition format.
    #[napi]
    pub fn transport_metrics_prometheus(&self) -> String {
        self.inner.transport_metrics_prometheus()
    }
}
