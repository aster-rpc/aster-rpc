//! The `[identity]`, `[identity.publisher]` and `[remote_config]` sections of
//! `AsterConfig` for JavaScript (gateway identity, design
//! `docs/_internal/web_gateway/gateway-identity.md`), lowered onto the Rust
//! `AsterConfig` fields `AsterServer` / `AsterClient` read. Loading stays with
//! the Rust loaders (`fromEnv` / `fromFile`), which also record where each
//! overridable setting came from; `sources` carries that provenance through
//! JavaScript so `aster.ops.NodeConfig` `get` reports `file` / `env`, and a
//! value changed in code since reports `code`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use napi_derive::napi;

use aster::{
    AsterConfig as RsConfig, ConfigSource, IdentityPublisherConfig, TrustedIssuer,
    SETTING_ACCEPT_ON_BEHALF_OF, SETTING_CLOCK_LEEWAY, SETTING_EDGE_TOKEN_MAX_LIFETIME,
    SETTING_MAX_KEY_SET_AGE, SETTING_TRUSTED_ISSUERS,
};

use crate::config::{invalid, opt_u64, u64_out, U64Value};

/// An identity provider whose tokens the server accepts in the
/// `aster-on-behalf-of` entry (`[[identity.trusted_issuers]]`). Its keys come
/// from the fleet namespace (written by a node holding
/// `aster.identity-publisher`); this says whom to trust and how to map roles.
/// Needs Enrollment.
#[napi(object, js_name = "TrustedIssuer")]
#[derive(Default, Clone)]
pub struct JsTrustedIssuer {
    /// The exact `iss` claim.
    pub iss: String,
    /// Accepted `aud` values (required).
    pub audiences: Vec<String>,
    /// The claim roles are read from (an array of strings, or one
    /// space-separated string); unset: the tokens grant no roles.
    pub roles_claim: Option<String>,
    /// Claim value → Aster role; unmapped values are dropped.
    pub role_map: Option<HashMap<String, String>>,
    /// Longest `exp - iat` accepted (default 900).
    pub max_token_lifetime_secs: Option<U64Value>,
}

/// `[identity.publisher]`: on the node that publishes issuer keys (normally
/// the gateway). Fetches each issuer's JWK set over validated HTTPS and
/// writes it into the fleet namespace. Needs Enrollment, a credential
/// granting `aster.identity-publisher` and the root's sealed write grant.
#[napi(object, js_name = "IdentityPublisherConfig")]
#[derive(Default, Clone)]
pub struct JsIdentityPublisherConfig {
    /// The issuers (`iss`) to publish keys for.
    pub issuers: Vec<String>,
    /// Issuer → the `https` URL of its JWK set.
    pub jwks_url: Option<HashMap<String, String>>,
    /// Scheduled refresh, 60..=86400 s (default 3600).
    pub refresh_secs: Option<U64Value>,
    /// Shortest gap between unknown-`kid` refreshes of one issuer, 1..=refreshSecs (default 60).
    pub min_refresh_secs: Option<U64Value>,
    /// Extra PEM trust anchors (files) for the providers' TLS.
    pub ca_certs: Option<Vec<String>>,
    /// Trust only `caCerts`, not the WebPKI roots.
    pub ca_only: Option<bool>,
}

/// Where a loaded setting came from, and its value then (JSON), so a value
/// changed in code afterwards no longer claims that source.
#[napi(object, js_name = "ConfigSettingSource")]
#[derive(Clone)]
pub struct JsSettingSource {
    /// `file` or `env`.
    pub source: String,
    /// The setting's value when it was loaded, as JSON.
    pub value: String,
}

