//! [`AsterServer`] — the declarative, high-level producer.
//!
//! Mirrors the Python/TS `AsterServer`: one node, one identity, RPC plus the
//! built-in protocol ALPNs registered for you, and your services served once
//! [`start`](AsterServerBuilder::start) returns. It folds the three steps from
//! the low-level API (resolve config → [`Node::start_with_alpns`] → build an
//! [`Server`] and [`serve`](Server::serve)) into one builder.
//!
//! ```no_run
//! # use aster::rpc::AsterServer;
//! # async fn run<S: aster::rpc::ServiceDispatch>(svc: S) -> aster::Result<()> {
//! let srv = AsterServer::builder()
//!     .service(svc)                 // repeat for each service
//!     .identity(".aster-identity")  // restart-stable endpoint identity
//!     .persistent("/var/lib/app")   // durable blobs / docs / gossip
//!     .start()
//!     .await?;
//!
//! println!("address: {}", srv.address()?);
//! srv.run().await; // serve until the node closes
//! # Ok(())
//! # }
//! ```
//!
//! ## Differences from the Python binding
//!
//! - Services are added with chained [`service`](AsterServerBuilder::service)
//!   calls rather than a `services=[…]` list (Rust can't hold heterogeneous
//!   service instances in one `Vec`).
//! - [`identity`](AsterServerBuilder::identity) persists only the endpoint
//!   **secret key** (64 hex chars), load-or-create. The Python `.aster-identity`
//!   is a richer JSON credential file (root pubkey + enrollment material); Gate-1
//!   credential plumbing isn't in the Rust crate yet, so this is the key alone.
//! - Lifecycle is explicit ([`run`](AsterServer::run) /
//!   [`shutdown`](AsterServer::shutdown)) rather than `async with`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{AsterConfig, RelayMode};
use crate::error::{Error, Result};
use crate::id::{NodeId, SecretKey};
use crate::node::Node;
use crate::ticket::{Credential, Ticket};

use super::auth::{AttributeStore, Authenticator};
use super::server::{Dispatcher, HttpTransport, Server, ServerHandle, ServiceDispatch, RPC_ALPN};

/// Builder for an [`AsterServer`]. Start one with [`AsterServer::builder`].
#[derive(Default)]
pub struct AsterServerBuilder {
    services: Vec<Arc<dyn ServiceDispatch>>,
    config: Option<AsterConfig>,
    identity: Option<PathBuf>,
    persistent: Option<PathBuf>,
    relay: Option<RelayMode>,
    discovery: Option<bool>,
    hooks: Option<bool>,
    attributes: Option<AttributeStore>,
    authenticator: Option<Arc<dyn Authenticator>>,
    http: Option<Box<dyn HttpTransport>>,
    // Deferred `Server::register_session` calls (each captures a factory).
    #[allow(clippy::type_complexity)]
    session_appliers: Vec<Box<dyn FnOnce(Server) -> Server + Send>>,
    extra_alpns: Vec<Vec<u8>>,
}

impl AsterServerBuilder {
    /// Add a service to serve (chainable; call once per service). At least one
    /// is required before [`start`](Self::start).
    pub fn service(mut self, svc: impl ServiceDispatch) -> Self {
        self.services.push(Arc::new(svc));
        self
    }

