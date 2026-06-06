//! Node configuration and its builder.

use crate::error::{Error, Result};
use crate::id::{PublicKey, SecretKey};
use aster_transport_core::{trust::HookFailureMode, CoreEndpointConfig};
use base64::Engine;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How the node uses relay servers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RelayMode {
    /// Use the default (n0) relay servers.
    #[default]
    Default,
    /// Use a specific set of relay URLs.
    Custom(Vec<String>),
    /// Use the staging relay servers.
    Staging,
    /// Disable relays entirely (direct connections only).
    Disabled,
}

/// Published service metadata loaded from `.aster-identity`.
///
/// Values are JSON so callers can inspect token/contract metadata without
/// depending on TOML internals.
pub type PublishedServices = BTreeMap<String, BTreeMap<String, serde_json::Value>>;

/// A peer entry selected from `.aster-identity`.
#[derive(Clone, Debug, Default)]
pub struct AsterIdentityPeer {
    /// Peer name from the identity file.
    pub name: Option<String>,
    /// Peer role, usually `"producer"` or `"consumer"`.
    pub role: Option<String>,
    /// Root public key for the mesh this peer belongs to.
    pub root_pubkey: Option<PublicKey>,
    /// Credential type (`type` in TOML), for example `"policy"`.
    pub credential_type: Option<String>,
    /// Credential expiry timestamp, if present.
    pub expires_at: Option<i64>,
    /// Endpoint id for producer credentials.
    pub endpoint_id: Option<String>,
    /// Hex-encoded signature.
    pub signature: Option<String>,
    /// Peer attributes. `aster.role` and `aster.name` are synthesized from the
    /// top-level peer fields when absent, matching the TypeScript binding.
    pub attributes: BTreeMap<String, String>,
    /// Published-service metadata available to this peer.
    pub published_services: PublishedServices,
}

/// Secret key plus selected peer loaded from `.aster-identity`.
#[derive(Clone, Debug)]
pub struct LoadedIdentity {
    /// Path that was loaded.
    pub path: PathBuf,
    /// Node secret key from `[node].secret_key`, if present.
    pub secret_key: Option<SecretKey>,
    /// Selected peer entry, if any matched the requested name/role.
    pub peer: Option<AsterIdentityPeer>,
}

/// Configuration for an Aster [`Node`](crate::Node).
///
/// Build one explicitly with [`AsterConfig::builder`] or load the standard
/// `ASTER_*` environment variables with [`AsterConfig::from_env`]. A node is
/// in-memory unless [`storage_path`](Self::storage_path) is set, in which case
/// blobs, docs (namespaces / authors / entries), and the node's default author
/// are persisted there and survive a restart.
#[derive(Clone, Debug)]
pub struct AsterConfig {
    /// 32-byte Ed25519 root public key for the deployment mesh.
    pub root_pubkey: Option<PublicKey>,
    /// Path to a file containing the root public key as hex or JSON
    /// `{ "public_key": "<hex>" }`.
    pub root_pubkey_file: Option<PathBuf>,
    /// Path to a JSON enrollment credential signed by the root key.
    pub enrollment_credential_file: Option<PathBuf>,
    /// Cloud Instance Identity Document token, when required by credential
    /// policy.
    pub enrollment_credential_iid: Option<String>,
    /// Skip the consumer admission gate.
    pub allow_all_consumers: bool,
    /// Skip the producer admission gate.
    pub allow_all_producers: bool,

    /// Producer endpoint address for consumer-side dialing.
    pub endpoint_addr: Option<String>,

    /// Persistent node storage path. `None` selects an in-memory node.
    pub storage_path: Option<PathBuf>,

    /// Enable periodic blob garbage collection at this interval. `None` (the
    /// default) disables GC entirely — the blob store stays grow-only, with
    /// tags controlling retention but untagged blobs never reclaimed. When
    /// `Some(interval)`, the iroh-blobs GC loop sweeps every `interval`,
    /// collecting any blob no tag (or temp-tag) protects. GC is node-wide over
    /// the one shared blob store. For a deterministic manual sweep (e.g. in
    /// tests), see [`Blobs::gc_run_once`](crate::Blobs::gc_run_once).
    ///
    /// Currently exposed on the Rust facade only; no FFI/binding surface yet
    /// (see `docs/KNOWN_ISSUES.md`).
    pub gc_interval: Option<Duration>,

    /// Stable node identity key.
    pub secret_key: Option<SecretKey>,
    /// Relay mode for the endpoint.
    pub relay_mode: RelayMode,
    /// Local bind address, e.g. `"0.0.0.0:9000"`.
    pub bind_addr: Option<String>,
    /// Enable local-network peer discovery (mDNS).
    pub local_discovery: bool,
    /// Enable connection monitoring / remote-info tracking.
    pub enable_monitoring: bool,
    /// Enable admission hooks.
    pub enable_hooks: bool,
    /// Timeout in milliseconds for admission hook decisions.
    pub hook_timeout_ms: u64,
    /// Hook fallback behavior when the host callback is unavailable or times
    /// out.
    pub hook_failure_mode: HookFailureMode,
    /// Disable direct IP transports.
    pub clear_ip_transports: bool,
    /// Disable relay transports.
    pub clear_relay_transports: bool,
    /// Portmapper config: `"enabled"` or `"disabled"`.
    pub portmapper_config: Option<String>,
    /// Explicit HTTP/SOCKS proxy URL.
    pub proxy_url: Option<String>,
    /// Read proxy settings from `HTTP_PROXY` / `HTTPS_PROXY`.
    pub proxy_from_env: bool,

    /// Maximum number of incoming bidirectional QUIC streams.
    pub transport_max_concurrent_bidi_streams: Option<u64>,
    /// Maximum number of incoming unidirectional QUIC streams.
    pub transport_max_concurrent_uni_streams: Option<u64>,
    /// Per-stream QUIC receive window in bytes.
    pub transport_stream_receive_window: Option<u64>,
    /// Per-connection QUIC receive window in bytes.
    pub transport_receive_window: Option<u64>,
    /// QUIC send window in bytes.
    pub transport_send_window: Option<u64>,
    /// QUIC idle timeout in milliseconds.
    pub transport_max_idle_timeout_ms: Option<u64>,
    /// QUIC keep-alive interval in milliseconds.
    pub transport_keep_alive_interval_ms: Option<u64>,
    /// Initial QUIC UDP payload size before MTU discovery.
    pub transport_initial_mtu: Option<u16>,
    /// Incoming QUIC application datagram buffer in bytes. Zero disables
    /// incoming datagrams.
    pub transport_datagram_receive_buffer_size: Option<usize>,
    /// Outgoing QUIC application datagram buffer in bytes.
    pub transport_datagram_send_buffer_size: Option<usize>,
    /// Whether to use fair scheduling across send streams.
    pub transport_send_fairness: Option<bool>,
    /// Whether to use UDP generic segmentation offload when supported.
    pub transport_enable_segmentation_offload: Option<bool>,

    /// Log output format (`"text"` or `"json"`).
    pub log_format: String,
    /// Log level (`"debug"`, `"info"`, `"warning"`, `"error"`).
    pub log_level: String,
    /// Mask sensitive fields in logs.
    pub log_mask: bool,

    /// Path to a `.aster-identity` file.
    pub identity_file: Option<PathBuf>,

    pub(crate) inner: CoreEndpointConfig,
    pub(crate) data_dir: Option<PathBuf>,
}

impl AsterConfig {
    /// Start building a configuration.
    pub fn builder() -> AsterConfigBuilder {
        AsterConfigBuilder {
            config: Self::default(),
        }
    }

