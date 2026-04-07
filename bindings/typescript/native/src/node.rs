//! IrohNode — wraps CoreNode from aster_transport_core.
//!
//! Mirrors bindings/python/rust/src/node.rs but uses NAPI-RS instead of PyO3.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::CoreNode;

use crate::blobs::BlobsClient;
use crate::docs::DocsClient;
use crate::error::to_napi_err;
use crate::gossip::GossipClient;
use crate::hooks::NodeHookReceiver;
use crate::net::IrohConnection;

/// IrohNode — a peer-to-peer Iroh node with all protocols enabled.
///
/// Create via `IrohNode.memory()` or `IrohNode.persistent(path)`.
#[napi]
pub struct IrohNode {
    inner: CoreNode,
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
    #[napi(factory)]
    pub async fn memory_with_alpns(alpns: Vec<Buffer>) -> Result<IrohNode> {
        let alpn_vecs: Vec<Vec<u8>> = alpns.into_iter().map(|b| b.to_vec()).collect();
        let inner = CoreNode::memory_with_alpns(alpn_vecs, None)
            .await
            .map_err(to_napi_err)?;
        Ok(IrohNode { inner })
    }

    /// Create a persistent node with custom ALPNs.
    #[napi(factory)]
    pub async fn persistent_with_alpns(path: String, alpns: Vec<Buffer>) -> Result<IrohNode> {
        let alpn_vecs: Vec<Vec<u8>> = alpns.into_iter().map(|b| b.to_vec()).collect();
        let inner = CoreNode::persistent_with_alpns(path, alpn_vecs, None)
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
    /// Returns [alpn, connection].
    #[napi]
    pub async fn accept_aster(&self) -> Result<IrohConnection> {
        let (_alpn, conn) = self
            .inner
            .clone()
            .accept_aster()
            .await
            .map_err(to_napi_err)?;
        Ok(IrohConnection::from(conn))
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

    /// Gracefully shut down the node.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        self.inner.clone().close().await;
        Ok(())
    }
}
