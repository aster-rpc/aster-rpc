//! `AsterConfig` for JavaScript — a typed options object lowered onto
//! [`aster::AsterConfig`], plus the Rust env / TOML / identity loaders.
//!
//! Every endpoint knob in `docs/aster-rust-getstarted.md` §2.2 is a field
//! here. The `u64` / `usize` knobs (QUIC windows, buffer sizes, timeouts) are
//! `number | bigint` on the way in and a `number` on the way out unless the
//! value is above `Number.MAX_SAFE_INTEGER`, in which case it is a `bigint`.
//! (The previous `EndpointConfig` narrowed them to `u32`.)

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::{
    AsterConfig as RsConfig, AsterIdentityPeer, EnrollmentConfig as RsEnrollmentConfig,
    EnrollmentLimits as RsEnrollmentLimits, HookFailureMode, LoadedIdentity, PublicKey, RelayMode,
    RoleMappingConfig as RsRoleMappingConfig, SecretKey,
};

/// A non-negative integer knob: `number` (exact up to 2^53 - 1) or `bigint`.
pub type U64Value = Either<f64, BigInt>;

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Node / server / client configuration.
///
/// All fields are optional; omitted fields take the Rust defaults. Resolution
/// order when loading: defaults < `aster.toml` (`AsterConfig.fromFile`) <
/// `ASTER_*` environment (`AsterConfig.fromEnv`, and `fromFile` overlays it
/// too) < fields set in code.
#[napi(object, js_name = "AsterConfig")]
#[derive(Default)]
pub struct JsAsterConfig {
    // ── Trust ────────────────────────────────────────────────────────────
    /// Deployment root public key, 64 hex characters. `ASTER_ROOT_PUBKEY`,
    /// `[trust] root_pubkey`.
    pub root_pubkey: Option<String>,
    /// File holding the root public key (hex or `{"public_key": "<hex>"}`).
    /// `ASTER_ROOT_PUBKEY_FILE`, `[trust] root_pubkey_file`.
    pub root_pubkey_file: Option<String>,
    /// Pre-rotation commitment to the next root key (64 hex).
    /// `ASTER_NEXT_ROOT_COMMITMENT`, `[trust] next_root_commitment`.
    pub next_root_commitment: Option<String>,
    /// Rotations applied to reach the root (default 0). `ASTER_ROOT_SEQ`,
    /// `[trust] root_seq`.
    pub root_seq: Option<U64Value>,
    /// Applied rotation statements, oldest first; needs
    /// `nextRootCommitment`. `ASTER_ROOT_ROTATIONS` (comma-separated),
    /// `[trust] root_rotations`.
    pub root_rotations: Option<Vec<String>>,
    /// This node's enrollment credential: the chain file `aster enroll node
    /// --out` writes. Setting it enables enrollment.
    /// `ASTER_ENROLLMENT_CREDENTIAL`, `[trust] enrollment_credential`.
    pub enrollment_credential_file: Option<String>,
    /// This node's enrollment credential chain inline; wins over the file.
    /// `ASTER_ENROLLMENT_CREDENTIAL_CHAIN`, `[trust] enrollment_credential_chain`.
    pub enrollment_credential_chain: Option<String>,
    /// The `[enrollment]` section (see `EnrollmentConfig`).
    pub enrollment: Option<JsEnrollmentConfig>,
    // ── Connect ──────────────────────────────────────────────────────────
    /// Producer address for consumer-side dialing. `ASTER_ENDPOINT_ADDR`.
    pub endpoint_addr: Option<String>,
    // ── Storage ──────────────────────────────────────────────────────────
    /// Persistent node directory; in-memory when unset. `ASTER_STORAGE_PATH`,
    /// `[storage] path`.
    pub storage_path: Option<String>,
    /// Blob GC interval in milliseconds; `0` / unset disables GC.
    /// `ASTER_GC_INTERVAL_MS`, `[storage] gc_interval_ms`.
    pub gc_interval_ms: Option<Either<f64, BigInt>>,
    // ── Identity ─────────────────────────────────────────────────────────
    /// Node secret key, 64 hex characters. `ASTER_SECRET_KEY`,
    /// `[network] secret_key` (which also accept base64).
    pub secret_key: Option<String>,
    /// Secret key file (64 hex characters), loaded or created at start.
    /// `ASTER_SECRET_KEY_FILE`, `[network] secret_key_file`.
    pub secret_key_file: Option<String>,
    /// `.aster-identity` credential file. `ASTER_IDENTITY_FILE`, `[identity] file`.
    pub identity_file: Option<String>,
    // ── Network ──────────────────────────────────────────────────────────
    /// `"default"`, `"staging"`, `"disabled"` or `"custom"` (a bare URL means
    /// custom with that URL). `ASTER_RELAY_MODE`, `[network] relay_mode`.
    pub relay_mode: Option<String>,
    /// Custom relay URLs (selects `"custom"`). `ASTER_RELAY_URLS`,
    /// `[network] relay_urls`.
    pub relay_urls: Option<Vec<String>>,
    /// Local bind address, e.g. `"0.0.0.0:9000"`. `ASTER_BIND_ADDR`.
    pub bind_addr: Option<String>,
    /// mDNS local discovery. `ASTER_LOCAL_DISCOVERY`.
    pub local_discovery: Option<bool>,
    /// Connection monitoring / remote-info tracking. `ASTER_ENABLE_MONITORING`.
    pub enable_monitoring: Option<bool>,
    /// Admission hooks (Gate 0). `ASTER_ENABLE_HOOKS`.
    pub enable_hooks: Option<bool>,
    /// Hook decision timeout in ms (default 5000). `ASTER_HOOK_TIMEOUT_MS`.
    pub hook_timeout_ms: Option<Either<f64, BigInt>>,
    /// `"fail_open"` (default) or `"fail_closed"`. `ASTER_HOOK_FAILURE_MODE`.
    pub hook_failure_mode: Option<String>,
    /// Relay-only: disable direct IP transports. `ASTER_CLEAR_IP_TRANSPORTS`.
    pub clear_ip_transports: Option<bool>,
    /// Direct-only: disable relay transports. `ASTER_CLEAR_RELAY_TRANSPORTS`.
    pub clear_relay_transports: Option<bool>,
    /// UPnP / NAT-PMP port mapping; unset = default (enabled).
    /// `ASTER_PORTMAPPER_CONFIG` (`enabled` / `disabled`).
    pub portmapper: Option<bool>,
    /// HTTP/SOCKS proxy URL. `ASTER_PROXY_URL`.
    pub proxy_url: Option<String>,
    /// Read the proxy from `HTTP(S)_PROXY`. `ASTER_PROXY_FROM_ENV`.
    pub proxy_from_env: Option<bool>,
    // ── QUIC transport (`ASTER_TRANSPORT_*`, `[network] transport_*`) ──────
    /// Maximum concurrent incoming bidirectional streams.
    pub transport_max_concurrent_bidi_streams: Option<Either<f64, BigInt>>,
    /// Maximum concurrent incoming unidirectional streams.
    pub transport_max_concurrent_uni_streams: Option<Either<f64, BigInt>>,
    /// Per-stream receive window in bytes.
    pub transport_stream_receive_window: Option<Either<f64, BigInt>>,
    /// Per-connection receive window in bytes.
    pub transport_receive_window: Option<Either<f64, BigInt>>,
    /// Send window in bytes.
    pub transport_send_window: Option<Either<f64, BigInt>>,
    /// Idle timeout in milliseconds.
    pub transport_max_idle_timeout_ms: Option<Either<f64, BigInt>>,
    /// Keep-alive interval in milliseconds.
    pub transport_keep_alive_interval_ms: Option<Either<f64, BigInt>>,
    /// Initial UDP payload size before MTU discovery (u16).
    pub transport_initial_mtu: Option<u32>,
    /// Incoming datagram buffer in bytes; `0` disables incoming datagrams.
    pub transport_datagram_receive_buffer_size: Option<Either<f64, BigInt>>,
    /// Outgoing datagram buffer in bytes.
    pub transport_datagram_send_buffer_size: Option<Either<f64, BigInt>>,
    /// Fair scheduling across send streams at the same priority.
    pub transport_send_fairness: Option<bool>,
    /// UDP generic segmentation offload when supported.
    pub transport_enable_segmentation_offload: Option<bool>,
    /// Extra inbound ALPNs (merged with the built-in protocols). Not loaded
    /// from env / TOML.
    pub alpns: Option<Vec<Buffer>>,
    // ── Logging ──────────────────────────────────────────────────────────
    /// `"text"` or `"json"`. `ASTER_LOG_FORMAT`, `[logging] format`.
    pub log_format: Option<String>,
    /// `"debug"`, `"info"`, `"warning"`, `"error"`. `ASTER_LOG_LEVEL`.
    pub log_level: Option<String>,
    /// Mask sensitive fields in logs. `ASTER_LOG_MASK`.
    pub log_mask: Option<bool>,
    // ── Health (Rust `AsterConfig`) ──────────────────────────────────────
    /// Health HTTP port; `0` disables unless `healthEnabled`. `ASTER_HEALTH_PORT`,
    /// `[health] port`.
    pub health_port: Option<u32>,
    /// Health HTTP host. `ASTER_HEALTH_HOST`, `[health] host`.
    pub health_host: Option<String>,
    /// Force the listener on (`true`, ephemeral port when `healthPort` is 0) or
    /// off (`false`). Unset: on exactly when `healthPort` is non-zero.
    /// `ASTER_HEALTH_ENABLED`, `[health] enabled`.
    pub health_enabled: Option<bool>,
}

