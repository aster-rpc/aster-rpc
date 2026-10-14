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

mod enrollment;
pub use enrollment::{
    EnrollmentConfig, EnrollmentLimits, RoleMappingConfig, DEFAULT_ROLE_ATTRIBUTE,
};

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
    /// 32-byte Ed25519 root public key for the deployment: the root
    /// Enrollment trusts and coordination pins. Env `ASTER_ROOT_PUBKEY`;
    /// TOML `[trust] root_pubkey`.
    pub root_pubkey: Option<PublicKey>,
    /// Path to a file containing the root public key as hex or JSON
    /// `{ "public_key": "<hex>" }`. Env `ASTER_ROOT_PUBKEY_FILE`; TOML
    /// `[trust] root_pubkey_file`.
    pub root_pubkey_file: Option<PathBuf>,
    /// Pre-rotation commitment to the next root key (hex BLAKE3, as
    /// `aster trust show` prints it). With it, Enrollment accepts a rotation
    /// statement signed by that key; without it the root cannot rotate. Env
    /// `ASTER_NEXT_ROOT_COMMITMENT`; TOML `[trust] next_root_commitment`.
    pub next_root_commitment: Option<String>,
    /// Rotations applied to reach [`root_pubkey`](Self::root_pubkey); 0 for
    /// an original root. Env `ASTER_ROOT_SEQ`; TOML `[trust] root_seq`.
    pub root_seq: u64,
    /// The rotation statements applied to reach the root, oldest first, so
    /// Enrollment relays them to peers on an older root. Env
    /// `ASTER_ROOT_ROTATIONS` (comma-separated); TOML `[trust] root_rotations`.
    pub root_rotations: Vec<String>,
    /// Path to this node's enrollment credential (a delegation chain,
    /// `aster-chain-v1~…`, as `aster enroll node --out` writes it). Setting
    /// it switches enrollment on (see [`EnrollmentConfig`]). Env
    /// `ASTER_ENROLLMENT_CREDENTIAL`; TOML `[trust] enrollment_credential`
    /// (the names every binding already reads).
    pub enrollment_credential_file: Option<PathBuf>,
    /// This node's enrollment credential chain inline; wins over
    /// [`enrollment_credential_file`](Self::enrollment_credential_file). Env
    /// `ASTER_ENROLLMENT_CREDENTIAL_CHAIN`; TOML `[trust]
    /// enrollment_credential_chain`.
    pub enrollment_credential_chain: Option<String>,
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
    /// Env: `ASTER_GC_INTERVAL_MS`; TOML: `[storage] gc_interval_ms`. A value
    /// of `0` disables GC.
    pub gc_interval: Option<Duration>,

    /// Stable node identity key.
    pub secret_key: Option<SecretKey>,
    /// Path to the node's **secret key file** — the one on-disk identity
    /// format: the 32-byte key as 64 hex characters (see
    /// [`SecretKey::read_file`]). Resolved by [`Node::start`](crate::Node::start)
    /// through [`resolve_secret_key_file`](Self::resolve_secret_key_file):
    /// loaded when the file exists, otherwise created (mode `0600`) from
    /// [`secret_key`](Self::secret_key) or a fresh key.
    ///
    /// Env: `ASTER_SECRET_KEY_FILE`; TOML: `[network] secret_key_file`.
    pub secret_key_file: Option<PathBuf>,
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
    /// Disable relay transports (direct-IP-only). Start-up then does not wait
    /// for a home relay, whatever `relay_mode` is set to.
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

    /// Port of the health/readiness/metrics HTTP listener (`aster::ops`,
    /// feature `health`). `0` (the default) means disabled unless
    /// [`health_enabled`](Self::health_enabled) is `Some(true)`, in which case
    /// an ephemeral port is bound. See [`is_health_enabled`](Self::is_health_enabled).
    ///
    /// Env: `ASTER_HEALTH_PORT`; TOML: `[health] port` (the keys the Python/TS
    /// bindings read).
    pub health_port: u16,
    /// Bind host of the health listener. Default `127.0.0.1` (localhost only);
    /// use `0.0.0.0` for Kubernetes pod probes.
    ///
    /// Env: `ASTER_HEALTH_HOST`; TOML: `[health] host`.
    pub health_host: String,
    /// Force the health listener on (`Some(true)`) or off (`Some(false)`).
    /// `None` (the default) enables it exactly when `health_port != 0` — the
    /// Python/TS rule.
    ///
    /// Env: `ASTER_HEALTH_ENABLED`; TOML: `[health] enabled`.
    pub health_enabled: Option<bool>,

    /// Extra ALPNs the node accepts inbound connections on, in addition to the
    /// built-in blobs/docs/gossip protocols. Merged (deduplicated) with the
    /// list passed to [`Node::start_with_alpns`](crate::Node::start_with_alpns).
    /// Not loaded from env/TOML.
    pub alpns: Vec<Vec<u8>>,

    /// Enrollment for [`AsterServer`](crate::rpc::AsterServer) and
    /// `AsterClient`: TOML `[enrollment]`, env `ASTER_ENROLLMENT_*`. Off
    /// unless configured; see [`EnrollmentConfig`].
    pub enrollment: EnrollmentConfig,
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
    /// `ASTER_ENROLLMENT_CREDENTIAL`, `ASTER_ENROLLMENT_*`), connect/storage
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
            if let Some(value) = toml_string(trust, "next_root_commitment", label)? {
                self.next_root_commitment = Some(value);
            }
            if let Some(value) = toml_u64(trust, "root_seq", label)? {
                self.root_seq = value;
            }
            if let Some(value) = toml_string_list(trust, "root_rotations", label)? {
                self.root_rotations = value.into_iter().filter(|v| !v.is_empty()).collect();
            }
            if let Some(value) = toml_path(trust, "enrollment_credential", label)? {
                self.enrollment_credential_file = Some(value);
            }
            if let Some(value) = toml_string(trust, "enrollment_credential_chain", label)? {
                self.enrollment_credential_chain = Some(value);
            }
            for key in RETIRED_TRUST_KEYS {
                if trust.contains_key(key.0) {
                    return Err(retired_setting(&format!("{label}: [trust] {}", key.0)));
                }
            }
        }

        if let Some(connect) = toml_section(raw, "connect", label)? {
            if let Some(value) = toml_string(connect, "endpoint_addr", label)? {
                self.endpoint_addr = Some(value);
            }
        }

        if let Some(storage) = toml_section(raw, "storage", label)? {
            if let Some(value) = toml_path(storage, "path", label)? {
                self.storage_path = Some(value);
            }
            if let Some(value) = toml_u64(storage, "gc_interval_ms", label)? {
                self.gc_interval = gc_interval_from_ms(value);
            }
        }

        if let Some(network) = toml_section(raw, "network", label)? {
            if let Some(value) = toml_string_allow_empty(network, "secret_key", label)? {
                if value.trim().is_empty() {
                    self.secret_key = None;
                } else {
                    self.secret_key = Some(parse_secret_key(
                        &format!("{label} [network] secret_key"),
                        &value,
                    )?);
                }
            }
            if let Some(value) = toml_path(network, "secret_key_file", label)? {
                self.secret_key_file = Some(value);
            }
            let relay_mode = toml_string(network, "relay_mode", label)?;
            let relay_urls = toml_string_list(network, "relay_urls", label)?;
            self.apply_relay(
                relay_mode.as_deref(),
                relay_urls,
                &format!("{label} [network]"),
            )?;
            if let Some(value) = toml_string(network, "bind_addr", label)? {
                self.bind_addr = Some(value);
            }
            if let Some(value) = toml_bool(network, "local_discovery", label)? {
                self.local_discovery = value;
            }
            if let Some(value) = toml_bool(network, "enable_monitoring", label)? {
                self.enable_monitoring = value;
            }
            if let Some(value) = toml_bool(network, "enable_hooks", label)? {
                self.enable_hooks = value;
            }
            if let Some(value) = toml_u64(network, "hook_timeout_ms", label)? {
                self.hook_timeout_ms = value;
            }
            if let Some(value) = toml_string(network, "hook_failure_mode", label)? {
                self.hook_failure_mode = parse_hook_failure_mode(
                    &format!("{label} [network] hook_failure_mode"),
                    &value,
                )?;
            }
            if let Some(value) = toml_bool(network, "clear_ip_transports", label)? {
                self.clear_ip_transports = value;
            }
            if let Some(value) = toml_bool(network, "clear_relay_transports", label)? {
                self.clear_relay_transports = value;
            }
            if let Some(value) = toml_string(network, "portmapper_config", label)? {
                self.portmapper_config = Some(parse_portmapper(
                    &format!("{label} [network] portmapper_config"),
                    &value,
                )?);
            }
            if let Some(value) = toml_string(network, "proxy_url", label)? {
                self.proxy_url = Some(value);
            }
            if let Some(value) = toml_bool(network, "proxy_from_env", label)? {
                self.proxy_from_env = value;
            }

            macro_rules! overlay {
                ($field:ident, $parse:ident) => {
                    if let Some(value) = $parse(network, stringify!($field), label)? {
                        self.$field = Some(value);
                    }
                };
            }
            overlay!(transport_max_concurrent_bidi_streams, toml_u64);
            overlay!(transport_max_concurrent_uni_streams, toml_u64);
            overlay!(transport_stream_receive_window, toml_u64);
            overlay!(transport_receive_window, toml_u64);
            overlay!(transport_send_window, toml_u64);
            overlay!(transport_max_idle_timeout_ms, toml_u64);
            overlay!(transport_keep_alive_interval_ms, toml_u64);
            overlay!(transport_initial_mtu, toml_u16);
            overlay!(transport_datagram_receive_buffer_size, toml_usize);
            overlay!(transport_datagram_send_buffer_size, toml_usize);
            overlay!(transport_send_fairness, toml_bool);
            overlay!(transport_enable_segmentation_offload, toml_bool);
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

        self.enrollment.apply_toml(raw, label)?;

        if let Some(health) = toml_section(raw, "health", label)? {
            if let Some(value) = toml_u16(health, "port", label)? {
                self.health_port = value;
            }
            if let Some(value) = toml_string(health, "host", label)? {
                self.health_host = value;
            }
            if let Some(value) = toml_bool(health, "enabled", label)? {
                self.health_enabled = Some(value);
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
        if let Some(value) = env_string("ASTER_ENROLLMENT_CREDENTIAL_CHAIN") {
            self.enrollment_credential_chain = Some(value);
        }
        if let Some(value) = env_string("ASTER_NEXT_ROOT_COMMITMENT") {
            self.next_root_commitment = Some(value);
        }
        if let Some(value) = env_u64("ASTER_ROOT_SEQ")? {
            self.root_seq = value;
        }
        if let Some(value) = env_string("ASTER_ROOT_ROTATIONS") {
            self.root_rotations = value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect();
        }
        for key in RETIRED_TRUST_KEYS {
            if std::env::var_os(key.1).is_some() {
                return Err(retired_setting(key.1));
            }
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

        if let Some(value) = env_path("ASTER_STORAGE_PATH") {
            self.storage_path = Some(value);
        }
        if let Some(value) = env_u64("ASTER_GC_INTERVAL_MS")? {
            self.gc_interval = gc_interval_from_ms(value);
        }
        if let Some(value) = env_string_allow_empty("ASTER_SECRET_KEY") {
            if value.trim().is_empty() {
                self.secret_key = None;
            } else {
                self.secret_key = Some(parse_secret_key("ASTER_SECRET_KEY", &value)?);
            }
        }
        if let Some(value) = env_path("ASTER_SECRET_KEY_FILE") {
            self.secret_key_file = Some(value);
        }
        let relay_mode = env_string("ASTER_RELAY_MODE");
        let relay_urls = env_string("ASTER_RELAY_URLS").map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
        self.apply_relay(
            relay_mode.as_deref(),
            relay_urls,
            "ASTER_RELAY_MODE/ASTER_RELAY_URLS",
        )?;
        if let Some(value) = env_string("ASTER_BIND_ADDR") {
            self.bind_addr = Some(value);
        }
        if let Some(value) = env_bool("ASTER_LOCAL_DISCOVERY")? {
            self.local_discovery = value;
        }
        if let Some(value) = env_bool("ASTER_ENABLE_MONITORING")? {
            self.enable_monitoring = value;
        }
        if let Some(value) = env_bool("ASTER_ENABLE_HOOKS")? {
            self.enable_hooks = value;
        }
        if let Some(value) = env_u64("ASTER_HOOK_TIMEOUT_MS")? {
            self.hook_timeout_ms = value;
        }
        if let Some(value) = env_string("ASTER_HOOK_FAILURE_MODE") {
            self.hook_failure_mode = parse_hook_failure_mode("ASTER_HOOK_FAILURE_MODE", &value)?;
        }
        if let Some(value) = env_bool("ASTER_CLEAR_IP_TRANSPORTS")? {
            self.clear_ip_transports = value;
        }
        if let Some(value) = env_bool("ASTER_CLEAR_RELAY_TRANSPORTS")? {
            self.clear_relay_transports = value;
        }
        if let Some(value) = env_string("ASTER_PORTMAPPER_CONFIG") {
            self.portmapper_config = Some(parse_portmapper("ASTER_PORTMAPPER_CONFIG", &value)?);
        }
        if let Some(value) = env_string("ASTER_PROXY_URL") {
            self.proxy_url = Some(value);
        }
        if let Some(value) = env_bool("ASTER_PROXY_FROM_ENV")? {
            self.proxy_from_env = value;
        }

        macro_rules! overlay {
            ($field:ident, $parse:ident, $key:literal) => {
                if let Some(value) = $parse($key)? {
                    self.$field = Some(value);
                }
            };
        }
        overlay!(
            transport_max_concurrent_bidi_streams,
            env_u64,
            "ASTER_TRANSPORT_MAX_CONCURRENT_BIDI_STREAMS"
        );
        overlay!(
            transport_max_concurrent_uni_streams,
            env_u64,
            "ASTER_TRANSPORT_MAX_CONCURRENT_UNI_STREAMS"
        );
        overlay!(
            transport_stream_receive_window,
            env_u64,
            "ASTER_TRANSPORT_STREAM_RECEIVE_WINDOW"
        );
        overlay!(
            transport_receive_window,
            env_u64,
            "ASTER_TRANSPORT_RECEIVE_WINDOW"
        );
        overlay!(
            transport_send_window,
            env_u64,
            "ASTER_TRANSPORT_SEND_WINDOW"
        );
        overlay!(
            transport_max_idle_timeout_ms,
            env_u64,
            "ASTER_TRANSPORT_MAX_IDLE_TIMEOUT_MS"
        );
        overlay!(
            transport_keep_alive_interval_ms,
            env_u64,
            "ASTER_TRANSPORT_KEEP_ALIVE_INTERVAL_MS"
        );
        overlay!(
            transport_initial_mtu,
            env_u16,
            "ASTER_TRANSPORT_INITIAL_MTU"
        );
        overlay!(
            transport_datagram_receive_buffer_size,
            env_usize,
            "ASTER_TRANSPORT_DATAGRAM_RECEIVE_BUFFER_SIZE"
        );
        overlay!(
            transport_datagram_send_buffer_size,
            env_usize,
            "ASTER_TRANSPORT_DATAGRAM_SEND_BUFFER_SIZE"
        );
        overlay!(
            transport_send_fairness,
            env_bool,
            "ASTER_TRANSPORT_SEND_FAIRNESS"
        );
        overlay!(
            transport_enable_segmentation_offload,
            env_bool,
            "ASTER_TRANSPORT_ENABLE_SEGMENTATION_OFFLOAD"
        );

        if let Some(value) = env_string("ASTER_LOG_FORMAT") {
            self.log_format = value.to_ascii_lowercase();
        }
        if let Some(value) = env_string("ASTER_LOG_LEVEL") {
            self.log_level = value.to_ascii_lowercase();
        }
        if let Some(value) = env_bool("ASTER_LOG_MASK")? {
            self.log_mask = value;
        }

        if let Some(value) = env_u16("ASTER_HEALTH_PORT")? {
            self.health_port = value;
        }
        if let Some(value) = env_string("ASTER_HEALTH_HOST") {
            self.health_host = value;
        }
        if let Some(value) = env_bool("ASTER_HEALTH_ENABLED")? {
            self.health_enabled = Some(value);
        }
        self.enrollment.apply_env()?;

        Ok(())
    }

    /// Whether the health listener should run: [`health_enabled`](Self::health_enabled)
    /// when set, else `health_port != 0`.
    pub fn is_health_enabled(&self) -> bool {
        self.health_enabled.unwrap_or(self.health_port != 0)
    }

    /// The health listener's bind config, or `None` when disabled (see
    /// [`is_health_enabled`](Self::is_health_enabled)).
    #[cfg(feature = "health")]
    pub fn health_config(&self) -> Option<crate::ops::HealthConfig> {
        self.is_health_enabled()
            .then(|| crate::ops::HealthConfig::new(self.health_host.clone(), self.health_port))
    }

    /// Resolve the root public key from `root_pubkey` or `root_pubkey_file`.
    ///
    /// Missing files return `Ok(None)`. Malformed files return
    /// [`Error::InvalidArgument`].
    pub fn resolve_root_pubkey(&mut self) -> Result<Option<PublicKey>> {
        let Some(path) = self.root_pubkey_file.as_ref() else {
            return Ok(self.root_pubkey);
        };
        let path = expand_tilde(path);
        if !path.exists() {
            return Ok(self.root_pubkey);
        }

        let content = fs::read_to_string(&path)
            .map_err(|e| Error::InvalidArgument(format!("{}: {e}", path.to_string_lossy())))?;
        let key = parse_pubkey_file(&path, &content)?;

        // **Both sources are read, and they must agree.**
        //
        // The inline key used to short-circuit before the file was opened, so a
        // deployment that set both and rotated only one ran under the stale
        // value silently. There is one root key; two configured spellings of it
        // that differ is a configuration error, not a precedence question, and
        // guessing wrong here is not visible until a signature fails to verify
        // somewhere far away.
        if let Some(inline) = self.root_pubkey {
            if inline != key {
                return Err(Error::InvalidArgument(format!(
                    "root_pubkey is {inline} and {} holds {key}; one deployment has one \
                     ROOT, so set whichever is current and remove the other",
                    path.to_string_lossy(),
                )));
            }
        }
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

    /// Whether enrollment is on: [`EnrollmentConfig::enabled`] when set,
    /// otherwise whether a credential, an audience, a fleet namespace or an
    /// issuer delegation is configured. A half-configured enrollment is
    /// therefore an error at start, never an open node.
    pub fn is_enrollment_enabled(&self) -> bool {
        self.enrollment.enabled.unwrap_or(
            self.enrollment.defines_enrollment()
                || self.enrollment_credential_chain.is_some()
                || self.enrollment_credential_file.is_some(),
        )
    }

    /// Whether this configuration selects a persistent node.
    pub fn is_persistent(&self) -> bool {
        self.storage_path.is_some()
    }

    /// Resolve [`secret_key_file`](Self::secret_key_file) into
    /// [`secret_key`](Self::secret_key). [`Node::start`](crate::Node::start)
    /// calls this; bindings that need the key before starting a node can call
    /// it themselves (it is idempotent).
    ///
    /// - No file configured: returns the current `secret_key` unchanged.
    /// - File exists: its key is loaded ([`SecretKey::read_file`]). If an
    ///   explicit `secret_key` is also set and differs, this is an error — one
    ///   node has one identity, and silently preferring either spelling hides
    ///   a stale record.
    /// - File missing: the explicit `secret_key`, or a freshly generated one,
    ///   is written there ([`SecretKey::write_file`], mode `0600`, parent
    ///   directories created) so the next start reuses it.
    pub fn resolve_secret_key_file(&mut self) -> Result<Option<SecretKey>> {
        let Some(path) = self.secret_key_file.as_ref() else {
            return Ok(self.secret_key.clone());
        };
        let path = expand_tilde(path);
        let key = match SecretKey::read_file_if_exists(&path)? {
            Some(from_file) => {
                if let Some(inline) = self.secret_key.as_ref() {
                    if inline.to_bytes() != from_file.to_bytes() {
                        return Err(Error::InvalidArgument(format!(
                            "secret_key and {} hold different keys; one node has one \
                             identity, so set whichever is current and remove the other",
                            path.to_string_lossy(),
                        )));
                    }
                }
                from_file
            }
            None => {
                let key = self.secret_key.clone().unwrap_or_else(SecretKey::generate);
                key.write_file(&path)?;
                key
            }
        };
        self.secret_key = Some(key.clone());
        Ok(Some(key))
    }

    /// Lower this configuration to the transport core's endpoint config.
    ///
    /// Every endpoint/transport field is derived from the public fields here,
    /// so direct field assignment and builder calls are equivalent. Does
    /// **not** resolve [`secret_key_file`](Self::secret_key_file); call
    /// [`resolve_secret_key_file`](Self::resolve_secret_key_file) first.
    pub(crate) fn to_core_endpoint_config(&self) -> CoreEndpointConfig {
        let (relay_mode, relay_urls) = match &self.relay_mode {
            RelayMode::Default => ("default", Vec::new()),
            RelayMode::Custom(urls) => ("custom", urls.clone()),
            RelayMode::Staging => ("staging", Vec::new()),
            RelayMode::Disabled => ("disabled", Vec::new()),
        };
        CoreEndpointConfig {
            relay_mode: Some(relay_mode.into()),
            relay_urls,
            alpns: self.alpns.clone(),
            secret_key: self.secret_key.as_ref().map(|k| k.to_bytes().to_vec()),
            enable_discovery: self.local_discovery,
            enable_monitoring: self.enable_monitoring,
            enable_hooks: self.enable_hooks,
            hook_timeout_ms: self.hook_timeout_ms,
            hook_failure_mode: self.hook_failure_mode,
            bind_addr: self.bind_addr.clone(),
            clear_ip_transports: self.clear_ip_transports,
            clear_relay_transports: self.clear_relay_transports,
            portmapper_config: self.portmapper_config.clone(),
            proxy_url: self.proxy_url.clone(),
            proxy_from_env: self.proxy_from_env,
            transport_max_concurrent_bidi_streams: self.transport_max_concurrent_bidi_streams,
            transport_max_concurrent_uni_streams: self.transport_max_concurrent_uni_streams,
            transport_stream_receive_window: self.transport_stream_receive_window,
            transport_receive_window: self.transport_receive_window,
            transport_send_window: self.transport_send_window,
            transport_max_idle_timeout_ms: self.transport_max_idle_timeout_ms,
            transport_keep_alive_interval_ms: self.transport_keep_alive_interval_ms,
            transport_initial_mtu: self.transport_initial_mtu,
            transport_datagram_receive_buffer_size: self.transport_datagram_receive_buffer_size,
            transport_datagram_send_buffer_size: self.transport_datagram_send_buffer_size,
            transport_send_fairness: self.transport_send_fairness,
            transport_enable_segmentation_offload: self.transport_enable_segmentation_offload,
            data_dir: self
                .storage_path
                .as_ref()
                .map(|dir| dir.to_string_lossy().into_owned()),
        }
    }

    /// Binding support: the lowered [`CoreEndpointConfig`] (see
    /// `aster::binding_support`). Not part of the stable API.
    #[cfg(feature = "binding-support")]
    #[doc(hidden)]
    pub fn core_endpoint_config(&self) -> CoreEndpointConfig {
        self.to_core_endpoint_config()
    }

    /// Apply a relay mode and/or relay URL list from one source (env or TOML).
    ///
    /// `relay_urls` alone selects [`RelayMode::Custom`]; with a mode, the mode
    /// must be `custom`. `relay_mode = "custom"` alone keeps any URLs an
    /// earlier source supplied.
    fn apply_relay(
        &mut self,
        mode: Option<&str>,
        urls: Option<Vec<String>>,
        source: &str,
    ) -> Result<()> {
        match (mode, urls) {
            (None, None) => {}
            (mode, Some(urls)) => {
                if let Some(mode) = mode {
                    if !mode.trim().eq_ignore_ascii_case("custom") {
                        return Err(Error::InvalidArgument(format!(
                            "{source}: relay_urls requires relay_mode \"custom\", got {mode:?}"
                        )));
                    }
                }
                if urls.is_empty() {
                    return Err(Error::InvalidArgument(format!(
                        "{source}: relay_urls is empty"
                    )));
                }
                self.relay_mode = RelayMode::Custom(urls);
            }
            (Some(mode), None) => {
                let parsed = parse_relay_mode(mode);
                match (&parsed, &self.relay_mode) {
                    (RelayMode::Custom(new), RelayMode::Custom(_)) if new.is_empty() => {}
                    _ => self.relay_mode = parsed,
                }
            }
        }
        Ok(())
    }
}

impl Default for AsterConfig {
    fn default() -> Self {
        Self {
            root_pubkey: None,
            root_pubkey_file: None,
            next_root_commitment: None,
            root_seq: 0,
            root_rotations: Vec::new(),
            enrollment_credential_file: None,
            enrollment_credential_chain: None,
            endpoint_addr: None,
            storage_path: None,
            gc_interval: None,
            secret_key: None,
            secret_key_file: None,
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
            health_port: 0,
            health_host: "127.0.0.1".into(),
            health_enabled: None,
            alpns: Vec::new(),
            enrollment: EnrollmentConfig::default(),
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
        self.config.relay_mode = mode;
        self
    }

    /// Use exactly these relay servers ([`RelayMode::Custom`]). Shorthand for
    /// `relay(RelayMode::Custom(urls))`. Env: `ASTER_RELAY_URLS`
    /// (comma-separated); TOML: `[network] relay_urls = [..]`.
    pub fn relay_urls<I, S>(mut self, urls: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.relay_mode = RelayMode::Custom(urls.into_iter().map(Into::into).collect());
        self
    }

    /// Make the node persistent, storing all state under `dir`.
    pub fn persistent(mut self, dir: impl Into<PathBuf>) -> Self {
        self.config.storage_path = Some(dir.into());
        self
    }

    /// Enable periodic blob garbage collection at `interval`.
    ///
    /// Off by default: without this, the blob store is grow-only and only tags
    /// pin retention. With it set, untagged blobs are reclaimed every
    /// `interval`. See [`AsterConfig::gc_interval`] for full semantics and
    /// [`Blobs::gc_run_once`](crate::Blobs::gc_run_once) for a deterministic
    /// manual sweep. A zero `interval` disables GC.
    pub fn gc_interval(mut self, interval: Duration) -> Self {
        self.config.gc_interval = (!interval.is_zero()).then_some(interval);
        self
    }

    /// Pin the node's identity to a specific secret key. On a persistent node
    /// combined with the same `data_dir`, this gives a restart-stable
    /// [`NodeId`](crate::NodeId).
    pub fn secret_key(mut self, key: SecretKey) -> Self {
        self.config.secret_key = Some(key);
        self
    }

    /// Keep the node's identity in a secret key file (64 hex characters):
    /// loaded when present, created on first start otherwise. See
    /// [`AsterConfig::secret_key_file`] and
    /// [`AsterConfig::resolve_secret_key_file`].
    pub fn secret_key_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.secret_key_file = Some(path.into());
        self
    }

    /// Pin the deployment's **ROOT** public key.
    ///
    /// The same value `ASTER_ROOT_PUBKEY`, `ASTER_ROOT_PUBKEY_FILE` and
    /// `[trust] root_pubkey` supply; this is the in-code form, for a host that
    /// already holds it. Coordination consumes it as the key every signed
    /// genesis, join permit and recovery document verifies against, and as the
    /// identity of the group's immutable super-administrator.
    pub fn root_pubkey(mut self, key: PublicKey) -> Self {
        self.config.root_pubkey = Some(key);
        self
    }

    /// Bind the endpoint to a specific socket address, e.g. `"0.0.0.0:9000"`.
    pub fn bind_addr(mut self, addr: impl Into<String>) -> Self {
        self.config.bind_addr = Some(addr.into());
        self
    }

    /// Accept inbound connections on an extra ALPN (repeatable). See
    /// [`AsterConfig::alpns`].
    pub fn alpn(mut self, alpn: impl Into<Vec<u8>>) -> Self {
        let alpn = alpn.into();
        if !self.config.alpns.contains(&alpn) {
            self.config.alpns.push(alpn);
        }
        self
    }

    /// Enable local-network peer discovery (mDNS).
    pub fn discovery(mut self, enabled: bool) -> Self {
        self.config.local_discovery = enabled;
        self
    }

    /// Enable connection monitoring / remote-info tracking.
    pub fn monitoring(mut self, enabled: bool) -> Self {
        self.config.enable_monitoring = enabled;
        self
    }

    /// Enable admission hooks. Required for [`Node::take_admission`](crate::Node::take_admission)
    /// to return a handle.
    pub fn hooks(mut self, enabled: bool) -> Self {
        self.config.enable_hooks = enabled;
        self
    }

    /// Timeout (ms) for an admission decision before the configured hook
    /// failure behavior is applied. Only meaningful with [`hooks`](Self::hooks).
    pub fn hook_timeout_ms(mut self, ms: u64) -> Self {
        self.config.hook_timeout_ms = ms;
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
        self
    }

    /// Remove every direct IP (UDP/QUIC) transport: relay-only mode.
    pub fn clear_ip_transports(mut self, yes: bool) -> Self {
        self.config.clear_ip_transports = yes;
        self
    }

    /// Remove every relay transport: direct-IP-only mode. The node is ready as
    /// soon as it binds; it does not wait for a home relay, even with the
    /// default `relay_mode` (iroh's `online()` would otherwise never resolve).
    pub fn clear_relay_transports(mut self, yes: bool) -> Self {
        self.config.clear_relay_transports = yes;
        self
    }

    /// Enable (default) or disable the UPnP / NAT-PMP / PCP portmapper.
    /// Env: `ASTER_PORTMAPPER_CONFIG=enabled|disabled`.
    pub fn portmapper(mut self, enabled: bool) -> Self {
        self.config.portmapper_config = Some(if enabled { "enabled" } else { "disabled" }.into());
        self
    }

    /// Route relay / HTTPS traffic through an HTTP or SOCKS proxy, e.g.
    /// `"http://proxy:8080"`.
    pub fn proxy_url(mut self, url: impl Into<String>) -> Self {
        self.config.proxy_url = Some(url.into());
        self
    }

    /// Read the proxy from `HTTP_PROXY` / `HTTPS_PROXY`.
    pub fn proxy_from_env(mut self, yes: bool) -> Self {
        self.config.proxy_from_env = yes;
        self
    }

    /// Maximum number of incoming bidirectional QUIC streams.
    pub fn transport_max_concurrent_bidi_streams(mut self, n: u64) -> Self {
        self.config.transport_max_concurrent_bidi_streams = Some(n);
        self
    }

    /// Maximum number of incoming unidirectional QUIC streams.
    pub fn transport_max_concurrent_uni_streams(mut self, n: u64) -> Self {
        self.config.transport_max_concurrent_uni_streams = Some(n);
        self
    }

    /// Per-stream QUIC receive window in bytes.
    pub fn transport_stream_receive_window(mut self, bytes: u64) -> Self {
        self.config.transport_stream_receive_window = Some(bytes);
        self
    }

    /// Per-connection QUIC receive window in bytes.
    pub fn transport_receive_window(mut self, bytes: u64) -> Self {
        self.config.transport_receive_window = Some(bytes);
        self
    }

    /// QUIC send window in bytes.
    pub fn transport_send_window(mut self, bytes: u64) -> Self {
        self.config.transport_send_window = Some(bytes);
        self
    }

    /// QUIC idle timeout in milliseconds.
    pub fn transport_max_idle_timeout_ms(mut self, ms: u64) -> Self {
        self.config.transport_max_idle_timeout_ms = Some(ms);
        self
    }

    /// QUIC keep-alive interval in milliseconds.
    pub fn transport_keep_alive_interval_ms(mut self, ms: u64) -> Self {
        self.config.transport_keep_alive_interval_ms = Some(ms);
        self
    }

    /// Initial QUIC UDP payload size before MTU discovery.
    pub fn transport_initial_mtu(mut self, bytes: u16) -> Self {
        self.config.transport_initial_mtu = Some(bytes);
        self
    }

    /// Incoming QUIC application datagram buffer in bytes. Zero disables
    /// incoming datagrams.
    pub fn transport_datagram_receive_buffer_size(mut self, bytes: usize) -> Self {
        self.config.transport_datagram_receive_buffer_size = Some(bytes);
        self
    }

    /// Outgoing QUIC application datagram buffer in bytes.
    pub fn transport_datagram_send_buffer_size(mut self, bytes: usize) -> Self {
        self.config.transport_datagram_send_buffer_size = Some(bytes);
        self
    }

    /// Fair scheduling across send streams at the same priority.
    pub fn transport_send_fairness(mut self, yes: bool) -> Self {
        self.config.transport_send_fairness = Some(yes);
        self
    }

    /// UDP generic segmentation offload, when the platform supports it.
    pub fn transport_enable_segmentation_offload(mut self, yes: bool) -> Self {
        self.config.transport_enable_segmentation_offload = Some(yes);
        self
    }

    /// Path to a `.aster-identity` credential file (see
    /// [`AsterConfig::load_identity`]). Not the node key file — for that see
    /// [`secret_key_file`](Self::secret_key_file).
    pub fn identity_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.identity_file = Some(path.into());
        self
    }

    /// Force the health/metrics HTTP listener on or off (see
    /// [`AsterConfig::health_enabled`]). On with port `0` binds an ephemeral
    /// port.
    pub fn health(mut self, enabled: bool) -> Self {
        self.config.health_enabled = Some(enabled);
        self
    }

    /// Health listener port; non-zero enables it (see [`AsterConfig::health_port`]).
    pub fn health_port(mut self, port: u16) -> Self {
        self.config.health_port = port;
        self
    }

    /// Health listener bind host (default `127.0.0.1`).
    pub fn health_host(mut self, host: impl Into<String>) -> Self {
        self.config.health_host = host.into();
        self
    }

    /// This node's enrollment credential chain, inline (see
    /// [`AsterConfig::enrollment_credential_chain`]).
    pub fn enrollment_credential_chain(mut self, chain: impl Into<String>) -> Self {
        self.config.enrollment_credential_chain = Some(chain.into());
        self
    }

    /// This node's enrollment credential file (see
    /// [`AsterConfig::enrollment_credential_file`]).
    pub fn enrollment_credential_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.enrollment_credential_file = Some(path.into());
        self
    }

    /// Pin the next root (see [`AsterConfig::next_root_commitment`]), with the
    /// rotations already applied to reach the current root and their
    /// statements, as an `aster trust` profile records them.
    pub fn root_rotation(
        mut self,
        next_root_commitment: impl Into<String>,
        root_seq: u64,
        root_rotations: Vec<String>,
    ) -> Self {
        self.config.next_root_commitment = Some(next_root_commitment.into());
        self.config.root_seq = root_seq;
        self.config.root_rotations = root_rotations;
        self
    }

    /// Enrollment settings (see [`EnrollmentConfig`]); replaces any loaded
    /// from TOML or the environment.
    pub fn enrollment(mut self, enrollment: EnrollmentConfig) -> Self {
        self.config.enrollment = enrollment;
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

/// Parse an inline secret key: 64 hex characters (the canonical form, as in a
/// secret key file) or, for compatibility with existing `aster.toml` /
/// `.aster-identity` files, standard base64 of the 32 bytes. The two cannot
/// be confused: 64 hex characters never base64-decode to 32 bytes.
fn parse_secret_key(source: &str, value: &str) -> Result<SecretKey> {
    let value = value.trim();
    if value.len() == 64 {
        if let Ok(key) = SecretKey::from_hex(value) {
            return Ok(key);
        }
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|e| {
            Error::InvalidArgument(format!(
                "{source}: expected 64 hex characters or base64 of 32 bytes: {e}"
            ))
        })?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| Error::InvalidArgument(format!("{source}: secret key must be 32 bytes")))?;
    Ok(SecretKey::from_bytes(bytes))
}

fn parse_hook_failure_mode(source: &str, value: &str) -> Result<HookFailureMode> {
    HookFailureMode::from_config_str(value).ok_or_else(|| {
        Error::InvalidArgument(format!(
            "{source}: expected fail_open or fail_closed, got {value:?}"
        ))
    })
}

/// Portmapper setting, normalised to what the transport core accepts.
fn parse_portmapper(source: &str, value: &str) -> Result<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "enabled" | "enable" | "on" | "true" => Ok("enabled".into()),
        "disabled" | "disable" | "off" | "false" => Ok("disabled".into()),
        _ => Err(Error::InvalidArgument(format!(
            "{source}: expected enabled or disabled, got {value:?}"
        ))),
    }
}

