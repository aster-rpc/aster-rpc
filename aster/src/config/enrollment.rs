//! The `[enrollment]` configuration section and its `ASTER_ENROLLMENT_*`
//! environment variables.
//!
//! Plain data, compiled with or without the `enrollment` feature, so a
//! configuration file parses the same way in every build. `AsterServer` and
//! `AsterClient` turn it into an installed [`Enrollment`] (feature
//! `enrollment`, with `rpc` and `docs`); a build without the feature refuses
//! to start a node whose configuration enables enrollment.
//!
//! The contract every binding mirrors is the "AsterServer integration" section
//! of `docs/_internal/working_ideas/aster-enrollment.md`.
//!
//! [`Enrollment`]: https://docs.rs/aster/latest/aster/enrollment/struct.Enrollment.html

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{
    env_bool, env_path, env_string, env_u64, toml_bool, toml_path, toml_section, toml_string,
    toml_string_list, toml_u64,
};
use crate::error::{Error, Result};

/// The default claim attribute whose comma-separated value lists the peer's
/// roles (`aster.role`, the attribute Gate 3 reads).
pub const DEFAULT_ROLE_ATTRIBUTE: &str = "aster.role";

/// Enrollment for an `AsterServer` or `AsterClient`: which root to trust, this
/// node's credential, where policy comes from, who is admitted, and how
/// verified claims become Gate-3 roles.
///
/// Off unless [`AsterConfig::is_enrollment_enabled`]. Once enabled, every
/// value is validated when the server or client starts and any error stops
/// the start: enrollment never degrades to an open node.
///
/// What belongs to the root and to this node's identity lives in `[trust]`
/// beside the other trust settings, under the names an `aster trust` profile
/// uses: the trusted root ([`AsterConfig::root_pubkey`] /
/// [`AsterConfig::root_pubkey_file`]), its rotation pin
/// ([`AsterConfig::next_root_commitment`], [`AsterConfig::root_seq`],
/// [`AsterConfig::root_rotations`]) and this node's credential
/// ([`AsterConfig::enrollment_credential_file`] /
/// [`AsterConfig::enrollment_credential_chain`]). This section holds the rest.
///
/// [`AsterConfig::is_enrollment_enabled`]: crate::AsterConfig::is_enrollment_enabled
/// [`AsterConfig::root_pubkey`]: crate::AsterConfig::root_pubkey
/// [`AsterConfig::root_pubkey_file`]: crate::AsterConfig::root_pubkey_file
/// [`AsterConfig::next_root_commitment`]: crate::AsterConfig::next_root_commitment
/// [`AsterConfig::root_seq`]: crate::AsterConfig::root_seq
/// [`AsterConfig::root_rotations`]: crate::AsterConfig::root_rotations
/// [`AsterConfig::enrollment_credential_chain`]: crate::AsterConfig::enrollment_credential_chain
/// [`AsterConfig::enrollment_credential_file`]: crate::AsterConfig::enrollment_credential_file
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EnrollmentConfig {
    /// Force enrollment on or off. `None` (the default) enables it exactly
    /// when [`audience`](Self::audience), [`fleet_namespace`](Self::fleet_namespace),
    /// the issuer delegation or a credential (`[trust]`) is set, so a
    /// half-written configuration fails validation rather than running open.
    ///
    /// TOML `enabled`; env `ASTER_ENROLLMENT_ENABLED`.
    pub enabled: Option<bool>,
    /// The fleet audience the verifier and policy stores pin; credentials must
    /// name it. Required. TOML `audience`; env `ASTER_ENROLLMENT_AUDIENCE`.
    pub audience: Option<String>,
    /// The fleet policy namespace (64 hex characters, as printed by
    /// `aster enroll namespace create`). Required. TOML `fleet_namespace`;
    /// env `ASTER_ENROLLMENT_FLEET_NAMESPACE`.
    pub fleet_namespace: Option<String>,
    /// Durable policy state (denials and revision watermarks). Default:
    /// `<storage path>/enrollment.json` on a persistent node. A server with
    /// neither refuses to start; a client uses a temporary file. TOML
    /// `state_path`; env `ASTER_ENROLLMENT_STATE_PATH`.
    pub state_path: Option<PathBuf>,
    /// Policy replicas to enroll with and synchronize the fleet namespace
    /// from at start: `aster1…` tickets or 64-hex node ids. Empty on a server
    /// means it holds the fleet namespace itself and the root publishes into
    /// it (`aster enroll … --peer <server>`). TOML `replicas = [..]`; env
    /// `ASTER_ENROLLMENT_REPLICAS` (comma-separated).
    pub replicas: Vec<String>,
    /// Admission policy, as the `PolicyConfig` tree (`"tolerate_stale_policy"`,
    /// `{ all_of = [{ scope = "ops" }, "tolerate_stale_policy"] }`, …), kept
    /// as JSON. `None` admits any peer with a valid credential from the root,
    /// subject to every known revocation (`"tolerate_stale_policy"`). TOML
    /// `policy`; env `ASTER_ENROLLMENT_POLICY` (JSON).
    pub policy: Option<serde_json::Value>,
    /// Refuse to start on a clock the kernel reports unsynchronized (Linux).
    /// TOML `require_synchronized_clock`; env
    /// `ASTER_ENROLLMENT_REQUIRE_SYNCHRONIZED_CLOCK`.
    pub require_synchronized_clock: bool,
    /// Activation leeway for `iat`/`nbf` in seconds (default 60, at most
    /// 300). TOML `activation_leeway_secs`; env
    /// `ASTER_ENROLLMENT_ACTIVATION_LEEWAY_SECS`.
    pub activation_leeway_secs: Option<u64>,
    /// Handshake, policy and synchronization limits (`[enrollment.limits]`).
    pub limits: EnrollmentLimits,
    /// How verified claims become Gate-3 attributes (`[enrollment.roles]`).
    pub roles: RoleMappingConfig,
    /// Serve `aster.enrollment.IssuerV1` as a delegated issuer holding this
    /// delegation chain (`aster enroll delegate --out`). Server only. TOML
    /// `[enrollment.issuer] delegation_file`; env
    /// `ASTER_ENROLLMENT_ISSUER_DELEGATION_FILE`.
    pub issuer_delegation_file: Option<PathBuf>,
    /// Lifetime of the credentials this issuer signs, in seconds (default
    /// 24 h, shortened to the delegation). TOML `[enrollment.issuer] lifetime`
    /// (seconds or `"24h"`); env `ASTER_ENROLLMENT_ISSUER_LIFETIME`.
    pub issuer_lifetime_secs: Option<u64>,
    /// The issuer this node renews its credential from: an `aster1…` ticket
    /// or 64-hex node id. TOML `renew_from`; env `ASTER_ENROLLMENT_RENEW_FROM`.
    pub renew_from: Option<String>,
    /// Renewal switch. `None` renews from [`renew_from`](Self::renew_from)
    /// when set, and otherwise a client renews from the server it connects
    /// to when that server signed its credential. `Some(false)` never renews.
    /// TOML `renew`; env `ASTER_ENROLLMENT_RENEW`.
    pub renew: Option<bool>,
    /// Client only: synchronize the fleet namespace the server offers after
    /// enrolling (default `true`), so the client also learns revocations of
    /// the server. TOML `synchronize`; env `ASTER_ENROLLMENT_SYNCHRONIZE`.
    pub synchronize: Option<bool>,
}

