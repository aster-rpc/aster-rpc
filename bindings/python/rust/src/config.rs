//! `aster::AsterConfig` for Python.
//!
//! `NativeAsterConfig` carries a resolved `aster::AsterConfig`. The Python
//! `aster.AsterConfig` (a frozen dataclass in `aster/config.py`) is the user
//! surface; it round-trips through this class:
//!
//! - loading: `NativeAsterConfig.from_env()` / `.from_file(path)` run the Rust
//!   loaders, and `to_dict()` hands every field to the dataclass;
//! - starting: the dataclass builds `NativeAsterConfig(**fields)` and passes it
//!   to `Node.start`, so the Rust config is the single source of truth.
//!
//! Field names are the `aster::AsterConfig` field names (also the TOML keys),
//! except `gc_interval_ms` (the `Duration` in milliseconds) and the relay mode,
//! which is split into `relay_mode` (`"default"` / `"staging"` / `"disabled"` /
//! `"custom"`) plus `relay_urls`. `enrollment` is the `[enrollment]` section
//! (`aster::EnrollmentConfig`) as a dict of its field names, with `limits` and
//! `roles` nested dicts and `policy` as JSON text.

use std::path::PathBuf;
use std::time::Duration;

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use aster::{
    AsterConfig, ConfigSource, DiscoveryMode, EnrollmentConfig, EnrollmentLimits, HookFailureMode,
    RelayMode, RoleMappingConfig,
};

use crate::error::aster_err_to_py;
use crate::ids::{extract_public_key, extract_secret_key, PublicKey, SecretKey};

/// Every field `NativeAsterConfig(**kwargs)` accepts and `to_dict()` returns.
pub(crate) const FIELDS: &[&str] = &[
    "root_pubkey",
    "root_pubkey_file",
    "next_root_commitment",
    "root_seq",
    "root_rotations",
    "enrollment_credential_file",
    "enrollment_credential_chain",
    "endpoint_addr",
    "storage_path",
    "gc_interval_ms",
    "blob_zstd_level",
    "secret_key",
    "secret_key_file",
    "relay_mode",
    "relay_urls",
    "bind_addr",
    "bind_addr_v6",
    "advertise_addrs",
    "relay_ca_certs",
    "relay_ca_only",
    "discovery",
    "discovery_dns_origin",
    "discovery_pkarr_relays",
    "reuse_last_port",
    "local_discovery",
    "enable_monitoring",
    "enable_hooks",
    "hook_timeout_ms",
    "hook_failure_mode",
    "clear_ip_transports",
    "clear_relay_transports",
    "portmapper_config",
    "proxy_url",
    "proxy_from_env",
    "transport_max_concurrent_bidi_streams",
    "transport_max_concurrent_uni_streams",
    "transport_stream_receive_window",
    "transport_receive_window",
    "transport_send_window",
    "transport_max_idle_timeout_ms",
    "transport_keep_alive_interval_ms",
    "transport_initial_mtu",
    "transport_datagram_receive_buffer_size",
    "transport_datagram_send_buffer_size",
    "transport_send_fairness",
    "transport_enable_segmentation_offload",
    "log_format",
    "log_level",
    "log_mask",
    "health_port",
    "health_host",
    "health_enabled",
    "metrics_peer_labels",
    "metrics_peer_label_cap",
    "identity_file",
    "alpns",
    "enrollment",
    "identity_accept_on_behalf_of",
    "identity_trusted_issuers",
    "identity_edge_token_max_lifetime_secs",
    "identity_clock_leeway_secs",
    "identity_max_key_set_age_secs",
    "identity_publisher",
    "remote_config_enabled",
    "remote_config_lock",
    "setting_sources",
];

/// A resolved `aster::AsterConfig` (see module docs). Immutable.
#[pyclass(frozen, skip_from_py_object, module = "aster._aster")]
#[derive(Clone)]
pub struct NativeAsterConfig {
    pub(crate) inner: AsterConfig,
}

fn opt<'a, 'py, T>(v: &'a Bound<'py, PyAny>) -> PyResult<Option<T>>
where
    T: FromPyObject<'a, 'py>,
    T::Error: Into<PyErr>,
{
    if v.is_none() {
        Ok(None)
    } else {
        v.extract::<T>().map(Some).map_err(Into::into)
    }
}

fn opt_path(v: &Bound<'_, PyAny>) -> PyResult<Option<PathBuf>> {
    if v.is_none() {
        return Ok(None);
    }
    let p: PathBuf = v.extract()?;
    Ok(if p.as_os_str().is_empty() {
        None
    } else {
        Some(p)
    })
}

fn opt_str(v: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    Ok(opt::<String>(v)?.filter(|s| !s.trim().is_empty()))
}