    /// Convert this resolved config back into a builder so code assignments can
    /// intentionally override file/env values.
    pub fn into_builder(self) -> AsterConfigBuilder {
        AsterConfigBuilder { config: self }
    }

    /// Build a configuration from `ASTER_*` environment variables.
    ///
    /// Supported variables mirror the high-level bindings: trust fields
    /// (`ASTER_ROOT_PUBKEY`, `ASTER_ROOT_PUBKEY_FILE`,
    /// `ASTER_ENROLLMENT_CREDENTIAL`, `ASTER_ALLOW_ALL_*`), connect/storage
    /// fields, endpoint/network fields, transport tuning, logging, and
    /// `ASTER_IDENTITY_FILE`.
    pub fn from_env() -> Result<Self> {
        let mut config = Self::default();
        config.apply_env()?;
        Ok(config)
    }

    /// Build a configuration from an `aster.toml` file, then overlay `ASTER_*`
    /// environment variables.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let mut config = Self::default();
        config.apply_file(path)?;
        config.apply_env()?;
        Ok(config)
    }

    /// Apply an `aster.toml` file over the current configuration.
    pub fn apply_file(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .map_err(|e| Error::InvalidArgument(format!("{}: {e}", path.to_string_lossy())))?;
        let raw: toml::Value = content.parse().map_err(|e| {
            Error::InvalidArgument(format!("{}: invalid TOML: {e}", path.to_string_lossy()))
        })?;
        self.apply_toml_value(&raw, &path.to_string_lossy())
    }

    fn apply_toml_value(&mut self, raw: &toml::Value, label: &str) -> Result<()> {
        if let Some(trust) = toml_section(raw, "trust", label)? {
            if let Some(value) = toml_string(trust, "root_pubkey", label)? {
                self.root_pubkey = Some(parse_public_key(&format!("{label} [trust]"), &value)?);
            }
            if let Some(value) = toml_path(trust, "root_pubkey_file", label)? {
                self.root_pubkey_file = Some(value);
            }
            if let Some(value) = toml_path(trust, "enrollment_credential", label)? {
                self.enrollment_credential_file = Some(value);
            }
            if let Some(value) = toml_string(trust, "enrollment_credential_iid", label)? {
                self.enrollment_credential_iid = Some(value);
            }
            if let Some(value) = toml_bool(trust, "allow_all_consumers", label)? {
                self.allow_all_consumers = value;
            }
            if let Some(value) = toml_bool(trust, "allow_all_producers", label)? {
                self.allow_all_producers = value;
            }
        }

        if let Some(connect) = toml_section(raw, "connect", label)? {
            if let Some(value) = toml_string(connect, "endpoint_addr", label)? {
                self.endpoint_addr = Some(value);
            }
        }

        if let Some(storage) = toml_section(raw, "storage", label)? {
            if let Some(value) = toml_path(storage, "path", label)? {
                self.set_persistent(value);
            }
        }

        if let Some(network) = toml_section(raw, "network", label)? {
            if let Some(value) = toml_string_allow_empty(network, "secret_key", label)? {
                if value.trim().is_empty() {
                    self.set_secret_key(None);
                } else {
                    self.set_secret_key(Some(parse_secret_key(
                        &format!("{label} [network] secret_key"),
                        &value,
                    )?));
                }
            }
            if let Some(value) = toml_string(network, "relay_mode", label)? {
                self.set_relay_mode(parse_relay_mode(&value));
            }
            if let Some(value) = toml_string(network, "bind_addr", label)? {
                self.bind_addr = Some(value.clone());
                self.inner.bind_addr = Some(value);
            }
            if let Some(value) = toml_bool(network, "local_discovery", label)? {
                self.local_discovery = value;
                self.inner.enable_discovery = value;
            }
            if let Some(value) = toml_bool(network, "enable_monitoring", label)? {
                self.enable_monitoring = value;
                self.inner.enable_monitoring = value;
            }
            if let Some(value) = toml_bool(network, "enable_hooks", label)? {
                self.enable_hooks = value;
                self.inner.enable_hooks = value;
            }
            if let Some(value) = toml_u64(network, "hook_timeout_ms", label)? {
                self.hook_timeout_ms = value;
                self.inner.hook_timeout_ms = value;
            }
            if let Some(value) = toml_string(network, "hook_failure_mode", label)? {
                let mode = HookFailureMode::from_config_str(&value).ok_or_else(|| {
                    Error::InvalidArgument(format!(
                        "{label} [network] hook_failure_mode: expected fail_open or fail_closed, got {value:?}"
                    ))
                })?;
                self.hook_failure_mode = mode;
                self.inner.hook_failure_mode = mode;
            }
            if let Some(value) = toml_bool(network, "clear_ip_transports", label)? {
                self.clear_ip_transports = value;
                self.inner.clear_ip_transports = value;
            }
            if let Some(value) = toml_bool(network, "clear_relay_transports", label)? {
                self.clear_relay_transports = value;
                self.inner.clear_relay_transports = value;
            }
            if let Some(value) = toml_string(network, "portmapper_config", label)? {
                self.portmapper_config = Some(value.clone());
                self.inner.portmapper_config = Some(value);
            }
            if let Some(value) = toml_string(network, "proxy_url", label)? {
                self.proxy_url = Some(value.clone());
                self.inner.proxy_url = Some(value);
            }
            if let Some(value) = toml_bool(network, "proxy_from_env", label)? {
                self.proxy_from_env = value;
                self.inner.proxy_from_env = value;
            }

            self.transport_max_concurrent_bidi_streams =
                toml_u64(network, "transport_max_concurrent_bidi_streams", label)?
                    .or(self.transport_max_concurrent_bidi_streams);
            self.inner.transport_max_concurrent_bidi_streams =
                self.transport_max_concurrent_bidi_streams;

            self.transport_max_concurrent_uni_streams =
                toml_u64(network, "transport_max_concurrent_uni_streams", label)?
                    .or(self.transport_max_concurrent_uni_streams);
            self.inner.transport_max_concurrent_uni_streams =
                self.transport_max_concurrent_uni_streams;

            self.transport_stream_receive_window =
                toml_u64(network, "transport_stream_receive_window", label)?
                    .or(self.transport_stream_receive_window);
            self.inner.transport_stream_receive_window = self.transport_stream_receive_window;

            self.transport_receive_window = toml_u64(network, "transport_receive_window", label)?
                .or(self.transport_receive_window);
            self.inner.transport_receive_window = self.transport_receive_window;

            self.transport_send_window =
                toml_u64(network, "transport_send_window", label)?.or(self.transport_send_window);
            self.inner.transport_send_window = self.transport_send_window;

            self.transport_max_idle_timeout_ms =
                toml_u64(network, "transport_max_idle_timeout_ms", label)?
                    .or(self.transport_max_idle_timeout_ms);
            self.inner.transport_max_idle_timeout_ms = self.transport_max_idle_timeout_ms;

            self.transport_keep_alive_interval_ms =
                toml_u64(network, "transport_keep_alive_interval_ms", label)?
                    .or(self.transport_keep_alive_interval_ms);
            self.inner.transport_keep_alive_interval_ms = self.transport_keep_alive_interval_ms;

            self.transport_initial_mtu =
                toml_u16(network, "transport_initial_mtu", label)?.or(self.transport_initial_mtu);
            self.inner.transport_initial_mtu = self.transport_initial_mtu;

            self.transport_datagram_receive_buffer_size =
                toml_usize(network, "transport_datagram_receive_buffer_size", label)?
                    .or(self.transport_datagram_receive_buffer_size);
            self.inner.transport_datagram_receive_buffer_size =
                self.transport_datagram_receive_buffer_size;

            self.transport_datagram_send_buffer_size =
                toml_usize(network, "transport_datagram_send_buffer_size", label)?
                    .or(self.transport_datagram_send_buffer_size);
            self.inner.transport_datagram_send_buffer_size =
                self.transport_datagram_send_buffer_size;

            self.transport_send_fairness = toml_bool(network, "transport_send_fairness", label)?
                .or(self.transport_send_fairness);
            self.inner.transport_send_fairness = self.transport_send_fairness;

            self.transport_enable_segmentation_offload =
                toml_bool(network, "transport_enable_segmentation_offload", label)?
                    .or(self.transport_enable_segmentation_offload);
            self.inner.transport_enable_segmentation_offload =
                self.transport_enable_segmentation_offload;
        }

        if let Some(logging) = toml_section(raw, "logging", label)? {
            if let Some(value) = toml_string(logging, "format", label)? {
                self.log_format = value.to_ascii_lowercase();
            }
            if let Some(value) = toml_string(logging, "level", label)? {
                self.log_level = value.to_ascii_lowercase();
            }
            if let Some(value) = toml_bool(logging, "mask", label)? {
                self.log_mask = value;
            }
        }

        if let Some(identity) = toml_section(raw, "identity", label)? {
            if let Some(value) = toml_path(identity, "file", label)? {
                self.identity_file = Some(value);
            }
        }

        Ok(())
    }

    /// Apply `ASTER_*` environment variables over the current configuration.
    pub fn apply_env(&mut self) -> Result<()> {
        if let Some(value) = env_path("ASTER_ROOT_PUBKEY_FILE") {
            self.root_pubkey_file = Some(value);
        }
        if let Some(value) = env_path("ASTER_ENROLLMENT_CREDENTIAL") {
            self.enrollment_credential_file = Some(value);
        }
        if let Some(value) = env_string("ASTER_ENROLLMENT_CREDENTIAL_IID") {
            self.enrollment_credential_iid = Some(value);
        }
        if let Some(value) = env_string("ASTER_ENDPOINT_ADDR") {
            self.endpoint_addr = Some(value);
        }
        if let Some(value) = env_path("ASTER_IDENTITY_FILE") {
            self.identity_file = Some(value);
        }

        if let Some(value) = env_string("ASTER_ROOT_PUBKEY") {
            self.root_pubkey = Some(parse_public_key("ASTER_ROOT_PUBKEY", &value)?);
        }
        if let Some(value) = env_bool("ASTER_ALLOW_ALL_CONSUMERS")? {
            self.allow_all_consumers = value;
        }
        if let Some(value) = env_bool("ASTER_ALLOW_ALL_PRODUCERS")? {
            self.allow_all_producers = value;
        }

        if let Some(value) = env_path("ASTER_STORAGE_PATH") {
            self.set_persistent(value);
        }
        if let Some(value) = env_string_allow_empty("ASTER_SECRET_KEY") {
            if value.trim().is_empty() {
                self.set_secret_key(None);
            } else {
                self.set_secret_key(Some(parse_secret_key("ASTER_SECRET_KEY", &value)?));
            }
        }
        if let Some(value) = env_string("ASTER_RELAY_MODE") {
            self.set_relay_mode(parse_relay_mode(&value));
        }
        if let Some(value) = env_string("ASTER_BIND_ADDR") {
            self.bind_addr = Some(value.clone());
            self.inner.bind_addr = Some(value);
        }
        if let Some(value) = env_bool("ASTER_LOCAL_DISCOVERY")? {
            self.local_discovery = value;
            self.inner.enable_discovery = value;
        }
        if let Some(value) = env_bool("ASTER_ENABLE_MONITORING")? {
            self.enable_monitoring = value;
            self.inner.enable_monitoring = value;
        }
        if let Some(value) = env_bool("ASTER_ENABLE_HOOKS")? {
            self.enable_hooks = value;
            self.inner.enable_hooks = value;
        }
        if let Some(value) = env_u64("ASTER_HOOK_TIMEOUT_MS")? {
            self.hook_timeout_ms = value;
            self.inner.hook_timeout_ms = value;
        }
        if let Some(value) = env_string("ASTER_HOOK_FAILURE_MODE") {
            let mode = HookFailureMode::from_config_str(&value).ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "ASTER_HOOK_FAILURE_MODE: expected fail_open or fail_closed, got {value:?}"
                ))
            })?;
            self.hook_failure_mode = mode;
            self.inner.hook_failure_mode = mode;
        }
        if let Some(value) = env_bool("ASTER_CLEAR_IP_TRANSPORTS")? {
            self.clear_ip_transports = value;
            self.inner.clear_ip_transports = value;
        }
        if let Some(value) = env_bool("ASTER_CLEAR_RELAY_TRANSPORTS")? {
            self.clear_relay_transports = value;
            self.inner.clear_relay_transports = value;
        }
        if let Some(value) = env_string("ASTER_PORTMAPPER_CONFIG") {
            self.portmapper_config = Some(value.clone());
            self.inner.portmapper_config = Some(value);
        }
        if let Some(value) = env_string("ASTER_PROXY_URL") {
            self.proxy_url = Some(value.clone());
            self.inner.proxy_url = Some(value);
        }
        if let Some(value) = env_bool("ASTER_PROXY_FROM_ENV")? {
            self.proxy_from_env = value;
            self.inner.proxy_from_env = value;
        }

        self.transport_max_concurrent_bidi_streams =
            env_u64("ASTER_TRANSPORT_MAX_CONCURRENT_BIDI_STREAMS")?
                .or(self.transport_max_concurrent_bidi_streams);
        self.inner.transport_max_concurrent_bidi_streams =
            self.transport_max_concurrent_bidi_streams;

        self.transport_max_concurrent_uni_streams =
            env_u64("ASTER_TRANSPORT_MAX_CONCURRENT_UNI_STREAMS")?
                .or(self.transport_max_concurrent_uni_streams);
        self.inner.transport_max_concurrent_uni_streams = self.transport_max_concurrent_uni_streams;

        self.transport_stream_receive_window = env_u64("ASTER_TRANSPORT_STREAM_RECEIVE_WINDOW")?
            .or(self.transport_stream_receive_window);
        self.inner.transport_stream_receive_window = self.transport_stream_receive_window;

        self.transport_receive_window =
            env_u64("ASTER_TRANSPORT_RECEIVE_WINDOW")?.or(self.transport_receive_window);
        self.inner.transport_receive_window = self.transport_receive_window;

        self.transport_send_window =
            env_u64("ASTER_TRANSPORT_SEND_WINDOW")?.or(self.transport_send_window);
        self.inner.transport_send_window = self.transport_send_window;

        self.transport_max_idle_timeout_ms =
            env_u64("ASTER_TRANSPORT_MAX_IDLE_TIMEOUT_MS")?.or(self.transport_max_idle_timeout_ms);
        self.inner.transport_max_idle_timeout_ms = self.transport_max_idle_timeout_ms;

        self.transport_keep_alive_interval_ms = env_u64("ASTER_TRANSPORT_KEEP_ALIVE_INTERVAL_MS")?
            .or(self.transport_keep_alive_interval_ms);
        self.inner.transport_keep_alive_interval_ms = self.transport_keep_alive_interval_ms;

        self.transport_initial_mtu =
            env_u16("ASTER_TRANSPORT_INITIAL_MTU")?.or(self.transport_initial_mtu);
        self.inner.transport_initial_mtu = self.transport_initial_mtu;

        self.transport_datagram_receive_buffer_size =
            env_usize("ASTER_TRANSPORT_DATAGRAM_RECEIVE_BUFFER_SIZE")?
                .or(self.transport_datagram_receive_buffer_size);
        self.inner.transport_datagram_receive_buffer_size =
            self.transport_datagram_receive_buffer_size;

        self.transport_datagram_send_buffer_size =
            env_usize("ASTER_TRANSPORT_DATAGRAM_SEND_BUFFER_SIZE")?
                .or(self.transport_datagram_send_buffer_size);
        self.inner.transport_datagram_send_buffer_size = self.transport_datagram_send_buffer_size;

        self.transport_send_fairness =
            env_bool("ASTER_TRANSPORT_SEND_FAIRNESS")?.or(self.transport_send_fairness);
        self.inner.transport_send_fairness = self.transport_send_fairness;

        self.transport_enable_segmentation_offload =
            env_bool("ASTER_TRANSPORT_ENABLE_SEGMENTATION_OFFLOAD")?
                .or(self.transport_enable_segmentation_offload);
        self.inner.transport_enable_segmentation_offload =
            self.transport_enable_segmentation_offload;

        if let Some(value) = env_string("ASTER_LOG_FORMAT") {
            self.log_format = value.to_ascii_lowercase();
        }
        if let Some(value) = env_string("ASTER_LOG_LEVEL") {
            self.log_level = value.to_ascii_lowercase();
        }
        if let Some(value) = env_bool("ASTER_LOG_MASK")? {
            self.log_mask = value;
        }

        Ok(())
    }

    /// Resolve the root public key from `root_pubkey` or `root_pubkey_file`.
    ///
    /// Missing files return `Ok(None)`. Malformed files return
    /// [`Error::InvalidArgument`].
    pub fn resolve_root_pubkey(&mut self) -> Result<Option<PublicKey>> {
        if let Some(key) = self.root_pubkey {
            return Ok(Some(key));
        }

        let Some(path) = self.root_pubkey_file.as_ref() else {
            return Ok(None);
        };
        let path = expand_tilde(path);
        if !path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&path)
            .map_err(|e| Error::InvalidArgument(format!("{}: {e}", path.to_string_lossy())))?;
        let key = parse_pubkey_file(&path, &content)?;
        self.root_pubkey = Some(key);
        Ok(Some(key))
    }

    /// Resolve the `.aster-identity` path.
    ///
    /// Uses [`identity_file`](Self::identity_file) when set, otherwise checks
    /// for `.aster-identity` in the current working directory. Returns `None`
    /// when the selected/default file does not exist.
    pub fn resolve_identity_path(&self) -> Option<PathBuf> {
        if let Some(path) = self.identity_file.as_ref() {
            let path = expand_tilde(path);
            return path.exists().then_some(path);
        }

        let path = env::current_dir().ok()?.join(".aster-identity");
        path.exists().then_some(path)
    }

    /// Load `[node].secret_key` and a selected peer from `.aster-identity`.
    ///
    /// Selection matches the high-level bindings: `peer_name` wins; otherwise
    /// `role` selects the first matching peer; otherwise the first peer is used.
    /// Returns `Ok(None)` when no identity file is configured or auto-detected.
    pub fn load_identity(
        &self,
        peer_name: Option<&str>,
        role: Option<&str>,
    ) -> Result<Option<LoadedIdentity>> {
        let Some(path) = self.resolve_identity_path() else {
            return Ok(None);
        };
        self.load_identity_from_path(path, peer_name, role)
            .map(Some)
    }

    /// Load identity data from an explicit TOML path.
    pub fn load_identity_from_path(
        &self,
        path: impl AsRef<Path>,
        peer_name: Option<&str>,
        role: Option<&str>,
    ) -> Result<LoadedIdentity> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .map_err(|e| Error::InvalidArgument(format!("{}: {e}", path.to_string_lossy())))?;
        let raw: toml::Value = content.parse().map_err(|e| {
            Error::InvalidArgument(format!("{}: invalid TOML: {e}", path.to_string_lossy()))
        })?;
        parse_identity_value(path, &raw, peer_name, role)
    }

    /// Whether this configuration selects a persistent node.
    pub fn is_persistent(&self) -> bool {
        self.data_dir.is_some()
    }

    fn set_relay_mode(&mut self, mode: RelayMode) {
        match &mode {
            RelayMode::Default => {
                self.inner.relay_mode = Some("default".into());
                self.inner.relay_urls = Vec::new();
            }
            RelayMode::Custom(urls) => {
                self.inner.relay_mode = Some("custom".into());
                self.inner.relay_urls = urls.clone();
            }
            RelayMode::Staging => {
                self.inner.relay_mode = Some("staging".into());
                self.inner.relay_urls = Vec::new();
            }
            RelayMode::Disabled => {
                self.inner.relay_mode = Some("disabled".into());
                self.inner.relay_urls = Vec::new();
            }
        }
        self.relay_mode = mode;
    }

    fn set_persistent(&mut self, dir: PathBuf) {
        self.inner.data_dir = Some(dir.to_string_lossy().into_owned());
        self.storage_path = Some(dir.clone());
        self.data_dir = Some(dir);
    }

    fn set_secret_key(&mut self, key: Option<SecretKey>) {
        self.inner.secret_key = key.as_ref().map(|key| key.to_bytes().to_vec());
        self.secret_key = key;
    }
}