    /// Base configuration. Convenience setters below
    /// ([`persistent`](Self::persistent), [`relay`](Self::relay),
    /// [`discovery`](Self::discovery), [`hooks`](Self::hooks),
    /// [`identity`](Self::identity)) override the corresponding fields. Without
    /// this, an all-defaults in-memory config is used.
    pub fn config(mut self, config: AsterConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Persist the endpoint's secret key at `path` for a restart-stable
    /// [`NodeId`]. The file is created (64 hex chars) on first start and read on
    /// subsequent starts. Overrides any `secret_key` in [`config`](Self::config).
    pub fn identity(mut self, path: impl Into<PathBuf>) -> Self {
        self.identity = Some(path.into());
        self
    }

    /// Make the node persistent, storing blobs / docs / gossip state under `dir`.
    pub fn persistent(mut self, dir: impl Into<PathBuf>) -> Self {
        self.persistent = Some(dir.into());
        self
    }

    /// Select the relay mode.
    pub fn relay(mut self, mode: RelayMode) -> Self {
        self.relay = Some(mode);
        self
    }

    /// Enable local-network (mDNS) peer discovery.
    pub fn discovery(mut self, enabled: bool) -> Self {
        self.discovery = Some(enabled);
        self
    }

    /// Enable admission hooks (required to later call
    /// [`take_admission`](AsterServer::take_admission) for Gate 0).
    pub fn hooks(mut self, enabled: bool) -> Self {
        self.hooks = Some(enabled);
        self
    }

    /// Use a shared [`AttributeStore`] for Gate-3 capability checks. Keep a clone
    /// and populate it from your admission logic so per-call `requires` checks see
    /// each peer's roles. Without this, an empty store is used.
    pub fn attributes(mut self, store: AttributeStore) -> Self {
        self.attributes = Some(store);
        self
    }

    /// Attach a pre-dispatch [`Authenticator`] (runs before Gate-3 on every
    /// call; can reject and resolve principal + attributes). See
    /// [`Server::authenticator`](super::server::Server::authenticator).
    pub fn authenticator(mut self, auth: impl Authenticator) -> Self {
        self.authenticator = Some(Arc::new(auth));
        self
    }

    /// Register a **session-scoped** service: `factory` builds a fresh instance
    /// per session id. See [`Server::register_session`](super::server::Server::register_session).
    pub fn register_session<S, F>(mut self, factory: F) -> Self
    where
        S: ServiceDispatch,
        F: Fn() -> S + Send + Sync + 'static,
    {
        self.session_appliers
            .push(Box::new(move |s| s.register_session(factory)));
        self
    }

    /// Also serve over an additional transport (e.g. the HTTP/Salvo transport)
    /// on the *same* dispatcher. Off by default — Iroh-only. The transport crate
    /// provides the config type (`aster_transport_salvo::HttpConfig`); it's
    /// spawned at [`start`](Self::start) and aborted on shutdown.
    pub fn with_http(mut self, transport: impl HttpTransport) -> Self {
        self.http = Some(Box::new(transport));
        self
    }

    /// Register an additional inbound ALPN beyond `aster/1` and the built-in
    /// blobs / docs / gossip protocols (e.g. an admission ALPN).
    pub fn alpn(mut self, alpn: impl Into<Vec<u8>>) -> Self {
        self.extra_alpns.push(alpn.into());
        self
    }

    /// Build the node (RPC + built-in protocol ALPNs registered), start serving
    /// the registered services, and return the running [`AsterServer`].
    pub async fn start(self) -> Result<AsterServer> {
        if self.services.is_empty() {
            return Err(Error::InvalidArgument(
                "AsterServer requires at least one service".into(),
            ));
        }

        // Resolve config: base (or all-defaults) + convenience overrides.
        let mut builder = self
            .config
            .unwrap_or_else(|| AsterConfig::builder().build())
            .into_builder();
        if let Some(dir) = self.persistent {
            builder = builder.persistent(dir);
        }
        if let Some(mode) = self.relay {
            builder = builder.relay(mode);
        }
        if let Some(on) = self.discovery {
            builder = builder.discovery(on);
        }
        if let Some(on) = self.hooks {
            builder = builder.hooks(on);
        }
        // Identity file present → pin the endpoint key before start.
        if let Some(path) = &self.identity {
            if let Some(key) = read_identity(path)? {
                builder = builder.secret_key(key);
            }
        }
        let config = builder.build();

        // `aster/1` always; blobs/docs/gossip ALPNs are registered by core.
        let mut alpns = vec![RPC_ALPN.to_vec()];
        alpns.extend(self.extra_alpns);

        let node = Node::start_with_alpns(config, alpns).await?;

        // First boot with an identity path and no file yet → persist the key the
        // node just generated, so the NodeId is stable across restarts.
        if let Some(path) = &self.identity {
            if !path.exists() {
                write_identity(path, &node.export_secret_key()?)?;
            }
        }

        let attributes = self.attributes.unwrap_or_default();
        let mut server = Server::new(&node).attributes(attributes.clone());
        if let Some(auth) = self.authenticator {
            server = server.authenticator_arc(auth);
        }
        for svc in self.services {
            server = server.register_arc(svc);
        }
        for apply in self.session_appliers {
            server = apply(server);
        }
        // Grab the shareable dispatcher before `serve()` consumes the server, so
        // an HTTP transport can drive the same services.
        let dispatcher = server.dispatcher();
        let handle = server.serve();
        let http = self.http.map(|t| t.serve(dispatcher.clone()));

        Ok(AsterServer {
            node,
            handle,
            attributes,
            dispatcher,
            http,
        })
    }
}

/// A running, declarative Aster RPC server: a node serving blobs / docs / gossip
/// alongside `aster/1` RPC, all on one endpoint and one [`NodeId`].
///
/// Build one with [`AsterServer::builder`]. Keep the value alive for as long as
/// you want to serve; end it with [`run`](Self::run) (serve until the node
/// closes) or [`shutdown`](Self::shutdown) (graceful, flushes the store).
pub struct AsterServer {
    node: Node,
    handle: ServerHandle,
    attributes: AttributeStore,
    dispatcher: Dispatcher,
    /// Optional additional transport (HTTP) task, aborted on shutdown.
    http: Option<tokio::task::JoinHandle<()>>,
}

impl AsterServer {
    /// Start configuring a server.
    pub fn builder() -> AsterServerBuilder {
        AsterServerBuilder::default()
    }

