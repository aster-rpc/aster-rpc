//! The Aster node — the entry point for everything else.

use crate::admission::Admission;
use crate::config::AsterConfig;
use crate::error::{Error, Result};
use crate::id::{NodeAddr, NodeId, SecretKey};
use crate::net::Connection;
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
                CoreNode::persistent_with_alpns_and_gc(
                    dir.to_string_lossy().into_owned(),
                    alpns,
                    Some(config.inner.clone()),
                    config.gc_interval,
                )
                .await?
            }
            None => {
                CoreNode::memory_with_alpns_and_gc(
                    alpns,
                    Some(config.inner.clone()),
                    config.gc_interval,
                )
                .await?
            }
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

    /// Dial `peer` on a custom `alpn`, returning a [`Connection`]. The ALPN must
    /// be one the peer accepts (a built-in protocol ALPN or one it registered via
    /// [`start_with_alpns`](Node::start_with_alpns)).
    pub async fn connect(&self, peer: &NodeId, alpn: impl Into<Vec<u8>>) -> Result<Connection> {
        let conn = self
            .inner
            .net_client()
            .connect(peer.as_str().to_string(), alpn.into())
            .await?;
        Ok(Connection::new(conn))
    }

    /// Accept the next inbound connection on any custom ALPN registered via
    /// [`start_with_alpns`](Node::start_with_alpns). Returns the ALPN it arrived
    /// on plus the [`Connection`] (demultiplex your protocols by the ALPN). Errors
    /// once the node is closed.
    pub async fn accept(&self) -> Result<(Vec<u8>, Connection)> {
        let (alpn, conn) = self.inner.accept_aster().await?;
        Ok((alpn, Connection::new(conn)))
    }

    /// Dial a peer using full address material (id + relay + direct addresses)
    /// on a custom `alpn`. Use when you have a peer's [`NodeAddr`] (e.g. from a
    /// [`Ticket`](crate::Ticket)) but it isn't yet in the address book — e.g. to
    /// present a credential *before* a docs join.
    pub async fn connect_addr(
        &self,
        addr: NodeAddr,
        alpn: impl Into<Vec<u8>>,
    ) -> Result<Connection> {
        let conn = self
            .inner
            .net_client()
            .connect_node_addr(addr.to_core(), alpn.into())
            .await?;
        Ok(Connection::new(conn))
    }

    /// Register a peer's address material (id + relay + direct addresses) in this
    /// node's address book, so later operations addressed by node id —
    /// [`connect`](Node::connect), docs joins — can reach it without discovery.
    pub fn add_node_addr(&self, addr: NodeAddr) -> Result<()> {
        self.inner.add_node_addr_info(addr.to_core())?;
        Ok(())
    }

    /// Dial a peer straight from a [`Ticket`](crate::Ticket) on a custom `alpn`.
    /// All dialable material in the ticket — direct addresses and relay — is
    /// used; Aster owns decoding the `aster1` token, so callers never decompose
    /// it by hand. Use to present a credential before a docs join.
    pub async fn connect_ticket(
        &self,
        ticket: &crate::ticket::Ticket,
        alpn: impl Into<Vec<u8>>,
    ) -> Result<Connection> {
        self.connect_addr(ticket.to_node_addr()?, alpn).await
    }

    /// Register a peer's [`Ticket`](crate::Ticket) address material (id + relay +
    /// direct addresses) in this node's address book, so later by-id operations —
    /// [`connect`](Node::connect), docs joins — can reach it. Returns the peer's
    /// [`NodeId`] for convenience.
    pub fn add_ticket_addr(&self, ticket: &crate::ticket::Ticket) -> Result<NodeId> {
        let addr = ticket.to_node_addr()?;
        let id = addr.node_id.clone();
        self.add_node_addr(addr)?;
        Ok(id)
    }

    /// Open an RPC connection to `peer` on the Aster RPC ALPN (`aster/1`). The
    /// peer must be serving RPC (started with the RPC ALPN registered and an
    /// [`rpc::Server`](crate::rpc::Server) running). Requires the `rpc` feature.
    #[cfg(feature = "rpc")]
    pub async fn rpc_connect(&self, peer: &NodeId) -> Result<crate::rpc::RpcConnection> {
        let conn = self
            .inner
            .net_client()
            .connect(
                peer.as_str().to_string(),
                aster_transport_core::reactor::RPC_ALPN.to_vec(),
            )
            .await?;
        Ok(crate::rpc::RpcConnection::from_core(conn))
    }

    /// Clone of the underlying core node — used by [`rpc::Server`](crate::rpc::Server)
    /// to own the reactor accept loop.
    #[cfg(feature = "rpc")]
    pub(crate) fn core(&self) -> CoreNode {
        self.inner.clone()
    }

    /// Create an [`Expose`](crate::expose::Expose) handle for publishing local
    /// HTTP services over this node's connections (the `aster-expose` L7 relay).
    /// Register services on it, then either let the node own the accept loop via
    /// [`serve_expose`](Node::serve_expose) or route connections into it with
    /// [`Expose::handle`](crate::expose::Expose::handle). See
    /// `docs/aster-expose-getstarted.md`.
    #[cfg(feature = "expose")]
    pub fn expose(&self) -> crate::expose::Expose {
        crate::expose::Expose::new()
    }

    /// Own the accept loop for a node whose endpoint serves **only** expose:
    /// accept inbound connections, and feed each whose ALPN is in `alpns` to
    /// `expose` (others are dropped). Runs until the node closes. The node must
    /// have been started with those ALPNs registered via
    /// [`start_with_alpns`](Node::start_with_alpns).
    ///
    /// Don't combine this with a manual [`accept`](Node::accept) loop or an
    /// `rpc::Server` on the same node — they drain the
    /// same inbound queue. For that case, run your own accept loop and call
    /// [`Expose::handle`](crate::expose::Expose::handle) per expose connection.
    #[cfg(feature = "expose")]
    pub async fn serve_expose(
        &self,
        expose: &crate::expose::Expose,
        alpns: &[&[u8]],
    ) -> Result<()> {
        loop {
            let (alpn, conn) = self.accept().await?;
            if alpns.is_empty() || alpns.contains(&alpn.as_slice()) {
                expose.handle(&conn);
            }
            // Non-expose ALPN: drop `conn` (closes it). The node should be
            // started only with expose ALPNs when using this loop.
        }
    }

    /// Local topology view (locality ladder + path quality per peer).
    /// Populates only when the node was started with
    /// [`AsterConfigBuilder::monitoring(true)`](crate::AsterConfigBuilder::monitoring);
    /// check [`Topology::has_monitoring`](crate::topology::Topology::has_monitoring).
    pub fn topology(&self) -> crate::topology::Topology {
        crate::topology::Topology::new(self.inner.clone())
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
