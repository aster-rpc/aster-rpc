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
//!   **secret key file** (64 hex chars, [`SecretKey::read_file`]), load-or-create.
//!   This is the one on-disk node-key format every binding uses. The richer
//!   `.aster-identity` TOML credential file (peers, root pubkey, enrollment
//!   material) is read separately by [`AsterConfig::load_identity`].
//! - Lifecycle is explicit ([`run`](AsterServer::run) /
//!   [`shutdown`](AsterServer::shutdown)) rather than `async with`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{AsterConfig, RelayMode};
use crate::error::{Error, Result};
use crate::id::{NodeId, SecretKey};
use crate::node::Node;
use crate::ticket::{Credential, Ticket};

use super::auth::{AttributeStore, Authenticator, CapabilityRequirement};
use super::baseline::{
    LiveNodeInfo, LiveTopology, NodeIdentity, NodeIdentityHandle, NodeInfoServer, RequireGuard,
    TopologyServer,
};
use super::server::{
    AlpnHandler, Dispatcher, HttpTransport, Server, ServerHandle, ServiceDispatch, RPC_ALPN,
};
use crate::net::Connection;

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
    max_sessions_per_connection: Option<usize>,
    handler_timeout: Option<Option<std::time::Duration>>,
    // Non-RPC ALPN handlers; the ALPN is also registered on the node.
    routes: Vec<(Vec<u8>, AlpnHandler)>,
    // Baseline `aster.ops.NodeInfo` (default ON; inverted so Default works).
    disable_node_info: bool,
    node_identity: Option<NodeIdentity>,
    node_info_requires: Option<CapabilityRequirement>,
    // Baseline `aster.net.Topology` (default OFF — discloses network layout).
    enable_topology: bool,
    topology_requires: Option<CapabilityRequirement>,
    // Coordination voter hosting (default OFF). `Some` registers the reserved
    // `aster.coordination.*` services and requires a persistent identity.
    #[cfg(feature = "coordination")]
    coordination: Option<crate::coordination::host::CoordinationConfig>,
    // Foreign (binding) services and where their calls go.
    #[cfg(feature = "binding-support")]
    dynamic_services: Vec<super::dynamic::DynamicService>,
    #[cfg(feature = "binding-support")]
    dispatch_sink: Option<Arc<dyn super::foreign::DispatchSink>>,
    // Health listener override; `None` defers to the config's `[health]` keys.
    #[cfg(feature = "health")]
    health: Option<crate::ops::HealthConfig>,
    // Code overrides of the `[enrollment]` policy and role mapping.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    enrollment_overrides: crate::enrollment::runtime::Overrides,
    // Late activation: application services arrive later as an `RpcSurface`.
    deferred_surface: bool,
    // Durable audit of `aster.ops.NodeConfig` changes.
    #[cfg(feature = "docs")]
    audit: Option<Arc<dyn super::node_config::AuditSink>>,
}

impl AsterServerBuilder {
    /// Record every `aster.ops.NodeConfig` `set` and `clear` in `outbox`
    /// (kinds `aster.node_config.set` / `aster.node_config.clear`, with the
    /// caller, time and generation) before it is stored. A change the outbox
    /// refuses is not made (`UNAVAILABLE`). Without an outbox, changes are
    /// recorded only in the stored record and under the `aster::audit` log
    /// target.
    #[cfg(all(feature = "audit-outbox", feature = "docs"))]
    pub fn audit_outbox(mut self, outbox: crate::audit_outbox::AuditOutbox) -> Self {
        self.audit = Some(Arc::new(outbox));
        self
    }

    /// Serve `/healthz`, `/readyz` and `/metrics` on `config.host:config.port`
    /// (see [`crate::ops`]), overriding the config's `[health]` keys. Without
    /// this, the listener starts when [`AsterConfig::is_health_enabled`].
    ///
    /// The server reports **ready** once its services are being served, and
    /// not ready from the start of [`shutdown`](AsterServer::shutdown).
    #[cfg(feature = "health")]
    pub fn health(mut self, config: crate::ops::HealthConfig) -> Self {
        self.health = Some(config);
        self
    }