/// `[enrollment.limits]`: overrides of the Enrollment defaults
/// (`ASTER_ENROLLMENT_<FIELD>` upper-cased, e.g.
/// `ASTER_ENROLLMENT_HANDSHAKE_TIMEOUT_MS`).
#[napi(object, js_name = "EnrollmentLimits")]
#[derive(Default)]
pub struct JsEnrollmentLimits {
    pub handshake_timeout_ms: Option<U64Value>,
    pub step_timeout_ms: Option<U64Value>,
    pub policy_timeout_ms: Option<U64Value>,
    pub reconciliation_interval_ms: Option<U64Value>,
    pub max_admitted_peers: Option<U64Value>,
    /// Bound on enrolling with and synchronizing from a replica (default 15 000).
    pub sync_timeout_ms: Option<U64Value>,
}

/// `[enrollment.roles]`: how a peer's verified claims become the attributes
/// `requires` is checked against.
#[napi(object, js_name = "EnrollmentRoles")]
#[derive(Default)]
pub struct JsEnrollmentRoles {
    /// Claim attribute listing roles (default `"aster.role"`; `""` disables
    /// it). `ASTER_ENROLLMENT_ROLE_ATTRIBUTE`.
    pub attribute: Option<String>,
    /// Each verified scope is also a role. `ASTER_ENROLLMENT_SCOPES_AS_ROLES`.
    pub scopes_as_roles: Option<bool>,
    /// Scope → roles. `[enrollment.roles.scopes]`.
    pub scopes: Option<HashMap<String, Vec<String>>>,
    /// Tag → roles. `[enrollment.roles.tags]`.
    pub tags: Option<HashMap<String, Vec<String>>>,
    /// Pass the other claim attributes to handlers (default true).
    /// `ASTER_ENROLLMENT_INCLUDE_ATTRIBUTES`.
    pub include_attributes: Option<bool>,
}