    /// This server's connectable address as an `aster1…` ticket. Hand it to a
    /// client (`Ticket::from_base58` → [`Node::connect_ticket`]).
    pub fn address(&self) -> Result<String> {
        Ticket::from_node_addr(&self.node.addr(), Credential::Open)?.to_base58()
    }

    /// This server's [`NodeId`].
    pub fn id(&self) -> NodeId {
        self.node.id()
    }

    /// The underlying [`Node`] (escape hatch for direct transport access).
    pub fn node(&self) -> &Node {
        &self.node
    }

    /// The Gate-3 [`AttributeStore`] this server checks. Populate it from your
    /// admission logic to grant per-peer roles.
    pub fn attributes(&self) -> &AttributeStore {
        &self.attributes
    }

    /// The shared, transport-agnostic [`Dispatcher`]. Hand a clone to an
    /// additional transport (or compose your own) to serve the same services.
    pub fn dispatcher(&self) -> Dispatcher {
        self.dispatcher.clone()
    }

    /// Take the admission handle (one-shot) for Gate-0 connection gating.
    /// Returns `None` unless built with [`hooks(true)`](AsterServerBuilder::hooks).
    pub fn take_admission(&self) -> Option<crate::admission::Admission> {
        self.node.take_admission()
    }

    /// Blob store client backed by this server's node.
    #[cfg(feature = "blobs")]
    pub fn blobs(&self) -> crate::blobs::Blobs {
        self.node.blobs()
    }

    /// Document sync client backed by this server's node.
    #[cfg(feature = "docs")]
    pub fn docs(&self) -> crate::docs::Docs {
        self.node.docs()
    }

    /// Gossip pub-sub client backed by this server's node.
    #[cfg(feature = "gossip")]
    pub fn gossip(&self) -> crate::gossip::Gossip {
        self.node.gossip()
    }

    /// Serve until the node closes (or the loop is otherwise stopped). Mirrors
    /// `await srv.serve()`.
    pub async fn run(self) {
        self.handle.joined().await;
    }

    /// Stop serving and cleanly shut the node down, flushing persistent state.
    pub async fn shutdown(self) {
        if let Some(http) = &self.http {
            http.abort();
        }
        self.handle.abort();
        self.node.shutdown().await;
    }
}

/// Read a hex-encoded secret key from an identity file, if it exists.
fn read_identity(path: &Path) -> Result<Option<SecretKey>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::InvalidArgument(format!("identity file {}: {e}", path.display())))?;
    let bytes = decode_hex32(text.trim()).ok_or_else(|| {
        Error::InvalidArgument(format!(
            "identity file {}: expected 64 hex characters",
            path.display()
        ))
    })?;
    Ok(Some(SecretKey::from_bytes(bytes)))
}

/// Write a secret key to an identity file as 64 hex characters.
fn write_identity(path: &Path, key: &SecretKey) -> Result<()> {
    let hex = encode_hex(&key.to_bytes());
    std::fs::write(path, hex)
        .map_err(|e| Error::InvalidArgument(format!("identity file {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn encode_hex(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}
