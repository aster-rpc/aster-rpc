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
//! `"custom"`) plus `relay_urls`.

use std::path::PathBuf;
use std::time::Duration;

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use aster::{AsterConfig, HookFailureMode, RelayMode};

use crate::error::aster_err_to_py;
use crate::ids::{extract_public_key, extract_secret_key, PublicKey, SecretKey};

/// Every field `NativeAsterConfig(**kwargs)` accepts and `to_dict()` returns.
pub(crate) const FIELDS: &[&str] = &[
    "root_pubkey",
    "root_pubkey_file",
    "enrollment_credential_file",
    "enrollment_credential_iid",
    "allow_all_consumers",
    "allow_all_producers",
    "endpoint_addr",
    "storage_path",
    "gc_interval_ms",
    "secret_key",
    "secret_key_file",
    "relay_mode",
    "relay_urls",
    "bind_addr",
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
    "identity_file",
    "alpns",
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

fn parse_hook_failure_mode(v: &str) -> PyResult<HookFailureMode> {
    match v.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "" | "fail_open" | "open" => Ok(HookFailureMode::FailOpen),
        "fail_closed" | "closed" => Ok(HookFailureMode::FailClosed),
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
        "enrollment_credential_file" => c.enrollment_credential_file = opt_path(v)?,
        "enrollment_credential_iid" => c.enrollment_credential_iid = opt_str(v)?,
        "allow_all_consumers" => c.allow_all_consumers = strict_bool(key, v)?,
        "allow_all_producers" => c.allow_all_producers = strict_bool(key, v)?,
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
        // relay_mode / relay_urls are combined by the caller.
        "relay_mode" | "relay_urls" => {}
        "bind_addr" => c.bind_addr = opt_str(v)?,
        "local_discovery" => c.local_discovery = strict_bool(key, v)?,
        "enable_monitoring" => c.enable_monitoring = strict_bool(key, v)?,
        "enable_hooks" => c.enable_hooks = strict_bool(key, v)?,
        "hook_timeout_ms" => c.hook_timeout_ms = v.extract()?,
        "hook_failure_mode" => {
            c.hook_failure_mode = match opt::<String>(v)? {
                None => HookFailureMode::FailOpen,
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
        "identity_file" => c.identity_file = opt_path(v)?,
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

fn path_str(p: &Option<PathBuf>) -> Option<String> {
    p.as_ref().map(|p| p.to_string_lossy().into_owned())
}

impl NativeAsterConfig {
    pub(crate) fn from_kwargs(kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<AsterConfig> {
        let mut c = AsterConfig::default();
        let mut relay_mode: Option<String> = None;
        let mut relay_urls: Vec<String> = Vec::new();
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
                    _ => {}
                }
                apply_field(&mut c, &key, &v).map_err(|e| prefix_err(k.py(), &key, e))?;
            }
        }
        c.relay_mode = relay_from(relay_mode, relay_urls)?;
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
        d.set_item(
            "enrollment_credential_file",
            path_str(&c.enrollment_credential_file),
        )?;
        d.set_item(
            "enrollment_credential_iid",
            c.enrollment_credential_iid.clone(),
        )?;
        d.set_item("allow_all_consumers", c.allow_all_consumers)?;
        d.set_item("allow_all_producers", c.allow_all_producers)?;
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
        let (mode, urls): (&str, Vec<String>) = match &c.relay_mode {
            RelayMode::Default => ("default", Vec::new()),
            RelayMode::Staging => ("staging", Vec::new()),
            RelayMode::Disabled => ("disabled", Vec::new()),
            RelayMode::Custom(urls) => ("custom", urls.clone()),
        };
        d.set_item("relay_mode", mode)?;
        d.set_item("relay_urls", urls)?;
        d.set_item("bind_addr", c.bind_addr.clone())?;
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
        d.set_item("identity_file", path_str(&c.identity_file))?;
        let alpns = PyList::empty(py);
        for a in &c.alpns {
            alpns.append(PyBytes::new(py, a))?;
        }
        d.set_item("alpns", alpns)?;
        Ok(d)
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