impl Default for AsterConfig {
    fn default() -> Self {
        Self {
            root_pubkey: None,
            root_pubkey_file: None,
            enrollment_credential_file: None,
            enrollment_credential_iid: None,
            allow_all_consumers: false,
            allow_all_producers: true,
            endpoint_addr: None,
            storage_path: None,
            gc_interval: None,
            secret_key: None,
            relay_mode: RelayMode::Default,
            bind_addr: None,
            local_discovery: false,
            enable_monitoring: false,
            enable_hooks: false,
            hook_timeout_ms: 5000,
            hook_failure_mode: HookFailureMode::FailOpen,
            clear_ip_transports: false,
            clear_relay_transports: false,
            portmapper_config: None,
            proxy_url: None,
            proxy_from_env: false,
            transport_max_concurrent_bidi_streams: None,
            transport_max_concurrent_uni_streams: None,
            transport_stream_receive_window: None,
            transport_receive_window: None,
            transport_send_window: None,
            transport_max_idle_timeout_ms: None,
            transport_keep_alive_interval_ms: None,
            transport_initial_mtu: None,
            transport_datagram_receive_buffer_size: None,
            transport_datagram_send_buffer_size: None,
            transport_send_fairness: None,
            transport_enable_segmentation_offload: None,
            log_format: "text".into(),
            log_level: "info".into(),
            log_mask: true,
            identity_file: None,
            inner: CoreEndpointConfig::default(),
            data_dir: None,
        }
    }
}