/// The `[enrollment]` section, field for field the Rust `EnrollmentConfig`
/// (`docs/_internal/working_ideas/aster-enrollment.md` §15.3).
#[napi(object, js_name = "EnrollmentConfig")]
#[derive(Default)]
pub struct JsEnrollmentConfig {
    /// Force enrollment on or off. Unset: on when an audience, fleet
    /// namespace, issuer delegation or credential is set.
    /// `ASTER_ENROLLMENT_ENABLED`.
    pub enabled: Option<bool>,
    /// Fleet audience. Required. `ASTER_ENROLLMENT_AUDIENCE`.
    pub audience: Option<String>,
    /// Fleet policy namespace (64 hex). Required.
    /// `ASTER_ENROLLMENT_FLEET_NAMESPACE`.
    pub fleet_namespace: Option<String>,
    /// Durable denials and watermarks. `ASTER_ENROLLMENT_STATE_PATH`.
    pub state_path: Option<String>,
    /// Policy replicas (`aster1…` tickets or 64-hex node ids).
    /// `ASTER_ENROLLMENT_REPLICAS`.
    pub replicas: Option<Vec<String>>,
    /// Admission policy: the `PolicyConfig` tree (an object, or a bare name
    /// such as `"tolerate_stale_policy"`). `ASTER_ENROLLMENT_POLICY` (JSON).
    pub policy: Option<serde_json::Value>,
    /// Refuse to start on an unsynchronized clock (Linux).
    /// `ASTER_ENROLLMENT_REQUIRE_SYNCHRONIZED_CLOCK`.
    pub require_synchronized_clock: Option<bool>,
    /// `iat`/`nbf` leeway, 0–300 s (default 60).
    /// `ASTER_ENROLLMENT_ACTIVATION_LEEWAY_SECS`.
    pub activation_leeway_secs: Option<U64Value>,
    /// `[enrollment.limits]`.
    pub limits: Option<JsEnrollmentLimits>,
    /// `[enrollment.roles]`.
    pub roles: Option<JsEnrollmentRoles>,
    /// Server only: serve `aster.enrollment.IssuerV1` with this delegation.
    /// `[enrollment.issuer] delegation_file`,
    /// `ASTER_ENROLLMENT_ISSUER_DELEGATION_FILE`.
    pub issuer_delegation_file: Option<String>,
    /// Lifetime of issued credentials in seconds (default 24 h).
    /// `[enrollment.issuer] lifetime`, `ASTER_ENROLLMENT_ISSUER_LIFETIME`.
    pub issuer_lifetime_secs: Option<U64Value>,
    /// Issuer to renew this node's credential from (ticket or node id).
    /// `ASTER_ENROLLMENT_RENEW_FROM`.
    pub renew_from: Option<String>,
    /// `false` disables renewal. `ASTER_ENROLLMENT_RENEW`.
    pub renew: Option<bool>,
    /// Client: synchronize the fleet namespace the server offers (default
    /// true). `ASTER_ENROLLMENT_SYNCHRONIZE`.
    pub synchronize: Option<bool>,
}

