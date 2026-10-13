//! ABI 3.2: node configuration, `aster_node_start`, Gate 0 and admission.
//!
//! This is the C ABI over the `aster` crate (plan D1): a node is started from
//! an [`aster::AsterConfig`] through [`aster::Node::start`], so the C ABI, the
//! Rust SDK and the other bindings resolve configuration (env / TOML / secret
//! key file) the same way. The started node is an ordinary `aster_node_t`:
//! every node function (blobs, docs, gossip, tags, `aster_server_*`,
//! `aster_node_accept_aster`, `aster_accept`, `aster_connect`) works on it.
//!
//! # `aster_config_t` ownership
//!
//! One model, the same as the AEv1 envelope decode structs:
//!
//! - **Caller-built** (`aster_config_init`, then set fields): every
//!   `aster_bytes_t` / `aster_bytes_list_t` is borrowed by the library only for
//!   the duration of the call that reads it (`aster_node_start`). `storage` is
//!   NULL. `aster_config_init` never allocates.
//! - **Library-filled** (`aster_config_from_env`, `aster_config_from_toml`):
//!   strings and lists point into one library-owned allocation referenced by
//!   `storage`. Release it exactly once with `aster_config_release`, which frees
//!   the storage and zeroes every pointer. The caller may overwrite any field
//!   (including pointing a string at its own memory) before passing the struct
//!   to `aster_node_start`; release frees only `storage`, never caller memory.
//!   Do not release a copy of the struct as well.
//! - Strings are UTF-8 and *not* NUL-terminated; empty = `{NULL, 0}`.
//!
//! # Gate 0
//!
//! `aster_gate_t` wraps the core `GatePolicy` (trust mode, always-open
//! admission ALPNs, reject code/reason, admitted-peer store). Attach one to a
//! node at start (or later with `aster_node_set_gate`) and the node decides
//! inbound admission in Rust: allowed peers are accepted, others are closed
//! with the gate's reject code and reason, and no event reaches the caller.
//! Without a gate, a node started with `enable_hooks` raises
//! `ASTER_EVENT_HOOK_BEFORE_CONNECT` / `ASTER_EVENT_HOOK_AFTER_CONNECT`
//! events (same payloads as endpoint hooks, `event.handle` = node handle,
//! `event.related` = invocation) that the caller answers with
//! `aster_admission_respond` (accept, or reject with a code and reason).

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::slice;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use aster::{
    AsterConfig, GateDecision, GatePolicy, HookFailureMode, RelayMode, SecretKey, TrustMode,
};
use aster_transport_core::{CoreAfterHandshakeDecision, CoreHookReceiver, CoreNode};

use crate::{
    aster_bytes_list_t, aster_bytes_t, aster_event_kind_t, aster_hook_decision_t,
    aster_hook_failure_mode_t, aster_hook_invocation_t, aster_node_t, aster_operation_t,
    aster_relay_mode_t, aster_runtime_t, aster_status_t, check_cancelled,
    encode_hook_connect_payload, encode_hook_handshake_payload, load_runtime, set_last_error,
    BridgeRuntime, EventInternal, HookInvocationState, HookSender,
};

/// Opaque Gate-0 policy handle (scoped to a runtime).
pub type aster_gate_t = u64;

/// Gate-0 trust posture (`aster_gate_set_trust_mode`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_trust_mode_t {
    /// Unknown peers may use only the admission ALPNs (default).
    ASTER_TRUST_MODE_PROTECTED = 0,
    /// Local/dev: unknown peers may use any ALPN.
    ASTER_TRUST_MODE_OPEN_DEV = 1,
}

/// `aster_config_t.portmapper`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_portmapper_t {
    /// Not set: the transport default (enabled).
    ASTER_PORTMAPPER_DEFAULT = 0,
    ASTER_PORTMAPPER_ENABLED = 1,
    ASTER_PORTMAPPER_DISABLED = 2,
}

// Bits of `aster_config_t.transport_set`: a `transport_*` field is applied only
// when its bit is set (so 0 stays a real value where QUIC allows it).
pub const ASTER_TRANSPORT_MAX_CONCURRENT_BIDI_STREAMS: u32 = 1 << 0;
pub const ASTER_TRANSPORT_MAX_CONCURRENT_UNI_STREAMS: u32 = 1 << 1;
pub const ASTER_TRANSPORT_STREAM_RECEIVE_WINDOW: u32 = 1 << 2;
pub const ASTER_TRANSPORT_RECEIVE_WINDOW: u32 = 1 << 3;
pub const ASTER_TRANSPORT_SEND_WINDOW: u32 = 1 << 4;
pub const ASTER_TRANSPORT_MAX_IDLE_TIMEOUT_MS: u32 = 1 << 5;
pub const ASTER_TRANSPORT_KEEP_ALIVE_INTERVAL_MS: u32 = 1 << 6;
pub const ASTER_TRANSPORT_INITIAL_MTU: u32 = 1 << 7;
pub const ASTER_TRANSPORT_DATAGRAM_RECEIVE_BUFFER_SIZE: u32 = 1 << 8;
pub const ASTER_TRANSPORT_DATAGRAM_SEND_BUFFER_SIZE: u32 = 1 << 9;
pub const ASTER_TRANSPORT_SEND_FAIRNESS: u32 = 1 << 10;
pub const ASTER_TRANSPORT_ENABLE_SEGMENTATION_OFFLOAD: u32 = 1 << 11;
/// Every defined `ASTER_TRANSPORT_*` bit. Other bits are rejected.
pub const ASTER_TRANSPORT_ALL: u32 = (1 << 12) - 1;

/// Node configuration: every knob of `aster::AsterConfig` that a node uses
/// (docs/aster-rust-getstarted.md §2.2). Start from `aster_config_init` (or
/// `aster_config_from_env` / `aster_config_from_toml`), then set fields. See
/// the module docs for ownership.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_config_t {
    /// `sizeof(aster_config_t)`; checked by every function that reads it.
    pub struct_size: u32,
    /// One of `aster_relay_mode_t`. `ASTER_RELAY_MODE_CUSTOM` needs
    /// `relay_urls`; non-empty `relay_urls` with `DEFAULT` selects `CUSTOM`.
    pub relay_mode: u32,
    /// Relay URLs (UTF-8) for `ASTER_RELAY_MODE_CUSTOM`.
    pub relay_urls: aster_bytes_list_t,
    /// Inline secret key: exactly 32 raw bytes, or empty for none.
    pub secret_key: aster_bytes_t,
    /// Secret key file (64 hex chars): loaded if it exists, else created from
    /// `secret_key` or a fresh key (mode 0600) at start.
    pub secret_key_file: aster_bytes_t,
    /// Persistent data directory; empty = in-memory node.
    pub storage_path: aster_bytes_t,
    /// Blob GC interval in ms; 0 = no GC.
    pub gc_interval_ms: u64,
    /// Local bind address, e.g. `127.0.0.1:0`; empty = transport default.
    pub bind_addr: aster_bytes_t,
    /// Extra inbound ALPNs (e.g. `aster/1`) next to blobs/docs/gossip.
    pub alpns: aster_bytes_list_t,
    /// `.aster-identity` credential file (carried for round-trips; not used
    /// by `aster_node_start`).
    pub identity_file: aster_bytes_t,
    /// HTTP/SOCKS proxy URL; empty = none.
    pub proxy_url: aster_bytes_t,
    /// Admission hook timeout in ms; 0 = default (5000).
    pub hook_timeout_ms: u64,
    /// One of `aster_hook_failure_mode_t`.
    pub hook_failure_mode: u32,
    /// 1 = raise admission events (forced on when a gate is attached).
    pub enable_hooks: u32,
    pub enable_monitoring: u32,
    /// mDNS local discovery.
    pub local_discovery: u32,
    /// 1 = relay-only.
    pub clear_ip_transports: u32,
    /// 1 = direct-only.
    pub clear_relay_transports: u32,
    /// One of `aster_portmapper_t`.
    pub portmapper: u32,
    /// 1 = read `HTTP(S)_PROXY`.
    pub proxy_from_env: u32,
    /// Bitmask of `ASTER_TRANSPORT_*`: which `transport_*` fields are set.
    pub transport_set: u32,
    /// Initial QUIC MTU (≤ 65535).
    pub transport_initial_mtu: u32,
    /// Bool (0/1).
    pub transport_send_fairness: u32,
    /// Bool (0/1).
    pub transport_enable_segmentation_offload: u32,
    pub transport_max_concurrent_bidi_streams: u64,
    pub transport_max_concurrent_uni_streams: u64,
    pub transport_stream_receive_window: u64,
    pub transport_receive_window: u64,
    pub transport_send_window: u64,
    pub transport_max_idle_timeout_ms: u64,
    pub transport_keep_alive_interval_ms: u64,
    pub transport_datagram_receive_buffer_size: u64,
    pub transport_datagram_send_buffer_size: u64,
    /// Library-owned storage of a filled config; NULL for caller-built ones.
    pub storage: *mut c_void,
}

