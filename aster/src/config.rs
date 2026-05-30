//! Node configuration and its builder.

use crate::id::SecretKey;
use aster_transport_core::CoreEndpointConfig;
use std::path::PathBuf;

/// How the node uses relay servers.
#[derive(Clone, Debug, Default)]
pub enum RelayMode {
    /// Use the default (n0) relay servers.
    #[default]
    Default,
    /// Use a specific set of relay URLs.
    Custom(Vec<String>),
    /// Disable relays entirely (direct connections only).
    Disabled,
}

/// Configuration for an Aster [`Node`](crate::Node).
///
/// Build one with [`AsterConfig::builder`]. A node is in-memory unless a
/// [`data_dir`](AsterConfigBuilder::persistent) is set, in which case blobs,
/// docs (namespaces / authors / entries) and the node's default author are
/// persisted there and survive a restart.
#[derive(Clone, Debug)]
pub struct AsterConfig {
    pub(crate) inner: CoreEndpointConfig,
    pub(crate) data_dir: Option<PathBuf>,
}

impl AsterConfig {
    /// Start building a configuration.
    pub fn builder() -> AsterConfigBuilder {
        AsterConfigBuilder {
            inner: CoreEndpointConfig::default(),
            data_dir: None,
        }
    }

    /// Whether this configuration selects a persistent node.
    pub fn is_persistent(&self) -> bool {
        self.data_dir.is_some()
    }
}

impl Default for AsterConfig {
    fn default() -> Self {
        AsterConfig::builder().build()
    }
}

/// Fluent builder for [`AsterConfig`].
#[derive(Clone, Debug)]
pub struct AsterConfigBuilder {
    inner: CoreEndpointConfig,
    data_dir: Option<PathBuf>,
}

impl AsterConfigBuilder {
    /// Select the relay mode (default: [`RelayMode::Default`]).
    pub fn relay(mut self, mode: RelayMode) -> Self {
        match mode {
            RelayMode::Default => {
                self.inner.relay_mode = Some("default".into());
                self.inner.relay_urls = Vec::new();
            }
            RelayMode::Custom(urls) => {
                self.inner.relay_mode = Some("custom".into());
                self.inner.relay_urls = urls;
            }
            RelayMode::Disabled => {
                self.inner.relay_mode = Some("disabled".into());
                self.inner.relay_urls = Vec::new();
            }
        }
        self
    }

    /// Make the node persistent, storing all state under `dir`.
    pub fn persistent(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        self.inner.data_dir = Some(dir.to_string_lossy().into_owned());
        self.data_dir = Some(dir);
        self
    }

    /// Pin the node's identity to a specific secret key. On a persistent node
    /// combined with the same `data_dir`, this gives a restart-stable
    /// [`NodeId`](crate::NodeId).
    pub fn secret_key(mut self, key: SecretKey) -> Self {
        self.inner.secret_key = Some(key.to_bytes().to_vec());
        self
    }

    /// Bind the endpoint to a specific socket address, e.g. `"0.0.0.0:9000"`.
    pub fn bind_addr(mut self, addr: impl Into<String>) -> Self {
        self.inner.bind_addr = Some(addr.into());
        self
    }

    /// Enable local-network peer discovery (mDNS).
    pub fn discovery(mut self, enabled: bool) -> Self {
        self.inner.enable_discovery = enabled;
        self
    }

    /// Enable connection monitoring / remote-info tracking.
    pub fn monitoring(mut self, enabled: bool) -> Self {
        self.inner.enable_monitoring = enabled;
        self
    }

    /// Enable admission hooks. Required for [`Node::take_admission`](crate::Node::take_admission)
    /// to return a handle.
    pub fn hooks(mut self, enabled: bool) -> Self {
        self.inner.enable_hooks = enabled;
        self
    }

    /// Timeout (ms) for an admission decision before the connection is
    /// accepted by default. Only meaningful with [`hooks`](Self::hooks).
    pub fn hook_timeout_ms(mut self, ms: u64) -> Self {
        self.inner.hook_timeout_ms = ms;
        self
    }

    /// Finish building.
    pub fn build(self) -> AsterConfig {
        AsterConfig {
            inner: self.inner,
            data_dir: self.data_dir,
        }
    }
}
