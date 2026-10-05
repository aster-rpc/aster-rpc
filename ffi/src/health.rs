//! The ops listener (ABI 4.6): `/healthz`, `/readyz`, `/metrics` over plain
//! HTTP, `aster::ops::HealthServer` behind a handle.
//!
//! Design: `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md` §6, §7.5.
//!
//! ```text
//!   aster_health_start(rt, node, cfg) → h        joins / starts the OTel pipeline
//!   aster_server_attach_health(builder, h)       before aster_server_start:
//!                                                ready on start, not ready on destroy
//!   aster_health_set_ready / _set_healthy / _check_set / _addr
//!   aster_health_stop(rt, h)                     port freed on return
//! ```
//!
//! A listener holds a reference to the observability pipeline (it starts one
//! from the environment if none is running) and makes sure it has the
//! Prometheus pull reader, so `/metrics` and `aster_metrics_prometheus`
//! always have something to serve. `/readyz` is ready when the state is
//! healthy, the ready flag is set, every named check passes, and (once a log
//! bridge is open) no audit record was dropped in the last 30 s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aster::ops::{HealthConfig, HealthServer, HealthState};

use crate::{
    alloc_string, aster_bytes_t, aster_node_t, aster_runtime_t, aster_status_t, load_runtime,
    set_last_error, HandleRegistry,
};

/// A running ops listener (`aster_health_start`).
pub type aster_health_t = u64;

/// Where `aster_health_start` listens. 24 bytes (LP64).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct aster_health_config_t {
    /// `sizeof(aster_health_config_t)`.
    pub struct_size: u32,
    /// TCP port; 0 = an ephemeral port (read it with `aster_health_addr`).
    pub port: u16,
    pub _p: u16,
    /// Bind host (UTF-8); empty = `127.0.0.1`. Pods need `0.0.0.0`.
    pub host: aster_bytes_t,
}

pub(crate) struct HealthEntry {
    server: HealthServer,
    rt: tokio::runtime::Handle,
}

pub(crate) static HEALTHS: once_cell::sync::Lazy<HandleRegistry<HealthEntry>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

/// The health config of each node `aster_node_start` started, from its
/// `AsterConfig` (`ASTER_HEALTH_*`, `[health]`); `None` = disabled there.
static NODE_CONFIGS: Mutex<Option<HashMap<u64, Option<HealthConfig>>>> = Mutex::new(None);

pub(crate) fn remember_node_config(node: aster_node_t, cfg: Option<HealthConfig>) {
    NODE_CONFIGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(node, cfg);
}

pub(crate) fn forget_node_config(node: aster_node_t) {
    if let Some(m) = NODE_CONFIGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        m.remove(&node);
    }
}

fn node_config(node: aster_node_t) -> Option<HealthConfig> {
    NODE_CONFIGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .get(&node)
        .cloned()
        .flatten()
}

fn fail(s: aster_status_t, msg: impl ToString) -> i32 {
    set_last_error(msg);
    s as i32
}

fn invalid(msg: impl ToString) -> i32 {
    fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, msg)
}

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;

fn blocking_on_runtime(what: &str) -> Option<i32> {
    tokio::runtime::Handle::try_current().is_ok().then(|| {
        fail(
            aster_status_t::ASTER_STATUS_UNSUPPORTED,
            format!("{what} blocks; call it off the tokio runtime threads"),
        )
    })
}

fn state_of(h: aster_health_t) -> Result<HealthState, i32> {
    HEALTHS
        .get(h)
        .map(|e| e.server.state())
        .ok_or(aster_status_t::ASTER_STATUS_NOT_FOUND as i32)
}