    /// Admit enrolled peers with this policy instead of the configured
    /// `[enrollment] policy` (for example a custom
    /// [`AdmissionPolicy`](crate::enrollment::AdmissionPolicy)). Enrollment
    /// itself is still switched on by the configuration.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    pub fn enrollment_policy(
        mut self,
        policy: impl crate::enrollment::AdmissionPolicy + 'static,
    ) -> Self {
        self.enrollment_overrides.policy = Some(Arc::new(policy));
        self
    }

    /// Map verified claims to Gate-3 roles with `mapping` instead of the
    /// configured `[enrollment.roles]`.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    pub fn enrollment_roles(mut self, mapping: crate::enrollment::RoleMapping) -> Self {
        self.enrollment_overrides.roles = Some(mapping);
        self
    }

    /// Add a service to serve (chainable; call once per service). Optional as
    /// long as the baseline `aster.ops.NodeInfo` service is enabled (the
    /// default); with [`builtin_node_info(false)`](Self::builtin_node_info),
    /// at least one service is required before [`start`](Self::start).
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
    /// [`NodeId`]. The file is created (64 hex chars, see
    /// [`SecretKey::write_file`]) on first start — from the config's
    /// `secret_key` if set, else a fresh key — and read on subsequent starts.
    /// An existing file overrides any `secret_key` in [`config`](Self::config).
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

    /// Deliver foreign calls and lifecycle events to `sink` (binding support).
    /// Required when any [`dynamic_service`](Self::dynamic_service) is
    /// registered. See [`Server::dispatch_sink`](super::server::Server::dispatch_sink).
    #[cfg(feature = "binding-support")]
    pub fn dispatch_sink(mut self, sink: Arc<dyn super::foreign::DispatchSink>) -> Self {
        self.dispatch_sink = Some(sink);
        self
    }

    /// Serve a foreign (binding-implemented) service alongside the native and
    /// baseline ones. Validates `svc` now; a `(name, version)` collision with
    /// another service fails [`start`](Self::start). The service's
    /// `service_idx` in [`ForeignCall`](crate::binding_support::dispatch::ForeignCall)
    /// is its position among `dynamic_service` calls.
    #[cfg(feature = "binding-support")]
    pub fn dynamic_service(mut self, svc: super::dynamic::DynamicService) -> Result<Self> {
        svc.validate()?;
        self.dynamic_services.push(svc);
        Ok(self)
    }

    /// Cap active sessions per connection. See
    /// [`Server::max_sessions_per_connection`](super::server::Server::max_sessions_per_connection).
    pub fn max_sessions_per_connection(mut self, max: usize) -> Self {
        self.max_sessions_per_connection = Some(max);
        self
    }

    /// Cap how long any handler may run (`None` = no cap). See
    /// [`Server::handler_timeout`](super::server::Server::handler_timeout).
    pub fn handler_timeout(mut self, max: Option<std::time::Duration>) -> Self {
        self.handler_timeout = Some(max);
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

    /// Serve another protocol on `alpn` alongside RPC, on the **same** node and
    /// accept loop. The reactor routes `aster/1` to RPC and hands every
    /// connection on `alpn` to `handler` — no second acceptor (which would split
    /// inbound connections arbitrarily). The ALPN is registered for you.
    /// `handler` must not block — see [`AlpnHandler`](crate::rpc::AlpnHandler).
    ///
    /// This is how one identity hosts RPC plus, say, a media lane and a
    /// signaling relay: chain `route_alpn` (or [`with_expose`](Self::with_expose))
    /// once per protocol.
    pub fn route_alpn(
        mut self,
        alpn: impl Into<Vec<u8>>,
        handler: impl Fn(Connection) + Send + Sync + 'static,
    ) -> Self {
        let alpn = alpn.into();
        self.extra_alpns.push(alpn.clone());
        self.routes.push((alpn, std::sync::Arc::new(handler)));
        self
    }

    /// Serve an [`Expose`](crate::expose::Expose) registry on `alpn` alongside
    /// RPC — the one-call "RPC + expose on one node" path. Equivalent to
    /// [`route_alpn`](Self::route_alpn) with a handler that attaches + feeds each
    /// connection to `expose`.
    ///
    /// Build the handle with [`Expose::new`](crate::expose::Expose::new) (it's
    /// node-independent), register your services on it, then pass a clone here;
    /// keep the original to register / unexpose services later.
    ///
    /// ```no_run
    /// # use aster::{rpc::AsterServer, expose::Expose};
    /// # async fn run<S: aster::rpc::ServiceDispatch>(svc: S, a: aster::expose::AuthorizeFn, h: aster::expose::HttpHandler) -> aster::Result<()> {
    /// let expose = Expose::new();
    /// expose.expose_http("signaling", a, h);
    /// let srv = AsterServer::builder()
    ///     .service(svc)
    ///     .with_expose(b"portal/signaling/1".to_vec(), expose.clone())
    ///     .start()
    ///     .await?;
    /// # let _ = srv; Ok(())
    /// # }
    /// ```
    #[cfg(feature = "expose")]
    pub fn with_expose(self, alpn: impl Into<Vec<u8>>, expose: crate::expose::Expose) -> Self {
        self.route_alpn(alpn, move |conn| expose.handle(&conn))
    }

    /// Enable/disable the baseline `aster.ops.NodeInfo` service (default:
    /// **enabled**). With it enabled, a server with no application services is
    /// still startable — a bare node that can describe itself.
    pub fn builtin_node_info(mut self, enabled: bool) -> Self {
        self.disable_node_info = !enabled;
        self
    }

    /// The [`NodeIdentity`] the baseline `aster.ops.NodeInfo` service serves
    /// (name, tags, roles, attributes). `node_id` is stamped from the node's
    /// key at start — any value set here is overwritten. Without this, an
    /// empty identity (id only) is served.
    pub fn node_identity(mut self, identity: NodeIdentity) -> Self {
        self.node_identity = Some(identity);
        self
    }

    /// Gate every method of the baseline `aster.ops.NodeInfo` service behind a
    /// capability requirement (Gate 3). Default: no requirement (any dialer
    /// admitted to the transport can call `describe`).
    pub fn node_info_requires(mut self, requires: CapabilityRequirement) -> Self {
        self.node_info_requires = Some(requires);
        self
    }

    /// Enable the baseline `aster.net.Topology` service (default: **disabled**,
    /// unlike NodeInfo — the view discloses inferred network layout, and
    /// topology is a producer-node feature). Requires the node to be started
    /// with monitoring enabled
    /// ([`AsterConfigBuilder::monitoring(true)`](crate::AsterConfigBuilder::monitoring));
    /// [`start`](Self::start) refuses otherwise.
    pub fn builtin_topology(mut self, enabled: bool) -> Self {
        self.enable_topology = enabled;
        self
    }

    /// Gate every method of the baseline `aster.net.Topology` service behind a
    /// capability requirement (Gate 3). Only meaningful together with
    /// [`builtin_topology(true)`](Self::builtin_topology).
    pub fn topology_requires(mut self, requires: CapabilityRequirement) -> Self {
        self.topology_requires = Some(requires);
        self
    }

    /// Host a coordination voter: register the reserved
    /// `aster.coordination.Store` and `aster.coordination.Raft` v1 services
    /// (§3.3, §5.2, §5.3).
    ///
    /// Off unless called, and there is no config-file or environment route to
    /// it. That is the point of the "absent unless configured" rule: a node
    /// that did not ask to be a voter answers `NOT_FOUND` for both names rather
    /// than presenting a surface that merely refuses.
    ///
    /// [`start`](Self::start) **fails** unless the endpoint secret key is
    /// persistently configured -- via [`identity(path)`](Self::identity) or an
    /// explicit `secret_key` in [`config`](Self::config). Committed Raft
    /// membership names exact NodeIds, so a voter that restarts under a fresh
    /// key is not a recovered voter but a stranger, permanently unable to
    /// rejoin the group it is a member of. `AsterServer` otherwise defaults to
    /// an ephemeral in-memory identity, which is right for ordinary RPC and
    /// wrong here.
    ///
    /// Slice 0 registers the reservations only: every method answers
    /// `UNIMPLEMENTED`. What is live is the identity rule, the `iroh_only`
    /// refusal, and the §12.3 frame caps.
    #[cfg(feature = "coordination")]
    pub fn builtin_coordination(
        mut self,
        config: crate::coordination::host::CoordinationConfig,
    ) -> Self {
        self.coordination = Some(config);
        self
    }

    /// Start without the application's RPC surface; hand it over later with
    /// [`AsterServer::activate`]. See [`surface`](super::surface).
    ///
    /// [`start`](Self::start) then brings up the node, its identity, blobs /
    /// docs / gossip, admission, enrollment and the `aster/1` ALPN as usual,
    /// and serves only the *host* services: the baselines, the enrollment
    /// issuer, coordination, and anything added with
    /// [`service`](Self::service) here. Every call to any other service
    /// answers `UNAVAILABLE` ("rpc surface not active"), whatever its name,
    /// until [`activate`](AsterServer::activate) returns, and again after
    /// [`deactivate`](AsterServer::deactivate). The health listener reports
    /// not ready while no surface is active. With this set the server may
    /// start with no service at all.
    pub fn deferred_surface(mut self) -> Self {
        self.deferred_surface = true;
        self
    }

    /// Build the node (RPC + built-in protocol ALPNs registered), start serving
    /// the registered services, and return the running [`AsterServer`].
    pub async fn start(self) -> Result<AsterServer> {
        // A voter is a served surface, so hosting coordination satisfies the
        // "at least one service" rule on its own.
        #[cfg(feature = "coordination")]
        let hosts_coordination = self.coordination.is_some();
        #[cfg(not(feature = "coordination"))]
        let hosts_coordination = false;

        #[cfg(feature = "binding-support")]
        let has_foreign = !self.dynamic_services.is_empty();
        #[cfg(not(feature = "binding-support"))]
        let has_foreign = false;
        #[cfg(feature = "binding-support")]
        if has_foreign && self.dispatch_sink.is_none() {
            return Err(Error::InvalidArgument(
                "dynamic_service requires a dispatch_sink".into(),
            ));
        }

        if self.services.is_empty()
            && self.disable_node_info
            && !self.enable_topology
            && !hosts_coordination
            && !has_foreign
            && !self.deferred_surface
        {
            return Err(Error::InvalidArgument(
                "AsterServer requires at least one service (or an enabled baseline service)".into(),
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
        // The file wins over any `secret_key` in the config (documented on
        // `identity`); a missing file is created by `Node::start` from the
        // config's key or a fresh one (`AsterConfig::resolve_secret_key_file`).
        if let Some(path) = &self.identity {
            if let Some(key) = SecretKey::read_file_if_exists(path)? {
                builder = builder.secret_key(key);
            }
            builder = builder.secret_key_file(path.clone());
        }
        // `mut` for `resolve_root_pubkey(&mut self)` (coordination) and for
        // the hook settings Enrollment requires.
        #[cfg_attr(
            not(any(
                feature = "coordination",
                all(feature = "enrollment", feature = "docs")
            )),
            allow(unused_mut)
        )]
        let mut config = builder.build();
        let accept_on_behalf_of = config.identity_accept_on_behalf_of;
        // `aster.ops.NodeConfig`: what it overrides and what the file locks,
        // checked before the node starts.
        super::node_config::validate_locks(&config.remote_config_lock)?;
        #[cfg(feature = "docs")]
        let remote_config = config.remote_config_enabled.then(|| {
            (
                config.remote_config_lock.clone(),
                config.setting_sources.clone(),
            )
        });

        // The deployment ROOT public key, resolved once, here. Coordination
        // consumes this rather than asking for the same key again: one
        // deployment has one ROOT, and `resolved_root_key` refuses a
        // coordination-local key that disagrees with it instead of silently
        // preferring either.
        #[cfg(feature = "coordination")]
        let root_pubkey: Option<[u8; 32]> = config.resolve_root_pubkey()?.map(|key| key.to_bytes());

        // §3.3: a voter's NodeId is named by committed Raft membership, so it
        // MUST survive restart. Checked here -- after config resolution, before
        // the node starts and long before any storage is opened -- so a
        // misconfigured voter fails on its first start rather than on its first
        // *re*start, when the group has already recorded an identity it can no
        // longer produce. When storage lands, it opens after this point, not
        // before.
        #[cfg(feature = "coordination")]
        if self.coordination.is_some()
            && config.secret_key_file.is_none()
            && config.secret_key.is_none()
        {
            return Err(Error::InvalidArgument(
                "builtin_coordination requires a persistent endpoint identity: set \
                 AsterServerBuilder::identity(path) or an explicit secret_key in AsterConfig. \
                 A voter that restarts under a new key cannot rejoin its own group."
                    .into(),
            ));
        }

        // Topology reads the connection monitor; refuse a config that can
        // never populate it rather than serving permanently-empty views.
        if self.enable_topology && !config.enable_monitoring {
            return Err(Error::InvalidArgument(
                "builtin_topology(true) requires monitoring — enable it with \
                 AsterConfigBuilder::monitoring(true)"
                    .into(),
            ));
        }

        // Enrollment from `[enrollment]`: validated in full before the node
        // starts, so a misconfiguration fails here with nothing to clean up.
        #[cfg(all(feature = "enrollment", feature = "docs"))]
        let enrollment_plan = crate::enrollment::runtime::plan(
            &mut config,
            crate::enrollment::runtime::Side::Server,
            self.enrollment_overrides,
        )?;
        #[cfg(not(all(feature = "enrollment", feature = "docs")))]
        if config.is_enrollment_enabled() {
            return Err(Error::InvalidArgument(
                "enrollment is configured ([enrollment] / ASTER_ENROLLMENT_*) but aster was \
                 built without the `enrollment` and `docs` features"
                    .into(),
            ));
        }

        // `aster/1` always; blobs/docs/gossip ALPNs are registered by core.
        // `route_alpn`/`with_expose` also push their ALPN into `extra_alpns`, so
        // dedup before registering (a user may also have called `.alpn(...)`).
        let mut alpns = vec![RPC_ALPN.to_vec()];
        #[cfg(all(feature = "enrollment", feature = "docs"))]
        if enrollment_plan.is_some() {
            alpns.push(crate::enrollment::ENROLLMENT_ALPN.to_vec());
        }
        for alpn in self.extra_alpns {
            if !alpns.contains(&alpn) {
                alpns.push(alpn);
            }
        }

        // Health listener: an explicit `.health(..)` wins, else the config's
        // `[health]` keys. Refuse a config that asks for one this build cannot
        // serve rather than silently running without probes.
        #[cfg(feature = "health")]
        let health_config = self.health.or_else(|| config.health_config());
        #[cfg(not(feature = "health"))]
        if config.is_health_enabled() {
            return Err(Error::InvalidArgument(
                "the health listener is enabled ([health] / ASTER_HEALTH_*) but aster was \
                 built without the `health` feature"
                    .into(),
            ));
        }
        #[cfg(feature = "health")]
        let service_count = u32::try_from(self.services.len()).unwrap_or(u32::MAX);

        let node = Node::start_with_alpns(config, alpns).await?;

        // Bound before serving so a port conflict fails the start cleanly. It
        // answers not-ready until everything below is up.
        #[cfg(feature = "health")]
        let health = match health_config {
            None => None,
            Some(cfg) => match crate::ops::HealthServer::start(&node, cfg).await {
                Ok(health) => {
                    health.state().set_services(service_count);
                    Some(health)
                }
                Err(err) => {
                    node.shutdown().await;
                    return Err(err);
                }
            },
        };

        let attributes = self.attributes.unwrap_or_default();
        let mut server = Server::new(&node)
            .attributes(attributes.clone())
            .accept_on_behalf_of(accept_on_behalf_of);
        // Deferred: the slot is wired in before the acceptor starts, so no
        // call can ever see a server without it.
        let late_slot = self
            .deferred_surface
            .then(|| Arc::new(super::server::LateSlot::default()));
        if let Some(slot) = &late_slot {
            server = server.late_slot(slot.clone());
        }
        if let Some(auth) = self.authenticator {
            server = server.authenticator_arc(auth);
        }
        if let Some(max) = self.max_sessions_per_connection {
            server = server.max_sessions_per_connection(max);
        }
        if let Some(max) = self.handler_timeout {
            server = server.handler_timeout(max);
        }
        for svc in self.services {
            server = server.register_arc(svc);
        }
        // Enrollment: installed before anything is served. It owns Gate 0
        // (every non-enrollment ALPN), feeds Gate 3 the roles of each peer's
        // live session, and, as a delegated issuer, serves renewals.
        #[cfg(feature = "docs")]
        #[cfg_attr(not(all(feature = "enrollment", feature = "docs")), allow(unused_mut))]
        let mut config_authority: Option<Arc<dyn super::AttributeSource>> = None;
        #[cfg(all(feature = "enrollment", feature = "docs"))]
        let enrollment = match &enrollment_plan {
            None => None,
            Some(plan) => {
                let installed = match plan.install(&node) {
                    Ok(installed) => installed,
                    Err(err) => {
                        node.shutdown().await;
                        return Err(err);
                    }
                };
                let source = Arc::new(crate::enrollment::runtime::EnrollmentAttributes {
                    enrollment: installed.enrollment.clone(),
                    mapping: plan.mapping.clone(),
                });
                // NodeConfig admits a change only on a credential's own role.
                config_authority = Some(source.clone());
                attributes.set_source(source);
                if let Some(issuer) = &installed.issuer {
                    server = server.register(issuer.clone());
                }
                let routed = installed.enrollment.clone();
                server = server.route_alpn(crate::enrollment::ENROLLMENT_ALPN, move |connection| {
                    let enrollment = routed.clone();
                    tokio::spawn(async move {
                        if enrollment.accept(&connection).await.is_ok() {
                            connection.closed().await;
                        }
                    });
                });
                Some(installed)
            }
        };
        // Baseline `aster.ops.NodeConfig` — on unless `[remote_config]` turns
        // it off. The stored record is applied before anything is served.
        #[cfg(feature = "docs")]
        let node_config = match remote_config {
            None => None,
            Some((locks, sources)) => {
                let init = super::node_config::Init {
                    identity: server.live_identity(),
                    properties: server.live_properties(),
                    base_identity: super::identity::IdentitySettings {
                        accept_on_behalf_of,
                    },
                    sources,
                    locks,
                    authority: config_authority,
                    audit: self.audit,
                };
                match super::node_config::NodeConfigHandle::open(&node, init).await {
                    Ok(handle) => {
                        server = server.register_arc(Arc::new(RequireGuard {
                            inner: super::node_config::NodeConfigServer::new(
                                super::node_config::LiveNodeConfig {
                                    handle: handle.clone(),
                                },
                            ),
                            requires: Some(super::require_role(super::CONFIG_ADMIN_ROLE)),
                        }));
                        Some(handle)
                    }
                    Err(err) => {
                        node.shutdown().await;
                        return Err(err);
                    }
                }
            }
        };
        // Baseline `aster.ops.NodeInfo` — registered unless disabled, with the
        // node_id stamped from the just-started node's key. The handle stays
        // on the AsterServer so the served identity can be updated live.
        let mut identity_handle = None;
        if !self.disable_node_info {
            let mut identity = self.node_identity.unwrap_or_default();
            identity.node_id = node.id().to_string();
            let handle = NodeIdentityHandle::new(identity);
            server = server.register_arc(Arc::new(RequireGuard {
                inner: NodeInfoServer::new(LiveNodeInfo {
                    handle: handle.clone(),
                }),
                requires: self.node_info_requires,
            }));
            identity_handle = Some(handle);
        }
        // Baseline `aster.net.Topology` — opt-in; reads the node's live view.
        if self.enable_topology {
            server = server.register_arc(Arc::new(RequireGuard {
                inner: TopologyServer::new(LiveTopology { core: node.core() }),
                requires: self.topology_requires,
            }));
        }
        // Coordination services -- registered only when the host asked for them
        // (§3.3), and **before** the acceptor starts: bootstrap cannot run
        // until peers can dial this node, and every other initial voter is
        // waiting to dial it.
        //
        // The Raft service is live but late-bound. Its `Raft` does not exist
        // yet -- it needs storage, which needs the verified start mode -- so
        // until the runtime attaches below it answers `FailedPrecondition`,
        // which a sender classifies transient. That is exactly right during a
        // rolling start, and it is why the endpoint is a handle rather than a
        // constructor argument.
        //
        // `Store` is live and late-bound exactly like Raft: until the runtime
        // attaches it answers `FailedPrecondition`, which a client treats as
        // transient.
        #[cfg(feature = "coordination")]
        let coordination_endpoint = crate::coordination::network::service::RaftEndpoint::new();
        #[cfg(feature = "coordination")]
        let store_endpoint = crate::coordination::store::host::StoreEndpoint::new();
        #[cfg(feature = "coordination")]
        if self.coordination.is_some() {
            use crate::coordination::network::service::CoordinationRaftService;
            use crate::coordination::store::host::CoordinationStoreService;
            server = server.register_arc(Arc::new(CoordinationStoreService::new(
                store_endpoint.clone(),
            )));
            server = server.register_arc(Arc::new(CoordinationRaftService::new(
                coordination_endpoint.clone(),
            )));
        }
        for apply in self.session_appliers {
            server = apply(server);
        }
        // Foreign services last, so a collision with any native or baseline
        // service is refused here rather than silently shadowed.
        #[cfg(feature = "binding-support")]
        {
            if let Some(sink) = self.dispatch_sink {
                server = server.dispatch_sink(sink);
            }
            for svc in self.dynamic_services {
                server = match server.dynamic_service(svc) {
                    Ok(server) => server,
                    Err(err) => {
                        node.shutdown().await;
                        return Err(err);
                    }
                };
            }
        }
        for (alpn, handler) in self.routes {
            server = server.route_alpn_arc(alpn, handler);
        }
        // Grab the shareable dispatcher before `serve()` consumes the server, so
        // an HTTP transport can drive the same services.
        let dispatcher = server.dispatcher();
        #[cfg(feature = "binding-support")]
        let call_table = server.call_table();
        let handle = server.serve();
        let http = self.http.map(|t| t.serve(dispatcher.clone()));

        // The acceptor is running: enroll with the policy replicas (or hold
        // the fleet namespace for the root to publish into), then renew.
        #[cfg(all(feature = "enrollment", feature = "docs"))]
        let enrollment = match (enrollment_plan, enrollment) {
            (Some(plan), Some(installed)) => {
                if let Err(err) = plan.attach_policy(&node, &installed.enrollment).await {
                    handle.abort();
                    if let Some(http) = &http {
                        http.abort();
                    }
                    node.shutdown().await;
                    return Err(err);
                }
                let renewal = plan.start_renewal(&installed.enrollment, None);
                Some(ServerEnrollment {
                    enrollment: installed.enrollment,
                    issuer: installed.issuer,
                    _renewal: renewal,
                })
            }
            _ => None,
        };

        // The acceptor is running, so a peer can reach us. Only now open
        // storage, create the Raft, attach it, and run the start corridor.
        //
        // Any failure from here closes the node before returning: a half-started
        // voter that kept its endpoint alive would be dialable, answer
        // `FailedPrecondition` forever, and hold a storage directory lock that
        // no restart could take.
        #[cfg(feature = "coordination")]
        let coordination = match self.coordination {
            None => None,
            Some(coordination_config) => {
                let root = match coordination_config.resolved_root_key(root_pubkey) {
                    Ok(root) => root,
                    Err(err) => {
                        handle.abort();
                        node.shutdown().await;
                        return Err(err);
                    }
                };
                let started = coordination_start(
                    &coordination_config,
                    root,
                    &node,
                    coordination_endpoint,
                    store_endpoint,
                )
                .await;
                match started {
                    Ok(runtime) => Some(runtime),
                    Err(err) => {
                        handle.abort();
                        node.shutdown().await;
                        return Err(err);
                    }
                }
            }
        };

        // A deferred server is ready only once its surface is active.
        #[cfg(feature = "health")]
        if let Some(health) = &health {
            if late_slot.is_none() {
                health.set_ready(true);
            }
        }
        tracing::info!(
            target: crate::log_targets::SERVER,
            node_id = node.id().as_str(),
            "aster server started"
        );

        Ok(AsterServer {
            node,
            handle,
            attributes,
            dispatcher,
            http,
            identity_handle,
            #[cfg(feature = "docs")]
            node_config,
            #[cfg(feature = "binding-support")]
            call_table,
            #[cfg(feature = "coordination")]
            coordination,
            #[cfg(feature = "health")]
            health,
            #[cfg(all(feature = "enrollment", feature = "docs"))]
            enrollment,
            surface: late_slot.map(super::surface::SurfaceControl::new),
            #[cfg(feature = "health")]
            host_services: service_count,
        })
    }
}

/// The Enrollment an [`AsterServer`] installed from its configuration.
#[cfg(all(feature = "enrollment", feature = "docs"))]
struct ServerEnrollment {
    enrollment: crate::enrollment::Enrollment,
    issuer: Option<crate::enrollment::Issuer>,
    /// Renewal of this server's own credential; dropping it stops renewal.
    _renewal: Option<crate::enrollment::RenewalHandle>,
}

/// Binding support: the baseline services [`AsterServerBuilder`] registers,
/// for bindings that serve on a node they started themselves (the C ABI's
/// `aster_server_*`), through [`Server::new`]. Not part of the stable API.
#[cfg(feature = "binding-support")]
impl Server {
    /// Serve `aster.ops.NodeInfo` for `node` (what
    /// [`AsterServerBuilder::builtin_node_info`] does), optionally behind a
    /// service-wide requirement. Returns the handle that updates the served
    /// identity live.
    #[doc(hidden)]
    pub fn with_builtin_node_info(
        self,
        node: &Node,
        identity: NodeIdentity,
        requires: Option<CapabilityRequirement>,
    ) -> (Self, NodeIdentityHandle) {
        let mut identity = identity;
        identity.node_id = node.id().to_string();
        let handle = NodeIdentityHandle::new(identity);
        let server = self.register_arc(Arc::new(RequireGuard {
            inner: NodeInfoServer::new(LiveNodeInfo {
                handle: handle.clone(),
            }),
            requires,
        }));
        (server, handle)
    }

    /// Serve `aster.net.Topology` for `node` (what
    /// [`AsterServerBuilder::builtin_topology`] does). Its views stay empty
    /// unless the node was started with monitoring.
    #[doc(hidden)]
    pub fn with_builtin_topology(
        self,
        node: &Node,
        requires: Option<CapabilityRequirement>,
    ) -> Self {
        self.register_arc(Arc::new(RequireGuard {
            inner: TopologyServer::new(LiveTopology { core: node.core() }),
            requires,
        }))
    }
}

/// Run the §7.2 start corridor for a configured voter.
///
/// A free function rather than an inline block so the one identity failure has
/// one place to be handled. **No ticket is rendered here.** A join permit binds
/// the candidate's NodeId, and the `candidate_ticket` it carries is derived
/// from that NodeId — an Aster node's UDP port is ephemeral and it is reached
/// by discovery, so passing this node's current address ticket would have made
/// a permit stop verifying the moment the candidate restarted.
#[cfg(feature = "coordination")]
async fn coordination_start(
    config: &crate::coordination::host::CoordinationConfig,
    root: [u8; 32],
    node: &Node,
    endpoint: crate::coordination::network::service::RaftEndpoint,
    store_endpoint: crate::coordination::store::host::StoreEndpoint,
) -> Result<crate::coordination::runtime::owner::CoordinationRuntime> {
    let local = crate::coordination::ids::CoordinationNodeId::parse_hex(node.id().as_str())
        .map_err(|err| {
            Error::InvalidArgument(format!(
                "coordination requires a 32-byte endpoint identity: {err}"
            ))
        })?;
    crate::coordination::boot::start(config, &root, node, endpoint, store_endpoint, local).await
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
    /// Live identity served by the baseline `aster.ops.NodeInfo` service
    /// (`None` when the baseline service was disabled).
    identity_handle: Option<NodeIdentityHandle>,
    /// `aster.ops.NodeConfig` (`None` when `[remote_config]` turns it off).
    #[cfg(feature = "docs")]
    node_config: Option<super::node_config::NodeConfigHandle>,
    /// The table foreign calls are answered through (binding support).
    #[cfg(feature = "binding-support")]
    call_table: super::foreign::CallTable,
    /// The voter runtime, when this server hosts coordination.
    ///
    /// Owned here because §3.3 requires it: "AsterServer owns the
    /// `CoordinationRuntime` handle so dropping/shutdown cannot orphan voter
    /// tasks." A runtime held anywhere else would outlive the endpoint it
    /// serves.
    #[cfg(feature = "coordination")]
    coordination: Option<crate::coordination::runtime::owner::CoordinationRuntime>,
    /// The health/metrics HTTP listener, when enabled.
    #[cfg(feature = "health")]
    health: Option<crate::ops::HealthServer>,
    /// Enrollment, when `[enrollment]` is configured.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    enrollment: Option<ServerEnrollment>,
    /// The late-activation state machine (`deferred_surface()` only).
    surface: Option<super::surface::SurfaceControl>,
    /// Services reported to `/readyz` without a surface.
    #[cfg(feature = "health")]
    host_services: u32,
}

impl AsterServer {
    /// The health/readiness/metrics HTTP listener, when enabled by the config's
    /// `[health]` keys or [`AsterServerBuilder::health`]. Use its
    /// [`state`](crate::ops::HealthServer::state) to toggle readiness and add
    /// readiness checks.
    #[cfg(feature = "health")]
    pub fn health(&self) -> Option<&crate::ops::HealthServer> {
        self.health.as_ref()
    }

    /// The [`Enrollment`](crate::enrollment::Enrollment) this server
    /// installed from `[enrollment]`, or `None` when enrollment is off. Use it
    /// to check peers, follow revocations or apply root rotations.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    pub fn enrollment(&self) -> Option<&crate::enrollment::Enrollment> {
        self.enrollment.as_ref().map(|e| &e.enrollment)
    }

    /// The delegated [`Issuer`](crate::enrollment::Issuer) this server runs
    /// when `[enrollment.issuer] delegation_file` is set. It already serves
    /// `aster.enrollment.IssuerV1` renewals; use
    /// [`issue_for`](crate::enrollment::Issuer::issue_for) to enroll new nodes.
    #[cfg(all(feature = "enrollment", feature = "docs"))]
    pub fn issuer(&self) -> Option<&crate::enrollment::Issuer> {
        self.enrollment.as_ref().and_then(|e| e.issuer.as_ref())
    }

    /// A cloneable handle that runs this server's drain
    /// ([`ServerHandle::drainer`](super::ServerHandle::drainer)) without
    /// consuming the server, for a binding that keeps it behind a lock while
    /// the drain runs. Unlike [`drain`](Self::drain) it neither flips
    /// readiness nor shuts the server down afterwards.
    pub fn drainer(&self) -> super::ServerDrainer {
        self.handle.drainer()
    }

    /// Start configuring a server.
    pub fn builder() -> AsterServerBuilder {
        AsterServerBuilder::default()
    }

    /// The live [`NodeIdentityHandle`] behind the baseline `aster.ops.NodeInfo`
    /// service — keep a clone and update the served identity while the server
    /// runs (`node_id` stays pinned). `None` when the baseline service was
    /// disabled with [`builtin_node_info(false)`](AsterServerBuilder::builtin_node_info).
    ///
    /// ```no_run
    /// # fn f(srv: &aster::rpc::AsterServer) {
    /// if let Some(identity) = srv.node_identity() {
    ///     identity.update(|id| {
    ///         id.attributes
    ///             .push(aster::rpc::baseline::Attr::new("draining", true));
    ///     });
    /// }
    /// # }
    /// ```
    pub fn node_identity(&self) -> Option<NodeIdentityHandle> {
        self.identity_handle.clone()
    }

    /// A read-only handle to this server's `aster.ops.NodeConfig` state: the
    /// record, the effective settings and their sources, and the properties
    /// handlers see. `None` when `[remote_config] enabled = false`. Changes
    /// come only from admins over RPC.
    #[cfg(feature = "docs")]
    pub fn node_config(&self) -> Option<super::node_config::NodeConfigHandle> {
        self.node_config.clone()
    }

    /// The table through which the binding answers foreign calls
    /// ([`CallTable`](crate::binding_support::dispatch::CallTable)). Empty
    /// unless [`dynamic_service`](AsterServerBuilder::dynamic_service)s are
    /// registered.
    #[cfg(feature = "binding-support")]
    pub fn call_table(&self) -> super::foreign::CallTable {
        self.call_table.clone()
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
    /// admission logic to grant per-peer roles. With enrollment configured,
    /// each enrolled peer's roles come from its verified claims without any
    /// entry here.
    pub fn attributes(&self) -> &AttributeStore {
        &self.attributes
    }

    /// The shared, transport-agnostic [`Dispatcher`]. Hand a clone to an
    /// additional transport (or compose your own) to serve the same services.
    pub fn dispatcher(&self) -> Dispatcher {
        self.dispatcher.clone()
    }

    /// Take the admission handle (one-shot) for Gate-0 connection gating.
    /// Returns `None` unless built with [`hooks(true)`](AsterServerBuilder::hooks),
    /// and always `None` when enrollment is configured (Enrollment owns Gate 0).
    /// With the `otel` feature its accept/reject decisions are counted in
    /// `aster.admission.*` (and so on a health listener's `/metrics`).
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

    /// Local topology view backed by this server's node (see
    /// [`Node::topology`]). Independent of the baseline `aster.net.Topology`
    /// RPC service — this is the in-process view.
    pub fn topology(&self) -> crate::topology::Topology {
        self.node.topology()
    }

    /// Activate `surface`: install its services, publish its contracts per
    /// its [`ManifestPublication`](super::ManifestPublication), and report
    /// ready, as one step. Returns once the services are callable and their
    /// manifests are in the promised state; on error the server is as it was
    /// before the call (not callable, nothing advertised).
    ///
    /// Fails with `FAILED_PRECONDITION` unless the server was built with
    /// [`deferred_surface()`](AsterServerBuilder::deferred_surface) or when a
    /// surface is already active (surfaces are immutable: deactivate first),
    /// and with `InvalidArgument` when the surface is empty or names a
    /// service the server already serves.
    pub async fn activate(&self, surface: super::RpcSurface) -> Result<super::SurfaceActivation> {
        let control = self.surface_control()?;
        control.activate(self.surface_host(), surface).await
    }

    /// Stop the active surface, if any: report not ready, retract its
    /// registry entries, then stop dispatching to it (later calls answer
    /// `UNAVAILABLE`; calls already running finish). The node -- blobs, docs,
    /// gossip, host services -- stays up. A no-op when nothing is active.
    pub async fn deactivate(&self) -> Result<()> {
        let control = self.surface_control()?;
        control.deactivate(self.surface_host()).await
    }

    /// Whether a surface is active. Always [`Active`](super::SurfaceState::Active)
    /// for a server built without
    /// [`deferred_surface()`](AsterServerBuilder::deferred_surface): it serves
    /// its services from `start`.
    pub fn surface_state(&self) -> super::SurfaceState {
        match &self.surface {
            Some(control) => control.state(),
            None => super::SurfaceState::Active,
        }
    }

    fn surface_control(&self) -> Result<&super::surface::SurfaceControl> {
        self.surface.as_ref().ok_or_else(|| {
            Error::rpc(
                super::StatusCode::FailedPrecondition,
                "this AsterServer serves its services from start; build it with \
                 deferred_surface() to activate an rpc surface later",
            )
        })
    }

    fn surface_host(&self) -> super::surface::Host<'_> {
        super::surface::Host {
            node: &self.node,
            dispatcher: &self.dispatcher,
            #[cfg(feature = "health")]
            health: self.health.as_ref(),
            #[cfg(feature = "health")]
            host_services: self.host_services,
        }
    }

    /// Serve until the node closes (or the loop is otherwise stopped). Mirrors
    /// `await srv.serve()`.
    pub async fn run(self) {
        self.handle.joined().await;
    }

    /// A handle on this server's coordination voter, when it hosts one.
    ///
    /// `None` unless [`builtin_coordination`](AsterServerBuilder::builtin_coordination)
    /// was called. Use it to wait for readiness:
    /// `server.coordination().unwrap().wait_ready().await?`.
    #[cfg(feature = "coordination")]
    pub fn coordination(&self) -> Option<crate::coordination::Coordination> {
        self.coordination
            .as_ref()
            .map(crate::coordination::Coordination::of)
    }

    /// Drain, then [`shutdown`](Self::shutdown): the graceful way to take a
    /// replica out of rotation (a rolling restart, a scale-in).
    ///
    /// 1. Readiness goes false on the health listener, if enabled (`/readyz`
    ///    answers 503, so a load balancer or Kubernetes stops routing here).
    /// 2. New connections are closed right after their handshake with
    ///    [`DRAIN_CLOSE_CODE`](super::drain::DRAIN_CLOSE_CODE); new calls on
    ///    existing connections get `UNAVAILABLE` with the `aster.draining`
    ///    detail. Calls on sessions that already exist still run.
    /// 3. Each connection is closed with the drain code as soon as it has no
    ///    call in flight and no live session.
    /// 4. In-flight calls (every pattern) run until `grace`; at the deadline
    ///    every remaining connection is closed with the drain code, which
    ///    cancels them, and the drain waits up to
    ///    [`CANCEL_WAIT`](super::drain::CANCEL_WAIT) for them to end.
    /// 5. The server shuts down as [`shutdown`](Self::shutdown) does (an
    ///    active surface's registry entries are retracted there).
    ///
    /// Clients see a drain as [`Error::is_draining`](crate::Error::is_draining).
    /// This server publishes no endpoint leases itself; an application that
    /// publishes them should renew its lease with the `draining` health
    /// status before calling this.
    ///
    /// ```no_run
    /// # async fn f(srv: aster::rpc::AsterServer) {
    /// let report = srv.drain(std::time::Duration::from_secs(30)).await;
    /// println!("completed {} cancelled {}", report.calls_completed, report.calls_cancelled);
    /// # }
    /// ```
    pub async fn drain(self, grace: std::time::Duration) -> super::DrainReport {
        #[cfg(feature = "health")]
        if let Some(health) = &self.health {
            health.set_ready(false);
        }
        let report = self.handle.drain(grace).await;
        self.shutdown().await;
        report
    }

    /// Stop serving and cleanly shut the node down, flushing persistent state.
    ///
    /// §7.6's cancellation runs first and is the strict reverse of
    /// construction: controllers, RPC deregistration, `Raft::shutdown()`, the
    /// storage actors and read leases, and the directory lock last. Only then
    /// does the transport stop — a node torn down first would leave a peer's
    /// in-flight call reaching a Raft that is going away.
    pub async fn shutdown(self) {
        tracing::info!(
            target: crate::log_targets::SERVER,
            node_id = self.node.id().as_str(),
            "aster server shutting down"
        );
        // An active surface goes first: discovery retracted, dispatch
        // stopped, before anything below it closes.
        if let Some(control) = &self.surface {
            if let Err(err) = control.deactivate(self.surface_host()).await {
                tracing::warn!(
                    target: crate::log_targets::SERVER,
                    error = %err,
                    "rpc surface did not stop cleanly"
                );
            }
        }
        // Probes see not-ready first, and the port is released before the
        // node goes away.
        #[cfg(feature = "health")]
        if let Some(health) = &self.health {
            health.set_ready(false);
            health.shutdown().await;
        }
        // Moved out rather than taken through `&mut`, so this compiles
        // identically with and without the feature.
        #[cfg(feature = "coordination")]
        if let Some(runtime) = self.coordination {
            runtime.shutdown().await;
        }
        if let Some(http) = &self.http {
            http.abort();
        }
        self.handle.abort();
        self.node.shutdown().await;
    }
}