/// `[enrollment.limits]`: overrides of the Enrollment defaults; omitted values
/// keep them. Keys are the field names; env `ASTER_ENROLLMENT_<FIELD>`
/// upper-cased (for example `ASTER_ENROLLMENT_HANDSHAKE_TIMEOUT_MS`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnrollmentLimits {
    pub handshake_timeout_ms: Option<u64>,
    pub step_timeout_ms: Option<u64>,
    pub policy_timeout_ms: Option<u64>,
    pub reconciliation_interval_ms: Option<u64>,
    pub max_admitted_peers: Option<u64>,
    /// Bound on synchronizing the fleet namespace from a replica (default
    /// 15 000).
    pub sync_timeout_ms: Option<u64>,
}

/// `[enrollment.roles]`: how a peer's verified claims become the attributes
/// Gate 3 checks `requires` against.
///
/// The peer's roles are the union of:
/// - the comma-separated value of the string claim attribute
///   [`attribute`](Self::attribute) (default `aster.role`);
/// - every verified scope, when [`scopes_as_roles`](Self::scopes_as_roles);
/// - the roles [`scopes`](Self::scopes) and [`tags`](Self::tags) map each
///   verified scope or tag to.
///
/// They are joined, sorted, into the `aster.role` attribute. When
/// [`include_attributes`](Self::include_attributes) (the default), every other
/// claim attribute is also passed through as a string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleMappingConfig {
    /// Claim attribute listing roles; an empty string disables it. TOML
    /// `attribute`; env `ASTER_ENROLLMENT_ROLE_ATTRIBUTE`.
    pub attribute: String,
    /// Treat each verified scope as a role. TOML `scopes_as_roles`; env
    /// `ASTER_ENROLLMENT_SCOPES_AS_ROLES`.
    pub scopes_as_roles: bool,
    /// Scope → roles. TOML `[enrollment.roles.scopes]`.
    pub scopes: BTreeMap<String, Vec<String>>,
    /// Tag → roles. TOML `[enrollment.roles.tags]`.
    pub tags: BTreeMap<String, Vec<String>>,
    /// Pass the other claim attributes through. TOML `include_attributes`;
    /// env `ASTER_ENROLLMENT_INCLUDE_ATTRIBUTES`.
    pub include_attributes: bool,
}

