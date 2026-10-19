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

/// How the node publishes its addresses and finds a peer from a bare node ID
/// (`[network] discovery`, `ASTER_DISCOVERY`).
///
/// Only `Default` contacts n0 infrastructure (pkarr and DNS at `iroh.link`).
/// Relays are configured separately: with [`RelayMode::Default`] a node still
/// uses the n0 relays, so an air-gapped node also sets
/// [`RelayMode::Custom`] or [`RelayMode::Disabled`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DiscoveryMode {
    /// n0 pkarr publish/resolve and DNS lookup at `iroh.link`.
    #[default]
    Default,
    /// No address lookup at all: peers are dialed by full address (node ID
    /// plus direct address or relay URL).
    None,
    /// No global lookup; local-network mDNS only (turns
    /// [`AsterConfig::local_discovery`] on).
    Local,
    /// Only [`AsterConfig::discovery_dns_origin`] and
    /// [`AsterConfig::discovery_pkarr_relays`], e.g. a self-hosted
    /// iroh-dns-server. Setting either selects this mode.
    Custom,
}

impl DiscoveryMode {
    /// The config spelling: `default`, `none`, `local` or `custom`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::None => "none",
            Self::Local => "local",
            Self::Custom => "custom",
        }
    }

    /// Parse the config spelling (case-insensitive).
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(Self::Default),
            "none" => Ok(Self::None),
            "local" => Ok(Self::Local),
            "custom" => Ok(Self::Custom),
            other => Err(Error::InvalidArgument(format!(
                "discovery: expected default, none, local or custom, got {other:?}"
            ))),
        }
    }
}

/// Where a setting's value came from, lowest precedence first. Reported by
/// `aster.ops.NodeConfig` `get` next to each effective value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ConfigSource {
    /// The built-in default.
    #[default]
    Default,
    /// An `aster.toml` file.
    File,
    /// An `ASTER_*` environment variable.
    Env,
    /// Set in code (an [`AsterConfigBuilder`] method or a field assignment).
    Code,
    /// A remote override (`aster.ops.NodeConfig`).
    Override,
}

impl ConfigSource {
    /// The wire spelling: `default`, `file`, `env`, `code` or `override`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::File => "file",
            Self::Env => "env",
            Self::Code => "code",
            Self::Override => "override",
        }
    }
}

/// The [`AsterConfig::setting_sources`] key of `[identity] accept_on_behalf_of`.
pub const SETTING_ACCEPT_ON_BEHALF_OF: &str = "identity.accept_on_behalf_of";
/// The [`AsterConfig::setting_sources`] key of `[[identity.trusted_issuers]]`.
pub const SETTING_TRUSTED_ISSUERS: &str = "identity.trusted_issuers";
/// The [`AsterConfig::setting_sources`] key of
/// `[identity] edge_token_max_lifetime_secs`.
pub const SETTING_EDGE_TOKEN_MAX_LIFETIME: &str = "identity.edge_token_max_lifetime_secs";
/// The [`AsterConfig::setting_sources`] key of `[identity] clock_leeway_secs`.
pub const SETTING_CLOCK_LEEWAY: &str = "identity.clock_leeway_secs";
/// The [`AsterConfig::setting_sources`] key of
/// `[identity] max_key_set_age_secs`.
pub const SETTING_MAX_KEY_SET_AGE: &str = "identity.max_key_set_age_secs";

/// Default `[[identity.trusted_issuers]] max_token_lifetime_secs`.
pub const DEFAULT_PROVIDER_TOKEN_LIFETIME_SECS: u64 = 900;
/// Default `[identity] edge_token_max_lifetime_secs`, also its maximum.
pub const DEFAULT_EDGE_TOKEN_MAX_LIFETIME_SECS: u64 = 300;
/// Default `[identity] clock_leeway_secs`.
pub const DEFAULT_CLOCK_LEEWAY_SECS: u64 = 60;
/// Default `[identity] max_key_set_age_secs` (48 hours).
pub const DEFAULT_MAX_KEY_SET_AGE_SECS: u64 = 172_800;
/// Default `[identity.publisher] refresh_secs`.
pub const DEFAULT_PUBLISHER_REFRESH_SECS: u64 = 3600;
/// Shortest `[identity.publisher] refresh_secs`.
pub const MIN_PUBLISHER_REFRESH_SECS: u64 = 60;
/// Longest `[identity.publisher] refresh_secs`.
pub const MAX_PUBLISHER_REFRESH_SECS: u64 = 86_400;
/// Default `[identity.publisher] min_refresh_secs`.
pub const DEFAULT_PUBLISHER_MIN_REFRESH_SECS: u64 = 60;
/// Most issuers one publisher fetches for.
pub const MAX_PUBLISHER_ISSUERS: usize = 32;
/// Longest `[identity.publisher] jwks_url` value.
pub const MAX_JWKS_URL_BYTES: usize = 2048;

/// The issuer-key publisher (`[identity.publisher]`; design: "Issuer keys" in
/// `docs/_internal/web_gateway/gateway-identity.md`). Only on the node that
/// publishes keys, normally the gateway: it fetches each issuer's JWK set
/// over validated HTTPS and writes it into the fleet namespace for every
/// core to verify with. The node's credential must grant
/// `aster.identity-publisher`, and the root must have sealed it the fleet
/// namespace's write grant (`aster enroll identity-publisher grant`).
///
/// ```toml
/// [identity.publisher]
/// issuers = ["https://accounts.example.com"]
/// jwks_url = { "https://accounts.example.com" = "https://accounts.example.com/.well-known/jwks.json" }
/// refresh_secs = 3600        # default; 60..=86400
/// min_refresh_secs = 60      # default; unknown-kid refreshes at most this often
/// ca_certs = ["/etc/aster/idp-ca.pem"]   # extra roots for the IdP's TLS
/// ca_only = false            # true: trust only ca_certs, not the WebPKI roots
/// ```
///
/// Every issuer needs a `jwks_url` (`https`, no user info or fragment), and
/// every `jwks_url` key must be an issuer. `refresh_secs` must be at most
/// half of this node's `[identity] max_key_set_age_secs`, so a failed
/// refresh is retried before cores stop trusting the last set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityPublisherConfig {
    /// The issuers (`iss`) to publish keys for.
    pub issuers: Vec<String>,
    /// Issuer → the HTTPS URL of its JWK set.
    #[serde(default)]
    pub jwks_url: BTreeMap<String, String>,
    /// Scheduled refresh interval, with jitter.
    #[serde(default = "default_publisher_refresh")]
    pub refresh_secs: u64,
    /// Shortest gap between two refreshes of one issuer that tokens with an
    /// unknown `kid` trigger, 1..=`refresh_secs`. A notice inside the gap is
    /// served when it ends, so a rotation is never missed and a stream of
    /// made-up `kid`s costs at most one fetch per gap.
    #[serde(default = "default_publisher_min_refresh")]
    pub min_refresh_secs: u64,
    /// Extra PEM trust anchors for the identity providers' TLS, as for
    /// `relay_ca_certs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ca_certs: Vec<PathBuf>,
    /// Trust only `ca_certs`, not the built-in WebPKI roots.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ca_only: bool,
}

fn default_publisher_refresh() -> u64 {
    DEFAULT_PUBLISHER_REFRESH_SECS
}

fn default_publisher_min_refresh() -> u64 {
    DEFAULT_PUBLISHER_MIN_REFRESH_SECS
}

impl IdentityPublisherConfig {
    /// Publish `issuers`, each from its `jwks_url`, every
    /// [`DEFAULT_PUBLISHER_REFRESH_SECS`].
    pub fn new<I, S, U>(issuers: I) -> Self
    where
        I: IntoIterator<Item = (S, U)>,
        S: Into<String>,
        U: Into<String>,
    {
        let jwks_url: BTreeMap<String, String> = issuers
            .into_iter()
            .map(|(iss, url)| (iss.into(), url.into()))
            .collect();
        Self {
            issuers: jwks_url.keys().cloned().collect(),
            jwks_url,
            refresh_secs: DEFAULT_PUBLISHER_REFRESH_SECS,
            min_refresh_secs: DEFAULT_PUBLISHER_MIN_REFRESH_SECS,
            ca_certs: Vec::new(),
            ca_only: false,
        }
    }

    /// Refresh every `secs`.
    pub fn refresh_secs(mut self, secs: u64) -> Self {
        self.refresh_secs = secs;
        self
    }

    /// Let unknown-`kid` refreshes of one issuer run at most every `secs`.
    pub fn min_refresh_secs(mut self, secs: u64) -> Self {
        self.min_refresh_secs = secs;
        self
    }

    /// Trust `path` (PEM) for the identity providers' TLS too.
    pub fn ca_cert(mut self, path: impl Into<PathBuf>) -> Self {
        self.ca_certs.push(path.into());
        self
    }

    /// Trust only [`Self::ca_certs`].
    pub fn ca_only(mut self, only: bool) -> Self {
        self.ca_only = only;
        self
    }

