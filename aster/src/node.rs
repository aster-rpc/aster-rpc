//! The Aster node — the entry point for everything else.

use crate::admission::Admission;
use crate::config::AsterConfig;
use crate::error::{Error, Result};
use crate::id::{NodeAddr, NodeId, SecretKey};
use aster_transport_core::CoreNode;

#[cfg(feature = "blobs")]
use crate::blobs::Blobs;
#[cfg(feature = "docs")]
use crate::docs::Docs;
#[cfg(feature = "gossip")]
use crate::gossip::Gossip;

/// A running Aster node.
///
/// Create one with [`Node::start`]. Clone-cheap handles to the protocols are
/// obtained via [`blobs`](Node::blobs), [`docs`](Node::docs) and
/// [`gossip`](Node::gossip). Always call [`shutdown`](Node::shutdown) for a
/// clean, durable close (it flushes the store before tearing down).
#[derive(Clone)]
pub struct Node {
    inner: CoreNode,
}

impl Node {
    /// Start a node from `config`. The node is in-memory unless the config sets
    /// a persistent data directory.
    pub async fn start(config: AsterConfig) -> Result<Node> {
        Self::start_with_alpns(config, Vec::new()).await
    }

    /// Like [`start`](Node::start) but also accept inbound connections on each
    /// custom ALPN in `alpns` (for non-RPC protocols layered on the node).
    pub async fn start_with_alpns(config: AsterConfig, alpns: Vec<Vec<u8>>) -> Result<Node> {
        let core = match &config.data_dir {
            Some(dir) => {
                CoreNode::persistent_with_alpns(
                    dir.to_string_lossy().into_owned(),
                    alpns,
                    Some(config.inner.clone()),
                )
                .await?
            }
            None => CoreNode::memory_with_alpns(alpns, Some(config.inner.clone())).await?,
        };
        Ok(Node { inner: core })
    }

    /// This node's identity.
    pub fn id(&self) -> NodeId {
        NodeId::from_hex(self.inner.node_id())
    }

    /// This node's address (id + relay + direct addresses).
    pub fn addr(&self) -> NodeAddr {
        let core = self.inner.node_addr_info();
        NodeAddr {
            node_id: NodeId::from_hex(core.endpoint_id),
            relay_url: core.relay_url,
            direct_addresses: core.direct_addresses,
        }
    }

    /// Teach this node how to reach `other` directly (without relay or
    /// discovery). Useful for connecting in-memory nodes in tests; production
    /// peers are normally resolved by id via relay/discovery.
    pub fn add_peer(&self, other: &Node) -> Result<()> {
        self.inner.add_node_addr(&other.inner)?;
        Ok(())
    }

    /// Export this node's secret key, so the same identity can be restored on a
    /// later start via [`AsterConfigBuilder::secret_key`](crate::AsterConfigBuilder::secret_key).
    pub fn export_secret_key(&self) -> Result<SecretKey> {
        let bytes = self.inner.export_secret_key();
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::InvalidArgument("secret key is not 32 bytes".into()))?;
        Ok(SecretKey::from_bytes(arr))
    }

    /// Take the admission handle (one-shot). Returns `None` unless the node was
    /// started with [`hooks(true)`](crate::AsterConfigBuilder::hooks), or if it
    /// was already taken.
    ///
    /// Inbound peers are gated at the post-handshake stage — see
    /// [`Admission::next_handshake`].
    pub fn take_admission(&self) -> Option<Admission> {
        self.inner.take_hook_receiver().map(Admission::new)
    }

    /// Blob store client.
    #[cfg(feature = "blobs")]
    pub fn blobs(&self) -> Blobs {
        Blobs::new(self.inner.blobs_client())
    }

    /// Document sync client.
    #[cfg(feature = "docs")]
    pub fn docs(&self) -> Docs {
        Docs::new(self.inner.docs_client())
    }

    /// Gossip pub-sub client.
    #[cfg(feature = "gossip")]
    pub fn gossip(&self) -> Gossip {
        Gossip::new(self.inner.gossip_client())
    }

    /// Transport-level metrics in Prometheus text exposition format.
    #[cfg(feature = "metrics")]
    pub fn metrics_prometheus(&self) -> String {
        self.inner.transport_metrics_prometheus()
    }

    /// Cleanly shut the node down, flushing persistent state first.
    pub async fn shutdown(self) {
        self.inner.close().await;
    }
}
