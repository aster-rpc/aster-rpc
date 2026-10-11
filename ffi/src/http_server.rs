//! Owned authenticated or explicitly public HTTP/WebTransport listeners.
//!
//! Authentication uses typed kind-80 upcalls, independently of the server's
//! Iroh policy. Listener starts report bound readiness; stop joins owned
//! connection/call work. No callback result upgrades transport provenance.

use std::net::SocketAddr;
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::binding_support::upcall::auth::{ForeignAuthenticator, ForeignAuthenticatorOptions};
use aster_transport_salvo::{HttpConfig, OwnedListener, TlsMaterial};

use crate::server::{aster_server_t, SERVERS};
use crate::upcall::{aster_upcalls_t, upcalls_for};
use crate::{
    aster_buffer_t, aster_bytes_t, aster_runtime_t, aster_status_t, load_runtime, set_last_error,
    BridgeRuntime,
};

/// An owned TCP/optional QUIC listener.
pub type aster_http_listener_t = u64;

/// Enable WebTransport on the TCP listener's exact bound address and port.
pub const ASTER_HTTP_SERVER_WEBTRANSPORT: u32 = 1;

/// Inputs are copied during start. Only operator-supplied PEM is accepted.
/// Zero limits select 256 pending auth callbacks and 65536 payload bytes.
#[repr(C)]
pub struct aster_http_server_config_t {
    pub struct_size: u32,
    pub flags: u32,
    pub bind_addr: aster_bytes_t,
    pub cert_pem: aster_bytes_t,
    pub key_pem: aster_bytes_t,
    pub max_pending_auth: u32,
    pub max_auth_payload_bytes: u32,
}

pub(crate) struct HttpListenerState {
    parent_server: aster_server_t,
    tcp_addr: SocketAddr,
    quic_addr: Option<SocketAddr>,
    listener: Mutex<Option<OwnedListener>>,
}

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;

fn fail(status: aster_status_t, message: impl ToString) -> i32 {
    set_last_error(message);
    status as i32
}

pub(crate) fn blocking_on_runtime(what: &str) -> Option<i32> {
    tokio::runtime::Handle::try_current().is_ok().then(|| {
        fail(
            aster_status_t::ASTER_STATUS_UNSUPPORTED,
            format!("{what} blocks; call it off the tokio runtime threads"),
        )
    })
}

unsafe fn copy_bytes(value: &aster_bytes_t) -> Result<Vec<u8>, i32> {
    if value.ptr.is_null() {
        if value.len != 0 {
            return Err(fail(
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
                "NULL input with nonzero length",
            ));
        }
        return Ok(Vec::new());
    }
    // SAFETY: callers provide readable byte spans for the duration of start.
    Ok(unsafe { std::slice::from_raw_parts(value.ptr, value.len) }.to_vec())
}

/// Bind an authenticated listener over this server's registered services.
/// Both sockets are ready on success. The server and upcalls must belong to
/// runtime. Unknown flags, malformed config/PEM and binding failures are
/// INVALID_ARGUMENT. A concurrently closing owner returns ALREADY_CLOSED.
/// This blocks and is UNSUPPORTED on a tokio runtime thread.
///
/// # Safety
/// config and out_listener are valid; config's byte spans are readable.
#[no_mangle]
pub unsafe extern "C" fn aster_server_http_start(
    runtime: aster_runtime_t,
    server: aster_server_t,
    upcalls: aster_upcalls_t,
    config: *const aster_http_server_config_t,
    out_listener: *mut aster_http_listener_t,
) -> i32 {
    unsafe { start(runtime, server, Some(upcalls), config, out_listener) }
}

/// Explicitly bind a public listener without an authentication callback.
/// Public calls receive no identity attributes and cannot use session-scoped
/// services. Method access rules and Iroh-only requirements still apply.
/// Both auth limits must be zero. Ownership and errors match authenticated start.
///
/// # Safety
/// config and out_listener are valid; config's byte spans are readable.
#[no_mangle]
pub unsafe extern "C" fn aster_server_http_start_public(
    runtime: aster_runtime_t,
    server: aster_server_t,
    config: *const aster_http_server_config_t,
    out_listener: *mut aster_http_listener_t,
) -> i32 {
    unsafe { start(runtime, server, None, config, out_listener) }
}