impl Default for RoleMappingConfig {
    fn default() -> Self {
        Self {
            attribute: DEFAULT_ROLE_ATTRIBUTE.into(),
            scopes_as_roles: false,
            scopes: BTreeMap::new(),
            tags: BTreeMap::new(),
            include_attributes: true,
        }
    }
}

impl EnrollmentConfig {
    /// Whether this section sets a value that only means something with
    /// enrollment on. See [`AsterConfig::is_enrollment_enabled`](crate::AsterConfig::is_enrollment_enabled).
    pub(crate) fn defines_enrollment(&self) -> bool {
        self.audience.is_some()
            || self.fleet_namespace.is_some()
            || self.issuer_delegation_file.is_some()
    }

    pub(super) fn apply_toml(&mut self, raw: &toml::Value, label: &str) -> Result<()> {
        let Some(table) = toml_section(raw, "enrollment", label)? else {
            return Ok(());
        };
        let label = &format!("{label} [enrollment]");
        if let Some(value) = toml_bool(table, "enabled", label)? {
            self.enabled = Some(value);
        }
        if let Some(value) = toml_string(table, "audience", label)? {
            self.audience = Some(value);
        }
        if let Some(value) = toml_string(table, "fleet_namespace", label)? {
            self.fleet_namespace = Some(value);
        }
        if let Some(value) = toml_path(table, "state_path", label)? {
            self.state_path = Some(value);
        }
        if let Some(value) = toml_string_list(table, "replicas", label)? {
            self.replicas = value.into_iter().filter(|v| !v.is_empty()).collect();
        }
        if let Some(value) = table.get("policy") {
            let json = serde_json::to_value(value.clone())
                .map_err(|e| Error::InvalidArgument(format!("{label} policy: {e}")))?;
            self.policy = Some(json);
        }
        if let Some(value) = toml_bool(table, "require_synchronized_clock", label)? {
            self.require_synchronized_clock = value;
        }
        if let Some(value) = toml_u64(table, "activation_leeway_secs", label)? {
            self.activation_leeway_secs = Some(value);
        }
        if let Some(value) = toml_string(table, "renew_from", label)? {
            self.renew_from = Some(value);
        }
        if let Some(value) = toml_bool(table, "renew", label)? {
            self.renew = Some(value);
        }
        if let Some(value) = toml_bool(table, "synchronize", label)? {
            self.synchronize = Some(value);
        }

        if let Some(limits) = toml_section(&toml::Value::Table(table.clone()), "limits", label)? {
            let label = &format!("{label}.limits");
            let l = &mut self.limits;
            for (key, slot) in [
                ("handshake_timeout_ms", &mut l.handshake_timeout_ms),
                ("step_timeout_ms", &mut l.step_timeout_ms),
                ("policy_timeout_ms", &mut l.policy_timeout_ms),
                (
                    "reconciliation_interval_ms",
                    &mut l.reconciliation_interval_ms,
                ),
                ("max_admitted_peers", &mut l.max_admitted_peers),
                ("sync_timeout_ms", &mut l.sync_timeout_ms),
            ] {
                if let Some(value) = toml_u64(limits, key, label)? {
                    *slot = Some(value);
                }
            }
        }

        if let Some(roles) = toml_section(&toml::Value::Table(table.clone()), "roles", label)? {
            let label = &format!("{label}.roles");
            if let Some(value) = roles.get("attribute") {
                // An empty string is meaningful here: it disables the attribute.
                self.roles.attribute = value
                    .as_str()
                    .ok_or_else(|| {
                        Error::InvalidArgument(format!("{label}: attribute must be a string"))
                    })?
                    .trim()
                    .to_string();
            }
            if let Some(value) = toml_bool(roles, "scopes_as_roles", label)? {
                self.roles.scopes_as_roles = value;
            }
            if let Some(value) = toml_bool(roles, "include_attributes", label)? {
                self.roles.include_attributes = value;
            }
            for (key, map) in [
                ("scopes", &mut self.roles.scopes),
                ("tags", &mut self.roles.tags),
            ] {
                if let Some(section) = toml_section(&toml::Value::Table(roles.clone()), key, label)?
                {
                    for name in section.keys() {
                        let roles = toml_role_list(section, name, &format!("{label}.{key}"))?;
                        map.insert(name.clone(), roles);
                    }
                }
            }
        }

        if let Some(issuer) = toml_section(&toml::Value::Table(table.clone()), "issuer", label)? {
            let label = &format!("{label}.issuer");
            if let Some(value) = toml_path(issuer, "delegation_file", label)? {
                self.issuer_delegation_file = Some(value);
            }
            if let Some(value) = issuer.get("lifetime") {
                self.issuer_lifetime_secs = Some(match value {
                    toml::Value::Integer(n) if *n > 0 => *n as u64,
                    toml::Value::String(text) => {
                        parse_duration(&format!("{label} lifetime"), text)?
                    }
                    _ => {
                        return Err(Error::InvalidArgument(format!(
                        "{label}: lifetime must be positive seconds or a duration such as \"24h\""
                    )))
                    }
                });
            }
        }
        Ok(())
    }