fn gc_interval_from_ms(ms: u64) -> Option<Duration> {
    (ms > 0).then(|| Duration::from_millis(ms))
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

/// Settings of the retired legacy consumer admission, as (`[trust]` TOML key,
/// environment variable). They are refused rather than ignored: each one used
/// to change who could connect, so a configuration that still sets one (for
/// example `allow_all_consumers = false`, expecting a closed server) must not
/// start with a different meaning.
const RETIRED_TRUST_KEYS: &[(&str, &str)] = &[
    ("allow_all_consumers", "ASTER_ALLOW_ALL_CONSUMERS"),
    ("allow_all_producers", "ASTER_ALLOW_ALL_PRODUCERS"),
    (
        "enrollment_credential_iid",
        "ASTER_ENROLLMENT_CREDENTIAL_IID",
    ),
];

fn retired_setting(name: &str) -> Error {
    Error::InvalidArgument(format!(
        "{name} was removed with the legacy consumer admission: a node is open exactly \
         when Enrollment is not configured; to restrict who connects, set [trust] \
         enrollment_credential (ASTER_ENROLLMENT_CREDENTIAL) and [enrollment] instead \
         (see the Enrollment migration in the binding CHANGELOG)"
    ))
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

fn toml_string_list(table: &toml::Table, key: &str, label: &str) -> Result<Option<Vec<String>>> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                toml::Value::String(value) => Ok(value.trim().to_string()),
                _ => Err(Error::InvalidArgument(format!(
                    "{label}: {key} must be an array of strings"
                ))),
            })
            .collect::<Result<Vec<_>>>()
            .map(Some),
        Some(_) => Err(Error::InvalidArgument(format!(
            "{label}: {key} must be an array of strings"
        ))),
    }
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
        "ASTER_ENROLLMENT_CREDENTIAL_CHAIN",
        "ASTER_ENROLLMENT_CREDENTIAL_IID",
        "ASTER_NEXT_ROOT_COMMITMENT",
        "ASTER_ROOT_SEQ",
        "ASTER_ROOT_ROTATIONS",
        "ASTER_ENROLLMENT_AUDIENCE",
        "ASTER_ENROLLMENT_FLEET_NAMESPACE",
        "ASTER_ENROLLMENT_REPLICAS",
        "ASTER_ENROLLMENT_POLICY",
        "ASTER_ENROLLMENT_ROLE_ATTRIBUTE",
        "ASTER_ENROLLMENT_ISSUER_LIFETIME",
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
        "ASTER_RELAY_URLS",
        "ASTER_SECRET_KEY_FILE",
        "ASTER_GC_INTERVAL_MS",
        "ASTER_HEALTH_PORT",
        "ASTER_HEALTH_HOST",
        "ASTER_HEALTH_ENABLED",
    ];

    pub(super) fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    pub(super) struct EnvGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        pub(super) fn new() -> Self {
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
        // One panicking env test must not poison the lock for all the others.
        let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new();

        let root = [1u8; 32];
        let secret = [2u8; 32];
        env::set_var("ASTER_ROOT_PUBKEY", hex::encode(root));
        env::set_var("ASTER_ROOT_PUBKEY_FILE", "/tmp/root.pub");
        env::set_var("ASTER_ENROLLMENT_CREDENTIAL", "/tmp/node.cred");
        env::set_var("ASTER_ENROLLMENT_CREDENTIAL_CHAIN", "aster-chain-v1~inline");
        env::set_var("ASTER_NEXT_ROOT_COMMITMENT", "ab".repeat(32));
        env::set_var("ASTER_ROOT_SEQ", "2");
        env::set_var("ASTER_ROOT_ROTATIONS", "s1, s2");
        env::set_var("ASTER_ENROLLMENT_AUDIENCE", "mc");
        env::set_var("ASTER_ENROLLMENT_FLEET_NAMESPACE", "cd".repeat(32));
        env::set_var("ASTER_ENROLLMENT_REPLICAS", "aster1a, aster1b");
        env::set_var("ASTER_ENROLLMENT_POLICY", r#"{"scope":"ops"}"#);
        env::set_var("ASTER_ENROLLMENT_ROLE_ATTRIBUTE", "");
        env::set_var("ASTER_ENROLLMENT_ISSUER_LIFETIME", "2h");
        env::set_var("ASTER_ENDPOINT_ADDR", "aster1ticket");
        env::set_var("ASTER_IDENTITY_FILE", "/tmp/.aster-identity");
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
        assert_eq!(
            cfg.enrollment_credential_chain.as_deref(),
            Some("aster-chain-v1~inline")
        );
        assert_eq!(cfg.next_root_commitment, Some("ab".repeat(32)));
        assert_eq!(cfg.root_seq, 2);
        assert_eq!(cfg.root_rotations, ["s1", "s2"]);
        assert!(cfg.is_enrollment_enabled());
        assert_eq!(cfg.enrollment.audience.as_deref(), Some("mc"));
        assert_eq!(cfg.enrollment.fleet_namespace, Some("cd".repeat(32)));
        assert_eq!(cfg.enrollment.replicas, ["aster1a", "aster1b"]);
        assert_eq!(
            cfg.enrollment.policy,
            Some(serde_json::json!({"scope": "ops"}))
        );
        assert_eq!(cfg.enrollment.roles.attribute, "");
        assert_eq!(cfg.enrollment.issuer_lifetime_secs, Some(7200));
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

        let core = cfg.to_core_endpoint_config();
        assert_eq!(core.relay_mode.as_deref(), Some("disabled"));
        assert_eq!(core.secret_key.unwrap(), secret.to_vec());
        assert_eq!(core.bind_addr.as_deref(), Some("127.0.0.1:0"));
        assert!(core.enable_discovery);
        assert!(core.enable_monitoring);
        assert!(core.enable_hooks);
        assert_eq!(core.transport_receive_window, Some(8_000_000));
        assert_eq!(core.transport_send_fairness, Some(true));
    }

    #[test]
    fn from_env_rejects_invalid_values() {
        // One panicking env test must not poison the lock for all the others.
        let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new();

        env::set_var("ASTER_LOCAL_DISCOVERY", "maybe");
        let err = AsterConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("ASTER_LOCAL_DISCOVERY"));
        env::remove_var("ASTER_LOCAL_DISCOVERY");

        env::set_var("ASTER_ENROLLMENT_POLICY", "{not json");
        let err = AsterConfig::from_env().unwrap_err();
        assert!(err.to_string().contains("ASTER_ENROLLMENT_POLICY"));
        env::remove_var("ASTER_ENROLLMENT_POLICY");

        // Retired consumer-admission variables are refused, naming the
        // replacement, rather than silently ignored.
        for key in [
            "ASTER_ALLOW_ALL_CONSUMERS",
            "ASTER_ALLOW_ALL_PRODUCERS",
            "ASTER_ENROLLMENT_CREDENTIAL_IID",
        ] {
            env::set_var(key, "false");
            let err = AsterConfig::from_env().unwrap_err().to_string();
            assert!(err.contains(key), "{err}");
            assert!(err.contains("enrollment_credential"), "{err}");
            env::remove_var(key);
        }
    }

    #[test]
    fn from_file_env_and_builder_follow_resolution_order() {
        // One panicking env test must not poison the lock for all the others.
        let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
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
root_seq = 1

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
                // A TOML basic string: on Windows the temp path carries
                // backslashes, which TOML reads as escapes, so they are
                // doubled rather than written raw.
                dir.path()
                    .join("file-store")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\"),
                base64::engine::general_purpose::STANDARD.encode(file_secret)
            ),
        )
        .unwrap();

        env::set_var("ASTER_BIND_ADDR", "127.0.0.1:2222");
        env::set_var("ASTER_RELAY_MODE", "staging");
        env::set_var("ASTER_ROOT_SEQ", "2");

        let cfg = AsterConfig::from_file(&config_path).unwrap();
        assert_eq!(cfg.root_pubkey.unwrap().to_bytes(), file_root);
        assert_eq!(cfg.root_seq, 2); // env beats TOML
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
        let core = cfg.to_core_endpoint_config();
        assert_eq!(core.bind_addr.as_deref(), Some("127.0.0.1:3333"));
        assert_eq!(core.relay_mode.as_deref(), Some("disabled"));
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
        // One panicking env test must not poison the lock for all the others.
        let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
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