/// Fluent builder for [`AsterConfig`].
#[derive(Clone, Debug)]
pub struct AsterConfigBuilder {
    config: AsterConfig,
}

impl AsterConfigBuilder {
    /// Load `ASTER_*` environment variables into the builder.
    ///
    /// Subsequent builder assignments override these values.
    pub fn load_env(mut self) -> Result<Self> {
        self.config.apply_env()?;
        Ok(self)
    }

    /// Load an `aster.toml` file and then overlay `ASTER_*` environment
    /// variables.
    ///
    /// Subsequent builder assignments override both file and env values, giving
    /// the complete resolution order:
    /// defaults < TOML < env < builder assignments.
    pub fn load_file(mut self, path: impl AsRef<Path>) -> Result<Self> {
        self.config.apply_file(path)?;
        self.config.apply_env()?;
        Ok(self)
    }

    /// Select the relay mode (default: [`RelayMode::Default`]).
    pub fn relay(mut self, mode: RelayMode) -> Self {
        self.config.set_relay_mode(mode);
        self
    }

    /// Make the node persistent, storing all state under `dir`.
    pub fn persistent(mut self, dir: impl Into<PathBuf>) -> Self {
        self.config.set_persistent(dir.into());
        self
    }

    /// Enable periodic blob garbage collection at `interval`.
    ///
    /// Off by default: without this, the blob store is grow-only and only tags
    /// pin retention. With it set, untagged blobs are reclaimed every
    /// `interval`. See [`AsterConfig::gc_interval`] for full semantics and
    /// [`Blobs::gc_run_once`](crate::Blobs::gc_run_once) for a deterministic
    /// manual sweep.
    pub fn gc_interval(mut self, interval: Duration) -> Self {
        self.config.gc_interval = Some(interval);
        self
    }