/// The `[identity]` section (env `ASTER_IDENTITY_*`).
#[napi(object, js_name = "IdentityConfig")]
#[derive(Default, Clone)]
pub struct JsIdentityConfig {
    /// Accept calls made on behalf of a user (default false: a call carrying
    /// the entry is refused with `PERMISSION_DENIED`).
    /// `ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF`, `[identity] accept_on_behalf_of`.
    pub accept_on_behalf_of: Option<bool>,
    /// The providers whose tokens may be forwarded.
    /// `ASTER_IDENTITY_TRUSTED_ISSUERS` (JSON), `[[identity.trusted_issuers]]`.
    pub trusted_issuers: Option<Vec<JsTrustedIssuer>>,
    /// Longest edge-minted token lifetime, 1..=300 s (default 300).
    /// `ASTER_IDENTITY_EDGE_TOKEN_MAX_LIFETIME_SECS`.
    pub edge_token_max_lifetime_secs: Option<U64Value>,
    /// Clock leeway for `iat` / `nbf`, 0..=300 s (default 60).
    /// `ASTER_IDENTITY_CLOCK_LEEWAY_SECS`.
    pub clock_leeway_secs: Option<U64Value>,
    /// Oldest issuer key set trusted, 60..=2592000 s (default 172800).
    /// `ASTER_IDENTITY_MAX_KEY_SET_AGE_SECS`.
    pub max_key_set_age_secs: Option<U64Value>,
    /// `[identity.publisher]` (`ASTER_IDENTITY_PUBLISHER`, JSON).
    pub publisher: Option<JsIdentityPublisherConfig>,
    /// Filled by the loaders: where each overridable setting
    /// (`identity.accept_on_behalf_of`, …) came from. Leave it alone.
    pub sources: Option<HashMap<String, JsSettingSource>>,
}

/// The `[remote_config]` section: `aster.ops.NodeConfig`, remote overrides of
/// overridable sections (`[identity]` today). Served by default behind
/// `aster.config-admin`.
#[napi(object, js_name = "RemoteConfig")]
#[derive(Default, Clone)]
pub struct JsRemoteConfig {
    /// `false` turns the service off (a stored record is then ignored).
    /// `ASTER_REMOTE_CONFIG_ENABLED`, `[remote_config] enabled` (default true).
    pub enabled: Option<bool>,
    /// Sections that may not be overridden, e.g. `["identity"]`.
    /// `ASTER_REMOTE_CONFIG_LOCK` (comma-separated), `[remote_config] lock`.
    pub lock: Option<Vec<String>>,
}

const SETTINGS: [&str; 5] = [
    SETTING_ACCEPT_ON_BEHALF_OF,
    SETTING_TRUSTED_ISSUERS,
    SETTING_EDGE_TOKEN_MAX_LIFETIME,
    SETTING_CLOCK_LEEWAY,
    SETTING_MAX_KEY_SET_AGE,
];

/// The JSON of one overridable setting of `c`.
fn fingerprint(c: &RsConfig, key: &str) -> String {
    let value = match key {
        SETTING_ACCEPT_ON_BEHALF_OF => serde_json::json!(c.identity_accept_on_behalf_of),
        SETTING_TRUSTED_ISSUERS => {
            serde_json::to_value(&c.identity_trusted_issuers).unwrap_or_default()
        }
        SETTING_EDGE_TOKEN_MAX_LIFETIME => {
            serde_json::json!(c.identity_edge_token_max_lifetime_secs)
        }
        SETTING_CLOCK_LEEWAY => serde_json::json!(c.identity_clock_leeway_secs),
        SETTING_MAX_KEY_SET_AGE => serde_json::json!(c.identity_max_key_set_age_secs),
        _ => serde_json::Value::Null,
    };
    value.to_string()
}

fn source_of(name: &str) -> Option<ConfigSource> {
    match name {
        "file" => Some(ConfigSource::File),
        "env" => Some(ConfigSource::Env),
        _ => None,
    }
}

impl JsTrustedIssuer {
    fn to_rust(&self, at: usize) -> napi::Result<TrustedIssuer> {
        let mut issuer = TrustedIssuer::new(self.iss.clone(), self.audiences.clone());
        issuer.roles_claim = self.roles_claim.clone();
        issuer.role_map = self
            .role_map
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Some(v) = opt_u64(
            &format!("identity.trustedIssuers[{at}].maxTokenLifetimeSecs"),
            &self.max_token_lifetime_secs,
        )? {
            issuer.max_token_lifetime_secs = v;
        }
        Ok(issuer)
    }

    fn from_rust(t: &TrustedIssuer) -> Self {
        Self {
            iss: t.iss.clone(),
            audiences: t.audiences.clone(),
            roles_claim: t.roles_claim.clone(),
            role_map: (!t.role_map.is_empty()).then(|| {
                t.role_map
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            }),
            max_token_lifetime_secs: Some(u64_out(t.max_token_lifetime_secs)),
        }
    }
}

