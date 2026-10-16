//! ABI 7.3: cluster network settings of a node config (Kasm A6).
//!
//! `aster_config_t` has no spare room and checks `struct_size` exactly, so
//! these settings travel beside it rather than in it: an
//! `aster_network_config_t` held in the config's library-owned `storage`.
//! The layout of `aster_config_t` does not change, so every 7.x caller keeps
//! working.
//!
//! - `aster_config_from_env` / `aster_config_from_toml` read the settings
//!   (`ASTER_BIND_ADDR_V6`, `[network] advertise_addrs`, ...) with the rest
//!   of the config, into `storage`.
//! - `aster_config_network` reads them back as borrowed views.
//! - `aster_config_set_network` copies a caller's settings into `storage`
//!   (allocating it for a caller-built config), so the config must then be
//!   released with `aster_config_release`, like a filled one.
//! - `aster_node_start` / `aster_node_start_enrolled` apply them.

use std::slice;

use aster::{AsterConfig, DiscoveryMode};

use crate::node::{aster_config_t, ConfigStorage};
use crate::{aster_bytes_list_t, aster_bytes_t, aster_status_t, set_last_error};

/// `aster_network_config_t.discovery` (ABI 7.3): how a node publishes its
/// addresses and finds peers by bare node ID.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_discovery_t {
    /// n0 pkarr and DNS at `iroh.link`. With `discovery_dns_origin` or
    /// `discovery_pkarr_relays` set, selects `CUSTOM`.
    ASTER_DISCOVERY_DEFAULT = 0,
    /// No address lookup: dial peers by full address.
    ASTER_DISCOVERY_NONE = 1,
    /// Local-network mDNS only.
    ASTER_DISCOVERY_LOCAL = 2,
    /// Only `discovery_dns_origin` / `discovery_pkarr_relays`.
    ASTER_DISCOVERY_CUSTOM = 3,
}

/// Cluster network settings of a node config (ABI 7.3), next to
/// `aster_config_t`; see `aster_config_set_network`. Strings are UTF-8, not
/// NUL-terminated; empty = `{NULL, 0}`. A zeroed struct with `struct_size`
/// set is "nothing configured".
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_network_config_t {
    /// `sizeof(aster_network_config_t)`, set by the caller.
    pub struct_size: u32,
    /// One of `aster_discovery_t`.
    pub discovery: u32,
    /// IPv6 bind address, e.g. `[::]:7842`: pins the IPv6 UDP port as
    /// `aster_config_t.bind_addr` pins the IPv4 one.
    pub bind_addr_v6: aster_bytes_t,
    /// Extra direct addresses (`IP:port`) to advertise, e.g. a NodePort.
    /// Hints only: peers still verify the node key in the TLS handshake.
    pub advertise_addrs: aster_bytes_list_t,
    /// PEM files with CA certificates trusted for relay TLS, in addition to
    /// the WebPKI roots.
    pub relay_ca_certs: aster_bytes_list_t,
    /// 1 = trust only `relay_ca_certs`, not the WebPKI roots.
    pub relay_ca_only: u32,
    pub reserved: u32,
    /// DNS origin for custom discovery, e.g. `dns.example.internal`.
    pub discovery_dns_origin: aster_bytes_t,
    /// Pkarr relay URLs for custom discovery.
    pub discovery_pkarr_relays: aster_bytes_list_t,
}

/// The settings, owned. Default = nothing configured.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NetworkSettings {
    bind_addr_v6: Option<String>,
    advertise_addrs: Vec<String>,
    relay_ca_certs: Vec<String>,
    relay_ca_only: bool,
    discovery: DiscoveryMode,
    discovery_dns_origin: Option<String>,
    discovery_pkarr_relays: Vec<String>,
}

