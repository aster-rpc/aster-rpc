//! Late activation of an [`AsterServer`](super::AsterServer)'s RPC surface.
//!
//! A server built with
//! [`deferred_surface()`](super::AsterServerBuilder::deferred_surface) starts
//! its node -- identity, blobs, docs, gossip, admission, the `aster/1` ALPN
//! and any host services -- without its application services. Every call to
//! a service the host does not serve answers `UNAVAILABLE`. When the
//! application is ready it hands the server one immutable [`RpcSurface`]:
//!
//! ```no_run
//! # use aster::rpc::{AsterServer, ManifestPublication, RpcSurface, ServiceDispatch};
//! # async fn run<S: ServiceDispatch>(svc: S) -> aster::Result<()> {
//! let srv = AsterServer::builder()
//!     .persistent("/var/lib/app")
//!     .deferred_surface()
//!     .start()
//!     .await?;
//! // node, blobs, docs, gossip and srv.id() are usable; recovery runs here.
//! let activation = srv
//!     .activate(
//!         RpcSurface::builder()
//!             .service(svc)
//!             .manifest_publication(ManifestPublication::Strict)
//!             .build(),
//!     )
//!     .await?; // returns once callable and advertised
//! # let _ = activation;
//! srv.deactivate().await?; // the node stays up
//! # Ok(())
//! # }
//! ```
//!
//! Design: `docs/_internal/aster-server-late-activation.md`.

use std::collections::HashMap;
use std::sync::Arc;

use super::server::{LateServices, LateSlot, ServiceDispatch};
use super::sessions::{ServiceKey, SessionFactory, SessionInstance};
use crate::error::{Error, Result};

/// What an activation does about contract manifests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ManifestPublication {
    /// Publish nothing: services are served but not discoverable through a
    /// registry doc. The explicit opt-out.
    Disabled,
    /// Publish every service's contract (the default, as every other
    /// binding does). Start-up or activation still succeeds when publication
    /// fails or is impossible (no `docs`/`blobs` support, no writable
    /// registry); the failure is logged and the [`SurfaceActivation`]
    /// reports what was not published.
    #[default]
    BestEffort,
    /// Publish every service's contract or fail the activation. On failure
    /// nothing stays advertised, nothing stays callable, and the server is
    /// back where it was before the call. A service without a contract
    /// ([`ServiceDispatch::contract_artifacts`] is `None`) fails it too.
    Strict,
}

/// One immutable set of application services, activated on a running
/// deferred [`AsterServer`](super::AsterServer) in one step. Build it with
/// [`RpcSurface::builder`].
pub struct RpcSurface {
    services: Vec<Arc<dyn ServiceDispatch>>,
    /// Session-scoped services: a metadata probe and the factory.
    sessions: Vec<(Arc<dyn ServiceDispatch>, SessionFactory)>,
    publication: ManifestPublication,
    #[cfg(all(feature = "docs", feature = "blobs"))]
    registry: Option<crate::docs::Doc>,
}

impl RpcSurface {
    pub fn builder() -> RpcSurfaceBuilder {
        RpcSurfaceBuilder::default()
    }
}

/// Builder for an [`RpcSurface`].
#[derive(Default)]
pub struct RpcSurfaceBuilder {
    services: Vec<Arc<dyn ServiceDispatch>>,
    sessions: Vec<(Arc<dyn ServiceDispatch>, SessionFactory)>,
    publication: ManifestPublication,
    #[cfg(all(feature = "docs", feature = "blobs"))]
    registry: Option<crate::docs::Doc>,
}

impl RpcSurfaceBuilder {
    /// Add a shared service (chainable; once per service).
    pub fn service(mut self, svc: impl ServiceDispatch) -> Self {
        self.services.push(Arc::new(svc));
        self
    }

    /// Add a **session-scoped** service: `factory` builds one instance per
    /// session, as [`AsterServerBuilder::register_session`](super::AsterServerBuilder::register_session).
    pub fn register_session<S, F>(mut self, factory: F) -> Self
    where
        S: ServiceDispatch,
        F: Fn() -> S + Send + Sync + 'static,
    {
        let probe: Arc<dyn ServiceDispatch> = Arc::new(factory());
        let factory: SessionFactory =
            Arc::new(move || Ok(SessionInstance::Native(Arc::new(factory()))));
        self.sessions.push((probe, factory));
        self
    }