// ── lowering: aster_config_t -> AsterConfig ─────────────────────────────────

fn invalid(msg: impl ToString) -> i32 {
    set_last_error(msg);
    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
}

unsafe fn view<'a>(b: &aster_bytes_t) -> &'a [u8] {
    if b.ptr.is_null() || b.len == 0 {
        &[]
    } else {
        slice::from_raw_parts(b.ptr, b.len)
    }
}

unsafe fn list_view<'a>(l: &aster_bytes_list_t) -> &'a [aster_bytes_t] {
    if l.items.is_null() || l.len == 0 {
        &[]
    } else {
        slice::from_raw_parts(l.items, l.len)
    }
}

unsafe fn opt_str(b: &aster_bytes_t, field: &str) -> Result<Option<String>, String> {
    let bytes = view(b);
    if bytes.is_empty() {
        return Ok(None);
    }
    std::str::from_utf8(bytes)
        .map(|s| Some(s.to_string()))
        .map_err(|_| format!("aster_config_t.{field} is not UTF-8"))
}

fn flag(v: u32, field: &str) -> Result<bool, String> {
    match v {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(format!("aster_config_t.{field} must be 0 or 1, got {v}")),
    }
}

/// Validate and lower a C config to an [`AsterConfig`].
pub(crate) unsafe fn lower_config(c: &aster_config_t) -> Result<AsterConfig, String> {
    let expected = std::mem::size_of::<aster_config_t>() as u32;
    if c.struct_size != expected {
        return Err(format!(
            "aster_config_t.struct_size is {}, expected {expected}",
            c.struct_size
        ));
    }
    let mut cfg = AsterConfig::default();
    // SAFETY: storage is NULL for caller-built configs, or the library-owned
    // ConfigStorage from fill_config, kept alive until aster_config_release.
    // Health fields have no public ABI slots; preserve their parsed values here.
    if let Some(storage) = c.storage.cast::<ConfigStorage>().as_ref() {
        if let Some(health) = &storage.health {
            cfg.health_port = health.port;
            cfg.health_host.clone_from(&health.host);
            cfg.health_enabled = health.enabled;
        }
    }

    let mut urls = Vec::new();
    for (i, u) in list_view(&c.relay_urls).iter().enumerate() {
        match opt_str(u, "relay_urls")? {
            Some(s) => urls.push(s),
            None => return Err(format!("aster_config_t.relay_urls[{i}] is empty")),
        }
    }
    cfg.relay_mode = match c.relay_mode {
        x if x == aster_relay_mode_t::ASTER_RELAY_MODE_DEFAULT as u32 => {
            if urls.is_empty() {
                RelayMode::Default
            } else {
                RelayMode::Custom(urls)
            }
        }
        x if x == aster_relay_mode_t::ASTER_RELAY_MODE_CUSTOM as u32 => {
            if urls.is_empty() {
                return Err("relay_mode CUSTOM needs relay_urls".into());
            }
            RelayMode::Custom(urls)
        }
        x if x == aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32
            || x == aster_relay_mode_t::ASTER_RELAY_MODE_STAGING as u32 =>
        {
            if !urls.is_empty() {
                return Err("relay_urls requires relay_mode CUSTOM".into());
            }
            if x == aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32 {
                RelayMode::Disabled
            } else {
                RelayMode::Staging
            }
        }
        other => return Err(format!("unknown relay_mode {other}")),
    };

    let key = view(&c.secret_key);
    cfg.secret_key = match key.len() {
        0 => None,
        32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(key);
            Some(SecretKey::from_bytes(arr))
        }
        n => {
            return Err(format!(
                "aster_config_t.secret_key must be 32 bytes, got {n}"
            ))
        }
    };
    cfg.secret_key_file = opt_str(&c.secret_key_file, "secret_key_file")?.map(PathBuf::from);
    cfg.storage_path = opt_str(&c.storage_path, "storage_path")?.map(PathBuf::from);
    cfg.gc_interval = (c.gc_interval_ms > 0).then(|| Duration::from_millis(c.gc_interval_ms));
    cfg.bind_addr = opt_str(&c.bind_addr, "bind_addr")?;
    let mut alpns: Vec<Vec<u8>> = Vec::new();
    for (i, a) in list_view(&c.alpns).iter().enumerate() {
        let bytes = view(a);
        if bytes.is_empty() {
            return Err(format!("aster_config_t.alpns[{i}] is empty"));
        }
        if !alpns.iter().any(|x| x == bytes) {
            alpns.push(bytes.to_vec());
        }
    }
    cfg.alpns = alpns;
    cfg.identity_file = opt_str(&c.identity_file, "identity_file")?.map(PathBuf::from);
    cfg.proxy_url = opt_str(&c.proxy_url, "proxy_url")?;
    cfg.hook_timeout_ms = if c.hook_timeout_ms == 0 {
        5000
    } else {
        c.hook_timeout_ms
    };
    cfg.hook_failure_mode = match c.hook_failure_mode {
        x if x == aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_OPEN as u32 => {
            HookFailureMode::FailOpen
        }
        x if x == aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_CLOSED as u32 => {
            HookFailureMode::FailClosed
        }
        other => return Err(format!("unknown hook_failure_mode {other}")),
    };
    cfg.enable_hooks = flag(c.enable_hooks, "enable_hooks")?;
    cfg.enable_monitoring = flag(c.enable_monitoring, "enable_monitoring")?;
    cfg.local_discovery = flag(c.local_discovery, "local_discovery")?;
    cfg.clear_ip_transports = flag(c.clear_ip_transports, "clear_ip_transports")?;
    cfg.clear_relay_transports = flag(c.clear_relay_transports, "clear_relay_transports")?;
    cfg.portmapper_config = match c.portmapper {
        x if x == aster_portmapper_t::ASTER_PORTMAPPER_DEFAULT as u32 => None,
        x if x == aster_portmapper_t::ASTER_PORTMAPPER_ENABLED as u32 => Some("enabled".into()),
        x if x == aster_portmapper_t::ASTER_PORTMAPPER_DISABLED as u32 => Some("disabled".into()),
        other => return Err(format!("unknown portmapper {other}")),
    };
    cfg.proxy_from_env = flag(c.proxy_from_env, "proxy_from_env")?;

    let set = c.transport_set;
    if set & !ASTER_TRANSPORT_ALL != 0 {
        return Err(format!(
            "unknown transport_set bits {:#x}",
            set & !ASTER_TRANSPORT_ALL
        ));
    }
    let has = |bit: u32| set & bit != 0;
    let pick = |bit: u32, v: u64| has(bit).then_some(v);
    cfg.transport_max_concurrent_bidi_streams = pick(
        ASTER_TRANSPORT_MAX_CONCURRENT_BIDI_STREAMS,
        c.transport_max_concurrent_bidi_streams,
    );
    cfg.transport_max_concurrent_uni_streams = pick(
        ASTER_TRANSPORT_MAX_CONCURRENT_UNI_STREAMS,
        c.transport_max_concurrent_uni_streams,
    );
    cfg.transport_stream_receive_window = pick(
        ASTER_TRANSPORT_STREAM_RECEIVE_WINDOW,
        c.transport_stream_receive_window,
    );
    cfg.transport_receive_window = pick(ASTER_TRANSPORT_RECEIVE_WINDOW, c.transport_receive_window);
    cfg.transport_send_window = pick(ASTER_TRANSPORT_SEND_WINDOW, c.transport_send_window);
    cfg.transport_max_idle_timeout_ms = pick(
        ASTER_TRANSPORT_MAX_IDLE_TIMEOUT_MS,
        c.transport_max_idle_timeout_ms,
    );
    cfg.transport_keep_alive_interval_ms = pick(
        ASTER_TRANSPORT_KEEP_ALIVE_INTERVAL_MS,
        c.transport_keep_alive_interval_ms,
    );
    cfg.transport_initial_mtu =
        if has(ASTER_TRANSPORT_INITIAL_MTU) {
            Some(u16::try_from(c.transport_initial_mtu).map_err(|_| {
                format!("transport_initial_mtu {} > 65535", c.transport_initial_mtu)
            })?)
        } else {
            None
        };
    let to_usize = |v: u64, field: &str| {
        usize::try_from(v).map_err(|_| format!("{field} {v} does not fit in size_t"))
    };
    cfg.transport_datagram_receive_buffer_size =
        if has(ASTER_TRANSPORT_DATAGRAM_RECEIVE_BUFFER_SIZE) {
            Some(to_usize(
                c.transport_datagram_receive_buffer_size,
                "transport_datagram_receive_buffer_size",
            )?)
        } else {
            None
        };
    cfg.transport_datagram_send_buffer_size = if has(ASTER_TRANSPORT_DATAGRAM_SEND_BUFFER_SIZE) {
        Some(to_usize(
            c.transport_datagram_send_buffer_size,
            "transport_datagram_send_buffer_size",
        )?)
    } else {
        None
    };
    cfg.transport_send_fairness = if has(ASTER_TRANSPORT_SEND_FAIRNESS) {
        Some(flag(c.transport_send_fairness, "transport_send_fairness")?)
    } else {
        None
    };
    cfg.transport_enable_segmentation_offload = if has(ASTER_TRANSPORT_ENABLE_SEGMENTATION_OFFLOAD)
    {
        Some(flag(
            c.transport_enable_segmentation_offload,
            "transport_enable_segmentation_offload",
        )?)
    } else {
        None
    };
    Ok(cfg)
}

