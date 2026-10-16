//! Binding support: the `[identity]` and `[remote_config]` wiring
//! [`AsterServer`](super::AsterServer) does, for bindings that serve on a
//! node they started themselves (the C ABI's `aster_server_*`) through
//! [`Server::new`]. Not part of the stable API.
//!
//! The sequence is the facade's, in the same order:
//!
//! 1. [`plan`] validates `[identity]` as a whole, the `[remote_config]`
//!    locks, `[[identity.trusted_issuers]]` needing Enrollment and
//!    `[identity.publisher]`, before anything is served.
//! 2. [`IdentityPlan::apply`] sets the identity settings on the server,
//!    installs the (empty) issuer-key cache when the node has Enrollment,
//!    opens `aster.ops.NodeConfig` (applying a stored record) and registers it
//!    behind `aster.config-admin`.
//! 3. After the server serves and Enrollment's policy is attached,
//!    [`PendingIdentity::attach`] starts the issuer-key publisher (delivering
//!    to the policy replicas) and the issuer-key watcher on the fleet
//!    namespace.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::baseline::RequireGuard;
use super::identity::{publisher_plan, IdentitySettings, LiveIdentity};
use super::issuer_keys::{IssuerKeys, IssuerKeysHandle, IssuerKeysWatch};
use super::node_config::{
    validate_locks, AuditSink, Init, LiveNodeConfig, LiveProperties, NodeConfigHandle,
    NodeConfigServer, NodeProperties,
};
use super::Server;
use crate::config::{ConfigSource, IdentityPublisherConfig};
use crate::enrollment::runtime::Plan;
use crate::enrollment::Enrollment;
use crate::{AsterConfig, Error, Node, Result};

/// The `[identity]` / `[remote_config]` settings of one server, validated.
pub struct IdentityPlan {
    settings: IdentitySettings,
    remote_config: Option<(Vec<String>, BTreeMap<String, ConfigSource>)>,
    publisher: Option<IdentityPublisherConfig>,
    audit: Option<Arc<dyn AuditSink>>,
}

/// Validate `config`'s `[identity]` and `[remote_config]` for a server, as
/// [`AsterServerBuilder::start`](super::AsterServerBuilder::start) does:
/// `has_enrollment` is whether the node runs Enrollment (trusted issuers and
/// the publisher need it).
pub fn plan(config: &AsterConfig, has_enrollment: bool) -> Result<IdentityPlan> {
    let settings = IdentitySettings::from_config(config);
    settings
        .validate()
        .map_err(|e| Error::InvalidArgument(format!("[identity]: {e}")))?;
    validate_locks(&config.remote_config_lock)?;
    if !settings.trusted_issuers.is_empty() && !has_enrollment {
        return Err(Error::InvalidArgument(
            "[[identity.trusted_issuers]] needs [enrollment]: issuer keys come from the \
             fleet namespace"
                .into(),
        ));
    }
    let publisher = publisher_plan(config, has_enrollment)?;
    let remote_config = config.remote_config_enabled.then(|| {
        (
            config.remote_config_lock.clone(),
            config.setting_sources.clone(),
        )
    });
    Ok(IdentityPlan {
        settings,
        remote_config,
        publisher,
        audit: None,
    })
}

/// The `[identity.publisher]` section a **client** node runs, checked as
/// [`AsterClient`](super::AsterClient) checks it.
pub fn client_publisher_plan(
    config: &AsterConfig,
    has_enrollment: bool,
) -> Result<Option<IdentityPublisherConfig>> {
    publisher_plan(config, has_enrollment)
}

impl IdentityPlan {
    /// Whether calls on behalf of users are accepted (before NodeConfig).
    pub fn accept_on_behalf_of(&self) -> bool {
        self.settings.accept_on_behalf_of
    }

    /// Override `[identity] accept_on_behalf_of` (a binding's explicit server
    /// option wins over the config, as a builder call does in Rust).
    pub fn set_accept_on_behalf_of(&mut self, accept: bool) {
        self.settings.accept_on_behalf_of = accept;
    }

    /// Whether `aster.ops.NodeConfig` is served.
    pub fn serves_node_config(&self) -> bool {
        self.remote_config.is_some()
    }

    /// Record every `aster.ops.NodeConfig` `set` and `clear` in `outbox`
    /// ([`AsterServerBuilder::audit_outbox`](super::AsterServerBuilder::audit_outbox)).
    #[cfg(feature = "audit-outbox")]
    pub fn audit_outbox(&mut self, outbox: crate::audit_outbox::AuditOutbox) {
        self.audit = Some(Arc::new(outbox));
    }