#[cfg(test)]
fn tests_env_lock() -> &'static std::sync::Mutex<()> {
    tests::env_lock()
}

#[cfg(test)]
fn tests_env_guard() -> tests::EnvGuard {
    tests::EnvGuard::new()
}

#[cfg(test)]
mod binding_knob_tests {
    //! R1: every endpoint/transport knob the NAPI `EndpointConfig` and the C
    //! ABI `aster_endpoint_config_t` expose is reachable from `AsterConfig`
    //! and lowers into `CoreEndpointConfig` unchanged.
    use super::*;
    use std::sync::MutexGuard;

    fn lock_env() -> MutexGuard<'static, ()> {
        // Share the env lock with `tests` so env-mutating tests never overlap.
        super::tests_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn every_builder_knob_round_trips_into_core_endpoint_config() {
        let secret = SecretKey::from_bytes([0x11; 32]);
        let cfg = AsterConfig::builder()
            .relay_urls(["https://relay-a.example", "https://relay-b.example"])
            .secret_key(secret.clone())
            .persistent("/tmp/aster-knobs")
            .gc_interval(Duration::from_secs(90))
            .bind_addr("0.0.0.0:9000")
            .alpn(b"my/1".to_vec())
            .alpn(b"my/1".to_vec())
            .alpn(b"other/2".to_vec())
            .discovery(true)
            .monitoring(true)
            .hooks(true)
            .hook_timeout_ms(750)
            .hook_failure_mode(HookFailureMode::FailClosed)
            .clear_ip_transports(true)
            .clear_relay_transports(true)
            .portmapper(false)
            .proxy_url("socks5://proxy:1080")
            .proxy_from_env(true)
            .transport_max_concurrent_bidi_streams(1)
            .transport_max_concurrent_uni_streams(2)
            .transport_stream_receive_window(3)
            .transport_receive_window(4)
            .transport_send_window(5)
            .transport_max_idle_timeout_ms(6)
            .transport_keep_alive_interval_ms(7)
            .transport_initial_mtu(1200)
            .transport_datagram_receive_buffer_size(9)
            .transport_datagram_send_buffer_size(10)
            .transport_send_fairness(false)
            .transport_enable_segmentation_offload(true)
            .build();

        assert!(cfg.is_persistent());
        assert_eq!(cfg.gc_interval, Some(Duration::from_secs(90)));

        let core = cfg.to_core_endpoint_config();
        assert_eq!(core.relay_mode.as_deref(), Some("custom"));
        assert_eq!(
            core.relay_urls,
            vec![
                "https://relay-a.example".to_string(),
                "https://relay-b.example".to_string()
            ]
        );
        assert_eq!(core.secret_key, Some(secret.to_bytes().to_vec()));
        assert_eq!(core.data_dir.as_deref(), Some("/tmp/aster-knobs"));
        assert_eq!(core.bind_addr.as_deref(), Some("0.0.0.0:9000"));
        assert_eq!(core.alpns, vec![b"my/1".to_vec(), b"other/2".to_vec()]);
        assert!(core.enable_discovery);
        assert!(core.enable_monitoring);
        assert!(core.enable_hooks);
        assert_eq!(core.hook_timeout_ms, 750);
        assert_eq!(core.hook_failure_mode, HookFailureMode::FailClosed);
        assert!(core.clear_ip_transports);
        assert!(core.clear_relay_transports);
        assert_eq!(core.portmapper_config.as_deref(), Some("disabled"));
        assert_eq!(core.proxy_url.as_deref(), Some("socks5://proxy:1080"));
        assert!(core.proxy_from_env);
        assert_eq!(core.transport_max_concurrent_bidi_streams, Some(1));
        assert_eq!(core.transport_max_concurrent_uni_streams, Some(2));
        assert_eq!(core.transport_stream_receive_window, Some(3));
        assert_eq!(core.transport_receive_window, Some(4));
        assert_eq!(core.transport_send_window, Some(5));
        assert_eq!(core.transport_max_idle_timeout_ms, Some(6));
        assert_eq!(core.transport_keep_alive_interval_ms, Some(7));
        assert_eq!(core.transport_initial_mtu, Some(1200));
        assert_eq!(core.transport_datagram_receive_buffer_size, Some(9));
        assert_eq!(core.transport_datagram_send_buffer_size, Some(10));
        assert_eq!(core.transport_send_fairness, Some(false));
        assert_eq!(core.transport_enable_segmentation_offload, Some(true));
    }