fn opt_u64(field: &str, value: &Option<U64Value>) -> Result<Option<u64>> {
    value.as_ref().map(|v| u64_in(field, v)).transpose()
}

fn role_map(map: &Option<HashMap<String, Vec<String>>>) -> BTreeMap<String, Vec<String>> {
    map.iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

impl JsEnrollmentLimits {
    fn to_rust(&self) -> Result<RsEnrollmentLimits> {
        let f = |name: &str| format!("enrollment.limits.{name}");
        Ok(RsEnrollmentLimits {
            handshake_timeout_ms: opt_u64(&f("handshakeTimeoutMs"), &self.handshake_timeout_ms)?,
            step_timeout_ms: opt_u64(&f("stepTimeoutMs"), &self.step_timeout_ms)?,
            policy_timeout_ms: opt_u64(&f("policyTimeoutMs"), &self.policy_timeout_ms)?,
            reconciliation_interval_ms: opt_u64(
                &f("reconciliationIntervalMs"),
                &self.reconciliation_interval_ms,
            )?,
            max_admitted_peers: opt_u64(&f("maxAdmittedPeers"), &self.max_admitted_peers)?,
            sync_timeout_ms: opt_u64(&f("syncTimeoutMs"), &self.sync_timeout_ms)?,
        })
    }
}

impl JsEnrollmentRoles {
    fn to_rust(&self) -> RsRoleMappingConfig {
        let default = RsRoleMappingConfig::default();
        RsRoleMappingConfig {
            attribute: self.attribute.clone().unwrap_or(default.attribute),
            scopes_as_roles: self.scopes_as_roles.unwrap_or(default.scopes_as_roles),
            scopes: role_map(&self.scopes),
            tags: role_map(&self.tags),
            include_attributes: self
                .include_attributes
                .unwrap_or(default.include_attributes),
        }
    }
}

impl JsEnrollmentConfig {
    fn to_rust(&self) -> Result<RsEnrollmentConfig> {
        Ok(RsEnrollmentConfig {
            enabled: self.enabled,
            audience: self.audience.clone(),
            fleet_namespace: self.fleet_namespace.clone(),
            state_path: self
                .state_path
                .as_ref()
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
            replicas: self.replicas.clone().unwrap_or_default(),
            policy: self.policy.clone(),
            require_synchronized_clock: self.require_synchronized_clock.unwrap_or(false),
            activation_leeway_secs: opt_u64(
                "enrollment.activationLeewaySecs",
                &self.activation_leeway_secs,
            )?,
            limits: match &self.limits {
                None => RsEnrollmentLimits::default(),
                Some(limits) => limits.to_rust()?,
            },
            roles: self
                .roles
                .as_ref()
                .map_or_else(RsRoleMappingConfig::default, JsEnrollmentRoles::to_rust),
            issuer_delegation_file: self.issuer_delegation_file.as_ref().map(PathBuf::from),
            issuer_lifetime_secs: opt_u64(
                "enrollment.issuerLifetimeSecs",
                &self.issuer_lifetime_secs,
            )?,
            renew_from: self.renew_from.clone(),
            renew: self.renew,
            synchronize: self.synchronize,
        })
    }

    fn from_rust(e: &RsEnrollmentConfig) -> Self {
        let l = &e.limits;
        let out = |v: Option<u64>| v.map(u64_out);
        let map = |m: &BTreeMap<String, Vec<String>>| -> HashMap<String, Vec<String>> {
            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.to_string_lossy().into_owned());
        Self {
            enabled: e.enabled,
            audience: e.audience.clone(),
            fleet_namespace: e.fleet_namespace.clone(),
            state_path: path(&e.state_path),
            replicas: Some(e.replicas.clone()),
            policy: e.policy.clone(),
            require_synchronized_clock: Some(e.require_synchronized_clock),
            activation_leeway_secs: out(e.activation_leeway_secs),
            limits: Some(JsEnrollmentLimits {
                handshake_timeout_ms: out(l.handshake_timeout_ms),
                step_timeout_ms: out(l.step_timeout_ms),
                policy_timeout_ms: out(l.policy_timeout_ms),
                reconciliation_interval_ms: out(l.reconciliation_interval_ms),
                max_admitted_peers: out(l.max_admitted_peers),
                sync_timeout_ms: out(l.sync_timeout_ms),
            }),
            roles: Some(JsEnrollmentRoles {
                attribute: Some(e.roles.attribute.clone()),
                scopes_as_roles: Some(e.roles.scopes_as_roles),
                scopes: Some(map(&e.roles.scopes)),
                tags: Some(map(&e.roles.tags)),
                include_attributes: Some(e.roles.include_attributes),
            }),
            issuer_delegation_file: path(&e.issuer_delegation_file),
            issuer_lifetime_secs: out(e.issuer_lifetime_secs),
            renew_from: e.renew_from.clone(),
            renew: e.renew,
            synchronize: e.synchronize,
        }
    }
}

/// Whether `config` enables enrollment (Rust
/// `AsterConfig::is_enrollment_enabled`): `enrollment.enabled` when set,
/// otherwise when an audience, fleet namespace, issuer delegation or
/// credential is configured. The root alone does not enable it.
#[napi]
pub fn aster_config_is_enrollment_enabled(config: JsAsterConfig) -> Result<bool> {
    Ok(config.to_rust()?.is_enrollment_enabled())
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(Status::InvalidArg, msg.into())
}

pub(crate) fn aster_err(e: aster::Error) -> Error {
    match e {
        aster::Error::InvalidArgument(msg) => invalid(msg),
        other => Error::from_reason(other.to_string()),
    }
}

fn u64_in(field: &str, value: &U64Value) -> Result<u64> {
    match value {
        Either::A(n) => {
            if !n.is_finite() || *n < 0.0 || n.fract() != 0.0 || *n > u64::MAX as f64 {
                return Err(invalid(format!(
                    "{field} must be a non-negative integer, got {n}"
                )));
            }
            Ok(*n as u64)
        }
        Either::B(b) => {
            let (negative, v, lossless) = b.get_u64();
            if negative || !lossless {
                return Err(invalid(format!(
                    "{field} must fit in an unsigned 64-bit integer"
                )));
            }
            Ok(v)
        }
    }
}

fn usize_in(field: &str, value: &U64Value) -> Result<usize> {
    usize::try_from(u64_in(field, value)?)
        .map_err(|_| invalid(format!("{field} is too large for this platform")))
}

fn u64_out(v: u64) -> U64Value {
    if v <= MAX_SAFE_INTEGER {
        Either::A(v as f64)
    } else {
        Either::B(BigInt::from(v))
    }
}

fn parse_relay(mode: Option<&str>, urls: Option<&Vec<String>>) -> Result<Option<RelayMode>> {
    let urls = urls.filter(|u| !u.is_empty());
    match (mode.map(|m| m.trim()), urls) {
        (None, None) => Ok(None),
        // `default` alongside URLs is what `{ ...AsterConfig.fromEnv(),
        // relayUrls }` produces: the URLs are the more specific choice.
        (Some(m), Some(urls))
            if m.eq_ignore_ascii_case("custom") || m.eq_ignore_ascii_case("default") =>
        {
            Ok(Some(RelayMode::Custom(urls.clone())))
        }
        (None, Some(urls)) => Ok(Some(RelayMode::Custom(urls.clone()))),
        (Some(m), Some(_)) => Err(invalid(format!(
            "relayUrls requires relayMode \"custom\" (or \"default\"), got {m:?}"
        ))),
        (Some(m), None) => match m.to_ascii_lowercase().as_str() {
            "default" => Ok(Some(RelayMode::Default)),
            "staging" => Ok(Some(RelayMode::Staging)),
            "disabled" => Ok(Some(RelayMode::Disabled)),
            "custom" => Err(invalid("relayMode \"custom\" needs relayUrls")),
            "" => Ok(None),
            _ => Ok(Some(RelayMode::Custom(vec![m.to_string()]))),
        },
    }
}

impl JsAsterConfig {
    /// Lower onto [`aster::AsterConfig`]; unset fields keep the Rust defaults.
    pub(crate) fn to_rust(&self) -> Result<RsConfig> {
        let mut c = RsConfig::default();
        if let Some(v) = &self.root_pubkey {
            c.root_pubkey = Some(
                PublicKey::from_hex(v.trim()).map_err(|e| invalid(format!("rootPubkey: {e}")))?,
            );
        }
        c.root_pubkey_file = self.root_pubkey_file.as_ref().map(PathBuf::from);
        c.next_root_commitment = self.next_root_commitment.clone();
        if let Some(v) = &self.root_seq {
            c.root_seq = u64_in("rootSeq", v)?;
        }
        if let Some(v) = &self.root_rotations {
            c.root_rotations = v.clone();
        }
        c.enrollment_credential_file = self.enrollment_credential_file.as_ref().map(PathBuf::from);
        c.enrollment_credential_chain = self.enrollment_credential_chain.clone();
        if let Some(e) = &self.enrollment {
            c.enrollment = e.to_rust()?;
        }
        c.endpoint_addr = self.endpoint_addr.clone();
        c.storage_path = self
            .storage_path
            .as_ref()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        if let Some(v) = &self.gc_interval_ms {
            let ms = u64_in("gcIntervalMs", v)?;
            c.gc_interval = (ms > 0).then(|| Duration::from_millis(ms));
        }
        if let Some(v) = &self.secret_key {
            c.secret_key =
                Some(SecretKey::from_hex(v).map_err(|e| invalid(format!("secretKey: {e}")))?);
        }
        c.secret_key_file = self.secret_key_file.as_ref().map(PathBuf::from);
        c.identity_file = self.identity_file.as_ref().map(PathBuf::from);
        if let Some(mode) = parse_relay(self.relay_mode.as_deref(), self.relay_urls.as_ref())? {
            c.relay_mode = mode;
        }
        c.bind_addr = self.bind_addr.clone();
        if let Some(v) = self.local_discovery {
            c.local_discovery = v;
        }
        if let Some(v) = self.enable_monitoring {
            c.enable_monitoring = v;
        }
        if let Some(v) = self.enable_hooks {
            c.enable_hooks = v;
        }
        if let Some(v) = &self.hook_timeout_ms {
            c.hook_timeout_ms = u64_in("hookTimeoutMs", v)?;
        }
        if let Some(v) = &self.hook_failure_mode {
            c.hook_failure_mode = HookFailureMode::from_config_str(v).ok_or_else(|| {
                invalid(format!(
                    "hookFailureMode must be 'fail_open' or 'fail_closed', got {v:?}"
                ))
            })?;
        }
        if let Some(v) = self.clear_ip_transports {
            c.clear_ip_transports = v;
        }
        if let Some(v) = self.clear_relay_transports {
            c.clear_relay_transports = v;
        }
        c.portmapper_config = self
            .portmapper
            .map(|on| if on { "enabled" } else { "disabled" }.to_string());
        c.proxy_url = self.proxy_url.clone();
        if let Some(v) = self.proxy_from_env {
            c.proxy_from_env = v;
        }

        macro_rules! knob {
            ($field:ident, $conv:ident, $name:literal) => {
                if let Some(v) = &self.$field {
                    c.$field = Some($conv($name, v)?);
                }
            };
        }
        knob!(
            transport_max_concurrent_bidi_streams,
            u64_in,
            "transportMaxConcurrentBidiStreams"
        );
        knob!(
            transport_max_concurrent_uni_streams,
            u64_in,
            "transportMaxConcurrentUniStreams"
        );
        knob!(
            transport_stream_receive_window,
            u64_in,
            "transportStreamReceiveWindow"
        );
        knob!(transport_receive_window, u64_in, "transportReceiveWindow");
        knob!(transport_send_window, u64_in, "transportSendWindow");
        knob!(
            transport_max_idle_timeout_ms,
            u64_in,
            "transportMaxIdleTimeoutMs"
        );
        knob!(
            transport_keep_alive_interval_ms,
            u64_in,
            "transportKeepAliveIntervalMs"
        );
        knob!(
            transport_datagram_receive_buffer_size,
            usize_in,
            "transportDatagramReceiveBufferSize"
        );
        knob!(
            transport_datagram_send_buffer_size,
            usize_in,
            "transportDatagramSendBufferSize"
        );
        if let Some(v) = self.transport_initial_mtu {
            c.transport_initial_mtu = Some(u16::try_from(v).map_err(|_| {
                invalid(format!("transportInitialMtu must fit in 16 bits, got {v}"))
            })?);
        }
        c.transport_send_fairness = self.transport_send_fairness;
        c.transport_enable_segmentation_offload = self.transport_enable_segmentation_offload;
        if let Some(alpns) = &self.alpns {
            c.alpns = alpns.iter().map(|a| a.to_vec()).collect();
        }
        if let Some(v) = &self.log_format {
            c.log_format = v.to_ascii_lowercase();
        }
        if let Some(v) = &self.log_level {
            c.log_level = v.to_ascii_lowercase();
        }
        if let Some(v) = self.log_mask {
            c.log_mask = v;
        }
        if let Some(v) = self.health_port {
            c.health_port = u16::try_from(v)
                .map_err(|_| invalid(format!("healthPort must be 0..=65535, got {v}")))?;
        }
        if let Some(v) = &self.health_host {
            c.health_host = v.clone();
        }
        c.health_enabled = self.health_enabled;
        Ok(c)
    }

    /// Raise a resolved [`aster::AsterConfig`] into the JS object, with every
    /// field that has a value (defaults included) filled in.
    pub(crate) fn from_rust(c: &RsConfig) -> Self {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.to_string_lossy().into_owned());
        let (relay_mode, relay_urls) = match &c.relay_mode {
            RelayMode::Default => ("default", None),
            RelayMode::Staging => ("staging", None),
            RelayMode::Disabled => ("disabled", None),
            RelayMode::Custom(urls) => ("custom", Some(urls.clone())),
        };
        Self {
            root_pubkey: c.root_pubkey.map(|k| k.to_hex()),
            root_pubkey_file: path(&c.root_pubkey_file),
            next_root_commitment: c.next_root_commitment.clone(),
            root_seq: Some(u64_out(c.root_seq)),
            root_rotations: Some(c.root_rotations.clone()),
            enrollment_credential_file: path(&c.enrollment_credential_file),
            enrollment_credential_chain: c.enrollment_credential_chain.clone(),
            enrollment: Some(JsEnrollmentConfig::from_rust(&c.enrollment)),
            endpoint_addr: c.endpoint_addr.clone(),
            storage_path: path(&c.storage_path),
            gc_interval_ms: Some(u64_out(
                c.gc_interval
                    .map_or(0, |d| d.as_millis().min(u64::MAX as u128) as u64),
            )),
            secret_key: c.secret_key.as_ref().map(SecretKey::to_hex),
            secret_key_file: path(&c.secret_key_file),
            identity_file: path(&c.identity_file),
            relay_mode: Some(relay_mode.to_string()),
            relay_urls,
            bind_addr: c.bind_addr.clone(),
            local_discovery: Some(c.local_discovery),
            enable_monitoring: Some(c.enable_monitoring),
            enable_hooks: Some(c.enable_hooks),
            hook_timeout_ms: Some(u64_out(c.hook_timeout_ms)),
            hook_failure_mode: Some(c.hook_failure_mode.as_config_str().to_string()),
            clear_ip_transports: Some(c.clear_ip_transports),
            clear_relay_transports: Some(c.clear_relay_transports),
            portmapper: c.portmapper_config.as_deref().map(|v| v != "disabled"),
            proxy_url: c.proxy_url.clone(),
            proxy_from_env: Some(c.proxy_from_env),
            transport_max_concurrent_bidi_streams: c
                .transport_max_concurrent_bidi_streams
                .map(u64_out),
            transport_max_concurrent_uni_streams: c
                .transport_max_concurrent_uni_streams
                .map(u64_out),
            transport_stream_receive_window: c.transport_stream_receive_window.map(u64_out),
            transport_receive_window: c.transport_receive_window.map(u64_out),
            transport_send_window: c.transport_send_window.map(u64_out),
            transport_max_idle_timeout_ms: c.transport_max_idle_timeout_ms.map(u64_out),
            transport_keep_alive_interval_ms: c.transport_keep_alive_interval_ms.map(u64_out),
            transport_initial_mtu: c.transport_initial_mtu.map(u32::from),
            transport_datagram_receive_buffer_size: c
                .transport_datagram_receive_buffer_size
                .map(|v| u64_out(v as u64)),
            transport_datagram_send_buffer_size: c
                .transport_datagram_send_buffer_size
                .map(|v| u64_out(v as u64)),
            transport_send_fairness: c.transport_send_fairness,
            transport_enable_segmentation_offload: c.transport_enable_segmentation_offload,
            alpns: Some(c.alpns.iter().map(|a| Buffer::from(a.clone())).collect()),
            log_format: Some(c.log_format.clone()),
            log_level: Some(c.log_level.clone()),
            log_mask: Some(c.log_mask),
            health_port: Some(u32::from(c.health_port)),
            health_host: Some(c.health_host.clone()),
            health_enabled: c.health_enabled,
        }
    }
}