    /// How this surface's contracts are published (default
    /// [`BestEffort`](ManifestPublication::BestEffort)).
    pub fn manifest_publication(mut self, policy: ManifestPublication) -> Self {
        self.publication = policy;
        self
    }

    /// Publish into `registry` instead of the registry doc the server creates
    /// on its first publishing activation (and reuses after that). The
    /// server's node must be able to write it.
    #[cfg(all(feature = "docs", feature = "blobs"))]
    pub fn registry(mut self, registry: crate::docs::Doc) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn build(self) -> RpcSurface {
        RpcSurface {
            services: self.services,
            sessions: self.sessions,
            publication: self.publication,
            #[cfg(all(feature = "docs", feature = "blobs"))]
            registry: self.registry,
        }
    }
}

/// Whether a deferred server's surface is serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceState {
    /// No surface: calls to non-host services answer `UNAVAILABLE`.
    Inactive,
    /// A surface is installed, callable and advertised per its policy.
    Active,
}

/// One service's published contract.
#[derive(Debug, Clone)]
pub struct PublishedService {
    pub name: String,
    pub version: i32,
    pub contract_id: String,
}

/// The result of a successful [`activate`](super::AsterServer::activate).
#[derive(Clone)]
pub struct SurfaceActivation {
    /// Every `(name, version)` the surface serves.
    pub services: Vec<(String, i32)>,
    /// The policy the surface was activated under.
    pub publication: ManifestPublication,
    /// The contracts now advertised in [`registry`](Self::registry).
    pub published: Vec<PublishedService>,
    /// Why some services were not published. Only ever non-empty under
    /// [`BestEffort`](ManifestPublication::BestEffort): discovery is degraded.
    pub publication_failures: Vec<String>,
    /// The registry doc the contracts went to (`None` when nothing was
    /// published). Share it read-only (`share(ShareMode::Read)`) to let
    /// consumers discover the services.
    #[cfg(all(feature = "docs", feature = "blobs"))]
    pub registry: Option<crate::docs::Doc>,
}

impl std::fmt::Debug for SurfaceActivation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("SurfaceActivation");
        d.field("services", &self.services)
            .field("publication", &self.publication)
            .field("published", &self.published)
            .field("publication_failures", &self.publication_failures);
        #[cfg(all(feature = "docs", feature = "blobs"))]
        d.field(
            "registry",
            &self
                .registry
                .as_ref()
                .and_then(|r| r.id().ok())
                .map(|id| id.to_hex()),
        );
        d.finish()
    }
}

impl SurfaceActivation {
    /// Whether publication was asked for and did not fully happen.
    pub fn is_degraded(&self) -> bool {
        !self.publication_failures.is_empty()
    }
}

/// What an active surface left in the registry, for retraction.
#[cfg(all(feature = "docs", feature = "blobs"))]
struct Advertised {
    registry: crate::docs::Doc,
    author: crate::id::AuthorId,
    keys: Vec<Vec<u8>>,
}

struct Active {
    keys: Vec<ServiceKey>,
    /// What this activation listed under `aster.services`, to take back.
    listed: Vec<super::baseline::AdvertisedService>,
    #[cfg(all(feature = "docs", feature = "blobs"))]
    advertised: Option<Advertised>,
}

#[derive(Default)]
struct ControlState {
    active: Option<Active>,
    /// The registry doc this server created for publication, reused by every
    /// later activation that does not name its own.
    #[cfg(all(feature = "docs", feature = "blobs"))]
    own_registry: Option<crate::docs::Doc>,
}

/// The activation state machine a deferred server owns. One activation or
/// deactivation runs at a time.
pub(crate) struct SurfaceControl {
    slot: Arc<LateSlot>,
    state: tokio::sync::Mutex<ControlState>,
}