    #[test]
    fn defaults_lower_to_core_defaults() {
        let core = AsterConfig::default().to_core_endpoint_config();
        let expected = CoreEndpointConfig::default();
        assert_eq!(core.relay_mode.as_deref(), Some("default"));
        assert!(core.relay_urls.is_empty());
        assert_eq!(core.secret_key, None);
        assert_eq!(core.data_dir, None);
        assert_eq!(core.hook_timeout_ms, expected.hook_timeout_ms);
        assert_eq!(core.hook_failure_mode, expected.hook_failure_mode);
        assert_eq!(core.portmapper_config, expected.portmapper_config);
        assert!(!core.enable_discovery && !core.enable_hooks && !core.enable_monitoring);
        assert!(!core.clear_ip_transports && !core.clear_relay_transports);
        assert_eq!(core.transport_receive_window, None);
    }

    /// The old config kept a second copy of every endpoint field in sync by
    /// hand, so assigning a public field directly was silently ignored at
    /// start. Lowering now reads the public fields.
    #[test]
    fn direct_field_assignment_is_honoured() {
        let mut cfg = AsterConfig::builder().build();
        cfg.bind_addr = Some("127.0.0.1:4444".into());
        cfg.relay_mode = RelayMode::Disabled;
        cfg.storage_path = Some(PathBuf::from("/tmp/direct"));
        cfg.transport_initial_mtu = Some(1300);
        let core = cfg.to_core_endpoint_config();
        assert_eq!(core.bind_addr.as_deref(), Some("127.0.0.1:4444"));
        assert_eq!(core.relay_mode.as_deref(), Some("disabled"));
        assert_eq!(core.data_dir.as_deref(), Some("/tmp/direct"));
        assert_eq!(core.transport_initial_mtu, Some(1300));
        assert!(cfg.is_persistent());
    }