impl NetworkSettings {
    pub(crate) fn from_config(cfg: &AsterConfig) -> Self {
        Self {
            bind_addr_v6: cfg.bind_addr_v6.clone(),
            advertise_addrs: cfg.advertise_addrs.clone(),
            relay_ca_certs: cfg
                .relay_ca_certs
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            relay_ca_only: cfg.relay_ca_only,
            discovery: cfg.discovery,
            discovery_dns_origin: cfg.discovery_dns_origin.clone(),
            discovery_pkarr_relays: cfg.discovery_pkarr_relays.clone(),
        }
    }

    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub(crate) fn apply(&self, cfg: &mut AsterConfig) {
        cfg.bind_addr_v6.clone_from(&self.bind_addr_v6);
        cfg.advertise_addrs.clone_from(&self.advertise_addrs);
        cfg.relay_ca_certs = self.relay_ca_certs.iter().map(Into::into).collect();
        cfg.relay_ca_only = self.relay_ca_only;
        cfg.discovery = self.discovery;
        cfg.discovery_dns_origin
            .clone_from(&self.discovery_dns_origin);
        cfg.discovery_pkarr_relays
            .clone_from(&self.discovery_pkarr_relays);
    }

    /// Validate and copy a caller's struct.
    unsafe fn from_c(n: &aster_network_config_t) -> Result<Self, String> {
        let text = |b: &aster_bytes_t, field: &str| -> Result<Option<String>, String> {
            let bytes = view(b);
            if bytes.is_empty() {
                return Ok(None);
            }
            std::str::from_utf8(bytes)
                .map(|s| Some(s.to_string()))
                .map_err(|_| format!("aster_network_config_t.{field} is not UTF-8"))
        };
        let list = |l: &aster_bytes_list_t, field: &str| -> Result<Vec<String>, String> {
            list_view(l)
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    text(b, field)?
                        .ok_or_else(|| format!("aster_network_config_t.{field}[{i}] is empty"))
                })
                .collect()
        };
        let relay_ca_only = match n.relay_ca_only {
            0 => false,
            1 => true,
            v => {
                return Err(format!(
                    "aster_network_config_t.relay_ca_only must be 0 or 1, got {v}"
                ))
            }
        };
        if n.reserved != 0 {
            return Err("aster_network_config_t.reserved must be 0".into());
        }
        let dns_origin = text(&n.discovery_dns_origin, "discovery_dns_origin")?;
        let pkarr = list(&n.discovery_pkarr_relays, "discovery_pkarr_relays")?;
        let servers = dns_origin.is_some() || !pkarr.is_empty();
        let discovery = match n.discovery {
            x if x == aster_discovery_t::ASTER_DISCOVERY_DEFAULT as u32 => {
                if servers {
                    DiscoveryMode::Custom
                } else {
                    DiscoveryMode::Default
                }
            }
            x if x == aster_discovery_t::ASTER_DISCOVERY_CUSTOM as u32 => DiscoveryMode::Custom,
            x if x == aster_discovery_t::ASTER_DISCOVERY_NONE as u32
                || x == aster_discovery_t::ASTER_DISCOVERY_LOCAL as u32 =>
            {
                if servers {
                    return Err(
                        "discovery_dns_origin / discovery_pkarr_relays require discovery CUSTOM"
                            .into(),
                    );
                }
                if x == aster_discovery_t::ASTER_DISCOVERY_NONE as u32 {
                    DiscoveryMode::None
                } else {
                    DiscoveryMode::Local
                }
            }
            other => return Err(format!("unknown discovery {other}")),
        };
        Ok(Self {
            bind_addr_v6: text(&n.bind_addr_v6, "bind_addr_v6")?,
            advertise_addrs: list(&n.advertise_addrs, "advertise_addrs")?,
            relay_ca_certs: list(&n.relay_ca_certs, "relay_ca_certs")?,
            relay_ca_only,
            discovery,
            discovery_dns_origin: dns_origin,
            discovery_pkarr_relays: pkarr,
        })
    }
}

/// Settings plus the library-owned views `aster_config_network` hands out.
pub(crate) struct StoredNetwork {
    pub(crate) settings: NetworkSettings,
    bufs: Vec<Vec<u8>>,
    lists: Vec<Vec<aster_bytes_t>>,
    view: aster_network_config_t,
}