/// A list of strings from a list/tuple, or one comma-separated string (the env
/// spelling). Items are trimmed; empty items are dropped.
fn str_list(v: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    if v.is_none() {
        return Ok(Vec::new());
    }
    let items: Vec<String> = if let Ok(text) = v.extract::<String>() {
        text.split(',').map(str::to_string).collect()
    } else {
        v.extract()?
    };
    Ok(items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// A JSON-shaped Python value (`None`, a JSON string, or lists / dicts of
/// plain values) as `serde_json::Value`; `None` for `None` or an empty string.
fn from_json_value(v: &Bound<'_, PyAny>) -> PyResult<Option<serde_json::Value>> {
    if v.is_none() {
        return Ok(None);
    }
    let text: String = if let Ok(text) = v.extract::<String>() {
        if text.trim().is_empty() {
            return Ok(None);
        }
        text
    } else {
        v.py()
            .import("json")?
            .call_method1("dumps", (v,))?
            .extract()?
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| PyValueError::new_err(format!("expected JSON: {e}")))
}

/// A serializable value as plain Python values (`json.loads`).
fn to_json_py<'py>(py: Python<'py>, value: &impl serde::Serialize) -> PyResult<Bound<'py, PyAny>> {
    let text = serde_json::to_string(value).map_err(|e| PyValueError::new_err(e.to_string()))?;
    py.import("json")?.call_method1("loads", (text,))
}

fn parse_source(source: &str) -> PyResult<ConfigSource> {
    Ok(match source {
        "default" => ConfigSource::Default,
        "file" => ConfigSource::File,
        "env" => ConfigSource::Env,
        "code" => ConfigSource::Code,
        "override" => ConfigSource::Override,
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown source {other:?} (expected default, file, env, code or override)"
            )))
        }
    })
}

fn strict_bool(name: &str, v: &Bound<'_, PyAny>) -> PyResult<bool> {
    if !v.is_instance_of::<pyo3::types::PyBool>() {
        return Err(PyTypeError::new_err(format!("{name}: expected bool")));
    }
    v.extract()
}

fn opt_bool(name: &str, v: &Bound<'_, PyAny>) -> PyResult<Option<bool>> {
    if v.is_none() {
        return Ok(None);
    }
    strict_bool(name, v).map(Some)
}

/// `AsterConfig::blob_zstd_level` from Python: an int (`0` = off, larger
/// than 19 clamped), `None` (off, as `to_dict` reports it), or a string
/// (`ASTER_BLOB_ZSTD_LEVEL` syntax: a level, or `off` / `none` / `false` /
/// `disabled`).
pub(crate) fn blob_zstd_level_from_py(v: &Bound<'_, PyAny>) -> PyResult<Option<u8>> {
    const NAME: &str = "blob_zstd_level";
    if v.is_none() {
        return Ok(None);
    }
    if v.is_instance_of::<pyo3::types::PyBool>() {
        return Err(PyTypeError::new_err(format!(
            "{NAME}: expected an int level or \"off\", got bool"
        )));
    }
    let level: i64 = if let Ok(s) = v.extract::<String>() {
        let s = s.trim().to_ascii_lowercase();
        if matches!(s.as_str(), "off" | "none" | "false" | "disabled") {
            return Ok(None);
        }
        s.parse().map_err(|_| {
            PyValueError::new_err(format!(
                "{NAME}: expected a zstd level or \"off\", got {s:?}"
            ))
        })?
    } else {
        v.extract()?
    };
    if level < 0 {
        return Err(PyValueError::new_err(format!(
            "{NAME}: expected 0 (off) or a level, got {level}"
        )));
    }
    Ok((level > 0).then(|| level.min(19) as u8))
}

fn parse_hook_failure_mode(v: &str) -> PyResult<HookFailureMode> {
    match v.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "" | "fail_closed" | "closed" => Ok(HookFailureMode::FailClosed),
        "fail_open" | "open" => Ok(HookFailureMode::FailOpen),
        other => Err(PyValueError::new_err(format!(
            "hook_failure_mode: expected fail_open or fail_closed, got {other:?}"
        ))),
    }
}

fn hook_failure_mode_str(m: HookFailureMode) -> &'static str {
    match m {
        HookFailureMode::FailOpen => "fail_open",
        HookFailureMode::FailClosed => "fail_closed",
    }
}

fn relay_from(mode: Option<String>, urls: Vec<String>) -> PyResult<RelayMode> {
    let mode = mode.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
    let lower = mode.as_deref().map(str::to_ascii_lowercase);
    match lower.as_deref() {
        None | Some("custom") => {
            if urls.is_empty() {
                if lower.is_some() {
                    return Err(PyValueError::new_err(
                        "relay_mode='custom' requires relay_urls",
                    ));
                }
                Ok(RelayMode::Default)
            } else {
                Ok(RelayMode::Custom(urls))
            }
        }
        Some(m @ ("default" | "staging" | "disabled")) => {
            if !urls.is_empty() {
                return Err(PyValueError::new_err(format!(
                    "relay_urls requires relay_mode 'custom' (or None), got {m:?}"
                )));
            }
            Ok(match m {
                "staging" => RelayMode::Staging,
                "disabled" => RelayMode::Disabled,
                _ => RelayMode::Default,
            })
        }
        // A bare URL means a custom relay.
        Some(_) => {
            let mut all = vec![mode.unwrap_or_default()];
            all.extend(urls);
            Ok(RelayMode::Custom(all))
        }
    }
}

