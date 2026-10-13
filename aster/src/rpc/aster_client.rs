//! [`AsterClient`] — the declarative, high-level consumer.
//!
//! Mirrors the Python/TS `AsterClient`: one call resolves the configuration,
//! starts a node, enrolls with the server when `[enrollment]` is configured,
//! and opens the RPC connection typed clients wrap.
//!
//! ```no_run
//! # use aster::{AsterConfig, rpc::AsterClient};
//! # async fn run() -> aster::Result<()> {
//! let client = AsterClient::builder()
//!     .config(AsterConfig::from_env()?) // ASTER_ENDPOINT_ADDR, ASTER_ENROLLMENT_*
//!     .connect()
//!     .await?;
//! // let mc = MissionControlClient::new(client.rpc());
//! client.close().await;
//! # Ok(())
//! # }
//! ```
//!
//! With enrollment configured the client presents its credential from
//! config, verifies the server's (the handshake is reciprocal), synchronizes
//! the fleet namespace the server offers, and renews its credential when an
//! issuer is configured or the server signed it.

use std::path::PathBuf;

use crate::config::AsterConfig;
use crate::error::{Error, Result};
use crate::id::{NodeAddr, NodeId, SecretKey};
use crate::node::Node;
use crate::ticket::Ticket;

use super::client::RpcConnection;

/// Builder for an [`AsterClient`]. Start one with [`AsterClient::builder`].
#[derive(Default)]
pub struct AsterClientBuilder {
    config: Option<AsterConfig>,
    address: Option<String>,
    identity: Option<PathBuf>,
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    enrollment_policy: Option<std::sync::Arc<dyn crate::enrollment::AdmissionPolicy>>,
}

impl AsterClientBuilder {
    /// Base configuration. Without it, an all-defaults in-memory config is
    /// used (no enrollment).
    pub fn config(mut self, config: AsterConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// The server to connect to: its `aster1…` address ticket
    /// ([`AsterServer::address`](super::AsterServer::address)). Overrides the
    /// config's `endpoint_addr` (`ASTER_ENDPOINT_ADDR`, `[connect]
    /// endpoint_addr`).
    pub fn address(mut self, address: impl Into<String>) -> Self {
        self.address = Some(address.into());
        self
    }

    /// Keep this client's node key in a secret key file (load or create), for
    /// a restart-stable identity: the key a credential was issued for.
    pub fn identity(mut self, path: impl Into<PathBuf>) -> Self {
        self.identity = Some(path.into());
        self
    }

    /// Judge the server's enrollment evidence with this policy instead of
    /// the configured `[enrollment] policy`.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    pub fn enrollment_policy(
        mut self,
        policy: impl crate::enrollment::AdmissionPolicy + 'static,
    ) -> Self {
        self.enrollment_policy = Some(std::sync::Arc::new(policy));
        self
    }

    /// Start the node, enroll with the server when configured, and open the
    /// RPC connection.
    pub async fn connect(self) -> Result<AsterClient> {
        let mut builder = self
            .config
            .unwrap_or_else(|| AsterConfig::builder().build())
            .into_builder();
        if let Some(path) = &self.identity {
            if let Some(key) = SecretKey::read_file_if_exists(path)? {
                builder = builder.secret_key(key);
            }
            builder = builder.secret_key_file(path.clone());
        }
        #[cfg_attr(not(all(feature = "enrollment", feature = "docs")), allow(unused_mut))]
        let mut config = builder.build();
        let address = self
            .address
            .or_else(|| config.endpoint_addr.clone())
            .ok_or_else(|| {
                Error::InvalidArgument(
                    "AsterClient needs the server address: AsterClientBuilder::address, \
                     ASTER_ENDPOINT_ADDR or [connect] endpoint_addr"
                        .into(),
                )
            })?;
        let server = Ticket::from_base58(address.trim())?.to_node_addr()?;

        #[cfg(all(feature = "enrollment", feature = "docs"))]
        let plan = crate::enrollment::runtime::plan(
            &mut config,
            crate::enrollment::runtime::Side::Client,
            crate::enrollment::runtime::Overrides {
                policy: self.enrollment_policy,
                roles: None,
            },
        )?;
        #[cfg(not(all(feature = "enrollment", feature = "docs")))]
        if config.is_enrollment_enabled() {
            return Err(Error::InvalidArgument(
                "enrollment is configured ([enrollment] / ASTER_ENROLLMENT_*) but aster was \
                 built without the `enrollment` and `docs` features"
                    .into(),
            ));
        }

        #[cfg(all(feature = "enrollment", feature = "docs"))]
        let alpns = if plan.is_some() {
            vec![crate::enrollment::ENROLLMENT_ALPN.to_vec()]
        } else {
            Vec::new()
        };
        #[cfg(not(all(feature = "enrollment", feature = "docs")))]
        let alpns = Vec::new();
        let node = Node::start_with_alpns(config, alpns).await?;

        #[cfg(all(feature = "enrollment", feature = "docs"))]
        let enrollment = match plan {
            None => None,
            Some(plan) => match enroll(&node, plan, &server).await {
                Ok(enrolled) => Some(enrolled),
                Err(err) => {
                    node.shutdown().await;
                    return Err(err);
                }
            },
        };

        if let Err(err) = node.add_node_addr(server.clone()) {
            node.shutdown().await;
            return Err(err);
        }
        let rpc = match node.rpc_connect(&server.node_id).await {
            Ok(rpc) => rpc,
            Err(err) => {
                node.shutdown().await;
                return Err(err);
            }
        };
        Ok(AsterClient {
            node,
            server,
            rpc,
            #[cfg(all(feature = "enrollment", feature = "docs"))]
            enrollment,
        })
    }
}