    /// Pin the node's identity to a specific secret key. On a persistent node
    /// combined with the same `data_dir`, this gives a restart-stable
    /// [`NodeId`](crate::NodeId).
    pub fn secret_key(mut self, key: SecretKey) -> Self {
        self.config.set_secret_key(Some(key));
        self
    }

    /// Bind the endpoint to a specific socket address, e.g. `"0.0.0.0:9000"`.
    pub fn bind_addr(mut self, addr: impl Into<String>) -> Self {
        let addr = addr.into();
        self.config.bind_addr = Some(addr.clone());
        self.config.inner.bind_addr = Some(addr);
        self
    }

    /// Enable local-network peer discovery (mDNS).
    pub fn discovery(mut self, enabled: bool) -> Self {
        self.config.local_discovery = enabled;
        self.config.inner.enable_discovery = enabled;
        self
    }

    /// Enable connection monitoring / remote-info tracking.
    pub fn monitoring(mut self, enabled: bool) -> Self {
        self.config.enable_monitoring = enabled;
        self.config.inner.enable_monitoring = enabled;
        self
    }

    /// Enable admission hooks. Required for [`Node::take_admission`](crate::Node::take_admission)
    /// to return a handle.
    pub fn hooks(mut self, enabled: bool) -> Self {
        self.config.enable_hooks = enabled;
        self.config.inner.enable_hooks = enabled;
        self
    }

    /// Timeout (ms) for an admission decision before the configured hook
    /// failure behavior is applied. Only meaningful with [`hooks`](Self::hooks).
    pub fn hook_timeout_ms(mut self, ms: u64) -> Self {
        self.config.hook_timeout_ms = ms;
        self.config.inner.hook_timeout_ms = ms;
        self
    }

    /// Configure post-handshake hook fallback behavior for timeouts or closed
    /// hook channels.
    ///
    /// The default is [`HookFailureMode::FailOpen`] for compatibility with
    /// observability-only hooks. Protected admission flows should use
    /// [`HookFailureMode::FailClosed`].
    pub fn hook_failure_mode(mut self, mode: HookFailureMode) -> Self {
        self.config.hook_failure_mode = mode;
        self.config.inner.hook_failure_mode = mode;
        self
    }

    /// Finish building.
    pub fn build(self) -> AsterConfig {
        self.config
    }
}

fn env_string(key: &str) -> Option<String> {
    env_string_allow_empty(key).and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

fn env_string_allow_empty(key: &str) -> Option<String> {
    env::var(key).ok()
}

fn env_path(key: &str) -> Option<PathBuf> {
    env_string(key).map(PathBuf::from)
}

fn env_bool(key: &str) -> Result<Option<bool>> {
    let Some(value) = env_string(key) else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(Some(true)),
        "false" | "0" | "no" | "off" => Ok(Some(false)),
        _ => Err(Error::InvalidArgument(format!(
            "{key}: expected true/false, 1/0, yes/no, or on/off, got {value:?}"
        ))),
    }
}

fn env_u64(key: &str) -> Result<Option<u64>> {
    parse_optional_env(key)
}

fn env_u16(key: &str) -> Result<Option<u16>> {
    parse_optional_env(key)
}

fn env_usize(key: &str) -> Result<Option<usize>> {
    parse_optional_env(key)
}

fn parse_optional_env<T>(key: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let Some(value) = env_string(key) else {
        return Ok(None);
    };
    value
        .parse::<T>()
        .map(Some)
        .map_err(|e| Error::InvalidArgument(format!("{key}: invalid value {value:?}: {e}")))
}

fn parse_public_key(source: &str, value: &str) -> Result<PublicKey> {
    PublicKey::from_hex(value.trim()).map_err(|e| Error::InvalidArgument(format!("{source}: {e}")))
}

fn parse_secret_key(source: &str, value: &str) -> Result<SecretKey> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|e| Error::InvalidArgument(format!("{source}: invalid base64: {e}")))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| Error::InvalidArgument(format!("{source}: secret key must be 32 bytes")))?;
    Ok(SecretKey::from_bytes(bytes))
}

fn parse_relay_mode(value: &str) -> RelayMode {
    let value = value.trim();
    match value.to_ascii_lowercase().as_str() {
        "default" => RelayMode::Default,
        "disabled" => RelayMode::Disabled,
        "staging" => RelayMode::Staging,
        "custom" => RelayMode::Custom(Vec::new()),
        _ => RelayMode::Custom(vec![value.to_string()]),
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

fn parse_pubkey_file(path: &Path, content: &str) -> Result<PublicKey> {
    let content = content.trim();
    if content.starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(content).map_err(|e| {
            Error::InvalidArgument(format!("{}: invalid JSON: {e}", path.to_string_lossy()))
        })?;
        let Some(public_key) = value.get("public_key").and_then(|v| v.as_str()) else {
            return Err(Error::InvalidArgument(format!(
                "{}: JSON root pubkey file must contain a string public_key field",
                path.to_string_lossy()
            )));
        };
        return parse_public_key(&path.to_string_lossy(), public_key);
    }

    parse_public_key(&path.to_string_lossy(), content)
}

fn toml_section<'a>(
    raw: &'a toml::Value,
    section: &str,
    label: &str,
) -> Result<Option<&'a toml::Table>> {
    match raw.get(section) {
        None => Ok(None),
        Some(toml::Value::Table(table)) => Ok(Some(table)),
        Some(_) => Err(Error::InvalidArgument(format!(
            "{label} [{section}]: expected table"
        ))),
    }
}

fn toml_string(table: &toml::Table, key: &str, label: &str) -> Result<Option<String>> {
    toml_string_allow_empty(table, key, label).map(|value| {
        value.and_then(|value| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        })
    })
}

fn toml_string_allow_empty(table: &toml::Table, key: &str, label: &str) -> Result<Option<String>> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(Error::InvalidArgument(format!(
            "{label}: {key} must be a string"
        ))),
    }
}

fn toml_path(table: &toml::Table, key: &str, label: &str) -> Result<Option<PathBuf>> {
    Ok(toml_string(table, key, label)?.map(PathBuf::from))
}

fn toml_bool(table: &toml::Table, key: &str, label: &str) -> Result<Option<bool>> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Boolean(value)) => Ok(Some(*value)),
        Some(_) => Err(Error::InvalidArgument(format!(
            "{label}: {key} must be a boolean"
        ))),
    }
}