/// `discovery` plus the custom discovery servers, with the C ABI / builder
/// rule: servers with `discovery` unset or `"default"` select `"custom"`;
/// servers with `"none"` or `"local"` raise `ValueError`.
fn discovery_from(
    mode: Option<String>,
    dns_origin: Option<String>,
    pkarr_relays: Vec<String>,
) -> PyResult<(DiscoveryMode, Option<String>, Vec<String>)> {
    let parsed = match mode.as_deref() {
        None => DiscoveryMode::Default,
        Some(m) => DiscoveryMode::parse(m).map_err(aster_err_to_py)?,
    };
    let servers = dns_origin.is_some() || !pkarr_relays.is_empty();
    let mode = match (parsed, servers) {
        (DiscoveryMode::Default, true) => DiscoveryMode::Custom,
        (m @ (DiscoveryMode::None | DiscoveryMode::Local), true) => {
            return Err(PyValueError::new_err(format!(
                "discovery_dns_origin / discovery_pkarr_relays require discovery \"custom\", got {:?}",
                m.as_str()
            )))
        }
        (m, _) => m,
    };
    Ok((mode, dns_origin, pkarr_relays))
}

fn apply_field(c: &mut AsterConfig, key: &str, v: &Bound<'_, PyAny>) -> PyResult<()> {
    match key {
        "root_pubkey" => {
            c.root_pubkey = if v.is_none() {
                None
            } else {
                Some(extract_public_key(v)?)
            }
        }
        "root_pubkey_file" => c.root_pubkey_file = opt_path(v)?,
        "next_root_commitment" => c.next_root_commitment = opt_str(v)?,
        "root_seq" => c.root_seq = v.extract()?,
        "root_rotations" => {
            c.root_rotations = if v.is_none() {
                Vec::new()
            } else {
                v.extract::<Vec<String>>()?
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
        }
        "enrollment_credential_file" => c.enrollment_credential_file = opt_path(v)?,
        "enrollment_credential_chain" => c.enrollment_credential_chain = opt_str(v)?,
        "enrollment" => {
            c.enrollment = if v.is_none() {
                EnrollmentConfig::default()
            } else {
                enrollment_from_py(v.cast::<PyDict>()?)?
            }
        }
        "endpoint_addr" => c.endpoint_addr = opt_str(v)?,
        "storage_path" => c.storage_path = opt_path(v)?,
        "gc_interval_ms" => {
            c.gc_interval = opt::<u64>(v)?
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis)
        }
        "secret_key" => {
            c.secret_key = if v.is_none() {
                None
            } else {
                Some(extract_secret_key(v)?)
            }
        }
        "secret_key_file" => c.secret_key_file = opt_path(v)?,
        "blob_zstd_level" => c.blob_zstd_level = blob_zstd_level_from_py(v)?,
        "reuse_last_port" => c.reuse_last_port = strict_bool(key, v)?,
        // relay_mode / relay_urls are combined by the caller.
        "relay_mode" | "relay_urls" => {}
        "bind_addr" => c.bind_addr = opt_str(v)?,
        "bind_addr_v6" => c.bind_addr_v6 = opt_str(v)?,
        "advertise_addrs" => c.advertise_addrs = str_list(v)?,
        "relay_ca_certs" => {
            c.relay_ca_certs = if v.is_none() {
                Vec::new()
            } else if v.extract::<String>().is_ok() {
                str_list(v)?.into_iter().map(PathBuf::from).collect()
            } else {
                let mut out = Vec::new();
                for item in v.try_iter()? {
                    let p: PathBuf = item?.extract()?;
                    if !p.as_os_str().is_empty() {
                        out.push(p);
                    }
                }
                out
            }
        }
        "relay_ca_only" => c.relay_ca_only = strict_bool(key, v)?,
        // discovery / discovery_dns_origin / discovery_pkarr_relays are
        // combined by the caller.
        "discovery" | "discovery_dns_origin" | "discovery_pkarr_relays" => {}
        "local_discovery" => c.local_discovery = strict_bool(key, v)?,
        "enable_monitoring" => c.enable_monitoring = strict_bool(key, v)?,
        "enable_hooks" => c.enable_hooks = strict_bool(key, v)?,
        "hook_timeout_ms" => c.hook_timeout_ms = v.extract()?,
        "hook_failure_mode" => {
            c.hook_failure_mode = match opt::<String>(v)? {
                None => HookFailureMode::default(),
                Some(s) => parse_hook_failure_mode(&s)?,
            }
        }
        "clear_ip_transports" => c.clear_ip_transports = strict_bool(key, v)?,
        "clear_relay_transports" => c.clear_relay_transports = strict_bool(key, v)?,
        "portmapper_config" => {
            c.portmapper_config = match opt_str(v)? {
                None => None,
                Some(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "enabled" | "enable" | "on" | "true" => Some("enabled".to_string()),
                    "disabled" | "disable" | "off" | "false" => Some("disabled".to_string()),
                    other => {
                        return Err(PyValueError::new_err(format!(
                            "portmapper_config: expected 'enabled' or 'disabled', got {other:?}"
                        )))
                    }
                },
            }
        }
        "proxy_url" => c.proxy_url = opt_str(v)?,
        "proxy_from_env" => c.proxy_from_env = strict_bool(key, v)?,
        "transport_max_concurrent_bidi_streams" => {
            c.transport_max_concurrent_bidi_streams = opt(v)?
        }
        "transport_max_concurrent_uni_streams" => c.transport_max_concurrent_uni_streams = opt(v)?,
        "transport_stream_receive_window" => c.transport_stream_receive_window = opt(v)?,
        "transport_receive_window" => c.transport_receive_window = opt(v)?,
        "transport_send_window" => c.transport_send_window = opt(v)?,
        "transport_max_idle_timeout_ms" => c.transport_max_idle_timeout_ms = opt(v)?,
        "transport_keep_alive_interval_ms" => c.transport_keep_alive_interval_ms = opt(v)?,
        "transport_initial_mtu" => c.transport_initial_mtu = opt(v)?,
        "transport_datagram_receive_buffer_size" => {
            c.transport_datagram_receive_buffer_size = opt(v)?
        }
        "transport_datagram_send_buffer_size" => c.transport_datagram_send_buffer_size = opt(v)?,
        "transport_send_fairness" => c.transport_send_fairness = opt_bool(key, v)?,
        "transport_enable_segmentation_offload" => {
            c.transport_enable_segmentation_offload = opt_bool(key, v)?
        }
        "log_format" => c.log_format = v.extract::<String>()?.trim().to_ascii_lowercase(),
        "log_level" => c.log_level = v.extract::<String>()?.trim().to_ascii_lowercase(),
        "log_mask" => c.log_mask = strict_bool(key, v)?,
        "health_port" => c.health_port = v.extract()?,
        "health_host" => c.health_host = opt_str(v)?.unwrap_or_else(|| "127.0.0.1".to_string()),
        "health_enabled" => c.health_enabled = opt_bool(key, v)?,
        "metrics_peer_labels" => c.metrics_peer_labels = strict_bool(key, v)?,
        "metrics_peer_label_cap" => {
            c.metrics_peer_label_cap =
                opt::<usize>(v)?.unwrap_or(aster::DEFAULT_METRICS_PEER_LABEL_CAP)
        }
        "identity_file" => c.identity_file = opt_path(v)?,
        "identity_accept_on_behalf_of" => c.identity_accept_on_behalf_of = strict_bool(key, v)?,
        "identity_trusted_issuers" => {
            c.identity_trusted_issuers = match from_json_value(v)? {
                None => Vec::new(),
                Some(value) => serde_json::from_value(value)
                    .map_err(|e| PyValueError::new_err(format!("expected trusted issuers: {e}")))?,
            }
        }
        "identity_edge_token_max_lifetime_secs" => {
            c.identity_edge_token_max_lifetime_secs = v.extract()?
        }
        "identity_clock_leeway_secs" => c.identity_clock_leeway_secs = v.extract()?,
        "identity_max_key_set_age_secs" => c.identity_max_key_set_age_secs = v.extract()?,
        "identity_publisher" => {
            c.identity_publisher = match from_json_value(v)? {
                None => None,
                Some(value) => Some(serde_json::from_value(value).map_err(|e| {
                    PyValueError::new_err(format!("expected [identity.publisher] fields: {e}"))
                })?),
            }
        }
        "remote_config_enabled" => c.remote_config_enabled = strict_bool(key, v)?,
        "remote_config_lock" => c.remote_config_lock = str_list(v)?,
        "setting_sources" => {
            c.setting_sources.clear();
            if !v.is_none() {
                for (name, source) in v.cast::<PyDict>()?.iter() {
                    let name: String = name.extract()?;
                    let source: String = source.extract()?;
                    c.setting_sources.insert(name, parse_source(&source)?);
                }
            }
        }
        "alpns" => {
            let mut out: Vec<Vec<u8>> = Vec::new();
            if !v.is_none() {
                for item in v.try_iter()? {
                    let item = item?;
                    let alpn = if let Ok(s) = item.extract::<String>() {
                        s.into_bytes()
                    } else if let Ok(b) = item.cast::<PyBytes>() {
                        b.as_bytes().to_vec()
                    } else {
                        return Err(PyTypeError::new_err("alpns: expected bytes or str items"));
                    };
                    if !out.contains(&alpn) {
                        out.push(alpn);
                    }
                }
            }
            c.alpns = out;
        }
        other => {
            return Err(PyTypeError::new_err(format!(
                "AsterConfig: unknown field {other:?}"
            )))
        }
    }
    Ok(())
}