/// Defaults overlaid with `ASTER_*` environment variables (the Rust
/// `AsterConfig::from_env` loader). Every field is filled in.
#[napi]
pub fn aster_config_from_env() -> Result<JsAsterConfig> {
    let config = RsConfig::from_env().map_err(aster_err)?;
    Ok(JsAsterConfig::from_rust(&config))
}

/// Defaults < `aster.toml` at `path` < `ASTER_*` environment (the Rust
/// `AsterConfig::from_file` loader). Every field is filled in.
#[napi]
pub fn aster_config_from_file(path: String) -> Result<JsAsterConfig> {
    let config = RsConfig::from_file(&path).map_err(aster_err)?;
    Ok(JsAsterConfig::from_rust(&config))
}

/// Normalise a config through Rust: validates every field and returns it
/// with defaults filled in (`fromEnv`-shaped). Does not read env or files.
#[napi]
pub fn aster_config_resolve(config: JsAsterConfig) -> Result<JsAsterConfig> {
    let rust = config.to_rust()?;
    Ok(JsAsterConfig::from_rust(&rust))
}

/// The deployment root public key (hex) from `rootPubkey` / `rootPubkeyFile`
/// (Rust `AsterConfig::resolve_root_pubkey`): both are read and must agree; a
/// missing file falls back to `rootPubkey`; a malformed one is an error.
#[napi]
pub fn aster_config_resolve_root_pubkey(config: JsAsterConfig) -> Result<Option<String>> {
    let mut rust = config.to_rust()?;
    Ok(rust
        .resolve_root_pubkey()
        .map_err(aster_err)?
        .map(|k| k.to_hex()))
}