/// What [`SurfaceControl::activate`] needs from the server.
pub(crate) struct Host<'a> {
    #[cfg_attr(not(all(feature = "docs", feature = "blobs")), allow(dead_code))]
    pub(crate) node: &'a crate::node::Node,
    pub(crate) dispatcher: &'a super::server::Dispatcher,
    /// The served NodeInfo identity, when the baseline is on.
    pub(crate) identity: Option<&'a super::baseline::NodeIdentityHandle>,
    #[cfg(feature = "health")]
    pub(crate) health: Option<&'a crate::ops::HealthServer>,
    /// Services the server reports to `/readyz` before any surface.
    #[cfg(feature = "health")]
    pub(crate) host_services: u32,
}

impl SurfaceControl {
    pub(crate) fn new(slot: Arc<LateSlot>) -> Self {
        Self {
            slot,
            state: tokio::sync::Mutex::new(ControlState::default()),
        }
    }

    pub(crate) fn state(&self) -> SurfaceState {
        match self.slot.get() {
            Some(_) => SurfaceState::Active,
            None => SurfaceState::Inactive,
        }
    }

    pub(crate) async fn activate(
        &self,
        host: Host<'_>,
        surface: RpcSurface,
    ) -> Result<SurfaceActivation> {
        let mut state = self.state.lock().await;
        if state.active.is_some() {
            return Err(Error::rpc(
                super::StatusCode::FailedPrecondition,
                "an rpc surface is already active; deactivate it first",
            ));
        }

        // Validate the whole set before anything is staged.
        let mut late = LateServices {
            services: HashMap::new(),
            session_probes: HashMap::new(),
            session_factories: HashMap::new(),
        };
        let services: Vec<(Arc<dyn ServiceDispatch>, bool)> = surface
            .services
            .iter()
            .map(|svc| (svc.clone(), false))
            .chain(
                surface
                    .sessions
                    .iter()
                    .map(|(probe, _)| (probe.clone(), true)),
            )
            .collect();
        let mut keys: Vec<ServiceKey> = Vec::new();
        for (svc, _) in &services {
            let key = (svc.name().to_string(), svc.version());
            if keys.contains(&key) || host.dispatcher.serves_host(&key) {
                return Err(Error::InvalidArgument(format!(
                    "service '{}' v{} is already registered",
                    key.0, key.1
                )));
            }
            keys.push(key);
        }
        if keys.is_empty() {
            return Err(Error::InvalidArgument(
                "an rpc surface needs at least one service".into(),
            ));
        }

        // Phase 1: stage contracts (content-addressed blobs, referenced from
        // nowhere yet). A Strict failure here leaves nothing behind.
        #[allow(unused_mut)]
        let mut failures: Vec<String> = Vec::new();
        #[cfg(all(feature = "docs", feature = "blobs"))]
        let staged = match surface.publication {
            ManifestPublication::Disabled => None,
            policy => match stage_all(host.node, &services, policy, &mut failures).await {
                Ok(staged) => Some(staged),
                Err(err) => return Err(err),
            },
        };
        #[cfg(not(all(feature = "docs", feature = "blobs")))]
        match surface.publication {
            ManifestPublication::Disabled => {}
            ManifestPublication::BestEffort => failures
                .push("manifest publication needs the `docs` and `blobs` features".to_string()),
            ManifestPublication::Strict => {
                return Err(Error::InvalidArgument(
                    "ManifestPublication::Strict needs the `docs` and `blobs` features".into(),
                ))
            }
        }

        // Phase 2: install dispatch. Session instances a previous surface left
        // for these names (a call routed as it was withdrawn) are dropped
        // first, so they cannot be served by this one.
        for svc in &surface.services {
            late.services
                .insert((svc.name().to_string(), svc.version()), svc.clone());
        }
        for (probe, factory) in surface.sessions {
            let key = (probe.name().to_string(), probe.version());
            late.session_probes.insert(key.clone(), probe);
            late.session_factories.insert(key, factory);
        }
        host.dispatcher.session_store().drop_services(&keys);
        self.slot.install(Arc::new(late));
        // Until the activation commits, any exit -- an error below or the
        // caller dropping this future -- withdraws dispatch again.
        #[allow(unused_mut)]
        let mut rollback = Rollback {
            slot: &self.slot,
            dispatcher: host.dispatcher,
            keys: &keys,
            armed: true,
            #[cfg(all(feature = "docs", feature = "blobs"))]
            advertising: None,
        };

        // Phase 3: advertise. Dispatch is already live, so nothing is ever
        // discoverable before it is callable.
        #[allow(unused_mut)]
        let mut published = Vec::new();
        #[cfg(all(feature = "docs", feature = "blobs"))]
        let (advertised, registry) = match staged {
            None => (None, None),
            Some(staged) => {
                let registry = match surface.registry {
                    Some(doc) => Ok(doc),
                    None => own_registry(host.node, &mut state).await,
                };
                let outcome = match registry {
                    // The writes run in their own task so that dropping this
                    // future never interrupts one half-way. If it is dropped,
                    // `rollback` waits for the task and retracts its writes.
                    Ok(registry) => {
                        let task = tokio::spawn(advertise_all(registry, staged));
                        let task = rollback.advertising.insert(task);
                        let joined = task.await;
                        rollback.advertising = None;
                        match joined {
                            Ok(outcome) => outcome,
                            Err(err) => Err(Error::Transport(anyhow::anyhow!(
                                "manifest publication task: {err}"
                            ))),
                        }
                    }
                    Err(err) => Err(err),
                };
                match (outcome, surface.publication) {
                    (Ok((advertised, done)), _) => {
                        published = done;
                        let registry = advertised.registry.clone();
                        (Some(advertised), Some(registry))
                    }
                    (Err(err), ManifestPublication::Strict) => {
                        // Back to where the call started: `rollback` stops
                        // dispatch, and `advertise_all` retracted its writes.
                        return Err(err);
                    }
                    (Err(err), _) => {
                        failures.push(format!("registry: {err}"));
                        (None, None)
                    }
                }
            }
        };

        #[cfg(feature = "health")]
        if let Some(health) = host.health {
            let served = u32::try_from(keys.len()).unwrap_or(u32::MAX);
            health
                .state()
                .set_services(host.host_services.saturating_add(served));
            health.state().set_registry(!published.is_empty());
            health.set_ready(true);
        }

        tracing::info!(
            target: crate::log_targets::SERVER,
            services = keys.len(),
            published = published.len(),
            degraded = !failures.is_empty(),
            "rpc surface active"
        );
        rollback.armed = false;
        drop(rollback);
        // Discoverable only once callable and committed.
        let listed: Vec<_> = services
            .iter()
            .filter_map(|(svc, session)| {
                super::baseline::AdvertisedService::from_dispatch(svc.as_ref(), *session)
            })
            .collect();
        if let Some(identity) = host.identity {
            identity.advertise_services(&listed);
        }
        state.active = Some(Active {
            keys: keys.clone(),
            listed,
            #[cfg(all(feature = "docs", feature = "blobs"))]
            advertised,
        });
        Ok(SurfaceActivation {
            services: keys,
            publication: surface.publication,
            published,
            publication_failures: failures,
            #[cfg(all(feature = "docs", feature = "blobs"))]
            registry,
        })
    }