impl StoredNetwork {
    pub(crate) fn new(settings: NetworkSettings) -> Box<Self> {
        let mut st = Box::new(Self {
            settings,
            bufs: Vec::new(),
            lists: Vec::new(),
            view: empty_view(),
        });
        let s = st.settings.clone();
        let bind_addr_v6 = st.bytes(s.bind_addr_v6.unwrap_or_default().into_bytes());
        let advertise_addrs = st.list(s.advertise_addrs);
        let relay_ca_certs = st.list(s.relay_ca_certs);
        let discovery_dns_origin =
            st.bytes(s.discovery_dns_origin.unwrap_or_default().into_bytes());
        let discovery_pkarr_relays = st.list(s.discovery_pkarr_relays);
        st.view = aster_network_config_t {
            struct_size: std::mem::size_of::<aster_network_config_t>() as u32,
            discovery: match s.discovery {
                DiscoveryMode::Default => aster_discovery_t::ASTER_DISCOVERY_DEFAULT,
                DiscoveryMode::None => aster_discovery_t::ASTER_DISCOVERY_NONE,
                DiscoveryMode::Local => aster_discovery_t::ASTER_DISCOVERY_LOCAL,
                DiscoveryMode::Custom => aster_discovery_t::ASTER_DISCOVERY_CUSTOM,
            } as u32,
            bind_addr_v6,
            advertise_addrs,
            relay_ca_certs,
            relay_ca_only: s.relay_ca_only as u32,
            reserved: 0,
            discovery_dns_origin,
            discovery_pkarr_relays,
        };
        st
    }

    fn bytes(&mut self, v: Vec<u8>) -> aster_bytes_t {
        if v.is_empty() {
            return EMPTY;
        }
        let b = aster_bytes_t {
            ptr: v.as_ptr(),
            len: v.len(),
        };
        self.bufs.push(v);
        b
    }

    fn list(&mut self, items: Vec<String>) -> aster_bytes_list_t {
        if items.is_empty() {
            return EMPTY_LIST;
        }
        let views: Vec<aster_bytes_t> = items
            .into_iter()
            .map(|s| self.bytes(s.into_bytes()))
            .collect();
        let l = aster_bytes_list_t {
            items: views.as_ptr(),
            len: views.len(),
        };
        self.lists.push(views);
        l
    }
}

const EMPTY: aster_bytes_t = aster_bytes_t {
    ptr: std::ptr::null(),
    len: 0,
};
const EMPTY_LIST: aster_bytes_list_t = aster_bytes_list_t {
    items: std::ptr::null(),
    len: 0,
};

fn empty_view() -> aster_network_config_t {
    aster_network_config_t {
        struct_size: std::mem::size_of::<aster_network_config_t>() as u32,
        discovery: aster_discovery_t::ASTER_DISCOVERY_DEFAULT as u32,
        bind_addr_v6: EMPTY,
        advertise_addrs: EMPTY_LIST,
        relay_ca_certs: EMPTY_LIST,
        relay_ca_only: 0,
        reserved: 0,
        discovery_dns_origin: EMPTY,
        discovery_pkarr_relays: EMPTY_LIST,
    }
}

unsafe fn view(b: &aster_bytes_t) -> &[u8] {
    if b.ptr.is_null() || b.len == 0 {
        &[]
    } else {
        slice::from_raw_parts(b.ptr, b.len)
    }
}

unsafe fn list_view(l: &aster_bytes_list_t) -> &[aster_bytes_t] {
    if l.items.is_null() || l.len == 0 {
        &[]
    } else {
        slice::from_raw_parts(l.items, l.len)
    }
}

fn invalid(msg: impl Into<String>) -> i32 {
    set_last_error(msg.into());
    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
}

unsafe fn check_config_size(config: *const aster_config_t, func: &str) -> Result<(), i32> {
    let size = config.cast::<u32>().read_unaligned();
    let expected = std::mem::size_of::<aster_config_t>() as u32;
    if size != expected {
        return Err(invalid(format!(
            "{func}: aster_config_t.struct_size is {size}, expected {expected}"
        )));
    }
    Ok(())
}