// ── Secret key files ─────────────────────────────────────────────────────

/// Generate a fresh 32-byte node secret key (OS CSPRNG), as 64 hex characters.
#[napi]
pub fn secret_key_generate() -> String {
    SecretKey::generate().to_hex()
}

/// Read a secret key file (64 hex characters). Returns `null` when the file
/// does not exist; a malformed file is an error.
#[napi]
pub fn secret_key_read_file(path: String) -> Result<Option<String>> {
    Ok(SecretKey::read_file_if_exists(&path)
        .map_err(aster_err)?
        .map(|k| k.to_hex()))
}

/// Write `secretKey` (64 hex) to a new secret key file (mode 0600, parent
/// directories created). Refuses to overwrite an existing file.
#[napi]
pub fn secret_key_write_file(path: String, secret_key: String) -> Result<()> {
    SecretKey::from_hex(&secret_key)
        .map_err(aster_err)?
        .write_file(&path)
        .map_err(aster_err)
}

/// Load the secret key file at `path`, creating it with a fresh key first if
/// it does not exist. Returns the key as 64 hex characters.
#[napi]
pub fn secret_key_load_or_generate(path: String) -> Result<String> {
    Ok(SecretKey::load_or_generate(&path)
        .map_err(aster_err)?
        .to_hex())
}