/// `[enrollment]` from its dict form (see module docs). Unknown keys raise
/// `TypeError`; omitted keys keep the defaults.
fn enrollment_from_py(d: &Bound<'_, PyDict>) -> PyResult<EnrollmentConfig> {
    let mut e = EnrollmentConfig::default();
    for (k, v) in d.iter() {
        let key: String = k.extract()?;
        let named = |err: PyErr| prefix_err(d.py(), &format!("enrollment.{key}"), err);
        match key.as_str() {
            "enabled" => e.enabled = opt_bool(&key, &v).map_err(named)?,
            "audience" => e.audience = opt_str(&v).map_err(named)?,
            "fleet_namespace" => e.fleet_namespace = opt_str(&v).map_err(named)?,
            "state_path" => e.state_path = opt_path(&v).map_err(named)?,
            "replicas" => {
                e.replicas = if v.is_none() {
                    Vec::new()
                } else {
                    v.extract::<Vec<String>>()
                        .map_err(named)?
                        .into_iter()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                }
            }
            "policy" => {
                e.policy = match opt::<String>(&v).map_err(named)? {
                    None => None,
                    Some(text) => Some(serde_json::from_str(&text).map_err(|err| {
                        PyValueError::new_err(format!("enrollment.policy: invalid JSON: {err}"))
                    })?),
                }
            }
            "require_synchronized_clock" => {
                e.require_synchronized_clock = strict_bool(&key, &v).map_err(named)?
            }
            "activation_leeway_secs" => e.activation_leeway_secs = opt(&v).map_err(named)?,
            "limits" => {
                if !v.is_none() {
                    e.limits =
                        limits_from_py(v.cast::<PyDict>().map_err(PyErr::from).map_err(named)?)?;
                }
            }
            "roles" => {
                if !v.is_none() {
                    e.roles =
                        roles_from_py(v.cast::<PyDict>().map_err(PyErr::from).map_err(named)?)?;
                }
            }
            "issuer_delegation_file" => e.issuer_delegation_file = opt_path(&v).map_err(named)?,
            "issuer_lifetime_secs" => {
                e.issuer_lifetime_secs = opt::<u64>(&v).map_err(named)?;
                if e.issuer_lifetime_secs == Some(0) {
                    return Err(PyValueError::new_err(
                        "enrollment.issuer_lifetime_secs: must be positive",
                    ));
                }
            }
            "renew_from" => e.renew_from = opt_str(&v).map_err(named)?,
            "renew" => e.renew = opt_bool(&key, &v).map_err(named)?,
            "synchronize" => e.synchronize = opt_bool(&key, &v).map_err(named)?,
            other => {
                return Err(PyTypeError::new_err(format!(
                    "AsterConfig.enrollment: unknown field {other:?}"
                )))
            }
        }
    }
    Ok(e)
}