    /// Validate the section against this node's `max_key_set_age_secs`.
    /// `AsterServer` and `AsterClient` refuse to start on an error.
    pub fn validate(&self, max_key_set_age_secs: u64) -> Result<()> {
        let invalid =
            |message: String| Error::InvalidArgument(format!("[identity.publisher] {message}"));
        if self.issuers.is_empty() {
            return Err(invalid("issuers is empty".into()));
        }
        if self.issuers.len() > MAX_PUBLISHER_ISSUERS {
            return Err(invalid(format!("at most {MAX_PUBLISHER_ISSUERS} issuers")));
        }
        if !(MIN_PUBLISHER_REFRESH_SECS..=MAX_PUBLISHER_REFRESH_SECS).contains(&self.refresh_secs) {
            return Err(invalid(format!(
                "refresh_secs must be in {MIN_PUBLISHER_REFRESH_SECS}..={MAX_PUBLISHER_REFRESH_SECS}"
            )));
        }
        if !(1..=self.refresh_secs).contains(&self.min_refresh_secs) {
            return Err(invalid(
                "min_refresh_secs must be in 1..=refresh_secs".into(),
            ));
        }
        if self.refresh_secs.saturating_mul(2) > max_key_set_age_secs {
            return Err(invalid(format!(
                "refresh_secs ({}) must be at most half of [identity] max_key_set_age_secs ({}), \
                 so a failed refresh is retried before cores stop trusting the last key set",
                self.refresh_secs, max_key_set_age_secs
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for iss in &self.issuers {
            if iss.is_empty() || iss.len() > 256 || iss.chars().any(char::is_control) {
                return Err(invalid(format!("issuer {iss:?} is not a valid iss")));
            }
            if iss.contains('|') || (iss.len() == 64 && iss.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Err(invalid(format!(
                    "issuer {iss:?}: a node id or a value with '|' is not a provider issuer"
                )));
            }
            if !seen.insert(iss.as_str()) {
                return Err(invalid(format!("issuer {iss:?} is listed twice")));
            }
            let Some(url) = self.jwks_url.get(iss) else {
                return Err(invalid(format!("issuer {iss:?} has no jwks_url")));
            };
            check_jwks_url(url)
                .map_err(|reason| invalid(format!("jwks_url of {iss:?}: {reason}")))?;
        }
        if let Some(extra) = self
            .jwks_url
            .keys()
            .find(|iss| !seen.contains(iss.as_str()))
        {
            return Err(invalid(format!(
                "jwks_url names {extra:?}, which is not in issuers"
            )));
        }
        if self.ca_only && self.ca_certs.is_empty() {
            return Err(invalid("ca_only needs at least one ca_certs file".into()));
        }
        Ok(())
    }
}

/// The checks on a `jwks_url` that need no URL parser: `https://`, a host,
/// no user info, fragment, whitespace or control characters, bounded.
fn check_jwks_url(url: &str) -> std::result::Result<(), String> {
    if url.len() > MAX_JWKS_URL_BYTES {
        return Err(format!("longer than {MAX_JWKS_URL_BYTES} bytes"));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("contains whitespace or a control character".into());
    }
    let Some(rest) = url
        .get(..8)
        .filter(|scheme| scheme.eq_ignore_ascii_case("https://"))
        .map(|_| &url[8..])
    else {
        return Err("must be an https:// URL".into());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err("has no host".into());
    }
    if authority.contains('@') {
        return Err("may not carry user info".into());
    }
    if url.contains('#') {
        return Err("may not carry a fragment".into());
    }
    Ok(())
}

/// An external identity provider whose tokens a server accepts in
/// `aster-on-behalf-of` (`[[identity.trusted_issuers]]`; design:
/// "External identity providers (forwarded)" in
/// `docs/_internal/web_gateway/gateway-identity.md`). Its keys come from the
/// fleet namespace, published by a node holding `aster.identity-publisher`;
/// this entry says only whom to trust and how to map roles.
///
/// ```toml
/// [[identity.trusted_issuers]]
/// iss = "https://accounts.example.com"
/// audiences = ["odata-core"]                 # required
/// roles_claim = "groups"
/// role_map = { "data-readers" = "data.read", "data-admins" = "data.admin" }
/// max_token_lifetime_secs = 900              # default
/// ```
///
/// Validated at start-up (and as a whole on a NodeConfig override; see
/// `aster::rpc::identity::IdentitySettings::validate`): `iss` non-empty, at
/// most 256 bytes, no `|` (the principal is `iss|sub`), not a node id (64
/// hex, which selects the edge-minted path), unique; `audiences` non-empty;
/// `role_map` values are roles (no comma, no `aster.` prefix);
/// `max_token_lifetime_secs` in 1..=86400.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedIssuer {
    /// The exact `iss` claim.
    pub iss: String,
    /// Accepted `aud` values: the edge's OIDC client id, or a service
    /// audience obtained by token exchange. Required: an issuer without one
    /// fails start-up.
    pub audiences: Vec<String>,
    /// The claim roles are read from: an array of strings, or one string of
    /// space-separated values (e.g. `scope`). Unset: the issuer's tokens
    /// grant no roles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles_claim: Option<String>,
    /// Claim value → Aster role. **Values without a mapping are dropped**: a
    /// provider group grants nothing until it is mapped here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub role_map: BTreeMap<String, String>,
    /// Longest `exp - iat` accepted.
    #[serde(default = "default_provider_token_lifetime")]
    pub max_token_lifetime_secs: u64,
}

fn default_provider_token_lifetime() -> u64 {
    DEFAULT_PROVIDER_TOKEN_LIFETIME_SECS
}

impl TrustedIssuer {
    /// An issuer accepting `audiences`, with no role mapping.
    pub fn new<I, S>(iss: impl Into<String>, audiences: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            iss: iss.into(),
            audiences: audiences.into_iter().map(Into::into).collect(),
            roles_claim: None,
            role_map: BTreeMap::new(),
            max_token_lifetime_secs: DEFAULT_PROVIDER_TOKEN_LIFETIME_SECS,
        }
    }

    /// Read roles from `claim`.
    pub fn roles_claim(mut self, claim: impl Into<String>) -> Self {
        self.roles_claim = Some(claim.into());
        self
    }

    /// Map the claim value `value` to the Aster role `role`.
    pub fn map_role(mut self, value: impl Into<String>, role: impl Into<String>) -> Self {
        self.role_map.insert(value.into(), role.into());
        self
    }

    /// Accept tokens living at most `secs` (`exp - iat`).
    pub fn max_token_lifetime_secs(mut self, secs: u64) -> Self {
        self.max_token_lifetime_secs = secs;
        self
    }
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

    /// Serve blobs over zstd-compressed transport at this level, and make
    /// blob fetches prefer it. On by default at level 3; `None` serves and
    /// fetches plain only.
    ///
    /// The node registers the compressed blobs ALPN
    /// ([`alpns::BLOBS_ZSTD`](crate::admission::alpns::BLOBS_ZSTD)) beside the
    /// plain one, on the same router and behind the same Gate-0 admission; it
    /// is never an admission ALPN. Fetches default to
    /// [`BlobTransportCompression::PreferZstd`](crate::BlobTransportCompression::PreferZstd),
    /// falling back to plain for providers that do not serve it. Stored bytes
    /// and hashes are unaffected. The level is the sender's; 3 is a good
    /// default, 5 suits slow or relayed links. Levels are clamped to 1..=19.
    ///
    /// Env: `ASTER_BLOB_ZSTD_LEVEL`; TOML: `[storage] blob_zstd_level`. `0`
    /// or `off` turns compression off.
    pub blob_zstd_level: Option<u8>,

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
    /// Local bind address, e.g. `"0.0.0.0:9000"`. Pins one address family:
    /// an IPv4 address pins the IPv4 socket (the IPv6 one keeps a random port
    /// unless [`bind_addr_v6`](Self::bind_addr_v6) is set).
    ///
    /// Env: `ASTER_BIND_ADDR`; TOML: `[network] bind_addr`.
    pub bind_addr: Option<String>,
    /// IPv6 bind address, e.g. `"[::]:7842"`: pins the IPv6 socket the way
    /// [`bind_addr`](Self::bind_addr) pins the IPv4 one, so a default-deny
    /// NetworkPolicy can allow one UDP port per family. Must be IPv6;
    /// `bind_addr` must then be IPv4 (or unset). The bind is required: a host
    /// without IPv6 fails to start with it set.
    ///
    /// Env: `ASTER_BIND_ADDR_V6`; TOML: `[network] bind_addr_v6`.
    pub bind_addr_v6: Option<String>,
    /// Extra direct addresses (`IP:port`) the node is reachable on but cannot
    /// discover itself, such as a Kubernetes NodePort or a load balancer.
    /// Published with the discovered addresses, in the node's address and
    /// every discovery record, and tried by peers like any other.
    ///
    /// They are hints: a peer dialing this node still verifies its key in the
    /// TLS handshake, so a wrong or reused address cannot reach another node.
    /// Requires direct IP transports.
    ///
    /// Env: `ASTER_ADVERTISE_ADDRS` (comma-separated); TOML: `[network]
    /// advertise_addrs = [..]`.
    pub advertise_addrs: Vec<String>,
    /// PEM files whose certificates are trusted for relay TLS, in addition to
    /// the built-in WebPKI roots (or instead of them with
    /// [`relay_ca_only`](Self::relay_ca_only)): for a self-hosted relay with a
    /// private-CA certificate. Also used for custom discovery's pkarr relays.
    /// This is transport security for the relay hop only; peers are still
    /// authenticated by their node key end to end.
    ///
    /// Env: `ASTER_RELAY_CA_CERTS` (comma-separated paths); TOML: `[network]
    /// relay_ca_certs = [..]`.
    pub relay_ca_certs: Vec<PathBuf>,
    /// Trust only [`relay_ca_certs`](Self::relay_ca_certs) for relay TLS, not
    /// the WebPKI roots. Default `false`.
    ///
    /// Env: `ASTER_RELAY_CA_ONLY`; TOML: `[network] relay_ca_only`.
    pub relay_ca_only: bool,
    /// Address lookup for finding peers by bare node ID. Default
    /// [`DiscoveryMode::Default`] (n0 at `iroh.link`).
    ///
    /// Env: `ASTER_DISCOVERY`; TOML: `[network] discovery`.
    pub discovery: DiscoveryMode,
    /// DNS origin for [`DiscoveryMode::Custom`], e.g.
    /// `"dns.example.internal"`: peers are resolved from `_iroh.<id>.<origin>`
    /// TXT records.
    ///
    /// Env: `ASTER_DISCOVERY_DNS_ORIGIN`; TOML: `[network]
    /// discovery_dns_origin`.
    pub discovery_dns_origin: Option<String>,
    /// Pkarr relay URLs for [`DiscoveryMode::Custom`] (publish and resolve),
    /// e.g. `"https://dns.example.internal/pkarr"`.
    ///
    /// Env: `ASTER_DISCOVERY_PKARR_RELAYS` (comma-separated); TOML:
    /// `[network] discovery_pkarr_relays = [..]`.
    pub discovery_pkarr_relays: Vec<String>,
    /// On a persistent node, re-bind the UDP ports the node last bound when
    /// they are still free. Default `true`.
    ///
    /// Applies only when [`storage_path`](Self::storage_path) is set and
    /// [`bind_addr`](Self::bind_addr) leaves the port to the OS (unset, or
    /// port `0`). The bound IPv4 and IPv6 ports are recorded in
    /// `bind-ports.json` in the storage directory; on the next start each
    /// family tries its recorded port first and falls back to a fresh one if
    /// it is taken. Coming back on the same port keeps peers' cached direct
    /// addresses and the NAT mapping valid, so they reconnect sooner. An
    /// explicit non-zero port is always used as given and never recorded.
    ///
    /// Env: `ASTER_REUSE_LAST_PORT`; TOML: `[network] reuse_last_port`.
    pub reuse_last_port: bool,
    /// How long start-up waits for a home relay connection, in milliseconds
    /// (default 30000). A node with relay transport does not finish starting
    /// until it reaches a relay; past this bound start-up fails with
    /// [`aster_transport_core::RelayOnlineTimeout`], naming the relay URL(s),
    /// instead of hanging. Must be greater than zero. Direct-only nodes
    /// (`relay_mode = "disabled"`) never wait for a relay.
    ///
    /// Env: `ASTER_ONLINE_TIMEOUT_MS`; TOML: `[network] online_timeout_ms`.
    pub online_timeout_ms: u64,
    /// Enable local-network peer discovery (mDNS).
    pub local_discovery: bool,
    /// Enable connection monitoring / remote-info tracking.
    pub enable_monitoring: bool,
    /// Enable admission hooks.
    pub enable_hooks: bool,
    /// Timeout in milliseconds for admission hook decisions.
    pub hook_timeout_ms: u64,
    /// Hook fallback behavior when the host callback is unavailable or times
    /// out. Defaults to [`HookFailureMode::FailClosed`] (reject); `fail_open`
    /// is an explicit opt-in.
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
    /// Accept calls made on behalf of a user: the `aster-on-behalf-of`
    /// metadata entry, verified before Gate 3 (see `aster::rpc::identity`).
    /// Default `false`: a call carrying the entry is refused with
    /// `PERMISSION_DENIED` rather than run as the peer.
    ///
    /// Env: `ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF`; TOML:
    /// `[identity] accept_on_behalf_of`.
    pub identity_accept_on_behalf_of: bool,
    /// External identity providers whose tokens `aster-on-behalf-of` may
    /// carry (see [`TrustedIssuer`]). Needs Enrollment: the issuers' keys come
    /// from its fleet namespace.
    ///
    /// Env: `ASTER_IDENTITY_TRUSTED_ISSUERS` (a JSON array of the same
    /// objects); TOML: `[[identity.trusted_issuers]]`.
    pub identity_trusted_issuers: Vec<TrustedIssuer>,
    /// Longest lifetime (`exp - iat`) of an edge-minted token, 1..=300.
    /// Default 300.
    ///
    /// Env: `ASTER_IDENTITY_EDGE_TOKEN_MAX_LIFETIME_SECS`; TOML:
    /// `[identity] edge_token_max_lifetime_secs`.
    pub identity_edge_token_max_lifetime_secs: u64,
    /// Tolerance for a token issuer's clock running ahead, 0..=300, applied
    /// to `iat` and `nbf` only, never to `exp`. Default 60.
    ///
    /// Env: `ASTER_IDENTITY_CLOCK_LEEWAY_SECS`; TOML:
    /// `[identity] clock_leeway_secs`.
    pub identity_clock_leeway_secs: u64,
    /// Oldest issuer key set (by its fetch time) a server verifies with,
    /// 60..=2592000. Default 172800 (48 hours). A publisher that stops
    /// refreshing cannot keep a removed key alive past it; keep it well above
    /// the publisher's `refresh_secs` (at least twice).
    ///
    /// Env: `ASTER_IDENTITY_MAX_KEY_SET_AGE_SECS`; TOML:
    /// `[identity] max_key_set_age_secs`.
    pub identity_max_key_set_age_secs: u64,
    /// The issuer-key publisher, on the node that publishes keys (see
    /// [`IdentityPublisherConfig`]). Needs the `identity-publisher` feature.
    ///
    /// Env: `ASTER_IDENTITY_PUBLISHER` (a JSON object of the same fields);
    /// TOML: `[identity.publisher]`.
    pub identity_publisher: Option<IdentityPublisherConfig>,

    /// Serve `aster.ops.NodeConfig`, the remote overrides of overridable
    /// sections (`[identity]` today). Default `true`; `false` turns the
    /// service off, and the node then ignores any override it stored earlier.
    ///
    /// Env: `ASTER_REMOTE_CONFIG_ENABLED`; TOML: `[remote_config] enabled`.
    pub remote_config_enabled: bool,
    /// Overridable sections this node refuses to have overridden, e.g.
    /// `["identity"]`. A stored override of a section locked later is
    /// ignored. An unknown name fails `AsterServer` start.
    ///
    /// Env: `ASTER_REMOTE_CONFIG_LOCK` (comma-separated); TOML:
    /// `[remote_config] lock`.
    pub remote_config_lock: Vec<String>,
    /// Where each overridable setting came from, keyed `section.field`
    /// (e.g. [`SETTING_ACCEPT_ON_BEHALF_OF`]). Kept by the file, env and
    /// builder paths. An absent key is a default, or a direct field
    /// assignment, which is reported as [`ConfigSource::Code`] when the value
    /// differs from the default.
    pub setting_sources: BTreeMap<String, ConfigSource>,

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
    /// Label `aster.peer.bytes` per peer (`aster.peer`) instead of grouping it
    /// by locality level. For small fleets and debugging: the first
    /// [`metrics_peer_label_cap`](Self::metrics_peer_label_cap) peers this
    /// node sees keep their own series while its health server runs, every
    /// later peer is counted under `_OTHER`. Default off.
    ///
    /// Env: `ASTER_METRICS_PEER_LABELS`; TOML: `[metrics] peer_labels`.
    pub metrics_peer_labels: bool,
    /// How many peers get their own `aster.peer` series when
    /// [`metrics_peer_labels`](Self::metrics_peer_labels) is on. Default
    /// [`DEFAULT_METRICS_PEER_LABEL_CAP`].
    ///
    /// Env: `ASTER_METRICS_PEER_LABEL_CAP`; TOML: `[metrics] peer_label_cap`.
    pub metrics_peer_label_cap: usize,
    /// Label the client call metrics (`aster.rpc.client.requests_completed`,
    /// `.duration` and `.in_flight`) with the called peer (`aster.peer`), for
    /// the RPC connections this node opens. Peer ids are unbounded, so the
    /// first [`metrics_peer_label_cap`](Self::metrics_peer_label_cap) peers
    /// keep their own series and every later peer is counted under `_OTHER`.
    /// Default off.
    ///
    /// Env: `ASTER_METRICS_CLIENT_PEER_LABELS`; TOML: `[metrics] client_peer_labels`.
    pub metrics_client_peer_labels: bool,

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

    /// [`from_env`](Self::from_env), reading `ASTER_*` variables from `vars`
    /// instead of the process environment. Variables absent from `vars` are
    /// unset, whatever the process environment holds.
    pub fn from_env_vars<K, V>(vars: impl IntoIterator<Item = (K, V)>) -> Result<Self>
    where
        K: Into<String>,
        V: Into<String>,
    {
        let vars = collect_vars(vars);
        let mut config = Self::default();
        config.apply_env_from(|key| vars.get(key).cloned())?;
        Ok(config)
    }

    /// [`from_file`](Self::from_file), overlaying `ASTER_*` variables from
    /// `vars` instead of the process environment.
    pub fn from_file_with_env_vars<K, V>(
        path: impl AsRef<Path>,
        vars: impl IntoIterator<Item = (K, V)>,
    ) -> Result<Self>
    where
        K: Into<String>,
        V: Into<String>,
    {
        let vars = collect_vars(vars);
        let mut config = Self::default();
        config.apply_file(path)?;
        config.apply_env_from(|key| vars.get(key).cloned())?;
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
            if let Some(value) = storage.get("blob_zstd_level") {
                self.blob_zstd_level = match value {
                    toml::Value::Integer(level) if *level >= 0 => {
                        blob_zstd_level_from(*level as u64)
                    }
                    toml::Value::String(text) => {
                        parse_blob_zstd_level(&format!("{label}: blob_zstd_level"), text)?
                    }
                    _ => {
                        return Err(Error::InvalidArgument(format!(
                            "{label}: blob_zstd_level must be a non-negative integer or \"off\""
                        )))
                    }
                };
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
            if let Some(value) = toml_string(network, "bind_addr_v6", label)? {
                self.bind_addr_v6 = Some(value);
            }
            if let Some(value) = toml_string_list(network, "advertise_addrs", label)? {
                self.advertise_addrs = non_empty(value);
            }
            if let Some(value) = toml_string_list(network, "relay_ca_certs", label)? {
                self.relay_ca_certs = non_empty(value)
                    .into_iter()
                    .map(|p| expand_tilde(Path::new(&p)))
                    .collect();
            }
            if let Some(value) = toml_bool(network, "relay_ca_only", label)? {
                self.relay_ca_only = value;
            }
            self.apply_discovery(
                toml_string(network, "discovery", label)?.as_deref(),
                toml_string(network, "discovery_dns_origin", label)?,
                toml_string_list(network, "discovery_pkarr_relays", label)?,
                &format!("{label} [network]"),
            )?;
            if let Some(value) = toml_bool(network, "reuse_last_port", label)? {
                self.reuse_last_port = value;
            }
            if let Some(value) = toml_u64(network, "online_timeout_ms", label)? {
                self.online_timeout_ms = value;
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
            if let Some(value) = toml_bool(identity, "accept_on_behalf_of", label)? {
                self.identity_accept_on_behalf_of = value;
                self.setting_sources
                    .insert(SETTING_ACCEPT_ON_BEHALF_OF.into(), ConfigSource::File);
            }
            if let Some(value) = identity.get("trusted_issuers") {
                self.identity_trusted_issuers = value.clone().try_into().map_err(|e| {
                    Error::InvalidArgument(format!("{label} [[identity.trusted_issuers]]: {e}"))
                })?;
                self.setting_sources
                    .insert(SETTING_TRUSTED_ISSUERS.into(), ConfigSource::File);
            }
            if let Some(value) = toml_u64(identity, "edge_token_max_lifetime_secs", label)? {
                self.identity_edge_token_max_lifetime_secs = value;
                self.setting_sources
                    .insert(SETTING_EDGE_TOKEN_MAX_LIFETIME.into(), ConfigSource::File);
            }
            if let Some(value) = toml_u64(identity, "clock_leeway_secs", label)? {
                self.identity_clock_leeway_secs = value;
                self.setting_sources
                    .insert(SETTING_CLOCK_LEEWAY.into(), ConfigSource::File);
            }
            if let Some(value) = toml_u64(identity, "max_key_set_age_secs", label)? {
                self.identity_max_key_set_age_secs = value;
                self.setting_sources
                    .insert(SETTING_MAX_KEY_SET_AGE.into(), ConfigSource::File);
            }
            if let Some(value) = identity.get("publisher") {
                self.identity_publisher = Some(value.clone().try_into().map_err(|e| {
                    Error::InvalidArgument(format!("{label} [identity.publisher]: {e}"))
                })?);
            }
        }

        if let Some(remote) = toml_section(raw, "remote_config", label)? {
            if let Some(value) = toml_bool(remote, "enabled", label)? {
                self.remote_config_enabled = value;
            }
            if let Some(value) = toml_string_list(remote, "lock", label)? {
                self.remote_config_lock = non_empty(value);
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

        if let Some(metrics) = toml_section(raw, "metrics", label)? {
            if let Some(value) = toml_bool(metrics, "peer_labels", label)? {
                self.metrics_peer_labels = value;
            }
            if let Some(value) = toml_usize(metrics, "peer_label_cap", label)? {
                self.metrics_peer_label_cap = value;
            }
            if let Some(value) = toml_bool(metrics, "client_peer_labels", label)? {
                self.metrics_client_peer_labels = value;
            }
        }

        Ok(())
    }

    /// Apply `ASTER_*` environment variables over the current configuration.
    pub fn apply_env(&mut self) -> Result<()> {
        self.apply_env_from(|key| env::var(key).ok())
    }

    /// Apply `ASTER_*` variables read through `lookup` over the current
    /// configuration, with the same names and parsing as
    /// [`apply_env`](Self::apply_env).
    ///
    /// For hosts whose environment is not the process environment: Bun keeps
    /// `process.env` writes in its own map, so a binding there passes its
    /// `ASTER_*` entries here instead of letting Rust read `environ`.
    pub fn apply_env_from(&mut self, lookup: impl Fn(&str) -> Option<String>) -> Result<()> {
        let env = EnvVars(&lookup);
        if let Some(value) = env.path("ASTER_ROOT_PUBKEY_FILE") {
            self.root_pubkey_file = Some(value);
        }
        if let Some(value) = env.path("ASTER_ENROLLMENT_CREDENTIAL") {
            self.enrollment_credential_file = Some(value);
        }
        if let Some(value) = env.string("ASTER_ENROLLMENT_CREDENTIAL_CHAIN") {
            self.enrollment_credential_chain = Some(value);
        }
        if let Some(value) = env.string("ASTER_NEXT_ROOT_COMMITMENT") {
            self.next_root_commitment = Some(value);
        }
        if let Some(value) = env.u64("ASTER_ROOT_SEQ")? {
            self.root_seq = value;
        }
        if let Some(value) = env.string("ASTER_ROOT_ROTATIONS") {
            self.root_rotations = value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect();
        }
        for key in RETIRED_TRUST_KEYS {
            if env.is_set(key.1) {
                return Err(retired_setting(key.1));
            }
        }
        if let Some(value) = env.string("ASTER_ENDPOINT_ADDR") {
            self.endpoint_addr = Some(value);
        }
        if let Some(value) = env.path("ASTER_IDENTITY_FILE") {
            self.identity_file = Some(value);
        }

        if let Some(value) = env.string("ASTER_ROOT_PUBKEY") {
            self.root_pubkey = Some(parse_public_key("ASTER_ROOT_PUBKEY", &value)?);
        }

        if let Some(value) = env.path("ASTER_STORAGE_PATH") {
            self.storage_path = Some(value);
        }
        if let Some(value) = env.u64("ASTER_GC_INTERVAL_MS")? {
            self.gc_interval = gc_interval_from_ms(value);
        }
        if let Some(value) = env.string("ASTER_BLOB_ZSTD_LEVEL") {
            self.blob_zstd_level = parse_blob_zstd_level("ASTER_BLOB_ZSTD_LEVEL", &value)?;
        }
        if let Some(value) = env.string_allow_empty("ASTER_SECRET_KEY") {
            if value.trim().is_empty() {
                self.secret_key = None;
            } else {
                self.secret_key = Some(parse_secret_key("ASTER_SECRET_KEY", &value)?);
            }
        }
        if let Some(value) = env.path("ASTER_SECRET_KEY_FILE") {
            self.secret_key_file = Some(value);
        }
        let relay_mode = env.string("ASTER_RELAY_MODE");
        let relay_urls = env.string("ASTER_RELAY_URLS").map(|value| {
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
        if let Some(value) = env.string("ASTER_BIND_ADDR") {
            self.bind_addr = Some(value);
        }
        if let Some(value) = env.string("ASTER_BIND_ADDR_V6") {
            self.bind_addr_v6 = Some(value);
        }
        if let Some(value) = env.list("ASTER_ADVERTISE_ADDRS") {
            self.advertise_addrs = value;
        }
        if let Some(value) = env.list("ASTER_RELAY_CA_CERTS") {
            self.relay_ca_certs = value
                .into_iter()
                .map(|p| expand_tilde(Path::new(&p)))
                .collect();
        }
        if let Some(value) = env.bool("ASTER_RELAY_CA_ONLY")? {
            self.relay_ca_only = value;
        }
        self.apply_discovery(
            env.string("ASTER_DISCOVERY").as_deref(),
            env.string("ASTER_DISCOVERY_DNS_ORIGIN"),
            env.list("ASTER_DISCOVERY_PKARR_RELAYS"),
            "ASTER_DISCOVERY",
        )?;
        if let Some(value) = env.bool("ASTER_REUSE_LAST_PORT")? {
            self.reuse_last_port = value;
        }
        if let Some(value) = env.u64("ASTER_ONLINE_TIMEOUT_MS")? {
            self.online_timeout_ms = value;
        }
        if let Some(value) = env.bool("ASTER_LOCAL_DISCOVERY")? {
            self.local_discovery = value;
        }
        if let Some(value) = env.bool("ASTER_ENABLE_MONITORING")? {
            self.enable_monitoring = value;
        }
        if let Some(value) = env.bool("ASTER_ENABLE_HOOKS")? {
            self.enable_hooks = value;
        }
        if let Some(value) = env.u64("ASTER_HOOK_TIMEOUT_MS")? {
            self.hook_timeout_ms = value;
        }
        if let Some(value) = env.string("ASTER_HOOK_FAILURE_MODE") {
            self.hook_failure_mode = parse_hook_failure_mode("ASTER_HOOK_FAILURE_MODE", &value)?;
        }
        if let Some(value) = env.bool("ASTER_CLEAR_IP_TRANSPORTS")? {
            self.clear_ip_transports = value;
        }
        if let Some(value) = env.bool("ASTER_CLEAR_RELAY_TRANSPORTS")? {
            self.clear_relay_transports = value;
        }
        if let Some(value) = env.string("ASTER_PORTMAPPER_CONFIG") {
            self.portmapper_config = Some(parse_portmapper("ASTER_PORTMAPPER_CONFIG", &value)?);
        }
        if let Some(value) = env.string("ASTER_PROXY_URL") {
            self.proxy_url = Some(value);
        }
        if let Some(value) = env.bool("ASTER_PROXY_FROM_ENV")? {
            self.proxy_from_env = value;
        }

        macro_rules! overlay {
            ($field:ident, $parse:ident, $key:literal) => {
                if let Some(value) = env.$parse($key)? {
                    self.$field = Some(value);
                }
            };
        }
        overlay!(
            transport_max_concurrent_bidi_streams,
            u64,
            "ASTER_TRANSPORT_MAX_CONCURRENT_BIDI_STREAMS"
        );
        overlay!(
            transport_max_concurrent_uni_streams,
            u64,
            "ASTER_TRANSPORT_MAX_CONCURRENT_UNI_STREAMS"
        );
        overlay!(
            transport_stream_receive_window,
            u64,
            "ASTER_TRANSPORT_STREAM_RECEIVE_WINDOW"
        );
        overlay!(
            transport_receive_window,
            u64,
            "ASTER_TRANSPORT_RECEIVE_WINDOW"
        );
        overlay!(transport_send_window, u64, "ASTER_TRANSPORT_SEND_WINDOW");
        overlay!(
            transport_max_idle_timeout_ms,
            u64,
            "ASTER_TRANSPORT_MAX_IDLE_TIMEOUT_MS"
        );
        overlay!(
            transport_keep_alive_interval_ms,
            u64,
            "ASTER_TRANSPORT_KEEP_ALIVE_INTERVAL_MS"
        );
        overlay!(transport_initial_mtu, u16, "ASTER_TRANSPORT_INITIAL_MTU");
        overlay!(
            transport_datagram_receive_buffer_size,
            usize,
            "ASTER_TRANSPORT_DATAGRAM_RECEIVE_BUFFER_SIZE"
        );
        overlay!(
            transport_datagram_send_buffer_size,
            usize,
            "ASTER_TRANSPORT_DATAGRAM_SEND_BUFFER_SIZE"
        );
        overlay!(
            transport_send_fairness,
            bool,
            "ASTER_TRANSPORT_SEND_FAIRNESS"
        );
        overlay!(
            transport_enable_segmentation_offload,
            bool,
            "ASTER_TRANSPORT_ENABLE_SEGMENTATION_OFFLOAD"
        );

        if let Some(value) = env.string("ASTER_LOG_FORMAT") {
            self.log_format = value.to_ascii_lowercase();
        }
        if let Some(value) = env.string("ASTER_LOG_LEVEL") {
            self.log_level = value.to_ascii_lowercase();
        }
        if let Some(value) = env.bool("ASTER_LOG_MASK")? {
            self.log_mask = value;
        }

        if let Some(value) = env.u16("ASTER_HEALTH_PORT")? {
            self.health_port = value;
        }
        if let Some(value) = env.string("ASTER_HEALTH_HOST") {
            self.health_host = value;
        }
        if let Some(value) = env.bool("ASTER_HEALTH_ENABLED")? {
            self.health_enabled = Some(value);
        }
        if let Some(value) = env.bool("ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF")? {
            self.identity_accept_on_behalf_of = value;
            self.setting_sources
                .insert(SETTING_ACCEPT_ON_BEHALF_OF.into(), ConfigSource::Env);
        }
        if let Some(value) = env.string("ASTER_IDENTITY_TRUSTED_ISSUERS") {
            self.identity_trusted_issuers = serde_json::from_str(&value).map_err(|e| {
                Error::InvalidArgument(format!(
                    "ASTER_IDENTITY_TRUSTED_ISSUERS: expected a JSON array of trusted issuers: {e}"
                ))
            })?;
            self.setting_sources
                .insert(SETTING_TRUSTED_ISSUERS.into(), ConfigSource::Env);
        }
        if let Some(value) = env.u64("ASTER_IDENTITY_EDGE_TOKEN_MAX_LIFETIME_SECS")? {
            self.identity_edge_token_max_lifetime_secs = value;
            self.setting_sources
                .insert(SETTING_EDGE_TOKEN_MAX_LIFETIME.into(), ConfigSource::Env);
        }
        if let Some(value) = env.u64("ASTER_IDENTITY_CLOCK_LEEWAY_SECS")? {
            self.identity_clock_leeway_secs = value;
            self.setting_sources
                .insert(SETTING_CLOCK_LEEWAY.into(), ConfigSource::Env);
        }
        if let Some(value) = env.u64("ASTER_IDENTITY_MAX_KEY_SET_AGE_SECS")? {
            self.identity_max_key_set_age_secs = value;
            self.setting_sources
                .insert(SETTING_MAX_KEY_SET_AGE.into(), ConfigSource::Env);
        }
        if let Some(value) = env.string("ASTER_IDENTITY_PUBLISHER") {
            self.identity_publisher = Some(serde_json::from_str(&value).map_err(|e| {
                Error::InvalidArgument(format!(
                    "ASTER_IDENTITY_PUBLISHER: expected a JSON object of [identity.publisher] \
                     fields: {e}"
                ))
            })?);
        }
        if let Some(value) = env.bool("ASTER_REMOTE_CONFIG_ENABLED")? {
            self.remote_config_enabled = value;
        }
        if let Some(value) = env.list("ASTER_REMOTE_CONFIG_LOCK") {
            self.remote_config_lock = value;
        }
        if let Some(value) = env.bool("ASTER_METRICS_PEER_LABELS")? {
            self.metrics_peer_labels = value;
        }
        if let Some(value) = env.usize("ASTER_METRICS_PEER_LABEL_CAP")? {
            self.metrics_peer_label_cap = value;
        }
        if let Some(value) = env.bool("ASTER_METRICS_CLIENT_PEER_LABELS")? {
            self.metrics_client_peer_labels = value;
        }
        self.enrollment.apply_env(&env)?;

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
        self.is_health_enabled().then(|| {
            let config = crate::ops::HealthConfig::new(self.health_host.clone(), self.health_port);
            if self.metrics_peer_labels {
                config.with_peer_labels(self.metrics_peer_label_cap)
            } else {
                config
            }
        })
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
            enable_discovery: self.local_discovery || self.discovery == DiscoveryMode::Local,
            enable_monitoring: self.enable_monitoring,
            enable_hooks: self.enable_hooks,
            hook_timeout_ms: self.hook_timeout_ms,
            hook_failure_mode: self.hook_failure_mode,
            bind_addr: self.bind_addr.clone(),
            bind_addr_v6: self.bind_addr_v6.clone(),
            advertise_addrs: self.advertise_addrs.clone(),
            relay_ca_cert_files: self
                .relay_ca_certs
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            relay_ca_only: self.relay_ca_only,
            discovery: Some(
                match self.discovery {
                    DiscoveryMode::Local => "none",
                    mode => mode.as_str(),
                }
                .into(),
            ),
            // Servers count only in custom mode, so a later source that sets
            // `discovery = "none"` overrides servers an earlier one gave.
            discovery_dns_origin: self
                .discovery_dns_origin
                .clone()
                .filter(|_| self.discovery == DiscoveryMode::Custom),
            discovery_pkarr_relays: if self.discovery == DiscoveryMode::Custom {
                self.discovery_pkarr_relays.clone()
            } else {
                Vec::new()
            },
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
            reuse_last_port: self.reuse_last_port,
            online_timeout_ms: self.online_timeout_ms,
            blob_zstd_level: self.blob_zstd_level.map(i32::from),
        }
    }

    /// Binding support: the lowered [`CoreEndpointConfig`] (see
    /// `aster::binding_support`). Not part of the stable API.
    #[cfg(feature = "binding-support")]
    #[doc(hidden)]
    pub fn core_endpoint_config(&self) -> CoreEndpointConfig {
        self.to_core_endpoint_config()
    }

    /// Apply a discovery mode and/or custom discovery servers from one source
    /// (env or TOML). Servers alone select [`DiscoveryMode::Custom`]; with a
    /// mode in the same source, the mode must be `custom`.
    fn apply_discovery(
        &mut self,
        mode: Option<&str>,
        dns_origin: Option<String>,
        pkarr_relays: Option<Vec<String>>,
        source: &str,
    ) -> Result<()> {
        let parsed = mode
            .map(DiscoveryMode::parse)
            .transpose()
            .map_err(|e| Error::InvalidArgument(format!("{source}: {e}")))?;
        let servers = dns_origin.is_some() || pkarr_relays.is_some();
        if servers && parsed.is_some_and(|m| m != DiscoveryMode::Custom) {
            return Err(Error::InvalidArgument(format!(
                "{source}: discovery_dns_origin / discovery_pkarr_relays require discovery \"custom\", got {:?}",
                mode.unwrap_or_default()
            )));
        }
        if let Some(mode) = parsed {
            self.discovery = mode;
        }
        if let Some(origin) = dns_origin {
            self.discovery_dns_origin = Some(origin);
        }
        if let Some(relays) = pkarr_relays {
            self.discovery_pkarr_relays = non_empty(relays);
        }
        if servers {
            self.discovery = DiscoveryMode::Custom;
        }
        Ok(())
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
            blob_zstd_level: Some(DEFAULT_BLOB_ZSTD_LEVEL),
            secret_key: None,
            secret_key_file: None,
            relay_mode: RelayMode::Default,
            bind_addr: None,
            bind_addr_v6: None,
            advertise_addrs: Vec::new(),
            relay_ca_certs: Vec::new(),
            relay_ca_only: false,
            discovery: DiscoveryMode::Default,
            discovery_dns_origin: None,
            discovery_pkarr_relays: Vec::new(),
            reuse_last_port: true,
            online_timeout_ms: aster_transport_core::DEFAULT_ONLINE_TIMEOUT_MS,
            local_discovery: false,
            enable_monitoring: false,
            enable_hooks: false,
            hook_timeout_ms: 5000,
            hook_failure_mode: HookFailureMode::FailClosed,
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
            identity_accept_on_behalf_of: false,
            identity_trusted_issuers: Vec::new(),
            identity_edge_token_max_lifetime_secs: DEFAULT_EDGE_TOKEN_MAX_LIFETIME_SECS,
            identity_clock_leeway_secs: DEFAULT_CLOCK_LEEWAY_SECS,
            identity_max_key_set_age_secs: DEFAULT_MAX_KEY_SET_AGE_SECS,
            identity_publisher: None,
            remote_config_enabled: true,
            remote_config_lock: Vec::new(),
            setting_sources: BTreeMap::new(),
            health_port: 0,
            health_host: "127.0.0.1".into(),
            health_enabled: None,
            metrics_peer_labels: false,
            metrics_peer_label_cap: DEFAULT_METRICS_PEER_LABEL_CAP,
            metrics_client_peer_labels: false,
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

    /// Serve and prefer zstd-compressed blob transport at `level` (on by
    /// default at 3). See [`AsterConfig::blob_zstd_level`]. `0` turns it off.
    pub fn blob_zstd_level(mut self, level: u8) -> Self {
        self.config.blob_zstd_level = blob_zstd_level_from(level.into());
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

    /// Pin the IPv6 socket, e.g. `"[::]:7842"`. See
    /// [`AsterConfig::bind_addr_v6`].
    pub fn bind_addr_v6(mut self, addr: impl Into<String>) -> Self {
        self.config.bind_addr_v6 = Some(addr.into());
        self
    }

    /// Advertise an extra direct address (`IP:port`), e.g. a NodePort
    /// (repeatable). See [`AsterConfig::advertise_addrs`].
    pub fn advertise_addr(mut self, addr: impl Into<String>) -> Self {
        self.config.advertise_addrs.push(addr.into());
        self
    }

    /// Trust the CA certificates in this PEM file for relay TLS
    /// (repeatable). See [`AsterConfig::relay_ca_certs`].
    pub fn relay_ca_cert(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.relay_ca_certs.push(path.into());
        self
    }

    /// Trust only the [`relay_ca_cert`](Self::relay_ca_cert) files for relay
    /// TLS. See [`AsterConfig::relay_ca_only`].
    pub fn relay_ca_only(mut self, yes: bool) -> Self {
        self.config.relay_ca_only = yes;
        self
    }

    /// Choose the address lookup. See [`DiscoveryMode`].
    pub fn discovery_mode(mut self, mode: DiscoveryMode) -> Self {
        self.config.discovery = mode;
        self
    }

    /// Resolve peers from this DNS origin (selects
    /// [`DiscoveryMode::Custom`]). See [`AsterConfig::discovery_dns_origin`].
    pub fn discovery_dns_origin(mut self, origin: impl Into<String>) -> Self {
        self.config.discovery_dns_origin = Some(origin.into());
        self.config.discovery = DiscoveryMode::Custom;
        self
    }

    /// Publish to and resolve from this pkarr relay (repeatable; selects
    /// [`DiscoveryMode::Custom`]). See [`AsterConfig::discovery_pkarr_relays`].
    pub fn discovery_pkarr_relay(mut self, url: impl Into<String>) -> Self {
        self.config.discovery_pkarr_relays.push(url.into());
        self.config.discovery = DiscoveryMode::Custom;
        self
    }

    /// Re-bind the last bound ports on restart (default `true`). See
    /// [`AsterConfig::reuse_last_port`].
    pub fn reuse_last_port(mut self, yes: bool) -> Self {
        self.config.reuse_last_port = yes;
        self
    }

    /// Bound, in milliseconds, on start-up's wait for a home relay (default
    /// 30000). See [`AsterConfig::online_timeout_ms`].
    pub fn online_timeout_ms(mut self, ms: u64) -> Self {
        self.config.online_timeout_ms = ms;
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
    /// The default is [`HookFailureMode::FailClosed`]: a stalled or dropped
    /// admission loop rejects the connection. [`HookFailureMode::FailOpen`] is
    /// an explicit opt-in for observability-only hooks that must never block
    /// a connection.
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

    /// Accept calls made on behalf of a user (see
    /// [`AsterConfig::identity_accept_on_behalf_of`]).
    pub fn accept_on_behalf_of(mut self, accept: bool) -> Self {
        self.config.identity_accept_on_behalf_of = accept;
        self.config
            .setting_sources
            .insert(SETTING_ACCEPT_ON_BEHALF_OF.into(), ConfigSource::Code);
        self
    }

    /// Trust an external identity provider's tokens (adds to
    /// [`AsterConfig::identity_trusted_issuers`]).
    pub fn trusted_issuer(mut self, issuer: TrustedIssuer) -> Self {
        self.config.identity_trusted_issuers.push(issuer);
        self.config
            .setting_sources
            .insert(SETTING_TRUSTED_ISSUERS.into(), ConfigSource::Code);
        self
    }

    /// Replace the trusted issuers (see
    /// [`AsterConfig::identity_trusted_issuers`]).
    pub fn trusted_issuers(mut self, issuers: impl IntoIterator<Item = TrustedIssuer>) -> Self {
        self.config.identity_trusted_issuers = issuers.into_iter().collect();
        self.config
            .setting_sources
            .insert(SETTING_TRUSTED_ISSUERS.into(), ConfigSource::Code);
        self
    }

    /// Longest lifetime of an edge-minted token (see
    /// [`AsterConfig::identity_edge_token_max_lifetime_secs`]).
    pub fn edge_token_max_lifetime_secs(mut self, secs: u64) -> Self {
        self.config.identity_edge_token_max_lifetime_secs = secs;
        self.config
            .setting_sources
            .insert(SETTING_EDGE_TOKEN_MAX_LIFETIME.into(), ConfigSource::Code);
        self
    }

    /// Clock leeway for on-behalf-of tokens (see
    /// [`AsterConfig::identity_clock_leeway_secs`]).
    pub fn clock_leeway_secs(mut self, secs: u64) -> Self {
        self.config.identity_clock_leeway_secs = secs;
        self.config
            .setting_sources
            .insert(SETTING_CLOCK_LEEWAY.into(), ConfigSource::Code);
        self
    }

    /// Oldest issuer key set a server verifies with (see
    /// [`AsterConfig::identity_max_key_set_age_secs`]).
    pub fn max_key_set_age_secs(mut self, secs: u64) -> Self {
        self.config.identity_max_key_set_age_secs = secs;
        self.config
            .setting_sources
            .insert(SETTING_MAX_KEY_SET_AGE.into(), ConfigSource::Code);
        self
    }

    /// Run the issuer-key publisher (see
    /// [`AsterConfig::identity_publisher`]).
    pub fn identity_publisher(mut self, publisher: IdentityPublisherConfig) -> Self {
        self.config.identity_publisher = Some(publisher);
        self
    }

    /// Serve `aster.ops.NodeConfig` (on by default; see
    /// [`AsterConfig::remote_config_enabled`]).
    pub fn remote_config(mut self, enabled: bool) -> Self {
        self.config.remote_config_enabled = enabled;
        self
    }

    /// Lock overridable sections against remote override (see
    /// [`AsterConfig::remote_config_lock`]).
    pub fn remote_config_lock<I, S>(mut self, sections: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.remote_config_lock = sections.into_iter().map(Into::into).collect();
        self
    }

    /// Label `aster.peer.bytes` per peer, the first `cap` peers each under
    /// its own `aster.peer` series and the rest under `_OTHER` (see
    /// [`AsterConfig::metrics_peer_labels`]).
    pub fn metrics_peer_labels(mut self, cap: usize) -> Self {
        self.config.metrics_peer_labels = true;
        self.config.metrics_peer_label_cap = cap;
        self
    }

    /// Label client call metrics with the called peer (`aster.peer`), bounded
    /// by the peer-label cap (see
    /// [`AsterConfig::metrics_client_peer_labels`]).
    pub fn metrics_client_peer_labels(mut self, on: bool) -> Self {
        self.config.metrics_client_peer_labels = on;
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

/// Where `ASTER_*` variables are read from: the process environment
/// ([`AsterConfig::apply_env`]) or a caller-supplied lookup
/// ([`AsterConfig::apply_env_from`]).
pub(crate) struct EnvVars<'a>(&'a dyn Fn(&str) -> Option<String>);

impl EnvVars<'_> {
    fn is_set(&self, key: &str) -> bool {
        (self.0)(key).is_some()
    }

    pub(crate) fn string_allow_empty(&self, key: &str) -> Option<String> {
        (self.0)(key)
    }

    pub(crate) fn string(&self, key: &str) -> Option<String> {
        self.string_allow_empty(key).and_then(|value| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        })
    }

    /// A comma-separated list; empty items are dropped. `None` when unset or
    /// blank.
    fn list(&self, key: &str) -> Option<Vec<String>> {
        self.string(key).map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
    }

    // Paths from the environment and TOML expand a leading `~` at load time, so
    // every consumer sees the real path. systemd and other service managers pass
    // `~/...` through literally (no shell), so a check like `path.exists()` on the
    // raw value fails (portal-sync, 2026-10-15: `root_pubkey_file` refused to start).
    pub(crate) fn path(&self, key: &str) -> Option<PathBuf> {
        self.string(key)
            .map(|value| expand_tilde(Path::new(&value)))
    }

    pub(crate) fn bool(&self, key: &str) -> Result<Option<bool>> {
        let Some(value) = self.string(key) else {
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

    pub(crate) fn u64(&self, key: &str) -> Result<Option<u64>> {
        self.parse(key)
    }

    fn u16(&self, key: &str) -> Result<Option<u16>> {
        self.parse(key)
    }

    fn usize(&self, key: &str) -> Result<Option<usize>> {
        self.parse(key)
    }

    fn parse<T>(&self, key: &str) -> Result<Option<T>>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        let Some(value) = self.string(key) else {
            return Ok(None);
        };
        value
            .parse::<T>()
            .map(Some)
            .map_err(|e| Error::InvalidArgument(format!("{key}: invalid value {value:?}: {e}")))
    }
}

fn collect_vars<K, V>(vars: impl IntoIterator<Item = (K, V)>) -> BTreeMap<String, String>
where
    K: Into<String>,
    V: Into<String>,
{
    vars.into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
}

fn non_empty(values: Vec<String>) -> Vec<String> {
    values.into_iter().filter(|v| !v.is_empty()).collect()
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

/// Default for [`AsterConfig::metrics_peer_label_cap`].
pub const DEFAULT_METRICS_PEER_LABEL_CAP: usize = 64;

/// Default for [`AsterConfig::blob_zstd_level`].
pub const DEFAULT_BLOB_ZSTD_LEVEL: u8 = 3;

/// `0` turns compression off; anything else is clamped to zstd's non-"ultra"
/// range.
fn blob_zstd_level_from(level: u64) -> Option<u8> {
    (level > 0).then(|| level.min(19) as u8)
}

/// A level, or `off` / `none` / `false` / `disabled` (case-insensitive).
fn parse_blob_zstd_level(source: &str, value: &str) -> Result<Option<u8>> {
    let value = value.trim();
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "off" | "none" | "false" | "disabled"
    ) {
        return Ok(None);
    }
    value.parse::<u64>().map(blob_zstd_level_from).map_err(|_| {
        Error::InvalidArgument(format!(
            "{source}: expected a zstd level or \"off\", got {value:?}"
        ))
    })
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

pub(crate) fn expand_tilde(path: &Path) -> PathBuf {
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
    Ok(toml_string(table, key, label)?.map(|value| expand_tilde(Path::new(&value))))
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
        "ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF",
        "ASTER_IDENTITY_TRUSTED_ISSUERS",
        "ASTER_IDENTITY_EDGE_TOKEN_MAX_LIFETIME_SECS",
        "ASTER_IDENTITY_CLOCK_LEEWAY_SECS",
        "ASTER_IDENTITY_MAX_KEY_SET_AGE_SECS",
        "ASTER_IDENTITY_PUBLISHER",
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
        "ASTER_BIND_ADDR_V6",
        "ASTER_ADVERTISE_ADDRS",
        "ASTER_RELAY_CA_CERTS",
        "ASTER_RELAY_CA_ONLY",
        "ASTER_DISCOVERY",
        "ASTER_DISCOVERY_DNS_ORIGIN",
        "ASTER_DISCOVERY_PKARR_RELAYS",
        "ASTER_REUSE_LAST_PORT",
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
        "ASTER_BLOB_ZSTD_LEVEL",
        "ASTER_HEALTH_PORT",
        "ASTER_HEALTH_HOST",
        "ASTER_HEALTH_ENABLED",
        "ASTER_METRICS_PEER_LABELS",
        "ASTER_METRICS_PEER_LABEL_CAP",
        "ASTER_METRICS_CLIENT_PEER_LABELS",
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
        env::set_var("ASTER_REUSE_LAST_PORT", "false");
        env::set_var("ASTER_LOCAL_DISCOVERY", "on");
        env::set_var("ASTER_ENABLE_MONITORING", "true");
        env::set_var("ASTER_ENABLE_HOOKS", "true");
        env::set_var("ASTER_HOOK_TIMEOUT_MS", "1234");
        env::set_var("ASTER_HOOK_FAILURE_MODE", "fail-open");
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
        assert!(!cfg.reuse_last_port);
        assert!(cfg.local_discovery);
        assert!(cfg.enable_monitoring);
        assert!(cfg.enable_hooks);
        assert_eq!(cfg.hook_timeout_ms, 1234);
        assert_eq!(cfg.hook_failure_mode, HookFailureMode::FailOpen);
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
        assert!(!core.reuse_last_port);
        assert!(core.enable_discovery);
        assert!(core.enable_monitoring);
        assert!(core.enable_hooks);
        assert_eq!(core.transport_receive_window, Some(8_000_000));
        assert_eq!(core.transport_send_fairness, Some(true));
    }

    #[test]
    fn env_and_toml_paths_expand_a_leading_tilde() {
        // systemd passes `~/...` through literally; every path setting must
        // still resolve under $HOME (portal-sync root_pubkey_file, 2026-10-15).
        let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        let saved_home = env::var_os("HOME");
        env::set_var("HOME", home.path());

        env::set_var("ASTER_ROOT_PUBKEY_FILE", "~/root.pub");
        env::set_var("ASTER_STORAGE_PATH", "~/data");
        env::set_var("ASTER_SECRET_KEY_FILE", "~/node.key");
        let cfg = AsterConfig::from_env().unwrap();
        assert_eq!(
            cfg.root_pubkey_file.as_deref(),
            Some(home.path().join("root.pub").as_path())
        );
        assert_eq!(
            cfg.storage_path.as_deref(),
            Some(home.path().join("data").as_path())
        );
        assert_eq!(
            cfg.secret_key_file.as_deref(),
            Some(home.path().join("node.key").as_path())
        );
        for key in [
            "ASTER_ROOT_PUBKEY_FILE",
            "ASTER_STORAGE_PATH",
            "ASTER_SECRET_KEY_FILE",
        ] {
            env::remove_var(key);
        }

        let toml = home.path().join("aster.toml");
        fs::write(
            &toml,
            "[trust]\nroot_pubkey_file = \"~/root.pub\"\n[storage]\npath = \"~/data\"\n",
        )
        .unwrap();
        let cfg = AsterConfig::from_file(&toml).unwrap();
        assert_eq!(
            cfg.root_pubkey_file.as_deref(),
            Some(home.path().join("root.pub").as_path())
        );
        assert_eq!(
            cfg.storage_path.as_deref(),
            Some(home.path().join("data").as_path())
        );

        // Not a leading tilde: left alone.
        env::set_var("ASTER_STORAGE_PATH", "/srv/~data");
        assert_eq!(
            AsterConfig::from_env().unwrap().storage_path.as_deref(),
            Some(Path::new("/srv/~data"))
        );
        env::remove_var("ASTER_STORAGE_PATH");

        match saved_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }
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
    fn from_env_vars_reads_only_the_supplied_variables() {
        // Bun keeps `process.env` writes out of `environ`, so the TypeScript
        // binding hands its `ASTER_*` map in; that map is the whole source.
        let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::new();
        env::set_var("ASTER_BIND_ADDR", "127.0.0.1:1111");
        env::set_var("ASTER_ENROLLMENT_AUDIENCE", "process-audience");

        let config = AsterConfig::from_env_vars([
            ("ASTER_ADVERTISE_ADDRS", "192.0.2.10:7000, 192.0.2.11:7000"),
            ("ASTER_LOCAL_DISCOVERY", "true"),
            ("ASTER_ENROLLMENT_ROLE_ATTRIBUTE", " team "),
            ("ASTER_TRANSPORT_INITIAL_MTU", "1300"),
        ])
        .unwrap();
        assert_eq!(config.bind_addr, None);
        assert_eq!(
            config.advertise_addrs,
            vec!["192.0.2.10:7000", "192.0.2.11:7000"]
        );
        assert!(config.local_discovery);
        assert_eq!(config.transport_initial_mtu, Some(1300));
        assert_eq!(config.enrollment.audience, None);
        assert_eq!(config.enrollment.roles.attribute, "team");

        let err = AsterConfig::from_env_vars([("ASTER_ALLOW_ALL_CONSUMERS", "false")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("ASTER_ALLOW_ALL_CONSUMERS"), "{err}");

        // The process-environment loader is unchanged.
        let config = AsterConfig::from_env().unwrap();
        assert_eq!(config.bind_addr.as_deref(), Some("127.0.0.1:1111"));
        assert_eq!(
            config.enrollment.audience.as_deref(),
            Some("process-audience")
        );
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
reuse_last_port = false
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
        assert!(!cfg.reuse_last_port);
        assert!(AsterConfig::default().reuse_last_port);
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
        // from_file / from_env apply ASTER_* env vars; hold the shared lock.
        let _env = lock_env();
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
            .hook_failure_mode(HookFailureMode::FailOpen)
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
        assert_eq!(core.hook_failure_mode, HookFailureMode::FailOpen);
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
        assert_eq!(core.hook_failure_mode, HookFailureMode::FailClosed);
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

        assert_eq!(AsterConfig::from_env().unwrap().blob_zstd_level, Some(3));
        assert_eq!(
            AsterConfig::default()
                .to_core_endpoint_config()
                .blob_zstd_level,
            Some(3)
        );
        env::set_var("ASTER_BLOB_ZSTD_LEVEL", "5");
        let cfg = AsterConfig::from_env().unwrap();
        assert_eq!(cfg.blob_zstd_level, Some(5));
        assert_eq!(cfg.to_core_endpoint_config().blob_zstd_level, Some(5));
        env::set_var("ASTER_BLOB_ZSTD_LEVEL", "40");
        assert_eq!(AsterConfig::from_env().unwrap().blob_zstd_level, Some(19));
        env::set_var("ASTER_BLOB_ZSTD_LEVEL", "0");
        let cfg = AsterConfig::from_env().unwrap();
        assert_eq!(cfg.blob_zstd_level, None);
        assert_eq!(cfg.to_core_endpoint_config().blob_zstd_level, None);
        for off in ["off", "OFF", " disabled "] {
            env::set_var("ASTER_BLOB_ZSTD_LEVEL", off);
            assert_eq!(
                AsterConfig::from_env().unwrap().blob_zstd_level,
                None,
                "{off}"
            );
        }
        env::set_var("ASTER_BLOB_ZSTD_LEVEL", "fast");
        let err = AsterConfig::from_env().unwrap_err().to_string();
        assert!(err.contains("ASTER_BLOB_ZSTD_LEVEL"), "{err}");
        env::remove_var("ASTER_BLOB_ZSTD_LEVEL");
        assert_eq!(
            AsterConfig::builder()
                .blob_zstd_level(3)
                .build()
                .blob_zstd_level,
            Some(3)
        );
        assert_eq!(
            AsterConfig::builder()
                .blob_zstd_level(0)
                .build()
                .blob_zstd_level,
            None
        );

        env::set_var("ASTER_PORTMAPPER_CONFIG", "sometimes");
        let err = AsterConfig::from_env().unwrap_err().to_string();
        assert!(err.contains("ASTER_PORTMAPPER_CONFIG"), "{err}");
    }

    #[test]
    fn blob_zstd_level_toml_accepts_a_level_or_off() {
        // from_file / from_env apply ASTER_* env vars; hold the shared lock.
        let _env = lock_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        for (value, want) in [
            ("0", None),
            ("\"off\"", None),
            ("\"5\"", Some(5)),
            ("7", Some(7)),
        ] {
            fs::write(&path, format!("[storage]\nblob_zstd_level = {value}\n")).unwrap();
            assert_eq!(
                AsterConfig::from_file(&path).unwrap().blob_zstd_level,
                want,
                "{value}"
            );
        }
        fs::write(&path, "[storage]\nblob_zstd_level = true\n").unwrap();
        assert!(AsterConfig::from_file(&path).is_err());
        fs::write(&path, "[storage]\npath = \"/tmp/x\"\n").unwrap();
        assert_eq!(
            AsterConfig::from_file(&path).unwrap().blob_zstd_level,
            Some(3)
        );
    }

    #[test]
    fn identity_accept_on_behalf_of_from_toml_env_and_builder() {
        let _lock = lock_env();
        let _env = super::tests_env_guard();

        assert!(!AsterConfig::default().identity_accept_on_behalf_of);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        fs::write(&path, "[identity]\naccept_on_behalf_of = true\n").unwrap();
        assert!(
            AsterConfig::from_file(&path)
                .unwrap()
                .identity_accept_on_behalf_of
        );
        env::set_var("ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF", "false");
        assert!(
            !AsterConfig::from_file(&path)
                .unwrap()
                .identity_accept_on_behalf_of
        );
        env::remove_var("ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF");
        assert!(
            AsterConfig::builder()
                .accept_on_behalf_of(true)
                .build()
                .identity_accept_on_behalf_of
        );
    }

    #[test]
    fn identity_publisher_and_key_set_age_from_toml_env_and_builder() {
        let _lock = lock_env();
        let _env = super::tests_env_guard();

        let defaults = AsterConfig::default();
        assert_eq!(defaults.identity_max_key_set_age_secs, 172_800);
        assert!(defaults.identity_publisher.is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        fs::write(
            &path,
            "[identity]\nmax_key_set_age_secs = 86400\n\n[identity.publisher]\n\
             issuers = [\"https://idp\"]\n\
             jwks_url = { \"https://idp\" = \"https://idp/jwks\" }\n\
             min_refresh_secs = 30\nca_certs = [\"/etc/idp.pem\"]\n",
        )
        .unwrap();
        let cfg = AsterConfig::from_file(&path).unwrap();
        assert_eq!(cfg.identity_max_key_set_age_secs, 86_400);
        assert_eq!(
            cfg.setting_sources.get(SETTING_MAX_KEY_SET_AGE),
            Some(&ConfigSource::File)
        );
        let publisher = cfg.identity_publisher.unwrap();
        assert_eq!(publisher.issuers, ["https://idp"]);
        assert_eq!(publisher.jwks_url["https://idp"], "https://idp/jwks");
        assert_eq!(publisher.refresh_secs, DEFAULT_PUBLISHER_REFRESH_SECS);
        assert_eq!(publisher.min_refresh_secs, 30);
        assert_eq!(publisher.ca_certs, [PathBuf::from("/etc/idp.pem")]);
        assert!(!publisher.ca_only);
        publisher.validate(86_400).unwrap();
        // Unknown publisher keys are refused.
        fs::write(&path, "[identity.publisher]\nissuers = []\nurl = \"x\"\n").unwrap();
        assert!(AsterConfig::from_file(&path).is_err());

        fs::write(&path, "").unwrap();
        env::set_var("ASTER_IDENTITY_MAX_KEY_SET_AGE_SECS", "3600");
        env::set_var(
            "ASTER_IDENTITY_PUBLISHER",
            r#"{"issuers": ["https://idp"], "jwks_url": {"https://idp": "https://idp/k"}, "refresh_secs": 600}"#,
        );
        let cfg = AsterConfig::from_file(&path).unwrap();
        assert_eq!(cfg.identity_max_key_set_age_secs, 3600);
        assert_eq!(cfg.identity_publisher.as_ref().unwrap().refresh_secs, 600);
        env::set_var("ASTER_IDENTITY_PUBLISHER", "not json");
        assert!(AsterConfig::from_file(&path).is_err());
        env::remove_var("ASTER_IDENTITY_PUBLISHER");
        env::remove_var("ASTER_IDENTITY_MAX_KEY_SET_AGE_SECS");

        let built = AsterConfig::builder()
            .max_key_set_age_secs(7200)
            .identity_publisher(IdentityPublisherConfig::new([(
                "https://idp",
                "https://idp/k",
            )]))
            .build();
        assert_eq!(built.identity_max_key_set_age_secs, 7200);
        assert_eq!(
            built.setting_sources.get(SETTING_MAX_KEY_SET_AGE),
            Some(&ConfigSource::Code)
        );
        // 3600 s refresh against a 7200 s maximum age: exactly half is allowed.
        built.identity_publisher.unwrap().validate(7200).unwrap();
    }

    #[test]
    fn trusted_issuers_and_identity_limits_from_toml_env_and_builder() {
        let _lock = lock_env();
        let _env = super::tests_env_guard();

        let defaults = AsterConfig::default();
        assert!(defaults.identity_trusted_issuers.is_empty());
        assert_eq!(defaults.identity_edge_token_max_lifetime_secs, 300);
        assert_eq!(defaults.identity_clock_leeway_secs, 60);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        fs::write(
            &path,
            "[identity]\nedge_token_max_lifetime_secs = 120\nclock_leeway_secs = 10\n\
             [[identity.trusted_issuers]]\niss = \"https://accounts.example.com\"\n\
             audiences = [\"odata-core\"]\nroles_claim = \"groups\"\n\
             role_map = { \"data-readers\" = \"data.read\" }\n",
        )
        .unwrap();
        let file = AsterConfig::from_file(&path).unwrap();
        assert_eq!(
            file.identity_trusted_issuers,
            vec![
                TrustedIssuer::new("https://accounts.example.com", ["odata-core"])
                    .roles_claim("groups")
                    .map_role("data-readers", "data.read")
            ]
        );
        assert_eq!(
            file.identity_trusted_issuers[0].max_token_lifetime_secs,
            900
        );
        assert_eq!(file.identity_edge_token_max_lifetime_secs, 120);
        assert_eq!(file.identity_clock_leeway_secs, 10);
        assert_eq!(
            file.setting_sources.get(SETTING_TRUSTED_ISSUERS),
            Some(&ConfigSource::File)
        );

        // Audiences are required in the file form; unknown keys are refused.
        fs::write(
            &path,
            "[[identity.trusted_issuers]]\niss = \"https://accounts.example.com\"\n",
        )
        .unwrap();
        let err = AsterConfig::from_file(&path).unwrap_err().to_string();
        assert!(err.contains("audiences"), "{err}");
        fs::write(
            &path,
            "[[identity.trusted_issuers]]\niss = \"x\"\naudiences = [\"a\"]\njwks_url = \"y\"\n",
        )
        .unwrap();
        assert!(AsterConfig::from_file(&path).is_err());

        env::set_var(
            "ASTER_IDENTITY_TRUSTED_ISSUERS",
            r#"[{"iss": "https://idp.example.com", "audiences": ["api"], "max_token_lifetime_secs": 60}]"#,
        );
        env::set_var("ASTER_IDENTITY_CLOCK_LEEWAY_SECS", "5");
        env::set_var("ASTER_IDENTITY_EDGE_TOKEN_MAX_LIFETIME_SECS", "30");
        let from_env = AsterConfig::from_env().unwrap();
        assert_eq!(
            from_env.identity_trusted_issuers[0].iss,
            "https://idp.example.com"
        );
        assert_eq!(
            from_env.identity_trusted_issuers[0].max_token_lifetime_secs,
            60
        );
        assert_eq!(from_env.identity_clock_leeway_secs, 5);
        assert_eq!(from_env.identity_edge_token_max_lifetime_secs, 30);
        assert_eq!(
            from_env.setting_sources.get(SETTING_CLOCK_LEEWAY),
            Some(&ConfigSource::Env)
        );
        env::set_var("ASTER_IDENTITY_TRUSTED_ISSUERS", "not json");
        assert!(AsterConfig::from_env().is_err());
        env::remove_var("ASTER_IDENTITY_TRUSTED_ISSUERS");
        env::remove_var("ASTER_IDENTITY_CLOCK_LEEWAY_SECS");
        env::remove_var("ASTER_IDENTITY_EDGE_TOKEN_MAX_LIFETIME_SECS");

        let built = AsterConfig::builder()
            .trusted_issuer(TrustedIssuer::new("https://a", ["x"]))
            .trusted_issuer(TrustedIssuer::new("https://b", ["y"]))
            .edge_token_max_lifetime_secs(60)
            .clock_leeway_secs(0)
            .build();
        assert_eq!(built.identity_trusted_issuers.len(), 2);
        assert_eq!(built.identity_edge_token_max_lifetime_secs, 60);
        assert_eq!(built.identity_clock_leeway_secs, 0);
        assert_eq!(
            built.setting_sources.get(SETTING_EDGE_TOKEN_MAX_LIFETIME),
            Some(&ConfigSource::Code)
        );
    }

    #[test]
    fn remote_config_keys_and_setting_sources_follow_resolution_order() {
        let _lock = lock_env();
        let _env = super::tests_env_guard();
        let source = |c: &AsterConfig| c.setting_sources.get(SETTING_ACCEPT_ON_BEHALF_OF).copied();

        let defaults = AsterConfig::default();
        assert!(defaults.remote_config_enabled);
        assert!(defaults.remote_config_lock.is_empty());
        assert_eq!(source(&defaults), None);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        fs::write(
            &path,
            "[identity]\naccept_on_behalf_of = true\n\
             [remote_config]\nenabled = false\nlock = [\"identity\"]\n",
        )
        .unwrap();
        let file = AsterConfig::from_file(&path).unwrap();
        assert!(!file.remote_config_enabled);
        assert_eq!(file.remote_config_lock, vec!["identity".to_string()]);
        assert_eq!(source(&file), Some(ConfigSource::File));

        env::set_var("ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF", "false");
        env::set_var("ASTER_REMOTE_CONFIG_ENABLED", "true");
        env::set_var("ASTER_REMOTE_CONFIG_LOCK", " identity , ");
        let env_over = AsterConfig::from_file(&path).unwrap();
        assert!(env_over.remote_config_enabled);
        assert_eq!(env_over.remote_config_lock, vec!["identity".to_string()]);
        assert_eq!(source(&env_over), Some(ConfigSource::Env));
        env::remove_var("ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF");
        env::remove_var("ASTER_REMOTE_CONFIG_ENABLED");
        env::remove_var("ASTER_REMOTE_CONFIG_LOCK");

        let code = AsterConfig::from_file(&path)
            .unwrap()
            .into_builder()
            .accept_on_behalf_of(false)
            .remote_config(true)
            .remote_config_lock(Vec::<String>::new())
            .build();
        assert_eq!(source(&code), Some(ConfigSource::Code));
        assert!(code.remote_config_enabled && code.remote_config_lock.is_empty());
    }

    #[test]
    fn metrics_peer_label_keys_from_toml_env_and_builder() {
        let _lock = lock_env();
        let _env = super::tests_env_guard();

        let cfg = AsterConfig::default();
        assert!(!cfg.metrics_peer_labels);
        assert_eq!(cfg.metrics_peer_label_cap, DEFAULT_METRICS_PEER_LABEL_CAP);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        fs::write(&path, "[metrics]\npeer_labels = true\npeer_label_cap = 3\n").unwrap();
        let cfg = AsterConfig::from_file(&path).unwrap();
        assert!(cfg.metrics_peer_labels);
        assert_eq!(cfg.metrics_peer_label_cap, 3);

        env::set_var("ASTER_METRICS_PEER_LABEL_CAP", "1");
        let cfg = AsterConfig::from_file(&path).unwrap();
        assert_eq!(cfg.metrics_peer_label_cap, 1);
        env::set_var("ASTER_METRICS_PEER_LABELS", "false");
        let cfg = AsterConfig::from_file(&path).unwrap();
        assert!(!cfg.metrics_peer_labels);
        env::remove_var("ASTER_METRICS_PEER_LABELS");
        env::remove_var("ASTER_METRICS_PEER_LABEL_CAP");

        // The client-call peer label: off by default, TOML, env over TOML.
        assert!(!cfg.metrics_client_peer_labels);
        fs::write(&path, "[metrics]\nclient_peer_labels = true\n").unwrap();
        assert!(
            AsterConfig::from_file(&path)
                .unwrap()
                .metrics_client_peer_labels
        );
        env::set_var("ASTER_METRICS_CLIENT_PEER_LABELS", "false");
        assert!(
            !AsterConfig::from_file(&path)
                .unwrap()
                .metrics_client_peer_labels
        );
        env::remove_var("ASTER_METRICS_CLIENT_PEER_LABELS");
        assert!(
            AsterConfig::builder()
                .metrics_client_peer_labels(true)
                .build()
                .metrics_client_peer_labels
        );

        let cfg = AsterConfig::builder()
            .health(true)
            .metrics_peer_labels(2)
            .build();
        #[cfg(feature = "health")]
        assert_eq!(cfg.health_config().unwrap().peer_label_cap, Some(2));
        assert!(cfg.metrics_peer_labels);
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
blob_zstd_level = 4

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
        assert_eq!(cfg.blob_zstd_level, Some(4));
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

#[cfg(test)]
mod cluster_network_tests {
    //! Kasm A6: IPv6 bind pin, advertised addresses, relay CA trust and
    //! discovery, from env, TOML and the builder down to the core config.
    use super::*;
    use std::sync::MutexGuard;

    fn lock_env() -> MutexGuard<'static, ()> {
        super::tests_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn defaults_keep_n0_discovery_and_webpki() {
        let core = AsterConfig::default().to_core_endpoint_config();
        assert_eq!(core.discovery.as_deref(), Some("default"));
        assert!(core.bind_addr_v6.is_none() && core.advertise_addrs.is_empty());
        assert!(core.relay_ca_cert_files.is_empty() && !core.relay_ca_only);
        assert!(core.discovery_dns_origin.is_none() && core.discovery_pkarr_relays.is_empty());
    }

    #[test]
    fn env_reads_cluster_settings() {
        let _lock = lock_env();
        let _env = super::tests_env_guard();
        env::set_var("ASTER_BIND_ADDR", "0.0.0.0:7842");
        env::set_var("ASTER_BIND_ADDR_V6", "[::]:7842");
        env::set_var(
            "ASTER_ADVERTISE_ADDRS",
            " 203.0.113.10:31842 , [2001:db8::10]:31842 ,",
        );
        env::set_var(
            "ASTER_RELAY_CA_CERTS",
            "/etc/aster/ca.pem,/etc/aster/ca2.pem",
        );
        env::set_var("ASTER_RELAY_CA_ONLY", "true");
        env::set_var(
            "ASTER_DISCOVERY_PKARR_RELAYS",
            "https://dns.example.internal/pkarr",
        );
        env::set_var("ASTER_DISCOVERY_DNS_ORIGIN", "dns.example.internal");
        let cfg = AsterConfig::from_env().unwrap();
        assert_eq!(cfg.bind_addr_v6.as_deref(), Some("[::]:7842"));
        assert_eq!(
            cfg.advertise_addrs,
            ["203.0.113.10:31842", "[2001:db8::10]:31842"]
        );
        assert_eq!(cfg.relay_ca_certs.len(), 2);
        assert!(cfg.relay_ca_only);
        assert_eq!(cfg.discovery, DiscoveryMode::Custom);
        let core = cfg.to_core_endpoint_config();
        assert_eq!(core.bind_addr.as_deref(), Some("0.0.0.0:7842"));
        assert_eq!(core.bind_addr_v6.as_deref(), Some("[::]:7842"));
        assert_eq!(core.advertise_addrs.len(), 2);
        assert_eq!(
            core.relay_ca_cert_files,
            ["/etc/aster/ca.pem", "/etc/aster/ca2.pem"]
        );
        assert_eq!(core.discovery.as_deref(), Some("custom"));
        assert_eq!(
            core.discovery_dns_origin.as_deref(),
            Some("dns.example.internal")
        );
        assert_eq!(core.discovery_pkarr_relays.len(), 1);

        env::set_var("ASTER_DISCOVERY", "none");
        let err = AsterConfig::from_env().unwrap_err().to_string();
        assert!(err.contains("require discovery \"custom\""), "{err}");
        env::remove_var("ASTER_DISCOVERY_PKARR_RELAYS");
        env::remove_var("ASTER_DISCOVERY_DNS_ORIGIN");
        assert_eq!(
            AsterConfig::from_env().unwrap().discovery,
            DiscoveryMode::None
        );
        env::set_var("ASTER_DISCOVERY", "nope");
        assert!(AsterConfig::from_env().is_err());
        env::set_var("ASTER_DISCOVERY", "LOCAL");
        let core = AsterConfig::from_env().unwrap().to_core_endpoint_config();
        assert_eq!(core.discovery.as_deref(), Some("none"));
        assert!(core.enable_discovery, "local = mDNS only");
    }

    #[test]
    fn toml_reads_cluster_settings_and_env_overrides() {
        let _lock = lock_env();
        let _env = super::tests_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        fs::write(
            &path,
            r#"
[network]
relay_urls = ["https://relay.example.internal"]
bind_addr = "0.0.0.0:7842"
bind_addr_v6 = "[::]:7842"
advertise_addrs = ["203.0.113.10:31842"]
relay_ca_certs = ["/etc/aster/relay-ca.pem"]
relay_ca_only = true
discovery = "custom"
discovery_pkarr_relays = ["https://dns.example.internal/pkarr"]
discovery_dns_origin = "dns.example.internal"
"#,
        )
        .unwrap();
        let cfg = AsterConfig::from_file(&path).unwrap();
        assert_eq!(cfg.bind_addr_v6.as_deref(), Some("[::]:7842"));
        assert_eq!(cfg.advertise_addrs, ["203.0.113.10:31842"]);
        assert_eq!(
            cfg.relay_ca_certs,
            [PathBuf::from("/etc/aster/relay-ca.pem")]
        );
        assert!(cfg.relay_ca_only);
        assert_eq!(cfg.discovery, DiscoveryMode::Custom);
        assert_eq!(cfg.discovery_pkarr_relays.len(), 1);

        // A later source's `discovery = "none"` drops the file's servers.
        env::set_var("ASTER_DISCOVERY", "none");
        let core = AsterConfig::from_file(&path)
            .unwrap()
            .to_core_endpoint_config();
        assert_eq!(core.discovery.as_deref(), Some("none"));
        assert!(core.discovery_pkarr_relays.is_empty() && core.discovery_dns_origin.is_none());
        env::remove_var("ASTER_DISCOVERY");

        fs::write(
            &path,
            "[network]\ndiscovery = \"none\"\ndiscovery_dns_origin = \"x.example\"\n",
        )
        .unwrap();
        assert!(AsterConfig::from_file(&path).is_err());
        fs::write(&path, "[network]\nadvertise_addrs = \"203.0.113.10:1\"\n").unwrap();
        assert!(AsterConfig::from_file(&path).is_err(), "must be a list");
    }

    #[test]
    fn builder_sets_cluster_settings() {
        let cfg = AsterConfig::builder()
            .bind_addr("0.0.0.0:7842")
            .bind_addr_v6("[::]:7842")
            .advertise_addr("203.0.113.10:31842")
            .relay_ca_cert("/etc/aster/relay-ca.pem")
            .relay_ca_only(true)
            .discovery_mode(DiscoveryMode::None)
            .build();
        let core = cfg.to_core_endpoint_config();
        assert_eq!(core.bind_addr_v6.as_deref(), Some("[::]:7842"));
        assert_eq!(core.advertise_addrs, ["203.0.113.10:31842"]);
        assert!(core.relay_ca_only);
        assert_eq!(core.discovery.as_deref(), Some("none"));
        let custom = AsterConfig::builder()
            .discovery_pkarr_relay("https://dns.example.internal/pkarr")
            .build();
        assert_eq!(custom.discovery, DiscoveryMode::Custom);
    }

    #[test]
    fn online_timeout_defaults_and_is_set_from_toml_and_builder() {
        let default = AsterConfig::default();
        assert_eq!(default.online_timeout_ms, 30_000);
        assert_eq!(default.to_core_endpoint_config().online_timeout_ms, 30_000);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aster.toml");
        fs::write(&path, "[network]\nonline_timeout_ms = 1500\n").unwrap();
        assert_eq!(
            AsterConfig::from_file(&path).unwrap().online_timeout_ms,
            1500
        );

        let built = AsterConfig::builder().online_timeout_ms(250).build();
        assert_eq!(built.to_core_endpoint_config().online_timeout_ms, 250);
    }
}