    #[test]
    fn zero_gc_interval_disables_gc() {
        let cfg = AsterConfig::builder()
            .gc_interval(Duration::from_secs(0))
            .build();
        assert_eq!(cfg.gc_interval, None);
    }

    #[test]
    fn env_relay_urls_secret_key_file_gc_and_hex_secret() {
        let _lock = lock_env();
        let _env = super::tests_env_guard();

        let secret = [0x42u8; 32];
        env::set_var(
            "ASTER_RELAY_URLS",
            " https://r1.example , https://r2.example ,",
        );
        env::set_var("ASTER_SECRET_KEY", hex::encode(secret));
        env::set_var("ASTER_SECRET_KEY_FILE", "/tmp/node.key");
        env::set_var("ASTER_GC_INTERVAL_MS", "60000");
        env::set_var("ASTER_PORTMAPPER_CONFIG", "OFF");

        let cfg = AsterConfig::from_env().unwrap();
        assert_eq!(
            cfg.relay_mode,
            RelayMode::Custom(vec![
                "https://r1.example".into(),
                "https://r2.example".into()
            ])
        );
        assert_eq!(cfg.secret_key.as_ref().unwrap().to_bytes(), secret);
        assert_eq!(cfg.secret_key_file, Some(PathBuf::from("/tmp/node.key")));
        assert_eq!(cfg.gc_interval, Some(Duration::from_secs(60)));
        assert_eq!(cfg.portmapper_config.as_deref(), Some("disabled"));

        // An explicit custom mode keeps the URLs; any other mode conflicts.
        env::set_var("ASTER_RELAY_MODE", "custom");
        let cfg = AsterConfig::from_env().unwrap();
        assert!(matches!(cfg.relay_mode, RelayMode::Custom(ref u) if u.len() == 2));
        env::set_var("ASTER_RELAY_MODE", "disabled");
        let err = AsterConfig::from_env().unwrap_err().to_string();
        assert!(err.contains("relay_urls requires"), "{err}");
        env::remove_var("ASTER_RELAY_MODE");

        env::set_var("ASTER_GC_INTERVAL_MS", "0");
        assert_eq!(AsterConfig::from_env().unwrap().gc_interval, None);

        env::set_var("ASTER_PORTMAPPER_CONFIG", "sometimes");
        let err = AsterConfig::from_env().unwrap_err().to_string();
        assert!(err.contains("ASTER_PORTMAPPER_CONFIG"), "{err}");
    }