/// Start an ops listener for `node` (a node from `aster_node_start`). `cfg`
/// NULL = the node's `AsterConfig` (`ASTER_HEALTH_PORT` / `_HOST` /
/// `_ENABLED`, `[health]`); `INVALID_ARGUMENT` when that config has health
/// disabled. Joins the running observability pipeline, or starts one from
/// the environment, with the Prometheus pull reader. The state starts healthy
/// and not ready: attach it to a server (`aster_server_attach_health`) or set
/// it with `aster_health_set_ready`. Blocks until bound; refused on a tokio
/// runtime thread (`UNSUPPORTED`).
///
/// # Safety
/// `cfg` is NULL or points to a valid `aster_health_config_t` whose `host`
/// is readable; `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_health_start(
    rt: aster_runtime_t,
    node: aster_node_t,
    cfg: *const aster_health_config_t,
    out: *mut aster_health_t,
) -> i32 {
    if out.is_null() {
        return invalid("out is NULL");
    }
    if let Some(e) = blocking_on_runtime("aster_health_start") {
        return e;
    }
    let bridge = match load_runtime(rt) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let Some(core) = bridge.nodes.get(node) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    // SAFETY: caller contract.
    let config = match unsafe { cfg.as_ref() } {
        None => match node_config(node) {
            Some(c) => c,
            None => {
                return invalid(
                    "health is disabled in the node's config (ASTER_HEALTH_PORT / [health]); \
                     pass an aster_health_config_t",
                )
            }
        },
        Some(c) => {
            if c.struct_size as usize != std::mem::size_of::<aster_health_config_t>() {
                return invalid(format!(
                    "aster_health_config_t.struct_size {} != {}",
                    c.struct_size,
                    std::mem::size_of::<aster_health_config_t>()
                ));
            }
            let host = if c.host.ptr.is_null() || c.host.len == 0 {
                ""
            } else {
                // SAFETY: caller contract.
                match std::str::from_utf8(unsafe {
                    std::slice::from_raw_parts(c.host.ptr, c.host.len)
                }) {
                    Ok(h) => h,
                    Err(_) => return invalid("host is not UTF-8"),
                }
            };
            HealthConfig::new(host, c.port)
        }
    };
    let node = aster::Node::from_core_node((*core).clone());
    let handle = bridge.runtime.handle().clone();
    match bridge.runtime.block_on(HealthServer::start(&node, config)) {
        Ok(server) => {
            let id = HEALTHS.insert(HealthEntry { server, rt: handle });
            // SAFETY: checked non-null.
            unsafe { std::ptr::write(out, id) };
            OK
        }
        Err(e) => fail(aster_status_t::ASTER_STATUS_CONNECTION_REFUSED, e),
    }
}

/// Set the ready flag (`/readyz`). A server the listener is attached to sets
/// it on a successful `aster_server_start` and clears it first in
/// `aster_server_destroy`; this stays available for the binding's own gating.
#[no_mangle]
pub extern "C" fn aster_health_set_ready(h: aster_health_t, ready: bool) -> i32 {
    match state_of(h) {
        Ok(s) => {
            s.set_ready(ready);
            OK
        }
        Err(e) => e,
    }
}

/// Set liveness (`/healthz`; unhealthy also fails `/readyz`).
#[no_mangle]
pub extern "C" fn aster_health_set_healthy(h: aster_health_t, healthy: bool) -> i32 {
    match state_of(h) {
        Ok(s) => {
            s.set_healthy(healthy);
            OK
        }
        Err(e) => e,
    }
}

/// Set the named readiness check `name` (created failing on first use; every
/// check must pass for `/readyz`, which lists them under `checks`).
///
/// # Safety
/// `name` is readable for the call.
#[no_mangle]
pub unsafe extern "C" fn aster_health_check_set(
    h: aster_health_t,
    name: aster_bytes_t,
    ok: bool,
) -> i32 {
    let s = match state_of(h) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let raw = if name.ptr.is_null() || name.len == 0 {
        &[][..]
    } else {
        // SAFETY: caller contract.
        unsafe { std::slice::from_raw_parts(name.ptr, name.len) }
    };
    match std::str::from_utf8(raw) {
        Ok(n) if !n.is_empty() => {
            s.check(n).set(ok);
            OK
        }
        Ok(_) => invalid("check name is empty"),
        Err(_) => invalid("check name is not UTF-8"),
    }
}

/// The bound address as `"ip:port"` (the real port after port 0).
/// `*out` is library-owned; free with `aster_string_release(out->ptr,
/// out->len)`.
///
/// # Safety
/// `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn aster_health_addr(h: aster_health_t, out: *mut aster_bytes_t) -> i32 {
    if out.is_null() {
        return invalid("out is NULL");
    }
    let Some(e) = HEALTHS.get(h) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    // SAFETY: checked non-null.
    unsafe { std::ptr::write(out, alloc_string(e.server.addr().to_string())) };
    OK
}

/// Stop the listener and free the handle; the port is free on return.
/// Releases the listener's observability reference. Refused on a tokio
/// runtime thread (`UNSUPPORTED`).
#[no_mangle]
pub extern "C" fn aster_health_stop(rt: aster_runtime_t, h: aster_health_t) -> i32 {
    if let Some(e) = blocking_on_runtime("aster_health_stop") {
        return e;
    }
    if let Err(s) = load_runtime(rt) {
        return s as i32;
    }
    let Some(entry) = HEALTHS.remove(h) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    entry.server.set_ready(false);
    entry.rt.block_on(entry.server.shutdown());
    // The last reference drops here, off the runtime: releasing the pipeline
    // reference may shut the pipeline down (§9.1: not on a tokio thread).
    drop(entry);
    OK
}

/// The state behind `h`, for `aster_server_attach_health`.
pub(crate) fn attachable(h: aster_health_t) -> Option<Arc<HealthEntry>> {
    HEALTHS.get(h)
}

impl HealthEntry {
    pub(crate) fn state(&self) -> HealthState {
        self.server.state()
    }
}
