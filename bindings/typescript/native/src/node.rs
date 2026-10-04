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

use std::sync::Arc;

use crate::admission::{AdmissionHub, AdmissionRequests, GatePolicy};
use crate::blobs::BlobsClient;
use crate::config::{aster_err, JsAsterConfig};
use crate::docs::DocsClient;
use crate::error::to_napi_err;
use crate::gossip::GossipClient;
use crate::lease::{GrantorSerializer, MemorySerializer};
use crate::net::IrohConnection;
use crate::topology::Topology;

/// A running Aster node (QUIC endpoint + blobs / docs / gossip).
///
/// Create one with `Node.start(config)`; always `close()` it (it flushes the
/// persistent store before tearing down).
#[napi]
pub struct Node {
    inner: aster::Node,
    /// Gate 0 / admission state; `None` when hooks are off.
    admission: Option<Arc<AdmissionHub>>,
}

impl Node {
    /// The transport-core node, for sibling modules (reactor) and the
    /// still-core-backed methods below.
    pub(crate) fn core(&self) -> &CoreNode {
        self.inner.core_node()
    }

    /// The `aster` node (typed blobs / docs / gossip facades).
    pub(crate) fn aster(&self) -> &aster::Node {
        &self.inner
    }

    fn hub(&self) -> Result<&Arc<AdmissionHub>> {
        self.admission.as_ref().ok_or_else(|| {
            Error::from_reason(
                "node has no admission hooks (start it with enableHooks: true or a gate)",
            )
        })
    }
}

#[napi]
impl Node {
    /// Start a node from `config` (in-memory unless `storagePath` is set).
    ///
    /// A `secretKeyFile` is loaded, or created from `secretKey` / a fresh key
    /// when missing; an explicit `secretKey` that disagrees with an existing
    /// file is an error. `alpns` adds inbound ALPNs beyond blobs/docs/gossip.
    ///
    /// `gate` attaches a Gate-0 policy (and turns `enableHooks` on): inbound
    /// admission is then decided in Rust. Without a gate, a node with
    /// `enableHooks` hands decisions to `admission()`.
    #[napi(ts_return_type = "Promise<Node>")]
    pub fn start<'env>(
        env: &'env Env,
        config: Option<JsAsterConfig>,
        gate: Option<&GatePolicy>,
    ) -> Result<PromiseRaw<'env, Node>> {
        let mut config = config.unwrap_or_default().to_rust()?;
        if gate.is_some() {
            config.enable_hooks = true;
        }
        let hooks = config.enable_hooks;
        let gate = gate.map(|g| GatePolicy {
            inner: g.inner.clone(),
        });
        env.spawn_future(async move {
            let inner = aster::Node::start(config).await.map_err(aster_err)?;
            let admission = if hooks {
                inner
                    .core_node()
                    .take_hook_receiver()
                    .map(|rx| AdmissionHub::spawn(rx, gate.as_ref()))
            } else {
                None
            };
            Ok(Node { inner, admission })
        })
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

    /// Whether this node was started with admission hooks (`enableHooks` or
    /// a gate).
    #[napi]
    pub fn has_hooks(&self) -> bool {
        self.admission.is_some()
    }

    /// Attach (`gate`) or detach (`null`) a Gate-0 policy. Applies from the
    /// next admission decision. Errors on a node without admission hooks.
    #[napi]
    pub fn set_gate(&self, gate: Option<&GatePolicy>) -> Result<()> {
        self.hub()?.set_gate(gate);
        Ok(())
    }

    /// The admission requests this node raises while no gate is attached —
    /// iterate with `for await` and `accept()` / `reject(code, reason)` each.
    /// One iterator per node. Errors on a node without admission hooks.
    #[napi]
    pub fn admission(&self) -> Result<AdmissionRequests> {
        self.hub()?.open()
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
        BlobsClient::new(&self.inner)
    }

    /// The docs client for this node.
    #[napi]
    pub fn docs_client(&self) -> DocsClient {
        DocsClient::from(self.inner.docs())
    }

    /// The gossip client for this node.
    #[napi]
    pub fn gossip_client(&self) -> GossipClient {
        GossipClient::from(self.inner.gossip())
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

    /// This node's topology view (v1 peers; v2 shared swarm). Populates only
    /// when the node runs with `enableMonitoring`.
    #[napi]
    pub fn topology(&self) -> Topology {
        Topology::from(self.inner.topology())
    }

    /// A lease backend on the designated grantor `grantorId`
    /// (`aster.lease.Grantor` over RPC). Every op is bound to this node's
    /// authenticated identity. The grantor must be dialable (discovery or
    /// `addNodeAddr`).
    #[napi]
    pub async fn lease_grantor(&self, grantor_id: String) -> Result<GrantorSerializer> {
        let conn = self
            .inner
            .rpc_connect(&aster::NodeId::from_hex(grantor_id))
            .await
            .map_err(aster_err)?;
        let self_id = self.inner.id().into_string();
        Ok(GrantorSerializer::new(
            aster::rpc::lease::GrantorSerializer::new(conn, self_id.clone()),
            self_id,
        ))
    }

    /// Serve `aster.lease.Grantor` over `serializer` on this node — making it
    /// a designated grantor. The node must be started with the `aster/1`
    /// ALPN in `alpns`, and this owns its RPC accept loop: do not also run
    /// an RPC server on the same node.
    #[napi]
    pub fn serve_lease_grantor(
        &self,
        serializer: &MemorySerializer,
        options: Option<LeaseGrantorOptions>,
    ) -> LeaseGrantorServer {
        let mut grantor = aster::rpc::lease::LeaseGrantor::new(Arc::new(serializer.inner.clone()));
        if options.and_then(|o| o.open_revoke).unwrap_or(false) {
            grantor = grantor.revoke_requires(None);
        }
        // napi v3 does not enter the Tokio runtime around synchronous exports.
        let server = aster::rpc::Server::new(&self.inner).register(grantor);
        let handle = within_runtime_if_available(move || server.serve());
        LeaseGrantorServer {
            handle: std::sync::Mutex::new(Some(handle)),
        }
    }
}

/// Options for `node.serveLeaseGrantor`.
#[napi(object)]
pub struct LeaseGrantorOptions {
    /// Let any admitted caller `revoke` (default: the `operator` role only).
    pub open_revoke: Option<bool>,
}

/// A running lease grantor; `close()` stops serving.
#[napi]
pub struct LeaseGrantorServer {
    handle: std::sync::Mutex<Option<aster::rpc::ServerHandle>>,
}

#[napi]
impl LeaseGrantorServer {
    /// Stop serving (idempotent).
    #[napi]
    pub fn close(&self) {
        if let Some(h) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            h.abort();
        }
    }
}