    #[test]
    fn health_keys_from_toml_env_and_builder() {
        let _lock = lock_env();
        let _env = super::tests_env_guard();

        // Defaults: disabled, localhost.
        let cfg = AsterConfig::default();
        assert_eq!(cfg.health_port, 0);
        assert_eq!(cfg.health_host, "127.0.0.1");
        assert_eq!(cfg.health_enabled, None);
        assert!(!cfg.is_health_enabled());

        // TOML `[health]` (the keys the Python/TS bindings read).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        fs::write(&path, "[health]\nport = 8080\nhost = \"0.0.0.0\"\n").unwrap();
        let cfg = AsterConfig::from_file(&path).unwrap();
        assert_eq!(cfg.health_port, 8080);
        assert_eq!(cfg.health_host, "0.0.0.0");
        assert!(cfg.is_health_enabled(), "non-zero port enables");

        // Env overrides TOML; `enabled = false` wins over a port.
        env::set_var("ASTER_HEALTH_PORT", "9090");
        env::set_var("ASTER_HEALTH_HOST", "::1");
        let cfg = AsterConfig::from_file(&path).unwrap();
        assert_eq!(cfg.health_port, 9090);
        assert_eq!(cfg.health_host, "::1");
        env::set_var("ASTER_HEALTH_ENABLED", "false");
        let cfg = AsterConfig::from_env().unwrap();
        assert_eq!(cfg.health_enabled, Some(false));
        assert!(!cfg.is_health_enabled());
        env::remove_var("ASTER_HEALTH_ENABLED");

        // Enabled with port 0 = ephemeral.
        fs::write(&path, "[health]\nenabled = true\n").unwrap();
        env::remove_var("ASTER_HEALTH_PORT");
        let cfg = AsterConfig::from_file(&path).unwrap();
        assert_eq!(cfg.health_port, 0);
        assert!(cfg.is_health_enabled());

        // Bad values are errors, not silently disabled.
        env::set_var("ASTER_HEALTH_PORT", "70000");
        let err = AsterConfig::from_env().unwrap_err().to_string();
        assert!(err.contains("ASTER_HEALTH_PORT"), "{err}");
        env::remove_var("ASTER_HEALTH_PORT");
        fs::write(&path, "[health]\nport = 65536\n").unwrap();
        let err = AsterConfig::from_file(&path).unwrap_err().to_string();
        assert!(err.contains("port"), "{err}");

        // Builder.
        let cfg = AsterConfig::builder()
            .health_port(8081)
            .health_host("0.0.0.0")
            .build();
        assert!(cfg.is_health_enabled());
        let cfg = cfg.into_builder().health(false).build();
        assert!(!cfg.is_health_enabled());
        #[cfg(feature = "health")]
        {
            assert_eq!(cfg.health_config(), None);
            let on = cfg.into_builder().health(true).build();
            assert_eq!(
                on.health_config(),
                Some(crate::ops::HealthConfig::new("0.0.0.0", 8081))
            );
        }
    }