/// Set (or, with `network` NULL, clear) the cluster network settings of
/// `config` (ABI 7.3). The settings are validated and copied into the
/// config's library-owned `storage`, which this allocates for a caller-built
/// config: release such a config with `aster_config_release` once it has
/// been passed to `aster_node_start` (or is no longer needed). Views from an
/// earlier `aster_config_network` on this config become invalid.
///
/// `network->struct_size` must equal `sizeof(aster_network_config_t)`. A
/// zeroed struct with `struct_size` set means nothing configured. Settings are
/// checked further (address syntax, CA files) when the node starts.
///
/// # Safety
/// `config` points to a valid `aster_config_t`; `network` is NULL or valid.
#[no_mangle]
pub unsafe extern "C" fn aster_config_set_network(
    config: *mut aster_config_t,
    network: *const aster_network_config_t,
) -> i32 {
    if config.is_null() {
        return invalid("aster_config_set_network: config is NULL");
    }
    if let Err(status) = check_config_size(config, "aster_config_set_network") {
        return status;
    }
    let settings = if network.is_null() {
        NetworkSettings::default()
    } else {
        let size = network.cast::<u32>().read_unaligned();
        let expected = std::mem::size_of::<aster_network_config_t>() as u32;
        if size != expected {
            return invalid(format!(
                "aster_config_set_network: aster_network_config_t.struct_size is {size}, expected {expected}"
            ));
        }
        match NetworkSettings::from_c(&*network) {
            Ok(settings) => settings,
            Err(e) => return invalid(e),
        }
    };
    let c = &mut *config;
    if c.storage.is_null() {
        if settings.is_default() {
            return aster_status_t::ASTER_STATUS_OK as i32;
        }
        c.storage = Box::into_raw(Box::<ConfigStorage>::default()).cast();
    }
    // SAFETY: a non-NULL storage is the library's ConfigStorage (filled
    // configs) or the one allocated just above.
    let storage = &mut *c.storage.cast::<ConfigStorage>();
    storage.network = (!settings.is_default()).then(|| StoredNetwork::new(settings));
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Read the cluster network settings of `config` (ABI 7.3) into `*out` as
/// borrowed views, valid until `aster_config_release` or the next
/// `aster_config_set_network` on `config`. A config without any (for example
/// one from `aster_config_init`) reads as a zeroed struct (`DEFAULT`
/// discovery, nothing set). The caller sets `out->struct_size` to
/// `sizeof(aster_network_config_t)` before the call.
///
/// # Safety
/// `config` points to a valid `aster_config_t`; `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_config_network(
    config: *const aster_config_t,
    out: *mut aster_network_config_t,
) -> i32 {
    if config.is_null() || out.is_null() {
        return invalid("aster_config_network: config or out is NULL");
    }
    if let Err(status) = check_config_size(config, "aster_config_network") {
        return status;
    }
    let size = out.cast::<u32>().read_unaligned();
    let expected = std::mem::size_of::<aster_network_config_t>() as u32;
    if size != expected {
        return invalid(format!(
            "aster_config_network: out->struct_size is {size}, expected {expected}"
        ));
    }
    let c = &*config;
    let stored = c
        .storage
        .cast::<ConfigStorage>()
        .as_ref()
        .and_then(|st| st.network.as_ref());
    *out = stored.map_or_else(empty_view, |n| n.view);
    aster_status_t::ASTER_STATUS_OK as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{aster_config_init, aster_config_release, fill_config, lower_config};

    fn bytes(s: &'static str) -> aster_bytes_t {
        aster_bytes_t {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    fn accepted() {
        assert_eq!(
            crate::aster_abi_accept(crate::ASTER_ABI_VERSION_MAJOR),
            aster_status_t::ASTER_STATUS_OK as i32
        );
    }

    #[test]
    fn set_read_and_lower_on_a_caller_built_config() {
        accepted();
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { aster_config_init(&mut c) }, 0);
        assert!(c.storage.is_null());

        let addrs = [bytes("203.0.113.10:31842")];
        let cas = [bytes("/etc/aster/relay-ca.pem")];
        let mut n = empty_view();
        n.discovery = aster_discovery_t::ASTER_DISCOVERY_NONE as u32;
        n.bind_addr_v6 = bytes("[::]:7842");
        n.advertise_addrs = aster_bytes_list_t {
            items: addrs.as_ptr(),
            len: 1,
        };
        n.relay_ca_certs = aster_bytes_list_t {
            items: cas.as_ptr(),
            len: 1,
        };
        n.relay_ca_only = 1;
        assert_eq!(unsafe { aster_config_set_network(&mut c, &n) }, 0);
        assert!(!c.storage.is_null());

        let cfg = unsafe { lower_config(&c) }.unwrap();
        assert_eq!(cfg.bind_addr_v6.as_deref(), Some("[::]:7842"));
        assert_eq!(cfg.advertise_addrs, ["203.0.113.10:31842"]);
        assert!(cfg.relay_ca_only);
        assert_eq!(cfg.discovery, DiscoveryMode::None);

        let mut back = empty_view();
        back.discovery = 99;
        assert_eq!(unsafe { aster_config_network(&c, &mut back) }, 0);
        assert_eq!(
            back.discovery,
            aster_discovery_t::ASTER_DISCOVERY_NONE as u32
        );
        assert_eq!(unsafe { view(&back.bind_addr_v6) }, b"[::]:7842");
        assert_eq!(back.advertise_addrs.len, 1);

        // NULL clears; release frees the storage.
        assert_eq!(
            unsafe { aster_config_set_network(&mut c, std::ptr::null()) },
            0
        );
        let cfg = unsafe { lower_config(&c) }.unwrap();
        assert!(cfg.bind_addr_v6.is_none());
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
        assert!(c.storage.is_null());
    }

    #[test]
    fn fill_carries_network_settings_and_preserves_other_storage() {
        let cfg = AsterConfig::builder()
            .bind_addr("0.0.0.0:7842")
            .bind_addr_v6("[::]:7842")
            .discovery_pkarr_relay("https://dns.example.internal/pkarr")
            .health_port(18080)
            .build();
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        fill_config(&cfg, &mut c);
        let lowered = unsafe { lower_config(&c) }.unwrap();
        assert_eq!(lowered.bind_addr_v6.as_deref(), Some("[::]:7842"));
        assert_eq!(lowered.discovery, DiscoveryMode::Custom);
        assert_eq!(lowered.health_port, 18080);
        let mut n = empty_view();
        assert_eq!(unsafe { aster_config_network(&c, &mut n) }, 0);
        assert_eq!(
            n.discovery,
            aster_discovery_t::ASTER_DISCOVERY_CUSTOM as u32
        );
        assert_eq!(n.discovery_pkarr_relays.len, 1);
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
    }

    #[test]
    fn rejects_bad_input() {
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        c.struct_size = std::mem::size_of::<aster_config_t>() as u32;
        let mut n = empty_view();
        n.struct_size = 8;
        assert_ne!(unsafe { aster_config_set_network(&mut c, &n) }, 0);
        let mut n = empty_view();
        n.relay_ca_only = 2;
        assert_ne!(unsafe { aster_config_set_network(&mut c, &n) }, 0);
        let mut n = empty_view();
        n.discovery = 9;
        assert_ne!(unsafe { aster_config_set_network(&mut c, &n) }, 0);
        let mut n = empty_view();
        n.discovery = aster_discovery_t::ASTER_DISCOVERY_NONE as u32;
        n.discovery_dns_origin = bytes("dns.example.internal");
        assert_ne!(unsafe { aster_config_set_network(&mut c, &n) }, 0);
        let mut n = empty_view();
        n.discovery_dns_origin = bytes("dns.example.internal");
        assert_eq!(unsafe { aster_config_set_network(&mut c, &n) }, 0);
        assert_eq!(
            unsafe { lower_config(&c) }.unwrap().discovery,
            DiscoveryMode::Custom,
            "servers with DEFAULT select CUSTOM"
        );
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
        let mut out = empty_view();
        out.struct_size = 4;
        assert_ne!(unsafe { aster_config_network(&c, &mut out) }, 0);
    }
}
