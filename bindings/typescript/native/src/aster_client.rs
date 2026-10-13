//! `AsterClient` — wraps [`aster::rpc::AsterClient`], the declarative client
//! driven by [`AsterConfig`](crate::config::JsAsterConfig).
//!
//! The Rust client owns the whole connect sequence: it starts the node, and
//! when `[enrollment]` is configured it installs Enrollment, enrolls with the
//! server over the reciprocal handshake, synchronizes the fleet namespace and
//! starts renewal (`docs/_internal/working_ideas/aster-enrollment.md` §15.7)
//! before it opens the RPC connection. JavaScript gets the node (blobs, docs)
//! and the `aster/1` connection the TypeScript transport runs on.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::{Credential, NodeAddr, NodeId, Ticket};

use crate::config::{aster_err, JsAsterConfig};
use crate::net::IrohConnection;
use crate::node::Node;

/// An `aster1…` ticket as is; a 64-hex node id as an address-less ticket
/// (dialed through discovery).
fn ticket_for(address: &str) -> Result<String> {
    let address = address.trim();
    if address.starts_with("aster1") {
        return Ok(address.to_string());
    }
    let is_hex = address.len() == 64 && address.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_hex {
        return Err(Error::new(
            Status::InvalidArg,
            format!("AsterClient address must be an aster1… ticket or a 64-hex node id, got {address:?}"),
        ));
    }
    let addr = NodeAddr {
        node_id: NodeId::from_hex(address.to_ascii_lowercase()),
        relay_url: None,
        direct_addresses: Vec::new(),
    };
    Ticket::from_node_addr(&addr, Credential::Open)
        .and_then(|t| t.to_base58())
        .map_err(aster_err)
}

/// What a server's `aster.ops.NodeInfo.describe` returns; attribute values
/// as text (numbers and booleans in their display form).
#[napi(object)]
pub struct ServerIdentity {
    pub node_id: String,
    pub node_name: String,
    pub tags: Vec<String>,
    pub roles: Vec<String>,
    pub attributes: std::collections::HashMap<String, String>,
}

/// A connected client: a node plus an RPC connection to one server, enrolled
/// with it when the configuration enables enrollment.
///
/// `AsterClient.connect(config, address)`; always `close()` it (it stops
/// renewal and shuts the node down).
#[napi]
pub struct AsterClient {
    inner: std::sync::Arc<tokio::sync::Mutex<Option<aster::rpc::AsterClient>>>,
    node: aster::Node,
    rpc: std::sync::Mutex<aster::binding_support::CoreConnection>,
    server_id: String,
    enrolled: bool,
}

#[napi]
impl AsterClient {
    /// Start a node from `config` and connect to `address` (an `aster1…`
    /// ticket or a 64-hex node id). With `[enrollment]` configured the client
    /// presents its credential, enrolls and synchronizes before the RPC
    /// connection opens; any misconfiguration or refusal rejects with an
    /// error naming the problem.
    #[napi(ts_return_type = "Promise<AsterClient>")]
    pub fn connect<'env>(
        env: &'env Env,
        config: Option<JsAsterConfig>,
        address: String,
    ) -> Result<PromiseRaw<'env, AsterClient>> {
        let config = config.unwrap_or_default().to_rust()?;
        let ticket = ticket_for(&address)?;
        env.spawn_future(async move {
            let client = aster::rpc::AsterClient::builder()
                .config(config)
                .address(ticket)
                .connect()
                .await
                .map_err(aster_err)?;
            let node = client.node().clone();
            let rpc = client.rpc().core_connection().clone();
            let server_id = client.server_id().into_string();
            let enrolled = client.enrollment().is_some();
            Ok(AsterClient {
                inner: std::sync::Arc::new(tokio::sync::Mutex::new(Some(client))),
                node,
                rpc: std::sync::Mutex::new(rpc),
                server_id,
                enrolled,
            })
        })
    }

    /// This client's node (blobs, docs, gossip). Closed by `close()`.
    #[napi]
    pub fn node(&self) -> Node {
        Node::from_aster(self.node.clone())
    }

    /// The current `aster/1` connection to the server.
    #[napi]
    pub fn rpc_connection(&self) -> IrohConnection {
        IrohConnection::from(self.rpc.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// Open a fresh `aster/1` connection to the server (for example after
    /// the previous one closed) and make it current.
    #[napi]
    pub async fn reconnect(&self) -> Result<IrohConnection> {
        let mut guard = self.inner.lock().await;
        let client = guard
            .as_mut()
            .ok_or_else(|| Error::from_reason("AsterClient is closed"))?;
        let rpc = client.reconnect().await.map_err(aster_err)?;
        let core = rpc.core_connection().clone();
        *self.rpc.lock().unwrap_or_else(|e| e.into_inner()) = core.clone();
        Ok(IrohConnection::from(core))
    }

    /// The server's node id (hex).
    #[napi(getter)]
    pub fn server_id(&self) -> String {
        self.server_id.clone()
    }

    /// This client's node id (hex).
    #[napi(getter)]
    pub fn id(&self) -> String {
        self.node.id().into_string()
    }

    /// Whether `[enrollment]` installed Enrollment and the client enrolled
    /// with the server.
    #[napi(getter)]
    pub fn enrolled(&self) -> bool {
        self.enrolled
    }

    /// Whether this client renews its credential (from `renewFrom`, or from
    /// the server when the server signed it).
    #[napi]
    pub async fn is_renewing(&self) -> bool {
        self.inner
            .lock()
            .await
            .as_ref()
            .is_some_and(|c| c.is_renewing())
    }

    /// The credential chain this client currently presents (it changes when
    /// renewal succeeds); `null` without enrollment.
    #[napi]
    pub async fn credential(&self) -> Option<String> {
        let guard = self.inner.lock().await;
        let enrollment = guard.as_ref()?.enrollment()?;
        enrollment.credential().map(|(chain, _)| chain)
    }

    /// Call the server's baseline `aster.ops.NodeInfo.describe`. A server
    /// announces its contract registry there (the `aster.registry_namespace`
    /// attribute), which replaces the retired consumer-admission response.
    #[napi]
    pub async fn describe_server(&self) -> Result<ServerIdentity> {
        let rpc = {
            let guard = self.inner.lock().await;
            guard
                .as_ref()
                .ok_or_else(|| Error::from_reason("AsterClient is closed"))?
                .rpc()
        };
        let identity = aster::rpc::baseline::NodeInfoClient::new(rpc)
            .describe()
            .await
            .map_err(aster_err)?;
        let attributes = identity
            .attributes
            .iter()
            .map(|attr| {
                let value = attr.value.as_ref().map(|value| {
                    if let Some(text) = value.as_text() {
                        text.to_string()
                    } else if let Some(v) = value.as_int() {
                        v.to_string()
                    } else if let Some(v) = value.as_float() {
                        v.to_string()
                    } else if let Some(v) = value.as_bool() {
                        v.to_string()
                    } else {
                        String::new()
                    }
                });
                (attr.name.clone(), value.unwrap_or_default())
            })
            .collect();
        Ok(ServerIdentity {
            node_id: identity.node_id,
            node_name: identity.node_name,
            tags: identity.tags,
            roles: identity.roles,
            attributes,
        })
    }

    /// Stop renewing and shut the node down. Idempotent.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        let client = self.inner.lock().await.take();
        if let Some(client) = client {
            client.close().await;
        }
        Ok(())
    }
}