// ── filling: AsterConfig -> aster_config_t ──────────────────────────────────

/// Library-owned backing for a filled `aster_config_t`. Each inner `Vec`'s
/// heap buffer is stable once pushed, so views handed out stay valid.
#[derive(Default)]
struct ConfigStorage {
    bufs: Vec<Vec<u8>>,
    lists: Vec<Vec<aster_bytes_t>>,
    health: Option<StoredHealthConfig>,
}

struct StoredHealthConfig {
    port: u16,
    host: String,
    enabled: Option<bool>,
}

impl ConfigStorage {
    fn bytes(&mut self, v: Vec<u8>) -> aster_bytes_t {
        if v.is_empty() {
            return aster_bytes_t {
                ptr: std::ptr::null(),
                len: 0,
            };
        }
        let b = aster_bytes_t {
            ptr: v.as_ptr(),
            len: v.len(),
        };
        self.bufs.push(v);
        b
    }

    fn opt_str(&mut self, s: Option<String>) -> aster_bytes_t {
        self.bytes(s.map(String::into_bytes).unwrap_or_default())
    }

    fn list(&mut self, items: Vec<Vec<u8>>) -> aster_bytes_list_t {
        if items.is_empty() {
            return aster_bytes_list_t {
                items: std::ptr::null(),
                len: 0,
            };
        }
        let views: Vec<aster_bytes_t> = items.into_iter().map(|v| self.bytes(v)).collect();
        let l = aster_bytes_list_t {
            items: views.as_ptr(),
            len: views.len(),
        };
        self.lists.push(views);
        l
    }
}

fn path_str(p: &Option<PathBuf>) -> Option<String> {
    p.as_ref().map(|p| p.to_string_lossy().into_owned())
}

/// Fill `out` from `cfg`. Allocates storage for non-empty strings/lists or
/// non-default health metadata; otherwise `out.storage` is NULL.
pub(crate) fn fill_config(cfg: &AsterConfig, out: &mut aster_config_t) {
    let mut st = ConfigStorage::default();
    let defaults = AsterConfig::default();
    if cfg.health_port != defaults.health_port
        || cfg.health_host != defaults.health_host
        || cfg.health_enabled != defaults.health_enabled
    {
        st.health = Some(StoredHealthConfig {
            port: cfg.health_port,
            host: cfg.health_host.clone(),
            enabled: cfg.health_enabled,
        });
    }
    let (relay_mode, urls) = match &cfg.relay_mode {
        RelayMode::Default => (aster_relay_mode_t::ASTER_RELAY_MODE_DEFAULT, Vec::new()),
        RelayMode::Custom(u) => (
            aster_relay_mode_t::ASTER_RELAY_MODE_CUSTOM,
            u.iter().map(|s| s.clone().into_bytes()).collect(),
        ),
        RelayMode::Staging => (aster_relay_mode_t::ASTER_RELAY_MODE_STAGING, Vec::new()),
        RelayMode::Disabled => (aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED, Vec::new()),
    };
    let mut set = 0u32;
    let mut take = |bit: u32, v: Option<u64>| {
        if let Some(v) = v {
            set |= bit;
            v
        } else {
            0
        }
    };
    let bidi = take(
        ASTER_TRANSPORT_MAX_CONCURRENT_BIDI_STREAMS,
        cfg.transport_max_concurrent_bidi_streams,
    );
    let uni = take(
        ASTER_TRANSPORT_MAX_CONCURRENT_UNI_STREAMS,
        cfg.transport_max_concurrent_uni_streams,
    );
    let srw = take(
        ASTER_TRANSPORT_STREAM_RECEIVE_WINDOW,
        cfg.transport_stream_receive_window,
    );
    let rw = take(ASTER_TRANSPORT_RECEIVE_WINDOW, cfg.transport_receive_window);
    let sw = take(ASTER_TRANSPORT_SEND_WINDOW, cfg.transport_send_window);
    let idle = take(
        ASTER_TRANSPORT_MAX_IDLE_TIMEOUT_MS,
        cfg.transport_max_idle_timeout_ms,
    );
    let ka = take(
        ASTER_TRANSPORT_KEEP_ALIVE_INTERVAL_MS,
        cfg.transport_keep_alive_interval_ms,
    );
    let mtu = take(
        ASTER_TRANSPORT_INITIAL_MTU,
        cfg.transport_initial_mtu.map(u64::from),
    );
    let drb = take(
        ASTER_TRANSPORT_DATAGRAM_RECEIVE_BUFFER_SIZE,
        cfg.transport_datagram_receive_buffer_size.map(|v| v as u64),
    );
    let dsb = take(
        ASTER_TRANSPORT_DATAGRAM_SEND_BUFFER_SIZE,
        cfg.transport_datagram_send_buffer_size.map(|v| v as u64),
    );
    let fair = take(
        ASTER_TRANSPORT_SEND_FAIRNESS,
        cfg.transport_send_fairness.map(u64::from),
    );
    let gso = take(
        ASTER_TRANSPORT_ENABLE_SEGMENTATION_OFFLOAD,
        cfg.transport_enable_segmentation_offload.map(u64::from),
    );

    *out = aster_config_t {
        struct_size: std::mem::size_of::<aster_config_t>() as u32,
        relay_mode: relay_mode as u32,
        relay_urls: st.list(urls),
        secret_key: st.bytes(
            cfg.secret_key
                .as_ref()
                .map(|k| k.to_bytes().to_vec())
                .unwrap_or_default(),
        ),
        secret_key_file: st.opt_str(path_str(&cfg.secret_key_file)),
        storage_path: st.opt_str(path_str(&cfg.storage_path)),
        gc_interval_ms: cfg.gc_interval.map(|d| d.as_millis() as u64).unwrap_or(0),
        bind_addr: st.opt_str(cfg.bind_addr.clone()),
        alpns: st.list(cfg.alpns.clone()),
        identity_file: st.opt_str(path_str(&cfg.identity_file)),
        proxy_url: st.opt_str(cfg.proxy_url.clone()),
        hook_timeout_ms: cfg.hook_timeout_ms,
        hook_failure_mode: match cfg.hook_failure_mode {
            HookFailureMode::FailOpen => aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_OPEN,
            HookFailureMode::FailClosed => {
                aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_CLOSED
            }
        } as u32,
        enable_hooks: cfg.enable_hooks as u32,
        enable_monitoring: cfg.enable_monitoring as u32,
        local_discovery: cfg.local_discovery as u32,
        clear_ip_transports: cfg.clear_ip_transports as u32,
        clear_relay_transports: cfg.clear_relay_transports as u32,
        portmapper: match cfg.portmapper_config.as_deref() {
            None => aster_portmapper_t::ASTER_PORTMAPPER_DEFAULT,
            Some("disabled") => aster_portmapper_t::ASTER_PORTMAPPER_DISABLED,
            Some(_) => aster_portmapper_t::ASTER_PORTMAPPER_ENABLED,
        } as u32,
        proxy_from_env: cfg.proxy_from_env as u32,
        transport_set: set,
        transport_initial_mtu: mtu as u32,
        transport_send_fairness: fair as u32,
        transport_enable_segmentation_offload: gso as u32,
        transport_max_concurrent_bidi_streams: bidi,
        transport_max_concurrent_uni_streams: uni,
        transport_stream_receive_window: srw,
        transport_receive_window: rw,
        transport_send_window: sw,
        transport_max_idle_timeout_ms: idle,
        transport_keep_alive_interval_ms: ka,
        transport_datagram_receive_buffer_size: drb,
        transport_datagram_send_buffer_size: dsb,
        storage: std::ptr::null_mut(),
    };
    if !st.bufs.is_empty() || !st.lists.is_empty() || st.health.is_some() {
        out.storage = Box::into_raw(Box::new(st)) as *mut c_void;
    }
}