fn limits_from_py(d: &Bound<'_, PyDict>) -> PyResult<EnrollmentLimits> {
    let mut l = EnrollmentLimits::default();
    for (k, v) in d.iter() {
        let key: String = k.extract()?;
        let value: Option<u64> =
            opt(&v).map_err(|err| prefix_err(d.py(), &format!("enrollment.limits.{key}"), err))?;
        match key.as_str() {
            "handshake_timeout_ms" => l.handshake_timeout_ms = value,
            "step_timeout_ms" => l.step_timeout_ms = value,
            "policy_timeout_ms" => l.policy_timeout_ms = value,
            "reconciliation_interval_ms" => l.reconciliation_interval_ms = value,
            "max_admitted_peers" => l.max_admitted_peers = value,
            "sync_timeout_ms" => l.sync_timeout_ms = value,
            other => {
                return Err(PyTypeError::new_err(format!(
                    "AsterConfig.enrollment.limits: unknown field {other:?}"
                )))
            }
        }
    }
    Ok(l)
}

/// A role list: a list of strings, or one comma-separated string.
fn role_list(v: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    let items: Vec<String> = if let Ok(text) = v.extract::<String>() {
        text.split(',').map(str::to_string).collect()
    } else {
        v.extract()?
    };
    Ok(items
        .into_iter()
        .map(|role| role.trim().to_string())
        .filter(|role| !role.is_empty())
        .collect())
}

fn roles_from_py(d: &Bound<'_, PyDict>) -> PyResult<RoleMappingConfig> {
    let mut r = RoleMappingConfig::default();
    for (k, v) in d.iter() {
        let key: String = k.extract()?;
        let named = |err: PyErr| prefix_err(d.py(), &format!("enrollment.roles.{key}"), err);
        match key.as_str() {
            "attribute" => {
                r.attribute = opt::<String>(&v)
                    .map_err(named)?
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            }
            "scopes_as_roles" => r.scopes_as_roles = strict_bool(&key, &v).map_err(named)?,
            "include_attributes" => r.include_attributes = strict_bool(&key, &v).map_err(named)?,
            "scopes" | "tags" => {
                let mut map = std::collections::BTreeMap::new();
                if !v.is_none() {
                    for (name, roles) in v
                        .cast::<PyDict>()
                        .map_err(PyErr::from)
                        .map_err(named)?
                        .iter()
                    {
                        map.insert(name.extract::<String>()?, role_list(&roles).map_err(named)?);
                    }
                }
                if key == "scopes" {
                    r.scopes = map;
                } else {
                    r.tags = map;
                }
            }
            other => {
                return Err(PyTypeError::new_err(format!(
                    "AsterConfig.enrollment.roles: unknown field {other:?}"
                )))
            }
        }
    }
    Ok(r)
}