/// The public key / `NodeId` (hex) of a secret key (hex).
#[napi]
pub fn secret_key_public(secret_key: String) -> Result<String> {
    let key = SecretKey::from_hex(&secret_key).map_err(aster_err)?;
    Ok(key.public_key().to_hex())
}

// ── `.aster-identity` credential files (frozen auth, D3) ──────────────────

/// A `[[peers]]` entry selected from `.aster-identity`.
#[napi(object)]
pub struct IdentityPeer {
    pub name: Option<String>,
    pub role: Option<String>,
    /// Root public key (hex).
    pub root_pubkey: Option<String>,
    /// `type` in TOML, e.g. `"policy"`.
    pub credential_type: Option<String>,
    pub expires_at: Option<i64>,
    pub endpoint_id: Option<String>,
    pub signature: Option<String>,
    /// Peer attributes; `aster.role` / `aster.name` synthesized when absent.
    pub attributes: BTreeMap<String, String>,
    /// Published-service metadata (service name → JSON values).
    pub published_services: serde_json::Value,
}

/// `[node] secret_key` plus the selected peer from `.aster-identity`.
#[napi(object)]
pub struct AsterIdentity {
    /// The file that was loaded.
    pub path: String,
    /// `[node] secret_key` as 64 hex characters, if present.
    pub secret_key: Option<String>,
    /// The selected peer, if any matched.
    pub peer: Option<IdentityPeer>,
}