// ── config functions ────────────────────────────────────────────────────────

/// Fill `*out` with the defaults of `aster::AsterConfig` (relay DEFAULT,
/// in-memory, hooks off, hook timeout 5000 ms, fail-open, no transport
/// overrides). Never allocates (`storage` is NULL).
#[no_mangle]
pub unsafe extern "C" fn aster_config_init(out: *mut aster_config_t) -> i32 {
    let Some(out) = out.as_mut() else {
        return invalid("aster_config_init: out is NULL");
    };
    fill_config(&AsterConfig::default(), out);
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Fill `*out` from the `ASTER_*` environment over the defaults
/// (`AsterConfig::from_env`; variables in docs/aster-rust-getstarted.md §2.2).
/// Release with `aster_config_release`. A malformed variable returns
/// `ASTER_STATUS_INVALID_ARGUMENT` (with the last-error message) and leaves
/// `*out` zeroed.
#[no_mangle]
pub unsafe extern "C" fn aster_config_from_env(out: *mut aster_config_t) -> i32 {
    let Some(out) = out.as_mut() else {
        return invalid("aster_config_from_env: out is NULL");
    };
    *out = std::mem::zeroed();
    match AsterConfig::from_env() {
        Ok(cfg) => {
            fill_config(&cfg, out);
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(e) => invalid(e),
    }
}

/// Fill `*out` from an `aster.toml` file, then overlay the `ASTER_*`
/// environment (`AsterConfig::from_file`: defaults < file < env). `path` is
/// UTF-8. Release with `aster_config_release`. An unreadable or malformed file
/// returns `ASTER_STATUS_INVALID_ARGUMENT` and leaves `*out` zeroed.
#[no_mangle]
pub unsafe extern "C" fn aster_config_from_toml(
    path: aster_bytes_t,
    out: *mut aster_config_t,
) -> i32 {
    let Some(out) = out.as_mut() else {
        return invalid("aster_config_from_toml: out is NULL");
    };
    *out = std::mem::zeroed();
    let path = match opt_str(&path, "path") {
        Ok(Some(p)) => p,
        Ok(None) => return invalid("aster_config_from_toml: path is empty"),
        Err(e) => return invalid(e),
    };
    match AsterConfig::from_file(&path) {
        Ok(cfg) => {
            fill_config(&cfg, out);
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(e) => invalid(e),
    }
}

/// Free the storage of a config filled by `aster_config_from_env` /
/// `aster_config_from_toml` and zero its pointers. No-op when `storage` is
/// NULL (caller-built or `aster_config_init` configs). Call once per filled
/// struct.
#[no_mangle]
pub unsafe extern "C" fn aster_config_release(config: *mut aster_config_t) -> i32 {
    let Some(c) = config.as_mut() else {
        return invalid("aster_config_release: config is NULL");
    };
    if !c.storage.is_null() {
        drop(Box::from_raw(c.storage as *mut ConfigStorage));
        c.storage = std::ptr::null_mut();
    }
    let empty = aster_bytes_t {
        ptr: std::ptr::null(),
        len: 0,
    };
    let empty_list = aster_bytes_list_t {
        items: std::ptr::null(),
        len: 0,
    };
    c.relay_urls = empty_list;
    c.alpns = empty_list;
    c.secret_key = empty;
    c.secret_key_file = empty;
    c.storage_path = empty;
    c.bind_addr = empty;
    c.identity_file = empty;
    c.proxy_url = empty;
    aster_status_t::ASTER_STATUS_OK as i32
}

// ── Gate 0 ──────────────────────────────────────────────────────────────────

/// A gate handle's state. Setters replace the policy value; the admitted-peer
/// store inside it is shared, so admissions survive setter calls.
pub(crate) struct GateEntry {
    pub(crate) policy: RwLock<GatePolicy>,
}

/// Who decides a hooked node's admission. Shared with the node's admission
/// drainers, so `aster_node_set_gate` takes effect immediately. A node
/// started by `aster_node_start_enrolled` has no slot: Enrollment consumes
/// its hooks directly.
#[derive(Default)]
pub(crate) struct SlotState {
    /// The attached gate, if any.
    pub(crate) gate: Option<Arc<GateEntry>>,
}

/// Per-node admission slot (see [`SlotState`]).
pub(crate) type GateSlot = RwLock<SlotState>;

fn gate_entry(bridge: &BridgeRuntime, gate: aster_gate_t) -> Result<Arc<GateEntry>, i32> {
    bridge.gates.get(gate).ok_or_else(|| {
        set_last_error(format!("unknown gate handle {gate}"));
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    })
}

/// Parse a peer id argument: 64 hex characters (case-insensitive).
unsafe fn peer_id(b: &aster_bytes_t) -> Result<String, i32> {
    let s = view(b);
    if s.len() != 64 || !s.iter().all(u8::is_ascii_hexdigit) {
        return Err(invalid("peer must be a 64-character hex node id"));
    }
    Ok(String::from_utf8_lossy(s).to_ascii_lowercase())
}

macro_rules! try_status {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(s) => return s,
        }
    };
}

fn bridge_for(runtime: aster_runtime_t) -> Result<Arc<BridgeRuntime>, i32> {
    load_runtime(runtime).map_err(|s| s as i32)
}

/// Create a Gate-0 policy: PROTECTED, admission ALPNs =
/// `aster.producer_admission` and `aster.admission`, reject code 403 with
/// reason "peer not admitted", no admitted peers. (Since 5.1 the retired
/// consumer admission ALPN is not in the default set; Enrollment, through
/// `aster_node_start_enrolled`, replaces it.)
/// Free with `aster_gate_free`.
#[no_mangle]
pub unsafe extern "C" fn aster_gate_new(
    runtime: aster_runtime_t,
    out_gate: *mut aster_gate_t,
) -> i32 {
    if out_gate.is_null() {
        return invalid("aster_gate_new: out_gate is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    *out_gate = bridge.gates.insert(GateEntry {
        policy: RwLock::new(GatePolicy::protected().with_admission_alpns(vec![
            aster::alpns::PRODUCER_ADMISSION.to_vec(),
            aster::alpns::DELEGATED_ADMISSION.to_vec(),
        ])),
    });
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Free a gate handle. Nodes it is attached to keep using it until detached
/// or closed.
#[no_mangle]
pub unsafe extern "C" fn aster_gate_free(runtime: aster_runtime_t, gate: aster_gate_t) -> i32 {
    let bridge = try_status!(bridge_for(runtime));
    if bridge.gates.remove(gate).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

fn update_policy(entry: &GateEntry, f: impl FnOnce(GatePolicy) -> GatePolicy) {
    let mut guard = entry.policy.write().unwrap_or_else(|e| e.into_inner());
    let current = guard.clone();
    *guard = f(current);
}

/// Set the trust mode (`aster_trust_mode_t`).
#[no_mangle]
pub unsafe extern "C" fn aster_gate_set_trust_mode(
    runtime: aster_runtime_t,
    gate: aster_gate_t,
    mode: u32,
) -> i32 {
    let mode = match mode {
        x if x == aster_trust_mode_t::ASTER_TRUST_MODE_PROTECTED as u32 => TrustMode::Protected,
        x if x == aster_trust_mode_t::ASTER_TRUST_MODE_OPEN_DEV as u32 => TrustMode::OpenDev,
        other => return invalid(format!("unknown trust mode {other}")),
    };
    let bridge = try_status!(bridge_for(runtime));
    let entry = try_status!(gate_entry(&bridge, gate));
    update_policy(&entry, |p| p.set_mode(mode));
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Replace the always-open admission ALPN set (may be empty: then no ALPN is
/// open to un-admitted peers in PROTECTED mode).
#[no_mangle]
pub unsafe extern "C" fn aster_gate_set_admission_alpns(
    runtime: aster_runtime_t,
    gate: aster_gate_t,
    alpns: aster_bytes_list_t,
) -> i32 {
    let mut list = Vec::new();
    for (i, a) in list_view(&alpns).iter().enumerate() {
        let bytes = view(a);
        if bytes.is_empty() {
            return invalid(format!("admission alpns[{i}] is empty"));
        }
        list.push(bytes.to_vec());
    }
    let bridge = try_status!(bridge_for(runtime));
    let entry = try_status!(gate_entry(&bridge, gate));
    update_policy(&entry, |p| p.with_admission_alpns(list));
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Set the QUIC application close code and reason used when the gate rejects
/// a peer (defaults 403 / "peer not admitted").
#[no_mangle]
pub unsafe extern "C" fn aster_gate_set_reject(
    runtime: aster_runtime_t,
    gate: aster_gate_t,
    code: u32,
    reason: aster_bytes_t,
) -> i32 {
    let reason = view(&reason).to_vec();
    let bridge = try_status!(bridge_for(runtime));
    let entry = try_status!(gate_entry(&bridge, gate));
    update_policy(&entry, |p| p.with_reject(code, reason));
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Admit `peer` (64-hex node id, manual source, no expiry).
#[no_mangle]
pub unsafe extern "C" fn aster_gate_admit(
    runtime: aster_runtime_t,
    gate: aster_gate_t,
    peer: aster_bytes_t,
) -> i32 {
    let peer = try_status!(peer_id(&peer));
    let bridge = try_status!(bridge_for(runtime));
    let entry = try_status!(gate_entry(&bridge, gate));
    entry
        .policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .admit_peer(peer);
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Revoke `peer`'s admission. Revoking an unknown peer is OK. Existing
/// connections are not closed; new ones are gated.
#[no_mangle]
pub unsafe extern "C" fn aster_gate_revoke(
    runtime: aster_runtime_t,
    gate: aster_gate_t,
    peer: aster_bytes_t,
) -> i32 {
    let peer = try_status!(peer_id(&peer));
    let bridge = try_status!(bridge_for(runtime));
    let entry = try_status!(gate_entry(&bridge, gate));
    entry
        .policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .revoke(&peer);
    aster_status_t::ASTER_STATUS_OK as i32
}

/// `*out_admitted` = 1 if `peer` holds an unexpired admission, else 0.
#[no_mangle]
pub unsafe extern "C" fn aster_gate_is_admitted(
    runtime: aster_runtime_t,
    gate: aster_gate_t,
    peer: aster_bytes_t,
    out_admitted: *mut u32,
) -> i32 {
    if out_admitted.is_null() {
        return invalid("aster_gate_is_admitted: out_admitted is NULL");
    }
    let peer = try_status!(peer_id(&peer));
    let bridge = try_status!(bridge_for(runtime));
    let entry = try_status!(gate_entry(&bridge, gate));
    *out_admitted = entry
        .policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_admitted(&peer) as u32;
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Evaluate the gate for `(peer, alpn)` without connecting: `*out_decision` is
/// `ASTER_HOOK_DECISION_ALLOW` or `ASTER_HOOK_DECISION_DENY`. For callers that
/// answer admission events themselves.
#[no_mangle]
pub unsafe extern "C" fn aster_gate_should_allow(
    runtime: aster_runtime_t,
    gate: aster_gate_t,
    peer: aster_bytes_t,
    alpn: aster_bytes_t,
    out_decision: *mut u32,
) -> i32 {
    if out_decision.is_null() {
        return invalid("aster_gate_should_allow: out_decision is NULL");
    }
    let peer = try_status!(peer_id(&peer));
    let alpn = view(&alpn);
    let bridge = try_status!(bridge_for(runtime));
    let entry = try_status!(gate_entry(&bridge, gate));
    let allowed = entry
        .policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .should_allow(&peer, alpn)
        .is_allowed();
    *out_decision = if allowed {
        aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW
    } else {
        aster_hook_decision_t::ASTER_HOOK_DECISION_DENY
    } as u32;
    aster_status_t::ASTER_STATUS_OK as i32
}

// ── admission drainers ──────────────────────────────────────────────────────

/// Drain a hook receiver into completion-queue events (`event.handle` =
/// `owner`, `event.related` = invocation id), or — when `gate` currently holds
/// a policy — decide in Rust without an event.
pub(crate) fn spawn_hook_drainers(
    bridge: &Arc<BridgeRuntime>,
    owner: u64,
    user_data: u64,
    receiver: CoreHookReceiver,
    gate: Option<Arc<GateSlot>>,
) {
    let CoreHookReceiver {
        before_connect_rx,
        after_handshake_rx,
    } = receiver;

    let current_gate = |slot: &Option<Arc<GateSlot>>| -> Option<Arc<GateEntry>> {
        slot.as_ref()
            .and_then(|s| s.read().unwrap_or_else(|e| e.into_inner()).gate.clone())
    };
    let b = bridge.clone();
    let slot = gate.clone();
    bridge.runtime.spawn(async move {
        let mut rx = before_connect_rx;
        while let Some((info, reply_tx)) = rx.recv().await {
            // Outbound dials are this node's own; a gate never blocks them.
            if current_gate(&slot).is_some() {
                let _ = reply_tx.send(true);
                continue;
            }
            let invocation_id = b.hook_invocations.insert(HookInvocationState {
                sender: std::sync::Mutex::new(Some(HookSender::BeforeConnect(reply_tx))),
            });
            let event = EventInternal::new(
                aster_event_kind_t::ASTER_EVENT_HOOK_BEFORE_CONNECT,
                aster_status_t::ASTER_STATUS_OK,
                0,
                owner,
                invocation_id,
                user_data,
                0,
            );
            if !b
                .emit_pushed_with_data(event, encode_hook_connect_payload(&info))
                .await
            {
                break;
            }
        }
    });

    let b = bridge.clone();
    let slot = gate;
    bridge.runtime.spawn(async move {
        let mut rx = after_handshake_rx;
        while let Some((info, reply_tx)) = rx.recv().await {
            if let Some(entry) = current_gate(&slot) {
                let decision = entry
                    .policy
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .should_allow(&info.remote_endpoint_id, &info.alpn);
                let _ = reply_tx.send(match decision {
                    GateDecision::Allow => CoreAfterHandshakeDecision::Accept,
                    GateDecision::Reject { error_code, reason } => {
                        CoreAfterHandshakeDecision::Reject { error_code, reason }
                    }
                });
                continue;
            }
            let invocation_id = b.hook_invocations.insert(HookInvocationState {
                sender: std::sync::Mutex::new(Some(HookSender::AfterConnect(reply_tx))),
            });
            let event = EventInternal::new(
                aster_event_kind_t::ASTER_EVENT_HOOK_AFTER_CONNECT,
                aster_status_t::ASTER_STATUS_OK,
                0,
                owner,
                invocation_id,
                user_data,
                0,
            );
            if !b
                .emit_pushed_with_data(event, encode_hook_handshake_payload(&info))
                .await
            {
                break;
            }
        }
    });
}

/// Answer an admission event (`ASTER_EVENT_HOOK_AFTER_CONNECT` or
/// `ASTER_EVENT_HOOK_BEFORE_CONNECT`, from a node or an endpoint).
///
/// `decision` is `ASTER_HOOK_DECISION_ALLOW` or `ASTER_HOOK_DECISION_DENY`.
/// On DENY of an inbound (after-connect) invocation the connection is closed
/// with `reject_code` and `reason` (borrowed for the call; may be empty); both
/// are ignored for ALLOW and for outbound (before-connect) invocations.
/// Consumes the invocation: a second call returns `ASTER_STATUS_NOT_FOUND`.
/// Answering after the hook timeout is OK (the failure mode already applied).
#[no_mangle]
pub unsafe extern "C" fn aster_admission_respond(
    runtime: aster_runtime_t,
    invocation: aster_hook_invocation_t,
    decision: u32,
    reject_code: u32,
    reason: aster_bytes_t,
) -> i32 {
    let allow = match decision {
        x if x == aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32 => true,
        x if x == aster_hook_decision_t::ASTER_HOOK_DECISION_DENY as u32 => false,
        other => return invalid(format!("unknown decision {other}")),
    };
    let reason = view(&reason).to_vec();
    let bridge = try_status!(bridge_for(runtime));
    let Some(state) = bridge.hook_invocations.remove(invocation) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let Some(sender) = state.sender.lock().ok().and_then(|mut g| g.take()) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    match sender {
        HookSender::BeforeConnect(tx) => {
            let _ = tx.send(allow);
        }
        HookSender::AfterConnect(tx) => {
            let _ = tx.send(if allow {
                CoreAfterHandshakeDecision::Accept
            } else {
                CoreAfterHandshakeDecision::Reject {
                    error_code: reject_code,
                    reason,
                }
            });
        }
    }
    aster_status_t::ASTER_STATUS_OK as i32
}

// ── node start ──────────────────────────────────────────────────────────────

/// Start a node from `config` via `aster::Node::start` (NULL = defaults).
///
/// Async: emits `ASTER_EVENT_NODE_CREATED` (`event.handle` = node) or
/// `ASTER_EVENT_ERROR` (message in the event data) on the completion queue.
/// The config is validated and copied before this returns, so the caller may
/// release it right away; a malformed config returns
/// `ASTER_STATUS_INVALID_ARGUMENT` and starts nothing.
///
/// `gate` (0 = none) attaches a Gate-0 policy (see `aster_node_set_gate`) and
/// forces `enable_hooks` on. With hooks on and no gate, admission events are
/// raised for the caller to answer with `aster_admission_respond`.
///
/// The only way to create a node or endpoint since ABI 4.0 (the
/// `aster_node_memory*` / `aster_node_persistent` / `aster_endpoint_create`
/// factories were removed).
#[no_mangle]
pub unsafe extern "C" fn aster_node_start(
    runtime: aster_runtime_t,
    config: *const aster_config_t,
    gate: aster_gate_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_node_start: out_operation is NULL");
    }
    let mut cfg = match config.as_ref() {
        None => AsterConfig::default(),
        Some(c) => match lower_config(c) {
            Ok(cfg) => cfg,
            Err(e) => return invalid(e),
        },
    };
    let bridge = try_status!(bridge_for(runtime));
    let gate_entry = if gate == 0 {
        None
    } else {
        cfg.enable_hooks = true;
        Some(try_status!(gate_entry(&bridge, gate)))
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    *out_operation = op_id;

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        let hooks = cfg.enable_hooks;
        // `aster_health_start` with a NULL config serves this.
        let health = cfg.health_config();
        match aster::Node::start(cfg).await {
            Ok(node) => {
                let core: CoreNode = node.core_node().clone();
                let receiver = if hooks {
                    core.take_hook_receiver()
                } else {
                    None
                };
                let handle = bridge2.nodes.insert(core);
                crate::health::remember_node_config(handle, health);
                let slot = receiver
                    .as_ref()
                    .map(|_| Arc::new(RwLock::new(SlotState { gate: gate_entry })));
                if let Some(slot) = &slot {
                    bridge2
                        .node_gates
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(handle, slot.clone());
                }
                let completed = bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_NODE_CREATED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    handle,
                    0,
                    user_data,
                    0,
                );
                if completed {
                    if let Some(receiver) = receiver {
                        spawn_hook_drainers(&bridge2, handle, user_data, receiver, slot);
                    }
                } else {
                    forget_node(&bridge2, handle);
                    bridge2.nodes.remove(handle);
                }
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Attach (`gate` ≠ 0) or detach (`gate` = 0) a Gate-0 policy on a node
/// started by `aster_node_start` with hooks enabled (or with a gate). Takes
/// effect for the next admission decision. Returns
/// `ASTER_STATUS_UNSUPPORTED` for nodes without admission hooks, and since
/// ABI 5.1 for nodes started by `aster_node_start_enrolled`.
#[no_mangle]
pub unsafe extern "C" fn aster_node_set_gate(
    runtime: aster_runtime_t,
    node: aster_node_t,
    gate: aster_gate_t,
) -> i32 {
    let bridge = try_status!(bridge_for(runtime));
    if bridge.nodes.get(node).is_none() {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    }
    let entry = if gate == 0 {
        None
    } else {
        Some(try_status!(gate_entry(&bridge, gate)))
    };
    let slot = bridge
        .node_gates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&node)
        .cloned();
    match slot {
        Some(slot) => {
            slot.write().unwrap_or_else(|e| e.into_inner()).gate = entry;
            aster_status_t::ASTER_STATUS_OK as i32
        }
        None if bridge
            .node_enrollments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&node) =>
        {
            set_last_error("Enrollment decides this node's admission; no gate can be attached");
            aster_status_t::ASTER_STATUS_UNSUPPORTED as i32
        }
        None => {
            set_last_error("node has no admission hooks (start it with enable_hooks or a gate)");
            aster_status_t::ASTER_STATUS_UNSUPPORTED as i32
        }
    }
}

/// Remove the gate slot of a node being freed/closed.
pub(crate) fn forget_node(bridge: &BridgeRuntime, node: aster_node_t) {
    crate::health::forget_node_config(node);
    crate::enrollment::forget_node(bridge, node);
    bridge
        .node_gates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&node);
}

/// Per-runtime map of node handle -> gate slot.
pub(crate) type NodeGates = std::sync::Mutex<HashMap<u64, Arc<GateSlot>>>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that read or write `ASTER_*` env variables.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn full_config() -> AsterConfig {
        let mut cfg = AsterConfig::builder()
            .relay_urls(["https://relay-a.example", "https://relay-b.example"])
            .secret_key(SecretKey::from_bytes([7u8; 32]))
            .secret_key_file("/tmp/aster-ffi-test/node.key")
            .persistent("/tmp/aster-ffi-test/data")
            .gc_interval(Duration::from_millis(1500))
            .bind_addr("127.0.0.1:0")
            .alpn(b"aster/1".to_vec())
            .alpn(b"custom/2".to_vec())
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
            .transport_datagram_receive_buffer_size(0)
            .transport_datagram_send_buffer_size(9)
            .transport_send_fairness(false)
            .transport_enable_segmentation_offload(true)
            .identity_file("/tmp/aster-ffi-test/.aster-identity")
            .build();
        cfg.gc_interval = Some(Duration::from_millis(1500));
        cfg
    }

    /// Every node knob, compared field by field (AsterConfig has no PartialEq).
    fn assert_same(a: &AsterConfig, b: &AsterConfig) {
        assert_eq!(a.relay_mode, b.relay_mode);
        assert_eq!(
            a.secret_key.as_ref().map(|k| k.to_bytes()),
            b.secret_key.as_ref().map(|k| k.to_bytes())
        );
        assert_eq!(a.secret_key_file, b.secret_key_file);
        assert_eq!(a.storage_path, b.storage_path);
        assert_eq!(a.gc_interval, b.gc_interval);
        assert_eq!(a.bind_addr, b.bind_addr);
        assert_eq!(a.health_port, b.health_port);
        assert_eq!(a.health_host, b.health_host);
        assert_eq!(a.health_enabled, b.health_enabled);
        assert_eq!(a.alpns, b.alpns);
        assert_eq!(a.identity_file, b.identity_file);
        assert_eq!(a.local_discovery, b.local_discovery);
        assert_eq!(a.enable_monitoring, b.enable_monitoring);
        assert_eq!(a.enable_hooks, b.enable_hooks);
        assert_eq!(a.hook_timeout_ms, b.hook_timeout_ms);
        assert_eq!(a.hook_failure_mode, b.hook_failure_mode);
        assert_eq!(a.clear_ip_transports, b.clear_ip_transports);
        assert_eq!(a.clear_relay_transports, b.clear_relay_transports);
        assert_eq!(a.portmapper_config, b.portmapper_config);
        assert_eq!(a.proxy_url, b.proxy_url);
        assert_eq!(a.proxy_from_env, b.proxy_from_env);
        assert_eq!(
            a.transport_max_concurrent_bidi_streams,
            b.transport_max_concurrent_bidi_streams
        );
        assert_eq!(
            a.transport_max_concurrent_uni_streams,
            b.transport_max_concurrent_uni_streams
        );
        assert_eq!(
            a.transport_stream_receive_window,
            b.transport_stream_receive_window
        );
        assert_eq!(a.transport_receive_window, b.transport_receive_window);
        assert_eq!(a.transport_send_window, b.transport_send_window);
        assert_eq!(
            a.transport_max_idle_timeout_ms,
            b.transport_max_idle_timeout_ms
        );
        assert_eq!(
            a.transport_keep_alive_interval_ms,
            b.transport_keep_alive_interval_ms
        );
        assert_eq!(a.transport_initial_mtu, b.transport_initial_mtu);
        assert_eq!(
            a.transport_datagram_receive_buffer_size,
            b.transport_datagram_receive_buffer_size
        );
        assert_eq!(
            a.transport_datagram_send_buffer_size,
            b.transport_datagram_send_buffer_size
        );
        assert_eq!(a.transport_send_fairness, b.transport_send_fairness);
        assert_eq!(
            a.transport_enable_segmentation_offload,
            b.transport_enable_segmentation_offload
        );
        // And the lowered endpoint config the node actually starts with.
        assert_eq!(
            format!("{:?}", a.core_endpoint_config()),
            format!("{:?}", b.core_endpoint_config())
        );
    }

    fn round_trip(cfg: &AsterConfig) -> AsterConfig {
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        fill_config(cfg, &mut c);
        let back = unsafe { lower_config(&c) }.expect("lower");
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
        assert!(c.storage.is_null() && c.bind_addr.ptr.is_null() && c.alpns.items.is_null());
        back
    }

    #[test]
    fn config_round_trips_every_knob() {
        let full = full_config();
        assert_same(&full, &round_trip(&full));
        let default = AsterConfig::default();
        assert_same(&default, &round_trip(&default));
        for mode in [RelayMode::Disabled, RelayMode::Staging, RelayMode::Default] {
            let cfg = AsterConfig::builder().relay(mode).build();
            assert_same(&cfg, &round_trip(&cfg));
        }
    }

    #[test]
    fn init_matches_rust_defaults_without_allocating() {
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { aster_config_init(&mut c) }, 0);
        assert!(c.storage.is_null());
        assert_eq!(
            c.struct_size as usize,
            std::mem::size_of::<aster_config_t>()
        );
        assert_eq!(c.hook_timeout_ms, 5000);
        assert_eq!(c.transport_set, 0);
        assert_same(
            &AsterConfig::default(),
            &unsafe { lower_config(&c) }.unwrap(),
        );
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
    }

    #[test]
    fn lower_rejects_malformed_configs() {
        let mut base: aster_config_t = unsafe { std::mem::zeroed() };
        unsafe { aster_config_init(&mut base) };
        let check = |mutate: &dyn Fn(&mut aster_config_t), needle: &str| {
            let mut c = base;
            mutate(&mut c);
            let err = unsafe { lower_config(&c) }.expect_err("must fail");
            assert!(err.contains(needle), "{err:?} lacks {needle:?}");
        };
        let short = [1u8; 31];
        let url = aster_bytes_t {
            ptr: b"https://r".as_ptr(),
            len: 9,
        };
        let urls = [url];
        check(&|c| c.struct_size = 8, "struct_size");
        check(&|c| c.relay_mode = 9, "relay_mode");
        check(&|c| c.relay_mode = 1, "CUSTOM");
        check(
            &|c| {
                c.relay_mode = 2;
                c.relay_urls = aster_bytes_list_t {
                    items: urls.as_ptr(),
                    len: 1,
                };
            },
            "CUSTOM",
        );
        check(
            &|c| {
                c.secret_key = aster_bytes_t {
                    ptr: short.as_ptr(),
                    len: short.len(),
                }
            },
            "32 bytes",
        );
        check(&|c| c.hook_failure_mode = 2, "hook_failure_mode");
        check(&|c| c.portmapper = 3, "portmapper");
        check(&|c| c.enable_hooks = 2, "enable_hooks");
        check(&|c| c.transport_set = 1 << 20, "transport_set");
        check(
            &|c| {
                c.transport_set = ASTER_TRANSPORT_INITIAL_MTU;
                c.transport_initial_mtu = 70_000;
            },
            "65535",
        );
        let bad = [0xffu8, 0xfe];
        check(
            &|c| {
                c.bind_addr = aster_bytes_t {
                    ptr: bad.as_ptr(),
                    len: 2,
                }
            },
            "UTF-8",
        );
        // DEFAULT + URLs selects CUSTOM (a bare URL list means custom).
        let mut c = base;
        c.relay_urls = aster_bytes_list_t {
            items: urls.as_ptr(),
            len: 1,
        };
        assert_eq!(
            unsafe { lower_config(&c) }.unwrap().relay_mode,
            RelayMode::Custom(vec!["https://r".into()])
        );
    }

    const ENV_KEYS: &[&str] = &[
        "ASTER_RELAY_MODE",
        "ASTER_BIND_ADDR",
        "ASTER_GC_INTERVAL_MS",
        "ASTER_ENABLE_HOOKS",
        "ASTER_HOOK_FAILURE_MODE",
        "ASTER_PORTMAPPER_CONFIG",
        "ASTER_TRANSPORT_INITIAL_MTU",
        "ASTER_SECRET_KEY",
        "ASTER_HEALTH_PORT",
        "ASTER_HEALTH_HOST",
        "ASTER_HEALTH_ENABLED",
    ];

    fn clear_env() {
        for k in ENV_KEYS {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn config_from_env_fills_the_struct() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        std::env::set_var("ASTER_RELAY_MODE", "disabled");
        std::env::set_var("ASTER_BIND_ADDR", "127.0.0.1:4567");
        std::env::set_var("ASTER_GC_INTERVAL_MS", "2500");
        std::env::set_var("ASTER_ENABLE_HOOKS", "true");
        std::env::set_var("ASTER_HOOK_FAILURE_MODE", "fail_closed");
        std::env::set_var("ASTER_PORTMAPPER_CONFIG", "disabled");
        std::env::set_var("ASTER_TRANSPORT_INITIAL_MTU", "1300");
        std::env::set_var("ASTER_SECRET_KEY", "07".repeat(32));
        std::env::set_var("ASTER_HEALTH_PORT", "18080");
        std::env::set_var("ASTER_HEALTH_HOST", "0.0.0.0");
        std::env::set_var("ASTER_HEALTH_ENABLED", "false");
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        let status = unsafe { aster_config_from_env(&mut c) };
        clear_env();
        assert_eq!(status, 0);
        assert_eq!(
            c.relay_mode,
            aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32
        );
        assert_eq!(unsafe { view(&c.bind_addr) }, b"127.0.0.1:4567");
        assert_eq!(c.gc_interval_ms, 2500);
        assert_eq!(c.enable_hooks, 1);
        assert_eq!(
            c.hook_failure_mode,
            aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_CLOSED as u32
        );
        assert_eq!(
            c.portmapper,
            aster_portmapper_t::ASTER_PORTMAPPER_DISABLED as u32
        );
        assert_eq!(c.transport_set, ASTER_TRANSPORT_INITIAL_MTU);
        assert_eq!(c.transport_initial_mtu, 1300);
        assert_eq!(unsafe { view(&c.secret_key) }, &[7u8; 32]);
        let health = unsafe { lower_config(&c) }.unwrap();
        assert_eq!(health.health_port, 18080);
        assert_eq!(health.health_host, "0.0.0.0");
        assert_eq!(health.health_enabled, Some(false));
        assert!(!c.storage.is_null());
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
        assert!(c.storage.is_null());
        // Releasing twice is a no-op.
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);

        // A malformed variable is INVALID_ARGUMENT and leaves the struct zeroed.
        std::env::set_var("ASTER_HOOK_FAILURE_MODE", "sometimes");
        let status = unsafe { aster_config_from_env(&mut c) };
        clear_env();
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);
        assert!(c.storage.is_null() && c.struct_size == 0);
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the filesystem; Miri isolation forbids mkdir")]
    fn config_from_toml_fills_every_section() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let dir = std::env::temp_dir().join(format!("aster-ffi-toml-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aster.toml");
        std::fs::write(
            &path,
            r#"
[storage]
path = "/var/lib/aster-test"
gc_interval_ms = 60000

[network]
relay_urls = ["https://relay.example"]
bind_addr = "0.0.0.0:9000"
secret_key_file = "/var/lib/aster-test/node.key"
local_discovery = true
enable_monitoring = true
enable_hooks = true
hook_timeout_ms = 1234
hook_failure_mode = "fail_closed"
clear_ip_transports = true
portmapper_config = "enabled"
proxy_url = "http://localhost:8080"
proxy_from_env = true
transport_max_concurrent_bidi_streams = 512
transport_keep_alive_interval_ms = 2000
transport_send_fairness = true

[health]
port = 18081
host = "0.0.0.0"
enabled = true

[identity]
file = "/etc/aster/.aster-identity"
"#,
        )
        .unwrap();
        let p = path.to_string_lossy().into_owned();
        let mut c: aster_config_t = unsafe { std::mem::zeroed() };
        let status = unsafe {
            aster_config_from_toml(
                aster_bytes_t {
                    ptr: p.as_ptr(),
                    len: p.len(),
                },
                &mut c,
            )
        };
        assert_eq!(status, 0);
        let cfg = unsafe { lower_config(&c) }.unwrap();
        assert_eq!(
            cfg.relay_mode,
            RelayMode::Custom(vec!["https://relay.example".into()])
        );
        assert_eq!(cfg.storage_path, Some(PathBuf::from("/var/lib/aster-test")));
        assert_eq!(cfg.gc_interval, Some(Duration::from_secs(60)));
        assert_eq!(cfg.bind_addr.as_deref(), Some("0.0.0.0:9000"));
        assert_eq!(cfg.health_port, 18081);
        assert_eq!(cfg.health_host, "0.0.0.0");
        assert_eq!(cfg.health_enabled, Some(true));
        assert_eq!(
            cfg.secret_key_file,
            Some(PathBuf::from("/var/lib/aster-test/node.key"))
        );
        assert!(cfg.local_discovery && cfg.enable_monitoring && cfg.enable_hooks);
        assert_eq!(cfg.hook_timeout_ms, 1234);
        assert_eq!(cfg.hook_failure_mode, HookFailureMode::FailClosed);
        assert!(cfg.clear_ip_transports && !cfg.clear_relay_transports);
        assert_eq!(cfg.portmapper_config.as_deref(), Some("enabled"));
        assert_eq!(cfg.proxy_url.as_deref(), Some("http://localhost:8080"));
        assert!(cfg.proxy_from_env);
        assert_eq!(cfg.transport_max_concurrent_bidi_streams, Some(512));
        assert_eq!(cfg.transport_keep_alive_interval_ms, Some(2000));
        assert_eq!(cfg.transport_send_fairness, Some(true));
        assert_eq!(cfg.transport_initial_mtu, None);
        assert_eq!(
            cfg.identity_file,
            Some(PathBuf::from("/etc/aster/.aster-identity"))
        );
        assert_eq!(unsafe { aster_config_release(&mut c) }, 0);

        // Env overlays the file.
        std::env::set_var("ASTER_BIND_ADDR", "127.0.0.1:1");
        let status = unsafe {
            aster_config_from_toml(
                aster_bytes_t {
                    ptr: p.as_ptr(),
                    len: p.len(),
                },
                &mut c,
            )
        };
        clear_env();
        assert_eq!(status, 0);
        assert_eq!(unsafe { view(&c.bind_addr) }, b"127.0.0.1:1");
        unsafe { aster_config_release(&mut c) };

        // Missing / malformed files.
        let missing = dir.join("nope.toml").to_string_lossy().into_owned();
        let status = unsafe {
            aster_config_from_toml(
                aster_bytes_t {
                    ptr: missing.as_ptr(),
                    len: missing.len(),
                },
                &mut c,
            )
        };
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);
        std::fs::write(&path, "[network\nbroken").unwrap();
        let status = unsafe {
            aster_config_from_toml(
                aster_bytes_t {
                    ptr: p.as_ptr(),
                    len: p.len(),
                },
                &mut c,
            )
        };
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);
        assert!(c.storage.is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn health_config_survives_owned_round_trip_and_public_overrides() {
        for enabled in [None, Some(true), Some(false)] {
            let original = AsterConfig {
                health_port: 18080,
                health_host: "0.0.0.0".into(),
                health_enabled: enabled,
                ..Default::default()
            };
            let mut c: aster_config_t = unsafe { std::mem::zeroed() };
            fill_config(&original, &mut c);
            // Caller-owned public overrides remain independent of opaque metadata.
            let bind = b"127.0.0.1:0";
            c.bind_addr = aster_bytes_t {
                ptr: bind.as_ptr(),
                len: bind.len(),
            };
            let lowered = unsafe { lower_config(&c) }.unwrap();
            assert_eq!(unsafe { aster_config_release(&mut c) }, 0);
            // The lowered Rust config must own its strings after C storage release.
            assert_eq!(lowered.health_port, original.health_port);
            assert_eq!(lowered.health_host, original.health_host);
            assert_eq!(lowered.health_enabled, enabled);
            assert_eq!(lowered.bind_addr.as_deref(), Some("127.0.0.1:0"));
        }
    }
}