fn enrollment_to_py<'py>(py: Python<'py>, e: &EnrollmentConfig) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("enabled", e.enabled)?;
    d.set_item("audience", e.audience.clone())?;
    d.set_item("fleet_namespace", e.fleet_namespace.clone())?;
    d.set_item("state_path", path_str(&e.state_path))?;
    d.set_item("replicas", e.replicas.clone())?;
    d.set_item(
        "policy",
        e.policy.as_ref().map(serde_json::Value::to_string),
    )?;
    d.set_item("require_synchronized_clock", e.require_synchronized_clock)?;
    d.set_item("activation_leeway_secs", e.activation_leeway_secs)?;
    let l = PyDict::new(py);
    l.set_item("handshake_timeout_ms", e.limits.handshake_timeout_ms)?;
    l.set_item("step_timeout_ms", e.limits.step_timeout_ms)?;
    l.set_item("policy_timeout_ms", e.limits.policy_timeout_ms)?;
    l.set_item(
        "reconciliation_interval_ms",
        e.limits.reconciliation_interval_ms,
    )?;
    l.set_item("max_admitted_peers", e.limits.max_admitted_peers)?;
    l.set_item("sync_timeout_ms", e.limits.sync_timeout_ms)?;
    d.set_item("limits", l)?;
    let r = PyDict::new(py);
    r.set_item("attribute", e.roles.attribute.clone())?;
    r.set_item("scopes_as_roles", e.roles.scopes_as_roles)?;
    r.set_item("scopes", e.roles.scopes.clone())?;
    r.set_item("tags", e.roles.tags.clone())?;
    r.set_item("include_attributes", e.roles.include_attributes)?;
    d.set_item("roles", r)?;
    d.set_item(
        "issuer_delegation_file",
        path_str(&e.issuer_delegation_file),
    )?;
    d.set_item("issuer_lifetime_secs", e.issuer_lifetime_secs)?;
    d.set_item("renew_from", e.renew_from.clone())?;
    d.set_item("renew", e.renew)?;
    d.set_item("synchronize", e.synchronize)?;
    Ok(d)
}

fn path_str(p: &Option<PathBuf>) -> Option<String> {
    p.as_ref().map(|p| p.to_string_lossy().into_owned())
}

impl NativeAsterConfig {
    pub(crate) fn from_kwargs(kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<AsterConfig> {
        let mut c = AsterConfig::default();
        let mut relay_mode: Option<String> = None;
        let mut relay_urls: Vec<String> = Vec::new();
        let mut discovery: Option<String> = None;
        let mut dns_origin: Option<String> = None;
        let mut pkarr_relays: Vec<String> = Vec::new();
        if let Some(kwargs) = kwargs {
            for (k, v) in kwargs.iter() {
                let key: String = k.extract()?;
                match key.as_str() {
                    "relay_mode" => relay_mode = opt::<String>(&v)?,
                    "relay_urls" => {
                        relay_urls = if v.is_none() {
                            Vec::new()
                        } else {
                            v.extract::<Vec<String>>()?
                                .into_iter()
                                .map(|u| u.trim().to_string())
                                .filter(|u| !u.is_empty())
                                .collect()
                        }
                    }
                    "discovery" => {
                        discovery = opt_str(&v).map_err(|e| prefix_err(k.py(), &key, e))?
                    }
                    "discovery_dns_origin" => {
                        dns_origin = opt_str(&v).map_err(|e| prefix_err(k.py(), &key, e))?
                    }
                    "discovery_pkarr_relays" => {
                        pkarr_relays = str_list(&v).map_err(|e| prefix_err(k.py(), &key, e))?
                    }
                    _ => {}
                }
                apply_field(&mut c, &key, &v).map_err(|e| prefix_err(k.py(), &key, e))?;
            }
        }
        c.relay_mode = relay_from(relay_mode, relay_urls)?;
        let (mode, dns_origin, pkarr_relays) = discovery_from(discovery, dns_origin, pkarr_relays)?;
        c.discovery = mode;
        c.discovery_dns_origin = dns_origin;
        c.discovery_pkarr_relays = pkarr_relays;
        Ok(c)
    }
}

fn prefix_err(py: Python<'_>, key: &str, e: PyErr) -> PyErr {
    // Name the field in the message, keeping the exception type.
    let msg = e.value(py).to_string();
    if msg.starts_with(key) || msg.starts_with("AsterConfig") {
        return e;
    }
    match e.get_type(py).call1((format!("{key}: {msg}"),)) {
        Ok(value) => PyErr::from_value(value),
        Err(_) => e,
    }
}

#[pymethods]
impl NativeAsterConfig {
    /// Build from keyword fields over `aster::AsterConfig::default()`. Unknown
    /// fields raise `TypeError`.
    #[new]
    #[pyo3(signature = (**kwargs))]
    fn new(kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<Self> {
        Ok(Self {
            inner: Self::from_kwargs(kwargs)?,
        })
    }

    /// `AsterConfig::from_env()`: defaults < `ASTER_*` environment.
    #[staticmethod]
    fn from_env() -> PyResult<Self> {
        AsterConfig::from_env()
            .map(|inner| Self { inner })
            .map_err(aster_err_to_py)
    }

    /// `AsterConfig::from_file(path)`: defaults < TOML file < `ASTER_*` env.
    #[staticmethod]
    fn from_file(path: PathBuf) -> PyResult<Self> {
        if !path.exists() {
            return Err(pyo3::exceptions::PyFileNotFoundError::new_err(format!(
                "config file not found: {}",
                path.display()
            )));
        }
        AsterConfig::from_file(path)
            .map(|inner| Self { inner })
            .map_err(aster_err_to_py)
    }

    /// Defaults < TOML file only (no env). Used for provenance reporting.
    #[staticmethod]
    fn file_only(path: PathBuf) -> PyResult<Self> {
        let mut inner = AsterConfig::default();
        inner.apply_file(path).map_err(aster_err_to_py)?;
        Ok(Self { inner })
    }

    /// The field names `to_dict()` returns and the constructor accepts.
    #[staticmethod]
    fn field_names() -> Vec<&'static str> {
        FIELDS.to_vec()
    }