fn toml_u64(table: &toml::Table, key: &str, label: &str) -> Result<Option<u64>> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Integer(value)) if *value >= 0 => Ok(Some(*value as u64)),
        Some(toml::Value::Integer(_)) => Err(Error::InvalidArgument(format!(
            "{label}: {key} must be non-negative"
        ))),
        Some(_) => Err(Error::InvalidArgument(format!(
            "{label}: {key} must be an integer"
        ))),
    }
}

fn toml_u16(table: &toml::Table, key: &str, label: &str) -> Result<Option<u16>> {
    let Some(value) = toml_u64(table, key, label)? else {
        return Ok(None);
    };
    value
        .try_into()
        .map(Some)
        .map_err(|_| Error::InvalidArgument(format!("{label}: {key} must fit in u16")))
}

fn toml_usize(table: &toml::Table, key: &str, label: &str) -> Result<Option<usize>> {
    let Some(value) = toml_u64(table, key, label)? else {
        return Ok(None);
    };
    value
        .try_into()
        .map(Some)
        .map_err(|_| Error::InvalidArgument(format!("{label}: {key} must fit in usize")))
}

fn parse_identity_value(
    path: &Path,
    raw: &toml::Value,
    peer_name: Option<&str>,
    role: Option<&str>,
) -> Result<LoadedIdentity> {
    let label = path.to_string_lossy();

    let secret_key = match raw.get("node") {
        None => None,
        Some(toml::Value::Table(node)) => {
            let secret = toml_string(node, "secret_key", &label)?;
            match secret {
                Some(secret) => Some(parse_secret_key(
                    &format!("{label} [node] secret_key"),
                    &secret,
                )?),
                None => None,
            }
        }
        Some(_) => {
            return Err(Error::InvalidArgument(format!(
                "{label} [node]: expected table"
            )))
        }
    };

    let top_published = parse_published_services(raw.get("published_services"), &label)?;
    let peers: &[toml::Value] = match raw.get("peers") {
        None => &[],
        Some(toml::Value::Array(peers)) => peers.as_slice(),
        Some(_) => {
            return Err(Error::InvalidArgument(format!(
                "{label} [[peers]]: expected array of tables"
            )))
        }
    };

    let mut selected: Option<&toml::Table> = None;
    for peer in peers {
        let table = peer
            .as_table()
            .ok_or_else(|| Error::InvalidArgument(format!("{label} [[peers]]: expected table")))?;

        let name_matches = peer_name
            .zip(table.get("name").and_then(|value| value.as_str()))
            .is_some_and(|(expected, actual)| expected == actual);
        let role_matches = role
            .zip(table.get("role").and_then(|value| value.as_str()))
            .is_some_and(|(expected, actual)| expected == actual);

        if peer_name.is_some() {
            if name_matches {
                selected = Some(table);
                break;
            }
        } else if role.is_some() {
            if role_matches {
                selected = Some(table);
                break;
            }
        } else {
            selected = Some(table);
            break;
        }
    }

    let peer = selected
        .map(|table| parse_identity_peer(&label, table, &top_published))
        .transpose()?;

    Ok(LoadedIdentity {
        path: path.to_path_buf(),
        secret_key,
        peer,
    })
}

fn parse_identity_peer(
    label: &str,
    table: &toml::Table,
    top_published: &PublishedServices,
) -> Result<AsterIdentityPeer> {
    let name = toml_string(table, "name", label)?;
    let role = toml_string(table, "role", label)?;
    let root_pubkey = toml_string(table, "root_pubkey", label)?
        .map(|value| parse_public_key(&format!("{label} [[peers]] root_pubkey"), &value))
        .transpose()?;
    let credential_type = toml_string(table, "type", label)?;
    let expires_at = match table.get("expires_at") {
        None => None,
        Some(toml::Value::Integer(value)) => Some(*value),
        Some(_) => {
            return Err(Error::InvalidArgument(format!(
                "{label} [[peers]] expires_at must be an integer"
            )))
        }
    };
    let endpoint_id = toml_string(table, "endpoint_id", label)?;
    let signature = toml_string(table, "signature", label)?;

    let mut attributes = parse_attributes(table.get("attributes"), label)?;
    if let Some(role) = role.as_ref() {
        attributes
            .entry("aster.role".to_string())
            .or_insert_with(|| role.clone());
    }
    if let Some(name) = name.as_ref() {
        attributes
            .entry("aster.name".to_string())
            .or_insert_with(|| name.clone());
    }

    let mut published_services = top_published.clone();
    let peer_published = parse_published_services(table.get("published_services"), label)?;
    for (service, metadata) in peer_published {
        published_services.insert(service, metadata);
    }

    Ok(AsterIdentityPeer {
        name,
        role,
        root_pubkey,
        credential_type,
        expires_at,
        endpoint_id,
        signature,
        attributes,
        published_services,
    })
}

fn parse_attributes(value: Option<&toml::Value>, label: &str) -> Result<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let table = value
        .as_table()
        .ok_or_else(|| Error::InvalidArgument(format!("{label} attributes: expected table")))?;
    let mut out = BTreeMap::new();
    for (key, value) in table {
        let value = match value {
            toml::Value::String(value) => value.clone(),
            toml::Value::Integer(value) => value.to_string(),
            toml::Value::Float(value) => value.to_string(),
            toml::Value::Boolean(value) => value.to_string(),
            toml::Value::Datetime(value) => value.to_string(),
            toml::Value::Array(_) | toml::Value::Table(_) => {
                return Err(Error::InvalidArgument(format!(
                    "{label} attributes.{key}: expected scalar"
                )))
            }
        };
        out.insert(key.clone(), value);
    }
    Ok(out)
}