/// The Enrollment an [`AsterClient`] installed from its configuration.
#[cfg(all(feature = "enrollment", feature = "docs"))]
struct ClientEnrollment {
    enrollment: crate::enrollment::Enrollment,
    renewal: Option<crate::enrollment::RenewalHandle>,
    /// Inbound enrollment handshakes (a peer presenting a renewed credential).
    accept: tokio::task::JoinHandle<()>,
    /// Kept so a temporary state directory lives as long as the client.
    _plan: crate::enrollment::runtime::Plan,
}

#[cfg(all(feature = "enrollment", feature = "docs"))]
impl Drop for ClientEnrollment {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

#[cfg(all(feature = "enrollment", feature = "docs"))]
async fn enroll(
    node: &Node,
    plan: crate::enrollment::runtime::Plan,
    server: &NodeAddr,
) -> Result<ClientEnrollment> {
    use crate::enrollment::runtime::serve_inbound_enrollment;
    let installed = plan.install(node)?;
    let enrollment = installed.enrollment;
    // The server re-presents its credential after renewing it, over a new
    // enrollment connection to this node.
    let accept = serve_inbound_enrollment(node, &enrollment);
    let enrolled = plan.enroll_with_server(&enrollment, server).await;
    if let Err(err) = enrolled {
        accept.abort();
        return Err(err);
    }
    let renewal = plan.start_renewal(&enrollment, Some(server));
    Ok(ClientEnrollment {
        enrollment,
        renewal,
        accept,
        _plan: plan,
    })
}

/// A connected, declarative Aster RPC client. Wrap [`rpc`](Self::rpc) in a
/// generated typed client (`MissionControlClient::new(client.rpc())`).
pub struct AsterClient {
    node: Node,
    server: NodeAddr,
    rpc: RpcConnection,
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    enrollment: Option<ClientEnrollment>,
}

impl AsterClient {
    /// Start configuring a client.
    pub fn builder() -> AsterClientBuilder {
        AsterClientBuilder::default()
    }

    /// Connect to `address` (an `aster1…` ticket) with `config`.
    pub async fn connect(address: impl Into<String>, config: AsterConfig) -> Result<Self> {
        Self::builder()
            .config(config)
            .address(address)
            .connect()
            .await
    }

    /// The RPC connection to the server (cheap to clone).
    pub fn rpc(&self) -> RpcConnection {
        self.rpc.clone()
    }

    /// Open a fresh RPC connection to the server, for example after the
    /// previous one closed.
    pub async fn reconnect(&mut self) -> Result<RpcConnection> {
        self.rpc = self.node.rpc_connect(&self.server.node_id).await?;
        Ok(self.rpc.clone())
    }

    /// The server's node id.
    pub fn server_id(&self) -> NodeId {
        self.server.node_id.clone()
    }

    /// This client's node id.
    pub fn id(&self) -> NodeId {
        self.node.id()
    }

    /// The underlying [`Node`] (escape hatch).
    pub fn node(&self) -> &Node {
        &self.node
    }

    /// The [`Enrollment`](crate::enrollment::Enrollment) installed from
    /// `[enrollment]`, or `None` when enrollment is off.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    pub fn enrollment(&self) -> Option<&crate::enrollment::Enrollment> {
        self.enrollment.as_ref().map(|e| &e.enrollment)
    }

    /// Whether this client is renewing its credential.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    pub fn is_renewing(&self) -> bool {
        self.enrollment
            .as_ref()
            .is_some_and(|e| e.renewal.is_some())
    }

    /// Stop renewing and close the node.
    pub async fn close(self) {
        #[cfg(all(feature = "enrollment", feature = "docs"))]
        drop(self.enrollment);
        self.node.shutdown().await;
    }
}