    /// Withdraw the active surface, if any: readiness first, then discovery,
    /// then dispatch. The surface is stopped even when retracting its
    /// registry entries fails; that failure is then returned.
    pub(crate) async fn deactivate(&self, host: Host<'_>) -> Result<()> {
        let mut state = self.state.lock().await;
        let Some(active) = state.active.take() else {
            return Ok(());
        };
        #[cfg(feature = "health")]
        if let Some(health) = host.health {
            health.set_ready(false);
            health.state().set_registry(false);
            health.state().set_services(host.host_services);
        }
        #[allow(unused_mut)]
        let mut result = Ok(());
        #[cfg(all(feature = "docs", feature = "blobs"))]
        if let Some(adv) = &active.advertised {
            if let Err(err) =
                super::publish::retract_keys(&adv.registry, &adv.author, &adv.keys).await
            {
                result = Err(Error::Transport(anyhow::anyhow!(
                    "rpc surface stopped, but its registry entries were not all retracted: {err}"
                )));
            }
        }
        if let Some(identity) = host.identity {
            identity.withdraw_services(&active.listed);
        }
        self.slot.clear();
        host.dispatcher.session_store().drop_services(&active.keys);
        tracing::info!(
            target: crate::log_targets::SERVER,
            services = active.keys.len(),
            "rpc surface stopped"
        );
        result
    }
}