fn parse_published_services(value: Option<&toml::Value>, label: &str) -> Result<PublishedServices> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let table = value.as_table().ok_or_else(|| {
        Error::InvalidArgument(format!("{label} published_services: expected table"))
    })?;
    let mut out = BTreeMap::new();
    for (service, metadata) in table {
        let metadata = metadata.as_table().ok_or_else(|| {
            Error::InvalidArgument(format!(
                "{label} published_services.{service}: expected table"
            ))
        })?;
        let mut service_out = BTreeMap::new();
        for (key, value) in metadata {
            let json_value = serde_json::to_value(value.clone()).map_err(|e| {
                Error::InvalidArgument(format!("{label} published_services.{service}.{key}: {e}"))
            })?;
            service_out.insert(key.clone(), json_value);
        }
        out.insert(service.clone(), service_out);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};

    const ASTER_ENV_KEYS: &[&str] = &[
        "ASTER_ROOT_PUBKEY",
        "ASTER_ROOT_PUBKEY_FILE",
        "ASTER_ENROLLMENT_CREDENTIAL",
        "ASTER_ENROLLMENT_CREDENTIAL_IID",
        "ASTER_ENDPOINT_ADDR",
        "ASTER_IDENTITY_FILE",
        "ASTER_ALLOW_ALL_CONSUMERS",
        "ASTER_ALLOW_ALL_PRODUCERS",
        "ASTER_STORAGE_PATH",
        "ASTER_SECRET_KEY",
        "ASTER_RELAY_MODE",
        "ASTER_BIND_ADDR",
        "ASTER_LOCAL_DISCOVERY",
        "ASTER_ENABLE_MONITORING",
        "ASTER_ENABLE_HOOKS",
        "ASTER_HOOK_TIMEOUT_MS",
        "ASTER_HOOK_FAILURE_MODE",
        "ASTER_CLEAR_IP_TRANSPORTS",
        "ASTER_CLEAR_RELAY_TRANSPORTS",
        "ASTER_PORTMAPPER_CONFIG",
        "ASTER_PROXY_URL",
        "ASTER_PROXY_FROM_ENV",
        "ASTER_TRANSPORT_MAX_CONCURRENT_BIDI_STREAMS",
        "ASTER_TRANSPORT_MAX_CONCURRENT_UNI_STREAMS",
        "ASTER_TRANSPORT_STREAM_RECEIVE_WINDOW",
        "ASTER_TRANSPORT_RECEIVE_WINDOW",
        "ASTER_TRANSPORT_SEND_WINDOW",
        "ASTER_TRANSPORT_MAX_IDLE_TIMEOUT_MS",
        "ASTER_TRANSPORT_KEEP_ALIVE_INTERVAL_MS",
        "ASTER_TRANSPORT_INITIAL_MTU",
        "ASTER_TRANSPORT_DATAGRAM_RECEIVE_BUFFER_SIZE",
        "ASTER_TRANSPORT_DATAGRAM_SEND_BUFFER_SIZE",
        "ASTER_TRANSPORT_SEND_FAIRNESS",
        "ASTER_TRANSPORT_ENABLE_SEGMENTATION_OFFLOAD",
        "ASTER_LOG_FORMAT",
        "ASTER_LOG_LEVEL",
        "ASTER_LOG_MASK",
    ];

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let saved = ASTER_ENV_KEYS
                .iter()
                .map(|key| (*key, env::var_os(key)))
                .collect();
            for key in ASTER_ENV_KEYS {
                env::remove_var(key);
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(value) => env::set_var(key, value),
                    None => env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn from_env_reads_trust_and_endpoint_config() {
        let _lock = env_lock().lock().unwrap();
        let _env = EnvGuard::new();

        let root = [1u8; 32];
        let secret = [2u8; 32];
        env::set_var("ASTER_ROOT_PUBKEY", hex::encode(root));
        env::set_var("ASTER_ROOT_PUBKEY_FILE", "/tmp/root.pub");
        env::set_var("ASTER_ENROLLMENT_CREDENTIAL", "/tmp/node.cred");
        env::set_var("ASTER_ENROLLMENT_CREDENTIAL_IID", "iid-token");
        env::set_var("ASTER_ENDPOINT_ADDR", "aster1ticket");
        env::set_var("ASTER_IDENTITY_FILE", "/tmp/.aster-identity");
        env::set_var("ASTER_ALLOW_ALL_CONSUMERS", "yes");
        env::set_var("ASTER_ALLOW_ALL_PRODUCERS", "0");
        env::set_var("ASTER_STORAGE_PATH", "/tmp/aster-data");
        env::set_var(
            "ASTER_SECRET_KEY",
            base64::engine::general_purpose::STANDARD.encode(secret),
        );
        env::set_var("ASTER_RELAY_MODE", "disabled");
        env::set_var("ASTER_BIND_ADDR", "127.0.0.1:0");
        env::set_var("ASTER_LOCAL_DISCOVERY", "on");
        env::set_var("ASTER_ENABLE_MONITORING", "true");
        env::set_var("ASTER_ENABLE_HOOKS", "true");
        env::set_var("ASTER_HOOK_TIMEOUT_MS", "1234");
        env::set_var("ASTER_HOOK_FAILURE_MODE", "fail-closed");
        env::set_var("ASTER_CLEAR_IP_TRANSPORTS", "true");
        env::set_var("ASTER_CLEAR_RELAY_TRANSPORTS", "false");
        env::set_var("ASTER_PORTMAPPER_CONFIG", "disabled");
        env::set_var("ASTER_PROXY_URL", "http://localhost:8080");
        env::set_var("ASTER_PROXY_FROM_ENV", "true");
        env::set_var("ASTER_TRANSPORT_MAX_CONCURRENT_BIDI_STREAMS", "512");
        env::set_var("ASTER_TRANSPORT_MAX_CONCURRENT_UNI_STREAMS", "128");
        env::set_var("ASTER_TRANSPORT_STREAM_RECEIVE_WINDOW", "2000000");
        env::set_var("ASTER_TRANSPORT_RECEIVE_WINDOW", "8000000");
        env::set_var("ASTER_TRANSPORT_SEND_WINDOW", "9000000");
        env::set_var("ASTER_TRANSPORT_MAX_IDLE_TIMEOUT_MS", "60000");
        env::set_var("ASTER_TRANSPORT_KEEP_ALIVE_INTERVAL_MS", "10000");
        env::set_var("ASTER_TRANSPORT_INITIAL_MTU", "1452");
        env::set_var("ASTER_TRANSPORT_DATAGRAM_RECEIVE_BUFFER_SIZE", "1000000");
        env::set_var("ASTER_TRANSPORT_DATAGRAM_SEND_BUFFER_SIZE", "2000000");
        env::set_var("ASTER_TRANSPORT_SEND_FAIRNESS", "true");
        env::set_var("ASTER_TRANSPORT_ENABLE_SEGMENTATION_OFFLOAD", "false");
        env::set_var("ASTER_LOG_FORMAT", "JSON");
        env::set_var("ASTER_LOG_LEVEL", "DEBUG");
        env::set_var("ASTER_LOG_MASK", "off");

        let cfg = AsterConfig::from_env().unwrap();

        assert_eq!(cfg.root_pubkey.unwrap().to_bytes(), root);
        assert_eq!(
            cfg.root_pubkey_file.as_ref().unwrap(),
            &PathBuf::from("/tmp/root.pub")
        );
        assert_eq!(
            cfg.enrollment_credential_file.as_ref().unwrap(),
            &PathBuf::from("/tmp/node.cred")
        );
        assert_eq!(cfg.enrollment_credential_iid.as_deref(), Some("iid-token"));
        assert!(cfg.allow_all_consumers);
        assert!(!cfg.allow_all_producers);
        assert_eq!(cfg.endpoint_addr.as_deref(), Some("aster1ticket"));
        assert_eq!(
            cfg.identity_file.as_ref().unwrap(),
            &PathBuf::from("/tmp/.aster-identity")
        );
        assert_eq!(
            cfg.storage_path.as_ref().unwrap(),
            &PathBuf::from("/tmp/aster-data")
        );
        assert!(cfg.is_persistent());
        assert_eq!(cfg.secret_key.as_ref().unwrap().to_bytes(), secret);
        assert_eq!(cfg.relay_mode, RelayMode::Disabled);
        assert_eq!(cfg.bind_addr.as_deref(), Some("127.0.0.1:0"));
        assert!(cfg.local_discovery);
        assert!(cfg.enable_monitoring);
        assert!(cfg.enable_hooks);
        assert_eq!(cfg.hook_timeout_ms, 1234);
        assert_eq!(cfg.hook_failure_mode, HookFailureMode::FailClosed);
        assert!(cfg.clear_ip_transports);
        assert!(!cfg.clear_relay_transports);
        assert_eq!(cfg.portmapper_config.as_deref(), Some("disabled"));
        assert_eq!(cfg.proxy_url.as_deref(), Some("http://localhost:8080"));
        assert!(cfg.proxy_from_env);
        assert_eq!(cfg.transport_max_concurrent_bidi_streams, Some(512));
        assert_eq!(cfg.transport_max_concurrent_uni_streams, Some(128));
        assert_eq!(cfg.transport_stream_receive_window, Some(2_000_000));
        assert_eq!(cfg.transport_receive_window, Some(8_000_000));
        assert_eq!(cfg.transport_send_window, Some(9_000_000));
        assert_eq!(cfg.transport_max_idle_timeout_ms, Some(60_000));
        assert_eq!(cfg.transport_keep_alive_interval_ms, Some(10_000));
        assert_eq!(cfg.transport_initial_mtu, Some(1452));
        assert_eq!(cfg.transport_datagram_receive_buffer_size, Some(1_000_000));
        assert_eq!(cfg.transport_datagram_send_buffer_size, Some(2_000_000));
        assert_eq!(cfg.transport_send_fairness, Some(true));
        assert_eq!(cfg.transport_enable_segmentation_offload, Some(false));
        assert_eq!(cfg.log_format, "json");
        assert_eq!(cfg.log_level, "debug");
        assert!(!cfg.log_mask);

        assert_eq!(cfg.inner.relay_mode.as_deref(), Some("disabled"));
        assert_eq!(cfg.inner.secret_key.unwrap(), secret.to_vec());
        assert_eq!(cfg.inner.bind_addr.as_deref(), Some("127.0.0.1:0"));
        assert!(cfg.inner.enable_discovery);
        assert!(cfg.inner.enable_monitoring);
        assert!(cfg.inner.enable_hooks);
        assert_eq!(cfg.inner.transport_receive_window, Some(8_000_000));
        assert_eq!(cfg.inner.transport_send_fairness, Some(true));
    }

    #[test]
    fn from_env_rejects_invalid_values() {
        let _lock = env_lock().lock().unwrap();
        let _env = EnvGuard::new();

        env::set_var("ASTER_ALLOW_ALL_CONSUMERS", "maybe");
        let err = AsterConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("ASTER_ALLOW_ALL_CONSUMERS"));
    }

    #[test]
    fn from_file_env_and_builder_follow_resolution_order() {
        let _lock = env_lock().lock().unwrap();
        let _env = EnvGuard::new();

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("aster.toml");
        let file_root = [5u8; 32];
        let file_secret = [6u8; 32];
        fs::write(
            &config_path,
            format!(
                r#"
[trust]
root_pubkey = "{}"
allow_all_consumers = false
allow_all_producers = false

[connect]
endpoint_addr = "file-endpoint"

[storage]
path = "{}"

[network]
secret_key = "{}"
relay_mode = "disabled"
bind_addr = "127.0.0.1:1111"
enable_hooks = true
transport_receive_window = 12345

[logging]
format = "json"
level = "debug"
mask = false
"#,
                hex::encode(file_root),
                dir.path().join("file-store").display(),
                base64::engine::general_purpose::STANDARD.encode(file_secret)
            ),
        )
        .unwrap();

        env::set_var("ASTER_BIND_ADDR", "127.0.0.1:2222");
        env::set_var("ASTER_RELAY_MODE", "staging");
        env::set_var("ASTER_ALLOW_ALL_CONSUMERS", "true");

        let cfg = AsterConfig::from_file(&config_path).unwrap();
        assert_eq!(cfg.root_pubkey.unwrap().to_bytes(), file_root);
        assert!(cfg.allow_all_consumers); // env beats TOML
        assert!(!cfg.allow_all_producers); // TOML beats default
        assert_eq!(cfg.endpoint_addr.as_deref(), Some("file-endpoint"));
        assert_eq!(cfg.secret_key.as_ref().unwrap().to_bytes(), file_secret);
        assert_eq!(cfg.bind_addr.as_deref(), Some("127.0.0.1:2222"));
        assert_eq!(cfg.relay_mode, RelayMode::Staging);
        assert_eq!(cfg.transport_receive_window, Some(12345));
        assert_eq!(cfg.log_format, "json");
        assert!(!cfg.log_mask);

        let cfg = AsterConfig::builder()
            .load_file(&config_path)
            .unwrap()
            .bind_addr("127.0.0.1:3333")
            .relay(RelayMode::Disabled)
            .build();
        assert_eq!(cfg.bind_addr.as_deref(), Some("127.0.0.1:3333"));
        assert_eq!(cfg.relay_mode, RelayMode::Disabled);
        assert_eq!(cfg.inner.bind_addr.as_deref(), Some("127.0.0.1:3333"));
        assert_eq!(cfg.inner.relay_mode.as_deref(), Some("disabled"));
    }

    #[test]
    fn resolve_root_pubkey_reads_hex_and_json_files() {
        let dir = tempfile::tempdir().unwrap();
        let hex_path = dir.path().join("root.pub");
        let json_path = dir.path().join("root.json");

        let hex_key = [3u8; 32];
        let json_key = [4u8; 32];
        fs::write(&hex_path, hex::encode(hex_key)).unwrap();
        fs::write(
            &json_path,
            format!(r#"{{"public_key":"{}"}}"#, hex::encode(json_key)),
        )
        .unwrap();

        let mut cfg = AsterConfig {
            root_pubkey_file: Some(hex_path),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_root_pubkey().unwrap().unwrap().to_bytes(),
            hex_key
        );

        let mut cfg = AsterConfig {
            root_pubkey_file: Some(json_path),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_root_pubkey().unwrap().unwrap().to_bytes(),
            json_key
        );
    }

    #[test]
    fn load_identity_selects_peer_and_synthesizes_metadata() {
        let _lock = env_lock().lock().unwrap();
        let _env = EnvGuard::new();

        let dir = tempfile::tempdir().unwrap();
        let identity_path = dir.path().join(".aster-identity");
        let secret = [7u8; 32];
        let producer_root = [8u8; 32];
        let consumer_root = [9u8; 32];
        fs::write(
            &identity_path,
            format!(
                r#"
[node]
secret_key = "{}"

[published_services.TaskManager]
producer_token = "tok_123"
contract_id = "abc123"

[[peers]]
name = "billing-producer"
role = "producer"
root_pubkey = "{}"
type = "policy"
expires_at = 1735689599
endpoint_id = "endpoint-producer"
signature = "sig-producer"
attributes = {{ custom = "yes" }}

[[peers]]
name = "analytics-consumer"
role = "consumer"
root_pubkey = "{}"
type = "policy"
expires_at = 1735689600
signature = "sig-consumer"
"#,
                base64::engine::general_purpose::STANDARD.encode(secret),
                hex::encode(producer_root),
                hex::encode(consumer_root)
            ),
        )
        .unwrap();

        let cfg = AsterConfig::default();
        let loaded = cfg
            .load_identity_from_path(&identity_path, None, Some("consumer"))
            .unwrap();
        assert_eq!(loaded.secret_key.unwrap().to_bytes(), secret);
        let peer = loaded.peer.unwrap();
        assert_eq!(peer.name.as_deref(), Some("analytics-consumer"));
        assert_eq!(peer.role.as_deref(), Some("consumer"));
        assert_eq!(peer.root_pubkey.unwrap().to_bytes(), consumer_root);
        assert_eq!(
            peer.attributes.get("aster.role").map(String::as_str),
            Some("consumer")
        );
        assert_eq!(
            peer.attributes.get("aster.name").map(String::as_str),
            Some("analytics-consumer")
        );
        assert_eq!(
            peer.published_services["TaskManager"]["producer_token"],
            serde_json::Value::String("tok_123".into())
        );

        let cfg = AsterConfig {
            identity_file: Some(identity_path),
            ..Default::default()
        };
        let loaded = cfg
            .load_identity(Some("billing-producer"), None)
            .unwrap()
            .unwrap();
        let peer = loaded.peer.unwrap();
        assert_eq!(peer.root_pubkey.unwrap().to_bytes(), producer_root);
        assert_eq!(
            peer.attributes.get("custom").map(String::as_str),
            Some("yes")
        );
        assert_eq!(
            peer.attributes.get("aster.role").map(String::as_str),
            Some("producer")
        );
    }
}