    pub(super) fn apply_env(&mut self) -> Result<()> {
        if let Some(value) = env_bool("ASTER_ENROLLMENT_ENABLED")? {
            self.enabled = Some(value);
        }
        if let Some(value) = env_string("ASTER_ENROLLMENT_AUDIENCE") {
            self.audience = Some(value);
        }
        if let Some(value) = env_string("ASTER_ENROLLMENT_FLEET_NAMESPACE") {
            self.fleet_namespace = Some(value);
        }
        if let Some(value) = env_path("ASTER_ENROLLMENT_STATE_PATH") {
            self.state_path = Some(value);
        }
        if let Some(value) = env_string("ASTER_ENROLLMENT_REPLICAS") {
            self.replicas = split_list(&value);
        }
        if let Some(value) = env_string("ASTER_ENROLLMENT_POLICY") {
            self.policy = Some(serde_json::from_str(&value).map_err(|e| {
                Error::InvalidArgument(format!("ASTER_ENROLLMENT_POLICY: invalid JSON: {e}"))
            })?);
        }
        if let Some(value) = env_bool("ASTER_ENROLLMENT_REQUIRE_SYNCHRONIZED_CLOCK")? {
            self.require_synchronized_clock = value;
        }
        if let Some(value) = env_u64("ASTER_ENROLLMENT_ACTIVATION_LEEWAY_SECS")? {
            self.activation_leeway_secs = Some(value);
        }
        let l = &mut self.limits;
        for (key, slot) in [
            (
                "ASTER_ENROLLMENT_HANDSHAKE_TIMEOUT_MS",
                &mut l.handshake_timeout_ms,
            ),
            ("ASTER_ENROLLMENT_STEP_TIMEOUT_MS", &mut l.step_timeout_ms),
            (
                "ASTER_ENROLLMENT_POLICY_TIMEOUT_MS",
                &mut l.policy_timeout_ms,
            ),
            (
                "ASTER_ENROLLMENT_RECONCILIATION_INTERVAL_MS",
                &mut l.reconciliation_interval_ms,
            ),
            (
                "ASTER_ENROLLMENT_MAX_ADMITTED_PEERS",
                &mut l.max_admitted_peers,
            ),
            ("ASTER_ENROLLMENT_SYNC_TIMEOUT_MS", &mut l.sync_timeout_ms),
        ] {
            if let Some(value) = env_u64(key)? {
                *slot = Some(value);
            }
        }
        if let Ok(value) = std::env::var("ASTER_ENROLLMENT_ROLE_ATTRIBUTE") {
            self.roles.attribute = value.trim().to_string();
        }
        if let Some(value) = env_bool("ASTER_ENROLLMENT_SCOPES_AS_ROLES")? {
            self.roles.scopes_as_roles = value;
        }
        if let Some(value) = env_bool("ASTER_ENROLLMENT_INCLUDE_ATTRIBUTES")? {
            self.roles.include_attributes = value;
        }
        if let Some(value) = env_path("ASTER_ENROLLMENT_ISSUER_DELEGATION_FILE") {
            self.issuer_delegation_file = Some(value);
        }
        if let Some(value) = env_string("ASTER_ENROLLMENT_ISSUER_LIFETIME") {
            self.issuer_lifetime_secs =
                Some(parse_duration("ASTER_ENROLLMENT_ISSUER_LIFETIME", &value)?);
        }
        if let Some(value) = env_string("ASTER_ENROLLMENT_RENEW_FROM") {
            self.renew_from = Some(value);
        }
        if let Some(value) = env_bool("ASTER_ENROLLMENT_RENEW")? {
            self.renew = Some(value);
        }
        if let Some(value) = env_bool("ASTER_ENROLLMENT_SYNCHRONIZE")? {
            self.synchronize = Some(value);
        }
        Ok(())
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// A role list: an array of strings, or one comma-separated string.
fn toml_role_list(table: &toml::Table, key: &str, label: &str) -> Result<Vec<String>> {
    match table.get(key) {
        Some(toml::Value::String(value)) => Ok(split_list(value)),
        Some(toml::Value::Array(_)) => Ok(toml_string_list(table, key, label)?
            .unwrap_or_default()
            .into_iter()
            .filter(|role| !role.is_empty())
            .collect()),
        _ => Err(Error::InvalidArgument(format!(
            "{label}.{key}: expected a string or an array of strings"
        ))),
    }
}

/// `<n>s`, `<n>m`, `<n>h` or `<n>d`; a bare number is seconds. Zero is
/// refused.
pub(crate) fn parse_duration(source: &str, value: &str) -> Result<u64> {
    let value = value.trim();
    let (digits, scale) = match value.char_indices().last() {
        Some((index, unit)) if unit.is_ascii_alphabetic() => (
            &value[..index],
            match unit {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                'd' => 86_400,
                _ => 0,
            },
        ),
        _ => (value, 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .filter(|n| scale > 0 && *n > 0)
        .and_then(|n| n.checked_mul(scale))
        .ok_or_else(|| {
            Error::InvalidArgument(format!(
                "{source}: invalid duration {value:?}; use a positive number with s, m, h or d"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> EnrollmentConfig {
        let raw: toml::Value = text.parse().unwrap();
        let mut config = EnrollmentConfig::default();
        config.apply_toml(&raw, "test").unwrap();
        config
    }

    fn enabled(text: &str) -> bool {
        let raw: toml::Value = text.parse().unwrap();
        let mut config = crate::AsterConfig::default();
        config.apply_toml_value(&raw, "test").unwrap();
        config.is_enrollment_enabled()
    }

    #[test]
    fn absent_section_is_disabled() {
        assert!(!enabled(""));
        assert!(!crate::AsterConfig::default().is_enrollment_enabled());
    }

    #[test]
    fn any_defining_value_enables_and_enabled_false_wins() {
        assert!(enabled("[enrollment]\naudience = \"mc\""));
        assert!(enabled("[trust]\nenrollment_credential = \"a.cred\""));
        assert!(enabled(
            "[trust]\nenrollment_credential_chain = \"aster-chain-v1~x\""
        ));
        assert!(!enabled("[enrollment]\nenabled = false\naudience = \"mc\""));
        assert!(enabled("[enrollment]\nenabled = true"));
        // Root and rotation pin alone (coordination uses the root) do not.
        assert!(!enabled(
            "[trust]\nroot_pubkey_file = \"r.pub\"\nroot_seq = 1"
        ));
    }

    #[test]
    fn retired_consumer_admission_keys_do_not_enable_or_open_anything() {
        assert!(!enabled(
            "[trust]\nallow_all_consumers = true\nenrollment_credential_iid = \"x\""
        ));
    }

    #[test]
    fn full_section_parses() {
        let config = parse(
            r#"
[enrollment]
audience = "mission-control"
fleet_namespace = "aa"
state_path = "/var/lib/mc/enrollment.json"
replicas = ["aster1abc", "bb"]
policy = { all_of = [{ scope = "ops" }, "tolerate_stale_policy"] }
activation_leeway_secs = 30
renew_from = "dd"
synchronize = false

[enrollment.limits]
handshake_timeout_ms = 5000
sync_timeout_ms = 2000

[enrollment.roles]
attribute = ""
scopes_as_roles = true
include_attributes = false
[enrollment.roles.scopes]
"fleet.ops" = ["ops.status", "ops.logs"]
[enrollment.roles.tags]
admin = "ops.admin, ops.logs"

[enrollment.issuer]
delegation_file = "issuer.chain"
lifetime = "6h"
"#,
        );
        assert_eq!(config.audience.as_deref(), Some("mission-control"));
        assert_eq!(config.replicas, ["aster1abc", "bb"]);
        assert_eq!(
            config.policy.unwrap(),
            serde_json::json!({"all_of": [{"scope": "ops"}, "tolerate_stale_policy"]})
        );
        assert_eq!(config.limits.handshake_timeout_ms, Some(5000));
        assert_eq!(config.limits.sync_timeout_ms, Some(2000));
        assert_eq!(config.roles.attribute, "");
        assert!(config.roles.scopes_as_roles);
        assert!(!config.roles.include_attributes);
        assert_eq!(config.roles.scopes["fleet.ops"], ["ops.status", "ops.logs"]);
        assert_eq!(config.roles.tags["admin"], ["ops.admin", "ops.logs"]);
        assert_eq!(config.issuer_lifetime_secs, Some(6 * 3600));
        assert_eq!(config.synchronize, Some(false));
    }

    #[test]
    fn durations_are_strict() {
        assert_eq!(parse_duration("t", "90").unwrap(), 90);
        assert_eq!(parse_duration("t", "2m").unwrap(), 120);
        for bad in ["", "0", "h", "1x", "-1h", "1.5h"] {
            assert!(parse_duration("t", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn wrong_types_are_errors() {
        let raw: toml::Value = "[enrollment]\naudience = 3".parse().unwrap();
        assert!(EnrollmentConfig::default().apply_toml(&raw, "t").is_err());
        let raw: toml::Value = "[enrollment.issuer]\nlifetime = 0".parse().unwrap();
        assert!(EnrollmentConfig::default().apply_toml(&raw, "t").is_err());
        let raw: toml::Value = "[enrollment.roles.scopes]\nx = 1".parse().unwrap();
        assert!(EnrollmentConfig::default().apply_toml(&raw, "t").is_err());
    }
}