    /// Every field as plain Python values (bytes / str / int / bool / None /
    /// list).
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let c = &self.inner;
        let d = PyDict::new(py);
        d.set_item(
            "root_pubkey",
            c.root_pubkey.map(|k| PyBytes::new(py, &k.to_bytes())),
        )?;
        d.set_item("root_pubkey_file", path_str(&c.root_pubkey_file))?;
        d.set_item("next_root_commitment", c.next_root_commitment.clone())?;
        d.set_item("root_seq", c.root_seq)?;
        d.set_item("root_rotations", c.root_rotations.clone())?;
        d.set_item(
            "enrollment_credential_file",
            path_str(&c.enrollment_credential_file),
        )?;
        d.set_item(
            "enrollment_credential_chain",
            c.enrollment_credential_chain.clone(),
        )?;
        d.set_item("endpoint_addr", c.endpoint_addr.clone())?;
        d.set_item("storage_path", path_str(&c.storage_path))?;
        d.set_item(
            "gc_interval_ms",
            c.gc_interval.map(|g| g.as_millis() as u64),
        )?;
        d.set_item(
            "secret_key",
            c.secret_key
                .as_ref()
                .map(|k| PyBytes::new(py, &k.to_bytes())),
        )?;
        d.set_item("secret_key_file", path_str(&c.secret_key_file))?;
        d.set_item("blob_zstd_level", c.blob_zstd_level)?;
        d.set_item("reuse_last_port", c.reuse_last_port)?;
        let (mode, urls): (&str, Vec<String>) = match &c.relay_mode {
            RelayMode::Default => ("default", Vec::new()),
            RelayMode::Staging => ("staging", Vec::new()),
            RelayMode::Disabled => ("disabled", Vec::new()),
            RelayMode::Custom(urls) => ("custom", urls.clone()),
        };
        d.set_item("relay_mode", mode)?;
        d.set_item("relay_urls", urls)?;
        d.set_item("bind_addr", c.bind_addr.clone())?;
        d.set_item("bind_addr_v6", c.bind_addr_v6.clone())?;
        d.set_item("advertise_addrs", c.advertise_addrs.clone())?;
        d.set_item(
            "relay_ca_certs",
            c.relay_ca_certs
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        )?;
        d.set_item("relay_ca_only", c.relay_ca_only)?;
        d.set_item("discovery", c.discovery.as_str())?;
        d.set_item("discovery_dns_origin", c.discovery_dns_origin.clone())?;
        d.set_item("discovery_pkarr_relays", c.discovery_pkarr_relays.clone())?;
        d.set_item("local_discovery", c.local_discovery)?;
        d.set_item("enable_monitoring", c.enable_monitoring)?;
        d.set_item("enable_hooks", c.enable_hooks)?;
        d.set_item("hook_timeout_ms", c.hook_timeout_ms)?;
        d.set_item(
            "hook_failure_mode",
            hook_failure_mode_str(c.hook_failure_mode),
        )?;
        d.set_item("clear_ip_transports", c.clear_ip_transports)?;
        d.set_item("clear_relay_transports", c.clear_relay_transports)?;
        d.set_item("portmapper_config", c.portmapper_config.clone())?;
        d.set_item("proxy_url", c.proxy_url.clone())?;
        d.set_item("proxy_from_env", c.proxy_from_env)?;
        d.set_item(
            "transport_max_concurrent_bidi_streams",
            c.transport_max_concurrent_bidi_streams,
        )?;
        d.set_item(
            "transport_max_concurrent_uni_streams",
            c.transport_max_concurrent_uni_streams,
        )?;
        d.set_item(
            "transport_stream_receive_window",
            c.transport_stream_receive_window,
        )?;
        d.set_item("transport_receive_window", c.transport_receive_window)?;
        d.set_item("transport_send_window", c.transport_send_window)?;
        d.set_item(
            "transport_max_idle_timeout_ms",
            c.transport_max_idle_timeout_ms,
        )?;
        d.set_item(
            "transport_keep_alive_interval_ms",
            c.transport_keep_alive_interval_ms,
        )?;
        d.set_item("transport_initial_mtu", c.transport_initial_mtu)?;
        d.set_item(
            "transport_datagram_receive_buffer_size",
            c.transport_datagram_receive_buffer_size,
        )?;
        d.set_item(
            "transport_datagram_send_buffer_size",
            c.transport_datagram_send_buffer_size,
        )?;
        d.set_item("transport_send_fairness", c.transport_send_fairness)?;
        d.set_item(
            "transport_enable_segmentation_offload",
            c.transport_enable_segmentation_offload,
        )?;
        d.set_item("log_format", c.log_format.clone())?;
        d.set_item("log_level", c.log_level.clone())?;
        d.set_item("log_mask", c.log_mask)?;
        d.set_item("health_port", c.health_port)?;
        d.set_item("health_host", c.health_host.clone())?;
        d.set_item("health_enabled", c.health_enabled)?;
        d.set_item("metrics_peer_labels", c.metrics_peer_labels)?;
        d.set_item("metrics_peer_label_cap", c.metrics_peer_label_cap)?;
        d.set_item("identity_file", path_str(&c.identity_file))?;
        let alpns = PyList::empty(py);
        for a in &c.alpns {
            alpns.append(PyBytes::new(py, a))?;
        }
        d.set_item("alpns", alpns)?;
        d.set_item("enrollment", enrollment_to_py(py, &c.enrollment)?)?;
        d.set_item(
            "identity_accept_on_behalf_of",
            c.identity_accept_on_behalf_of,
        )?;
        d.set_item(
            "identity_trusted_issuers",
            to_json_py(py, &c.identity_trusted_issuers)?,
        )?;
        d.set_item(
            "identity_edge_token_max_lifetime_secs",
            c.identity_edge_token_max_lifetime_secs,
        )?;
        d.set_item("identity_clock_leeway_secs", c.identity_clock_leeway_secs)?;
        d.set_item(
            "identity_max_key_set_age_secs",
            c.identity_max_key_set_age_secs,
        )?;
        let publisher = match &c.identity_publisher {
            Some(publisher) => to_json_py(py, publisher)?,
            None => py.None().into_bound(py),
        };
        d.set_item("identity_publisher", publisher)?;
        d.set_item("remote_config_enabled", c.remote_config_enabled)?;
        d.set_item("remote_config_lock", c.remote_config_lock.clone())?;
        let sources = PyDict::new(py);
        for (name, source) in &c.setting_sources {
            sources.set_item(name, source.as_str())?;
        }
        d.set_item("setting_sources", sources)?;
        Ok(d)
    }