unsafe fn start(
    runtime: aster_runtime_t,
    server: aster_server_t,
    upcalls: Option<aster_upcalls_t>,
    config: *const aster_http_server_config_t,
    out_listener: *mut aster_http_listener_t,
) -> i32 {
    if let Some(error) = blocking_on_runtime("aster_server_http_start") {
        return error;
    }
    if config.is_null() || out_listener.is_null() {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "NULL config or listener output",
        );
    }
    unsafe {
        ptr::write(out_listener, 0);
    }
    let config = unsafe { &*config };
    if config.struct_size as usize != std::mem::size_of::<aster_http_server_config_t>()
        || config.flags & !ASTER_HTTP_SERVER_WEBTRANSPORT != 0
    {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "unsupported HTTP server config size or flags",
        );
    }
    let bridge = match load_runtime(runtime) {
        Ok(bridge) => bridge,
        Err(s) => return s as i32,
    };
    let Some(owner) = SERVERS.get(server).filter(|s| s.runtime_owner == runtime) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    if owner
        .http_children
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .closed
        || bridge.closing.load(Ordering::Acquire)
    {
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    }
    let upcalls = match upcalls {
        Some(handle) => match upcalls_for(handle) {
            Ok(table) if table.runtime_owner == runtime => {
                if table.upcalls.is_closed() {
                    return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
                }
                Some(table)
            }
            _ => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
        },
        None => {
            if config.max_pending_auth != 0 || config.max_auth_payload_bytes != 0 {
                return fail(
                    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
                    "public listeners cannot configure authentication limits",
                );
            }
            None
        }
    };
    let (bind, cert, key) = match unsafe {
        (
            copy_bytes(&config.bind_addr),
            copy_bytes(&config.cert_pem),
            copy_bytes(&config.key_pem),
        )
    } {
        (Ok(bind), Ok(cert), Ok(key)) => (bind, cert, key),
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => return error,
    };
    let bind = match String::from_utf8(bind) {
        Ok(bind) if !bind.is_empty() => bind,
        _ => {
            return fail(
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
                "bind address must be nonempty UTF-8",
            )
        }
    };
    let auth = if let Some(upcalls) = upcalls {
        let defaults = ForeignAuthenticatorOptions::default();
        Some(
            match ForeignAuthenticator::with_options(
                upcalls.upcalls.clone(),
                ForeignAuthenticatorOptions {
                    max_pending: if config.max_pending_auth == 0 {
                        defaults.max_pending
                    } else {
                        config.max_pending_auth as usize
                    },
                    max_payload_bytes: if config.max_auth_payload_bytes == 0 {
                        defaults.max_payload_bytes
                    } else {
                        config.max_auth_payload_bytes as usize
                    },
                },
            ) {
                Ok(auth) => Arc::new(auth),
                Err(error) => return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, error),
            },
        )
    } else {
        None
    };
    let http = HttpConfig::new(bind, TlsMaterial::pem(cert, key))
        .webtransport(config.flags & ASTER_HTTP_SERVER_WEBTRANSPORT != 0);
    let listener = match bridge.runtime.block_on(async {
        match auth {
            Some(auth) => {
                http.start_authenticated(owner.dispatcher.clone(), auth)
                    .await
            }
            None => http.start_public(owner.dispatcher.clone()).await,
        }
    }) {
        Ok(listener) => listener,
        Err(error) => return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, error),
    };
    let state = HttpListenerState {
        parent_server: server,
        tcp_addr: listener.tcp_addr(),
        quic_addr: listener.quic_addr(),
        listener: Mutex::new(Some(listener)),
    };
    let mut children = owner
        .http_children
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if children.closed || bridge.closing.load(Ordering::Acquire) {
        drop(children);
        let mut listener = state
            .listener
            .into_inner()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap();
        let _ = bridge.runtime.block_on(listener.stop(Duration::ZERO));
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    }
    let id = bridge.http_listeners.insert(state);
    children.handles.push(id);
    drop(children);
    // Runtime close clears its registry independently of the server's child
    // lock. A close that won publication cannot leave a late owner behind.
    if bridge.closing.load(Ordering::Acquire) {
        let _ = stop_child(&bridge, id, Duration::ZERO);
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    }
    unsafe {
        ptr::write(out_listener, id);
    }
    OK
}

