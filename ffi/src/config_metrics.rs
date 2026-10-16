//! ABI 7.4: the `[metrics]` settings of a node config (per-peer labels on
//! `aster.peer.bytes`).
//!
//! Like the 7.3 network settings (`config_network.rs`), they travel beside
//! `aster_config_t` in its library-owned `storage`, so `aster_config_t`'s
//! layout does not change:
//!
//! - `aster_config_from_env` / `aster_config_from_toml` read
//!   `ASTER_METRICS_PEER_LABELS` / `ASTER_METRICS_PEER_LABEL_CAP` and
//!   `[metrics] peer_labels` / `peer_label_cap` into `storage`.
//! - `aster_config_metrics` reads them back; `aster_config_set_metrics`
//!   copies a caller's settings in (allocating `storage` for a caller-built
//!   config, which must then be released with `aster_config_release`).
//! - `aster_node_start` / `aster_node_start_enrolled` apply them: the node's
//!   ops listener (`aster_health_start`, with or without an explicit
//!   `aster_health_config_t`) labels `aster.peer.bytes` per peer.

use aster::{AsterConfig, DEFAULT_METRICS_PEER_LABEL_CAP};

use crate::node::{aster_config_t, ConfigStorage};
use crate::{aster_status_t, set_last_error};

/// `[metrics]` settings of a node config (ABI 7.4), next to `aster_config_t`;
/// see `aster_config_set_metrics`. 16 bytes. A zeroed struct with
/// `struct_size` set is "nothing configured" (peer labels off, default cap).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct aster_metrics_config_t {
    /// `sizeof(aster_metrics_config_t)`, set by the caller.
    pub struct_size: u32,
    /// 1 = label `aster.peer.bytes` per peer (`aster.peer`) instead of by
    /// locality level; 0 = off (the default).
    pub peer_labels: u32,
    /// How many peers get their own `aster.peer` series (later peers count
    /// under `_OTHER`); 0 = the default (`ASTER_METRICS_PEER_LABEL_CAP_DEFAULT`).
    pub peer_label_cap: u32,
    pub reserved: u32,
}

/// The default `peer_label_cap` (64).
pub const ASTER_METRICS_PEER_LABEL_CAP_DEFAULT: u32 = 64;
const _: () =
    assert!(DEFAULT_METRICS_PEER_LABEL_CAP == ASTER_METRICS_PEER_LABEL_CAP_DEFAULT as usize);

/// The settings, owned. Default = nothing configured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MetricsSettings {
    peer_labels: bool,
    peer_label_cap: usize,
}

impl Default for MetricsSettings {
    fn default() -> Self {
        MetricsSettings {
            peer_labels: false,
            peer_label_cap: DEFAULT_METRICS_PEER_LABEL_CAP,
        }
    }
}

impl MetricsSettings {
    pub(crate) fn from_config(cfg: &AsterConfig) -> Self {
        MetricsSettings {
            peer_labels: cfg.metrics_peer_labels,
            peer_label_cap: cfg.metrics_peer_label_cap,
        }
    }

    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub(crate) fn apply(&self, cfg: &mut AsterConfig) {
        cfg.metrics_peer_labels = self.peer_labels;
        cfg.metrics_peer_label_cap = self.peer_label_cap;
    }

    fn from_c(m: &aster_metrics_config_t) -> Result<Self, String> {
        let peer_labels = match m.peer_labels {
            0 => false,
            1 => true,
            v => {
                return Err(format!(
                    "aster_metrics_config_t.peer_labels must be 0 or 1, got {v}"
                ))
            }
        };
        if m.reserved != 0 {
            return Err("aster_metrics_config_t.reserved must be 0".into());
        }
        let peer_label_cap = match m.peer_label_cap {
            0 => DEFAULT_METRICS_PEER_LABEL_CAP,
            n => n as usize,
        };
        Ok(MetricsSettings {
            peer_labels,
            peer_label_cap,
        })
    }