    /// `AsterConfig::is_enrollment_enabled`: `[enrollment] enabled` when set,
    /// else whether an audience, fleet namespace, issuer delegation or
    /// credential is configured.
    fn is_enrollment_enabled(&self) -> bool {
        self.inner.is_enrollment_enabled()
    }

    /// `AsterConfig::resolve_root_pubkey`: the inline key and/or the
    /// `root_pubkey_file` (hex or JSON `{"public_key": ...}`). A missing file
    /// is ignored; a malformed file, or a file that disagrees with the inline
    /// key, raises `ValueError`.
    fn resolve_root_pubkey(&self) -> PyResult<Option<PublicKey>> {
        let mut c = self.inner.clone();
        c.resolve_root_pubkey()
            .map(|k| k.map(|inner| PublicKey { inner }))
            .map_err(aster_err_to_py)
    }

    /// `AsterConfig::resolve_secret_key_file`: load the key file (or create
    /// it from `secret_key` / a fresh key). Writes the file when missing. A
    /// malformed file, or one disagreeing with `secret_key`, raises
    /// `ValueError`. Returns the effective key (None when no key and no file).
    fn resolve_secret_key_file(&self) -> PyResult<Option<SecretKey>> {
        let mut c = self.inner.clone();
        c.resolve_secret_key_file()
            .map(|k| k.map(|inner| SecretKey { inner }))
            .map_err(aster_err_to_py)
    }

    /// `AsterConfig::resolve_identity_path`: `identity_file` if it exists,
    /// else `./.aster-identity` if it exists, else None.
    fn resolve_identity_path(&self) -> Option<String> {
        self.inner
            .resolve_identity_path()
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// The `[node] secret_key` of a `.aster-identity` file (hex or base64),
    /// parsed by `AsterConfig::load_identity_from_path`. None when absent.
    #[staticmethod]
    fn identity_secret_key(path: PathBuf) -> PyResult<Option<SecretKey>> {
        AsterConfig::default()
            .load_identity_from_path(path, None, None)
            .map(|id| id.secret_key.map(|inner| SecretKey { inner }))
            .map_err(aster_err_to_py)
    }

    /// Whether `storage_path` selects a persistent node.
    fn is_persistent(&self) -> bool {
        self.inner.is_persistent()
    }

    fn __repr__(&self) -> String {
        format!(
            "NativeAsterConfig(storage_path={:?}, relay_mode={:?}, enable_hooks={})",
            self.inner.storage_path, self.inner.relay_mode, self.inner.enable_hooks
        )
    }
}

pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NativeAsterConfig>()?;
    Ok(())
}