/// Undoes a not-yet-committed activation when dropped: withdraws dispatch at
/// once and, when the activation was cancelled while its registry writes were
/// running, retracts them once they finish.
struct Rollback<'a> {
    slot: &'a LateSlot,
    dispatcher: &'a super::server::Dispatcher,
    keys: &'a [ServiceKey],
    /// Cleared when the activation commits.
    armed: bool,
    /// The registry-write task while the activation is waiting on it.
    #[cfg(all(feature = "docs", feature = "blobs"))]
    advertising: Option<tokio::task::JoinHandle<AdvertiseOutcome>>,
}

impl Drop for Rollback<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.slot.clear();
        self.dispatcher.session_store().drop_services(self.keys);
        // Only set when the caller dropped the activation mid-publication: a
        // drop cannot await, so a task finishes the writes' undo. A failed
        // task has already retracted its own writes.
        #[cfg(all(feature = "docs", feature = "blobs"))]
        if let Some(task) = self.advertising.take() {
            let retract = async move {
                if let Ok(Ok((adv, _))) = task.await {
                    if let Err(err) =
                        super::publish::retract_keys(&adv.registry, &adv.author, &adv.keys).await
                    {
                        tracing::warn!(
                            target: crate::log_targets::SERVER,
                            error = %err,
                            "cancelled rpc surface activation: registry entries not all retracted"
                        );
                    }
                }
            };
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(retract);
                }
                Err(_) => tracing::warn!(
                    target: crate::log_targets::SERVER,
                    "cancelled rpc surface activation outside a runtime: registry entries left"
                ),
            }
        }
    }
}

/// What the registry-write task reports.
#[cfg(all(feature = "docs", feature = "blobs"))]
type AdvertiseOutcome = Result<(Advertised, Vec<PublishedService>)>;

#[cfg(all(feature = "docs", feature = "blobs"))]
struct Staged {
    author: crate::id::AuthorId,
    contracts: Vec<super::publish::StagedContract>,
}

/// What a one-step [`AsterServer`](super::AsterServer) published at start.
#[derive(Clone, Default)]
pub(crate) struct StartPublication {
    pub published: Vec<PublishedService>,
    pub failures: Vec<String>,
    #[cfg(all(feature = "docs", feature = "blobs"))]
    pub registry: Option<crate::docs::Doc>,
}

/// Best-effort publication for a server started in one step. Never fails:
/// a node that cannot publish (feature missing, docs or blobs unavailable)
/// logs and serves without manifests.
#[cfg(all(feature = "docs", feature = "blobs"))]
pub(crate) async fn publish_at_start(
    node: &crate::node::Node,
    services: &[(Arc<dyn ServiceDispatch>, bool)],
) -> StartPublication {
    let mut failures = Vec::new();
    let staged = match stage_all(
        node,
        services,
        ManifestPublication::BestEffort,
        &mut failures,
    )
    .await
    {
        Ok(staged) => staged,
        Err(err) => {
            failures.push(format!("staging: {err}"));
            return start_publication_skipped(failures);
        }
    };
    if staged.contracts.is_empty() {
        return start_publication_skipped(failures);
    }
    let registry = match node.docs().create().await {
        Ok(doc) => doc,
        Err(err) => {
            failures.push(format!("registry: {err}"));
            return start_publication_skipped(failures);
        }
    };
    match advertise_all(registry, staged).await {
        Ok((advertised, published)) => {
            if !failures.is_empty() {
                tracing::warn!(
                    target: crate::log_targets::SERVER,
                    failures = ?failures,
                    "some contracts were not published"
                );
            }
            StartPublication {
                published,
                failures,
                registry: Some(advertised.registry),
            }
        }
        Err(err) => {
            failures.push(format!("registry: {err}"));
            start_publication_skipped(failures)
        }
    }
}