unsafe fn address(
    runtime: aster_runtime_t,
    listener: aster_http_listener_t,
    quic: bool,
    out_buffer: *mut aster_buffer_t,
    out_ptr: *mut *const u8,
    out_len: *mut u32,
) -> i32 {
    if out_buffer.is_null() || out_ptr.is_null() || out_len.is_null() {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "NULL address output",
        );
    }
    unsafe {
        ptr::write(out_buffer, 0);
        ptr::write(out_ptr, ptr::null());
        ptr::write(out_len, 0);
    }
    let bridge = match load_runtime(runtime) {
        Ok(bridge) => bridge,
        Err(s) => return s as i32,
    };
    let Some(state) = bridge.http_listeners.get(listener) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let addr = if quic {
        state.quic_addr
    } else {
        Some(state.tcp_addr)
    };
    let Some(addr) = addr else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let (buffer, bytes) = bridge.buffers.insert(addr.to_string().into_bytes());
    unsafe {
        ptr::write(out_buffer, buffer);
        ptr::write(out_ptr, bytes.as_ptr());
        ptr::write(out_len, bytes.len() as u32);
    }
    OK
}

/// Copy the bound TCP address into a runtime-owned buffer. Release out_buffer
/// with aster_buffer_release. All output pointers must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_http_listener_tcp_addr(
    runtime: aster_runtime_t,
    listener: aster_http_listener_t,
    out_buffer: *mut aster_buffer_t,
    out_ptr: *mut *const u8,
    out_len: *mut u32,
) -> i32 {
    unsafe { address(runtime, listener, false, out_buffer, out_ptr, out_len) }
}

/// Copy the bound QUIC address; NOT_FOUND if WebTransport is disabled.
/// Buffer ownership and safety requirements match aster_http_listener_tcp_addr.
#[no_mangle]
pub unsafe extern "C" fn aster_http_listener_quic_addr(
    runtime: aster_runtime_t,
    listener: aster_http_listener_t,
    out_buffer: *mut aster_buffer_t,
    out_ptr: *mut *const u8,
    out_len: *mut u32,
) -> i32 {
    unsafe { address(runtime, listener, true, out_buffer, out_ptr, out_len) }
}

pub(crate) fn stop_child(
    bridge: &BridgeRuntime,
    listener: u64,
    grace: Duration,
) -> Result<(), String> {
    let Some(state) = bridge.http_listeners.get(listener) else {
        return Ok(());
    };
    let mut owned = state.listener.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(listener) = owned.as_mut() {
        bridge.runtime.block_on(listener.stop(grace))?;
    }
    owned.take();
    bridge.http_listeners.remove(listener);
    if let Some(parent) = SERVERS.get(state.parent_server) {
        parent
            .http_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .handles
            .retain(|id| *id != listener);
    }
    Ok(())
}

/// Stop accepting and join calls/connections, allowing grace_ms to drain before
/// cancellation, then free the owner. TCP/UDP ports are released on success.
/// Repeated stops return NOT_FOUND. Refused on tokio threads before mutation.
/// Runtime close drops owners into forced cleanup; close_and_wait joins the
/// runtime teardown. Server destroy joins children with zero grace.
#[no_mangle]
pub extern "C" fn aster_http_listener_stop(
    runtime: aster_runtime_t,
    listener: aster_http_listener_t,
    grace_ms: u32,
) -> i32 {
    if let Some(error) = blocking_on_runtime("aster_http_listener_stop") {
        return error;
    }
    let bridge = match load_runtime(runtime) {
        Ok(bridge) => bridge,
        Err(s) => return s as i32,
    };
    if bridge.http_listeners.get(listener).is_none() {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    }
    match stop_child(&bridge, listener, Duration::from_millis(grace_ms as u64)) {
        Ok(()) => OK,
        Err(error) => fail(aster_status_t::ASTER_STATUS_INTERNAL, error),
    }
}