    #[test]
    fn toml_relay_urls_secret_key_file_gc_and_hex_secret() {
        let _lock = lock_env();
        let _env = super::tests_env_guard();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        let secret = [0x24u8; 32];
        fs::write(
            &path,
            format!(
                r#"
[storage]
gc_interval_ms = 1500

[network]
secret_key = "{}"
secret_key_file = "/var/lib/app/node.key"
relay_urls = ["https://r1.example"]
portmapper_config = "enabled"
transport_initial_mtu = 1280
transport_send_fairness = false
"#,
                hex::encode(secret)
            ),
        )
        .unwrap();
        let cfg = AsterConfig::from_file(&path).unwrap();
        assert_eq!(cfg.gc_interval, Some(Duration::from_millis(1500)));
        assert_eq!(cfg.secret_key.as_ref().unwrap().to_bytes(), secret);
        assert_eq!(
            cfg.secret_key_file,
            Some(PathBuf::from("/var/lib/app/node.key"))
        );
        assert_eq!(
            cfg.relay_mode,
            RelayMode::Custom(vec!["https://r1.example".into()])
        );
        assert_eq!(cfg.portmapper_config.as_deref(), Some("enabled"));
        assert_eq!(cfg.transport_initial_mtu, Some(1280));
        assert_eq!(cfg.transport_send_fairness, Some(false));

        fs::write(
            &path,
            "[network]\nrelay_mode = \"staging\"\nrelay_urls = [\"https://r1.example\"]\n",
        )
        .unwrap();
        let err = AsterConfig::from_file(&path).unwrap_err().to_string();
        assert!(err.contains("relay_urls requires"), "{err}");

        fs::write(&path, "[network]\nrelay_urls = \"https://r1.example\"\n").unwrap();
        let err = AsterConfig::from_file(&path).unwrap_err().to_string();
        assert!(err.contains("array of strings"), "{err}");
    }