#[cfg(not(all(feature = "docs", feature = "blobs")))]
pub(crate) async fn publish_at_start(
    _node: &crate::node::Node,
    _services: &[(Arc<dyn ServiceDispatch>, bool)],
) -> StartPublication {
    tracing::info!(
        target: crate::log_targets::SERVER,
        "manifest publication skipped: built without the `docs` and `blobs` features"
    );
    StartPublication::default()
}

#[cfg(all(feature = "docs", feature = "blobs"))]
fn start_publication_skipped(failures: Vec<String>) -> StartPublication {
    tracing::info!(
        target: crate::log_targets::SERVER,
        failures = ?failures,
        "manifest publication skipped; serving without published contracts"
    );
    StartPublication {
        failures,
        ..StartPublication::default()
    }
}

/// Collect and upload every service's contract. Under Strict any failure is
/// returned; under BestEffort it is recorded in `failures` and skipped.
#[cfg(all(feature = "docs", feature = "blobs"))]
async fn stage_all(
    node: &crate::node::Node,
    services: &[(Arc<dyn ServiceDispatch>, bool)],
    policy: ManifestPublication,
    failures: &mut Vec<String>,
) -> Result<Staged> {
    let strict = policy == ManifestPublication::Strict;
    let author = node.docs().default_author().await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let blobs = node.blobs();
    let mut contracts = Vec::new();
    for (svc, session_scoped) in services {
        let label = format!("{} v{}", svc.name(), svc.version());
        let Some(mut artifacts) = svc.contract_artifacts() else {
            let why = format!("{label}: no contract to publish (hand-written dispatcher)");
            if strict {
                return Err(Error::InvalidArgument(format!(
                    "strict manifest publication: {why}"
                )));
            }
            failures.push(why);
            continue;
        };
        if *session_scoped {
            artifacts.contract.scoped = super::ScopeKind::Session;
        }
        match super::publish::stage_contract(
            &blobs,
            &author,
            &artifacts.contract,
            &artifacts.type_defs,
            now,
        )
        .await
        {
            Ok(staged) => contracts.push(staged),
            Err(err) if strict => {
                return Err(Error::Transport(anyhow::anyhow!(
                    "strict manifest publication: {label}: {err}"
                )))
            }
            Err(err) => failures.push(format!("{label}: {err}")),
        }
    }
    Ok(Staged { author, contracts })
}

/// Write every staged contract into `registry`. On any failure, retract what
/// this call wrote and return the error.
#[cfg(all(feature = "docs", feature = "blobs"))]
async fn advertise_all(registry: crate::docs::Doc, staged: Staged) -> AdvertiseOutcome {
    let mut published = Vec::new();
    let mut keys = Vec::new();
    for contract in &staged.contracts {
        if let Err(err) =
            super::publish::advertise_contract(&registry, &staged.author, contract, &mut keys).await
        {
            // Best effort: a partial advertisement must not survive.
            let _ = super::publish::retract_keys(&registry, &staged.author, &keys).await;
            return Err(Error::Transport(anyhow::anyhow!(
                "manifest publication: {} v{}: {err}",
                contract.name(),
                contract.version()
            )));
        }
        published.push(PublishedService {
            name: contract.name().to_string(),
            version: contract.version(),
            contract_id: contract.published.contract_id.clone(),
        });
    }
    Ok((
        Advertised {
            registry,
            author: staged.author,
            keys,
        },
        published,
    ))
}

/// The registry doc this server publishes into when a surface names none:
/// created on first use, then reused.
#[cfg(all(feature = "docs", feature = "blobs"))]
async fn own_registry(
    node: &crate::node::Node,
    state: &mut ControlState,
) -> Result<crate::docs::Doc> {
    if let Some(doc) = &state.own_registry {
        return Ok(doc.clone());
    }
    let doc = node.docs().create().await?;
    state.own_registry = Some(doc.clone());
    Ok(doc)
}