    fn to_c(self) -> aster_metrics_config_t {
        aster_metrics_config_t {
            struct_size: std::mem::size_of::<aster_metrics_config_t>() as u32,
            peer_labels: u32::from(self.peer_labels),
            peer_label_cap: u32::try_from(self.peer_label_cap).unwrap_or(u32::MAX),
            reserved: 0,
        }
    }
}

fn invalid(msg: impl Into<String>) -> i32 {
    set_last_error(msg.into());
    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
}

unsafe fn check_sizes(config: *const aster_config_t, metrics: *const u32, func: &str) -> i32 {
    let size = config.cast::<u32>().read_unaligned();
    let expected = std::mem::size_of::<aster_config_t>() as u32;
    if size != expected {
        return invalid(format!(
            "{func}: aster_config_t.struct_size is {size}, expected {expected}"
        ));
    }
    if !metrics.is_null() {
        let size = metrics.read_unaligned();
        let expected = std::mem::size_of::<aster_metrics_config_t>() as u32;
        if size != expected {
            return invalid(format!(
                "{func}: aster_metrics_config_t.struct_size is {size}, expected {expected}"
            ));
        }
    }
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Set (or, with `metrics` NULL, clear) the `[metrics]` settings of `config`
/// (ABI 7.4). They are validated and copied into the config's library-owned
/// `storage`, which this allocates for a caller-built config: release such a
/// config with `aster_config_release` once it has been passed to
/// `aster_node_start` (or is no longer needed).
///
/// `metrics->struct_size` must equal `sizeof(aster_metrics_config_t)`;
/// `peer_labels` is 0 or 1; `reserved` is 0.
///
/// # Safety
/// `config` points to a valid `aster_config_t`; `metrics` is NULL or valid.
#[no_mangle]
pub unsafe extern "C" fn aster_config_set_metrics(
    config: *mut aster_config_t,
    metrics: *const aster_metrics_config_t,
) -> i32 {
    if config.is_null() {
        return invalid("aster_config_set_metrics: config is NULL");
    }
    let status = check_sizes(config, metrics.cast(), "aster_config_set_metrics");
    if status != aster_status_t::ASTER_STATUS_OK as i32 {
        return status;
    }
    let settings = match metrics.as_ref() {
        None => MetricsSettings::default(),
        Some(m) => match MetricsSettings::from_c(m) {
            Ok(s) => s,
            Err(e) => return invalid(e),
        },
    };
    let c = &mut *config;
    if c.storage.is_null() {
        if settings.is_default() {
            return aster_status_t::ASTER_STATUS_OK as i32;
        }
        c.storage = Box::into_raw(Box::<ConfigStorage>::default()).cast();
    }
    // SAFETY: a non-NULL storage is the library's ConfigStorage (filled
    // configs, or one allocated by aster_config_set_network / just above).
    let storage = &mut *c.storage.cast::<ConfigStorage>();
    storage.metrics = (!settings.is_default()).then_some(settings);
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Read the `[metrics]` settings of `config` (ABI 7.4) into `*out`. A config
/// without any reads as peer labels off with the default cap. The caller sets
/// `out->struct_size` to `sizeof(aster_metrics_config_t)` before the call.
///
/// # Safety
/// `config` points to a valid `aster_config_t`; `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_config_metrics(
    config: *const aster_config_t,
    out: *mut aster_metrics_config_t,
) -> i32 {
    if config.is_null() || out.is_null() {
        return invalid("aster_config_metrics: config or out is NULL");
    }
    let status = check_sizes(config, out.cast_const().cast(), "aster_config_metrics");
    if status != aster_status_t::ASTER_STATUS_OK as i32 {
        return status;
    }
    let stored = (*config)
        .storage
        .cast::<ConfigStorage>()
        .as_ref()
        .and_then(|st| st.metrics);
    *out = stored.unwrap_or_default().to_c();
    aster_status_t::ASTER_STATUS_OK as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{aster_config_init, aster_config_release, fill_config, lower_config};

    fn view() -> aster_metrics_config_t {
        aster_metrics_config_t {
            struct_size: std::mem::size_of::<aster_metrics_config_t>() as u32,
            ..Default::default()
        }
    }

    #[test]
    fn set_read_and_lower_on_a_caller_built_config() {
        assert_eq!(
            crate::aster_abi_accept(crate::ASTER_ABI_VERSION_MAJOR),
            aster_status_t::ASTER_STATUS_OK as i32
        );
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { aster_config_init(&mut c) }, 0);
        let mut back = view();
        assert_eq!(unsafe { aster_config_metrics(&c, &mut back) }, 0);
        assert_eq!(back.peer_labels, 0);
        assert_eq!(back.peer_label_cap, ASTER_METRICS_PEER_LABEL_CAP_DEFAULT);
        assert!(c.storage.is_null());

        let mut m = view();
        m.peer_labels = 1;
        m.peer_label_cap = 3;
        assert_eq!(unsafe { aster_config_set_metrics(&mut c, &m) }, 0);
        assert!(!c.storage.is_null());
        let cfg = unsafe { lower_config(&c) }.unwrap();
        assert!(cfg.metrics_peer_labels);
        assert_eq!(cfg.metrics_peer_label_cap, 3);
        assert_eq!(
            AsterConfig {
                health_enabled: Some(true),
                ..cfg.clone()
            }
            .health_config()
            .unwrap()
            .peer_label_cap,
            Some(3)
        );
        let mut back = view();
        assert_eq!(unsafe { aster_config_metrics(&c, &mut back) }, 0);
        assert_eq!((back.peer_labels, back.peer_label_cap), (1, 3));

        // 0 = default cap.
        m.peer_label_cap = 0;
        assert_eq!(unsafe { aster_config_set_metrics(&mut c, &m) }, 0);
        let cfg = unsafe { lower_config(&c) }.unwrap();
        assert_eq!(cfg.metrics_peer_label_cap, DEFAULT_METRICS_PEER_LABEL_CAP);

        assert_eq!(
            unsafe { aster_config_set_metrics(&mut c, std::ptr::null()) },
            0
        );
        assert!(!unsafe { lower_config(&c) }.unwrap().metrics_peer_labels);
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
        assert!(c.storage.is_null());
    }

    #[test]
    fn fill_carries_metrics_beside_network_settings() {
        let cfg = AsterConfig::builder()
            .bind_addr_v6("[::]:7842")
            .metrics_peer_labels(5)
            .build();
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        fill_config(&cfg, &mut c);
        let lowered = unsafe { lower_config(&c) }.unwrap();
        assert!(lowered.metrics_peer_labels);
        assert_eq!(lowered.metrics_peer_label_cap, 5);
        assert_eq!(lowered.bind_addr_v6.as_deref(), Some("[::]:7842"));
        let mut back = view();
        assert_eq!(unsafe { aster_config_metrics(&c, &mut back) }, 0);
        assert_eq!((back.peer_labels, back.peer_label_cap), (1, 5));
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
    }

    #[test]
    fn rejects_bad_input() {
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        c.struct_size = std::mem::size_of::<aster_config_t>() as u32;
        let mut m = view();
        m.struct_size = 8;
        assert_ne!(unsafe { aster_config_set_metrics(&mut c, &m) }, 0);
        let mut m = view();
        m.peer_labels = 2;
        assert_ne!(unsafe { aster_config_set_metrics(&mut c, &m) }, 0);
        let mut m = view();
        m.reserved = 1;
        assert_ne!(unsafe { aster_config_set_metrics(&mut c, &m) }, 0);
        assert!(c.storage.is_null());
        let mut out = view();
        out.struct_size = 4;
        assert_ne!(unsafe { aster_config_metrics(&c, &mut out) }, 0);
        c.struct_size = 8;
        assert_ne!(unsafe { aster_config_metrics(&c, &mut view()) }, 0);
    }
}