    #[test]
    fn inline_secret_key_accepts_hex_and_base64() {
        let bytes = [0x5au8; 32];
        let hex_key = parse_secret_key("t", &hex::encode(bytes)).unwrap();
        let b64_key = parse_secret_key(
            "t",
            &base64::engine::general_purpose::STANDARD.encode(bytes),
        )
        .unwrap();
        assert_eq!(hex_key.to_bytes(), bytes);
        assert_eq!(b64_key.to_bytes(), bytes);
        assert!(parse_secret_key("t", "not a key").is_err());
    }

    #[test]
    fn secret_key_file_is_created_then_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("node.key");

        // Missing file + no inline key → a fresh key is generated and written.
        let mut cfg = AsterConfig::builder().secret_key_file(&path).build();
        let created = cfg.resolve_secret_key_file().unwrap().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("{}\n", created.to_hex()));
        assert_eq!(text.trim().len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // Idempotent.
        assert_eq!(
            cfg.resolve_secret_key_file().unwrap().unwrap().to_bytes(),
            created.to_bytes()
        );

        // Existing file → loaded.
        let mut again = AsterConfig::builder().secret_key_file(&path).build();
        let loaded = again.resolve_secret_key_file().unwrap().unwrap();
        assert_eq!(loaded.to_bytes(), created.to_bytes());
        assert_eq!(
            again.to_core_endpoint_config().secret_key,
            Some(created.to_bytes().to_vec())
        );

        // Existing file + a different inline key → refused.
        let mut conflicting = AsterConfig::builder()
            .secret_key(SecretKey::from_bytes([1; 32]))
            .secret_key_file(&path)
            .build();
        let err = conflicting
            .resolve_secret_key_file()
            .unwrap_err()
            .to_string();
        assert!(err.contains("different keys"), "{err}");

        // Missing file + inline key → the inline key is what gets persisted.
        let other = dir.path().join("other.key");
        let inline = SecretKey::from_bytes([3; 32]);
        let mut cfg = AsterConfig::builder()
            .secret_key(inline.clone())
            .secret_key_file(&other)
            .build();
        cfg.resolve_secret_key_file().unwrap();
        assert_eq!(
            SecretKey::read_file(&other).unwrap().to_bytes(),
            inline.to_bytes()
        );
    }

    #[test]
    fn secret_key_file_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        assert!(SecretKey::read_file_if_exists(&path).unwrap().is_none());
        assert!(SecretKey::read_file(&path).is_err());

        let key = SecretKey::load_or_generate(&path).unwrap();
        assert_eq!(
            SecretKey::load_or_generate(&path).unwrap().to_bytes(),
            key.to_bytes()
        );
        // Never overwrites an existing identity.
        assert!(SecretKey::generate().write_file(&path).is_err());
        assert_eq!(
            SecretKey::read_file(&path).unwrap().to_bytes(),
            key.to_bytes()
        );

        // Uppercase hex and surrounding whitespace are accepted on read.
        fs::write(&path, format!("  {}\r\n", key.to_hex().to_uppercase())).unwrap();
        assert_eq!(
            SecretKey::read_file(&path).unwrap().to_bytes(),
            key.to_bytes()
        );

        // Anything else is not a secret key file.
        fs::write(&path, "deadbeef").unwrap();
        assert!(SecretKey::read_file(&path).is_err());
        fs::write(&path, "[node]\nsecret_key = \"x\"\n").unwrap();
        assert!(SecretKey::read_file(&path).is_err());
    }
}

#[cfg(test)]
mod root_pubkey_tests {
    use super::*;

    fn key(byte: u8) -> PublicKey {
        PublicKey::from_bytes([byte; 32])
    }

    /// **Both sources are read, and they must agree.**
    ///
    /// The inline key used to short-circuit before the file was ever opened, so
    /// a deployment that set both and rotated only one ran on the stale value
    /// silently — and nothing said so. There is one root key; two configured
    /// spellings of it that differ is a configuration error, not a precedence
    /// question, and guessing wrong is not visible until a signature fails to
    /// verify somewhere far away.
    #[test]
    fn a_root_key_configured_twice_must_agree() {
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join("root.pub");
        std::fs::write(&path, key(9).to_hex()).expect("write the key file");

        // File alone.
        let mut only_file = AsterConfig::builder().build();
        only_file.root_pubkey_file = Some(path.clone());
        assert_eq!(only_file.resolve_root_pubkey().expect("file"), Some(key(9)));

        // Inline alone.
        let mut only_inline = AsterConfig::builder().build();
        only_inline.root_pubkey = Some(key(7));
        assert_eq!(
            only_inline.resolve_root_pubkey().expect("inline"),
            Some(key(7))
        );

        // Both, agreeing.
        let mut agreeing = AsterConfig::builder().build();
        agreeing.root_pubkey = Some(key(9));
        agreeing.root_pubkey_file = Some(path.clone());
        assert_eq!(
            agreeing.resolve_root_pubkey().expect("agreement"),
            Some(key(9))
        );

        // Both, disagreeing: refused rather than resolved by precedence, and
        // the message names the file so an operator can see which record is
        // stale. Restoring the inline short-circuit returns `Some(key(7))` here
        // and this fails.
        let mut conflicting = AsterConfig::builder().build();
        conflicting.root_pubkey = Some(key(7));
        conflicting.root_pubkey_file = Some(path);
        let err = conflicting
            .resolve_root_pubkey()
            .expect_err("one deployment has one ROOT");
        let rendered = err.to_string();
        assert!(rendered.contains(&key(7).to_hex()), "{rendered}");
        assert!(rendered.contains(&key(9).to_hex()), "{rendered}");
        assert!(rendered.contains("root.pub"), "{rendered}");
    }
}