impl JsIdentityPublisherConfig {
    fn to_rust(&self) -> napi::Result<IdentityPublisherConfig> {
        // Through serde so the Rust defaults (refresh 3600, min 60) apply.
        let mut publisher: IdentityPublisherConfig =
            serde_json::from_value(serde_json::json!({ "issuers": self.issuers }))
                .map_err(|e| invalid(format!("identity.publisher: {e}")))?;
        publisher.jwks_url = self
            .jwks_url
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<BTreeMap<_, _>>();
        if let Some(v) = opt_u64("identity.publisher.refreshSecs", &self.refresh_secs)? {
            publisher.refresh_secs = v;
        }
        if let Some(v) = opt_u64("identity.publisher.minRefreshSecs", &self.min_refresh_secs)? {
            publisher.min_refresh_secs = v;
        }
        publisher.ca_certs = self.ca_certs.iter().flatten().map(PathBuf::from).collect();
        publisher.ca_only = self.ca_only.unwrap_or(false);
        Ok(publisher)
    }

    fn from_rust(p: &IdentityPublisherConfig) -> Self {
        Self {
            issuers: p.issuers.clone(),
            jwks_url: Some(
                p.jwks_url
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            refresh_secs: Some(u64_out(p.refresh_secs)),
            min_refresh_secs: Some(u64_out(p.min_refresh_secs)),
            ca_certs: Some(
                p.ca_certs
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            ),
            ca_only: Some(p.ca_only),
        }
    }
}

impl JsIdentityConfig {
    /// Set the section on `c`; unset fields keep the Rust defaults. Loaded
    /// sources are kept for settings whose value is still the loaded one.
    pub(crate) fn apply(&self, c: &mut RsConfig) -> napi::Result<()> {
        if let Some(v) = self.accept_on_behalf_of {
            c.identity_accept_on_behalf_of = v;
        }
        if let Some(issuers) = &self.trusted_issuers {
            c.identity_trusted_issuers = issuers
                .iter()
                .enumerate()
                .map(|(i, t)| t.to_rust(i))
                .collect::<napi::Result<_>>()?;
        }
        if let Some(v) = opt_u64(
            "identity.edgeTokenMaxLifetimeSecs",
            &self.edge_token_max_lifetime_secs,
        )? {
            c.identity_edge_token_max_lifetime_secs = v;
        }
        if let Some(v) = opt_u64("identity.clockLeewaySecs", &self.clock_leeway_secs)? {
            c.identity_clock_leeway_secs = v;
        }
        if let Some(v) = opt_u64("identity.maxKeySetAgeSecs", &self.max_key_set_age_secs)? {
            c.identity_max_key_set_age_secs = v;
        }
        if let Some(p) = &self.publisher {
            c.identity_publisher = Some(p.to_rust()?);
        }
        for (key, loaded) in self.sources.iter().flatten() {
            let Some(source) = source_of(&loaded.source) else {
                continue;
            };
            if SETTINGS.contains(&key.as_str()) && fingerprint(c, key) == loaded.value {
                c.setting_sources.insert(key.clone(), source);
            }
        }
        Ok(())
    }

    pub(crate) fn from_rust(c: &RsConfig) -> Self {
        let sources: HashMap<String, JsSettingSource> = c
            .setting_sources
            .iter()
            .filter_map(|(key, source)| {
                let name = match source {
                    ConfigSource::File => "file",
                    ConfigSource::Env => "env",
                    _ => return None,
                };
                Some((
                    key.clone(),
                    JsSettingSource {
                        source: name.into(),
                        value: fingerprint(c, key),
                    },
                ))
            })
            .collect();
        Self {
            accept_on_behalf_of: Some(c.identity_accept_on_behalf_of),
            trusted_issuers: Some(
                c.identity_trusted_issuers
                    .iter()
                    .map(JsTrustedIssuer::from_rust)
                    .collect(),
            ),
            edge_token_max_lifetime_secs: Some(u64_out(c.identity_edge_token_max_lifetime_secs)),
            clock_leeway_secs: Some(u64_out(c.identity_clock_leeway_secs)),
            max_key_set_age_secs: Some(u64_out(c.identity_max_key_set_age_secs)),
            publisher: c
                .identity_publisher
                .as_ref()
                .map(JsIdentityPublisherConfig::from_rust),
            sources: (!sources.is_empty()).then_some(sources),
        }
    }
}

impl JsRemoteConfig {
    pub(crate) fn apply(&self, c: &mut RsConfig) {
        if let Some(v) = self.enabled {
            c.remote_config_enabled = v;
        }
        if let Some(lock) = &self.lock {
            c.remote_config_lock = lock.clone();
        }
    }

    pub(crate) fn from_rust(c: &RsConfig) -> Self {
        Self {
            enabled: Some(c.remote_config_enabled),
            lock: Some(c.remote_config_lock.clone()),
        }
    }
}