impl From<AsterIdentityPeer> for IdentityPeer {
    fn from(p: AsterIdentityPeer) -> Self {
        Self {
            name: p.name,
            role: p.role,
            root_pubkey: p.root_pubkey.map(|k| k.to_hex()),
            credential_type: p.credential_type,
            expires_at: p.expires_at,
            endpoint_id: p.endpoint_id,
            signature: p.signature,
            attributes: p.attributes,
            published_services: serde_json::to_value(p.published_services)
                .unwrap_or(serde_json::Value::Null),
        }
    }
}

impl From<LoadedIdentity> for AsterIdentity {
    fn from(l: LoadedIdentity) -> Self {
        Self {
            path: l.path.to_string_lossy().into_owned(),
            secret_key: l.secret_key.as_ref().map(SecretKey::to_hex),
            peer: l.peer.map(IdentityPeer::from),
        }
    }
}

/// Load a `.aster-identity` file (Rust `AsterConfig::load_identity`).
///
/// `path` defaults to `./.aster-identity`. Peer selection: `peerName` wins,
/// else the first peer with `role`, else the first peer. Returns `null` when
/// the file does not exist; a malformed file is an error.
#[napi]
pub fn load_aster_identity(
    path: Option<String>,
    peer_name: Option<String>,
    role: Option<String>,
) -> Result<Option<AsterIdentity>> {
    let config = RsConfig {
        identity_file: path.map(PathBuf::from),
        ..RsConfig::default()
    };
    Ok(config
        .load_identity(peer_name.as_deref(), role.as_deref())
        .map_err(aster_err)?
        .map(AsterIdentity::from))
}

/// Parse TOML text into plain JS values (tables → objects, arrays → arrays).
/// Used by the frozen credential readers that need the raw `[[peers]]`
/// entries (signed attributes must not be rewritten). Datetimes become
/// RFC 3339 strings.
#[napi]
pub fn parse_toml(text: String) -> Result<serde_json::Value> {
    let raw: toml::Value = text
        .parse()
        .map_err(|e| invalid(format!("invalid TOML: {e}")))?;
    Ok(toml_to_json(raw))
}

fn toml_to_json(v: toml::Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        toml::Value::String(s) => J::String(s),
        toml::Value::Integer(i) => J::from(i),
        toml::Value::Float(f) => serde_json::Number::from_f64(f).map_or(J::Null, J::Number),
        toml::Value::Boolean(b) => J::Bool(b),
        toml::Value::Datetime(d) => J::String(d.to_string()),
        toml::Value::Array(a) => J::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            J::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect())
        }
    }
}
