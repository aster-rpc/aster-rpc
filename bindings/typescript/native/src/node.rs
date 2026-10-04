//! `Node` — wraps [`aster::Node`] (plan D1).
//!
//! Started from an [`AsterConfig`](crate::config::JsAsterConfig) via
//! `Node.start(config)`; the RPC path, hooks, blobs/docs/gossip clients still
//! go through the transport-core node, reached with the `binding-support`
//! accessor (`aster::Node::core_node`).

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::binding_support::core::CoreNodeAddr;
use aster::binding_support::CoreNode;

use crate::blobs::BlobsClient;
use crate::config::{aster_err, JsAsterConfig};
use crate::docs::DocsClient;
use crate::error::to_napi_err;
use crate::gossip::GossipClient;
use crate::hooks::NodeHookReceiver;
use crate::net::IrohConnection;

/// A running Aster node (QUIC endpoint + blobs / docs / gossip).
///
/// Create one with `Node.start(config)`; always `close()` it (it flushes the
/// persistent store before tearing down).
#[napi]
pub struct Node {
    inner: aster::Node,
}

impl Node {
    /// The transport-core node, for sibling modules (reactor) and the
    /// still-core-backed methods below.
    pub(crate) fn core(&self) -> &CoreNode {
        self.inner.core_node()
    }
}

#[napi]
impl Node {
    /// Start a node from `config` (in-memory unless `storagePath` is set).
    ///
    /// A `secretKeyFile` is loaded, or created from `secretKey` / a fresh key
    /// when missing; an explicit `secretKey` that disagrees with an existing
    /// file is an error. `alpns` adds inbound ALPNs beyond blobs/docs/gossip.
    #[napi(factory)]
    pub async fn start(config: Option<JsAsterConfig>) -> Result<Node> {
        let config = config.unwrap_or_default().to_rust()?;
        let inner = aster::Node::start(config).await.map_err(aster_err)?;
        Ok(Node { inner })
    }

    /// This node's id (the hex public key of its secret key).
    #[napi]
    pub fn node_id(&self) -> String {
        self.inner.id().into_string()
    }

    /// The node's address info (debug format).
    #[napi]
    pub fn node_addr(&self) -> String {
        format!("{:?}", self.core().node_addr_info())
    }

    /// Direct socket addresses (`"ip:port"`) this node currently advertises.
    /// Empty until the endpoint has bound and discovered its local addresses.
    /// Used to put dialable material into `aster1` tickets.
    #[napi]
    pub fn direct_addresses(&self) -> Vec<String> {
        self.inner.addr().direct_addresses
    }

    /// The node's secret key (32 bytes). Persist it (or use `secretKeyFile`)
    /// for a restart-stable identity — the store does not keep it.
    #[napi]
    pub fn export_secret_key(&self) -> Result<Buffer> {
        let key = self.inner.export_secret_key().map_err(aster_err)?;
        Ok(Buffer::from(key.to_bytes().to_vec()))
    }

    /// Whether this node was started with hooks enabled.
    #[napi]
    pub fn has_hooks(&self) -> bool {
        self.core().has_hooks()
    }

    /// Take the hook receiver (one-shot). `null` if hooks are not enabled or
    /// it was already taken.
    #[napi]
    pub fn take_hook_receiver(&self) -> Option<NodeHookReceiver> {
        self.core()
            .take_hook_receiver()
            .map(NodeHookReceiver::from_core)
    }

    /// Accept the next incoming connection on a non-builtin ALPN. The ALPN is
    /// readable as `conn.alpn()`.
    #[napi]
    pub async fn accept_aster(&self) -> Result<IrohConnection> {
        let (alpn, conn) = self
            .core()
            .clone()
            .accept_aster()
            .await
            .map_err(to_napi_err)?;
        let mut iroh_conn = IrohConnection::from(conn);
        iroh_conn.alpn_tag = Some(String::from_utf8_lossy(&alpn).to_string());
        Ok(iroh_conn)
    }

    /// The blobs client for this node.
    #[napi]
    pub fn blobs_client(&self) -> BlobsClient {
        BlobsClient::from(self.core().blobs_client())
    }

    /// The docs client for this node.
    #[napi]
    pub fn docs_client(&self) -> DocsClient {
        DocsClient::from(self.core().docs_client())
    }

    /// The gossip client for this node.
    #[napi]
    pub fn gossip_client(&self) -> GossipClient {
        GossipClient::from(self.core().gossip_client())
    }

    /// Add another local node's address info (in-process peer discovery).
    #[napi]
    pub fn add_node_addr(&self, other: &Node) -> Result<()> {
        self.core().add_node_addr(other.core()).map_err(to_napi_err)
    }

    /// Connect to a node by id (hex) on `alpn`, relying on discovery.
    #[napi]
    pub async fn connect(&self, node_id: String, alpn: Buffer) -> Result<IrohConnection> {
        let conn = self
            .core()
            .net_client()
            .connect(node_id, alpn.to_vec())
            .await
            .map_err(to_napi_err)?;
        Ok(IrohConnection::from(conn))
    }

    /// Connect using full address info (id + direct addresses + relay URL) —
    /// needed when the peer is not discoverable (LAN / loopback, no relay).
    #[napi]
    pub async fn connect_node_addr(
        &self,
        endpoint_id: String,
        alpn: Buffer,
        direct_addrs: Option<Vec<String>>,
        relay_url: Option<String>,
    ) -> Result<IrohConnection> {
        let addr = CoreNodeAddr {
            endpoint_id,
            relay_url,
            direct_addresses: direct_addrs.unwrap_or_default(),
        };
        let conn = self
            .core()
            .net_client()
            .connect_node_addr(addr, alpn.to_vec())
            .await
            .map_err(to_napi_err)?;
        Ok(IrohConnection::from(conn))
    }

    /// Shut the node down cleanly (flushes persistent state first).
    /// Closes the shared node: every handle to it stops working.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        self.inner.clone().shutdown().await;
        Ok(())
    }

    /// Transport metrics in Prometheus text exposition format.
    #[napi]
    pub fn metrics_prometheus(&self) -> String {
        self.inner.metrics_prometheus()
    }
}