    /// Before serving: the settings, the issuer-key cache (with Enrollment)
    /// and `aster.ops.NodeConfig`, whose stored record is applied here.
    pub async fn apply(
        self,
        server: Server,
        node: &Node,
        enrollment: Option<(&Enrollment, &Plan)>,
    ) -> Result<(Server, PendingIdentity)> {
        let mut server = server.identity_settings(self.settings.clone());
        let live = server.live_identity();
        let properties = server.live_properties();
        let issuer_keys = Arc::new(IssuerKeys::default());
        if enrollment.is_some() {
            live.set_keys(issuer_keys.clone());
        }
        let node_config = match self.remote_config {
            None => None,
            Some((locks, sources)) => {
                let init = Init {
                    identity: live.clone(),
                    properties: properties.clone(),
                    base_identity: self.settings.clone(),
                    sources,
                    locks,
                    authority: enrollment.map(|(e, plan)| plan.attribute_source(e)),
                    audit: self.audit,
                };
                let handle = NodeConfigHandle::open(node, init).await?;
                server = server.register_arc(Arc::new(RequireGuard {
                    inner: NodeConfigServer::new(LiveNodeConfig {
                        handle: handle.clone(),
                    }),
                    requires: Some(super::require_role(super::CONFIG_ADMIN_ROLE)),
                }));
                Some(handle)
            }
        };
        Ok((
            server,
            PendingIdentity {
                live,
                properties,
                issuer_keys,
                node_config,
                publisher: self.publisher,
                max_key_set_age_secs: self.settings.max_key_set_age_secs,
            },
        ))
    }
}

/// A server's identity wiring between [`IdentityPlan::apply`] and serving.
pub struct PendingIdentity {
    live: Arc<LiveIdentity>,
    properties: LiveProperties,
    issuer_keys: Arc<IssuerKeys>,
    node_config: Option<NodeConfigHandle>,
    #[cfg_attr(not(feature = "identity-publisher"), allow(dead_code))]
    publisher: Option<IdentityPublisherConfig>,
    #[cfg_attr(not(feature = "identity-publisher"), allow(dead_code))]
    max_key_set_age_secs: u64,
}

impl PendingIdentity {
    /// After the server serves (and, with Enrollment, its policy is
    /// attached): start the publisher and the issuer-key watcher.
    pub async fn attach(
        self,
        node: &Node,
        enrollment: Option<(&Enrollment, &Plan)>,
    ) -> Result<ServerIdentity> {
        let mut out = ServerIdentity {
            properties: self.properties,
            node_config: self.node_config,
            issuer_keys: None,
            _watch: None,
            #[cfg(feature = "identity-publisher")]
            publisher: None,
        };
        let Some((enrollment, plan)) = enrollment else {
            return Ok(out);
        };
        #[cfg(feature = "identity-publisher")]
        if let Some(config) = &self.publisher {
            let publisher = super::jwks_publisher::IssuerKeyPublisher::start(
                node,
                enrollment.clone(),
                config,
                self.max_key_set_age_secs,
                plan.replica_ids(),
            )?;
            let handle = publisher.handle();
            self.issuer_keys
                .set_kid_notice(Arc::new(move |iss: &str, kid: &str| {
                    handle.notice_kid(iss, kid)
                }));
            out.publisher = Some(publisher);
        }
        let watch = super::issuer_keys::watch(
            node,
            enrollment.clone(),
            plan.mapping.clone(),
            self.live,
            self.issuer_keys.clone(),
        )
        .await?;
        out.issuer_keys = Some(IssuerKeysHandle(self.issuer_keys));
        out._watch = Some(watch);
        Ok(out)
    }
}

/// A serving server's identity state; dropping it stops the issuer-key
/// watcher and the publisher.
pub struct ServerIdentity {
    properties: LiveProperties,
    node_config: Option<NodeConfigHandle>,
    issuer_keys: Option<IssuerKeysHandle>,
    _watch: Option<IssuerKeysWatch>,
    #[cfg(feature = "identity-publisher")]
    publisher: Option<super::jwks_publisher::IssuerKeyPublisher>,
}

impl ServerIdentity {
    /// The NodeConfig properties handlers see now
    /// ([`CallContext::node_properties`](super::CallContext::node_properties)).
    pub fn node_properties(&self) -> Arc<NodeProperties> {
        self.properties.load()
    }

    /// The generation whose effect is in force (`0`: never changed, or
    /// NodeConfig is off).
    pub fn generation(&self) -> u64 {
        self.node_config
            .as_ref()
            .map_or(0, NodeConfigHandle::generation)
    }

    /// `aster.ops.NodeConfig`'s state; `None` when `[remote_config]` turns it
    /// off.
    pub fn node_config(&self) -> Option<&NodeConfigHandle> {
        self.node_config.as_ref()
    }

    /// The issuer keys read from the fleet namespace (with Enrollment).
    pub fn issuer_keys(&self) -> Option<&IssuerKeysHandle> {
        self.issuer_keys.as_ref()
    }

    /// The issuer-key publisher from `[identity.publisher]`.
    #[cfg(feature = "identity-publisher")]
    pub fn publisher(&self) -> Option<super::jwks_publisher::PublisherHandle> {
        self.publisher.as_ref().map(|p| p.handle())
    }
}
