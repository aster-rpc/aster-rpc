//! Server drain (ABI 7.2): `aster_server_drain` takes a dispatch server
//! (`aster_server_start`) out of rotation gracefully. Rust
//! `aster::rpc::ServerHandle::drain`; semantics in `aster::rpc::drain`.
//!
//! ```text
//!   aster_server_drain(rt, server, grace_ms, &report)   blocks until drained
//!   aster_server_destroy(rt, server)                    then as before
//! ```

use std::ptr;
use std::time::Duration;

use crate::server::{aster_server_t, SERVERS};
use crate::{aster_runtime_t, aster_status_t, load_runtime, set_last_error};

/// The QUIC application close code a draining server closes connections
/// with (new ones at accept, idle ones while draining, the rest at the
/// deadline). A client seeing it should reconnect to another replica.
pub const ASTER_DRAIN_CLOSE_CODE: u64 = 503;

/// What `aster_server_drain` did. 64 bytes. Calls count those admitted to a
/// handler, not those refused at routing.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct aster_drain_report_t {
    /// Calls in flight when the drain started.
    pub calls_in_flight: u64,
    /// Calls that finished before the deadline (those in flight at the
    /// start, and calls on existing sessions admitted during the drain).
    pub calls_completed: u64,
    /// Calls still running at the deadline, cancelled by closing their
    /// connection.
    pub calls_cancelled: u64,
    /// New calls answered `UNAVAILABLE` (detail `aster.draining`).
    pub calls_refused: u64,
    /// Connections the drain closed with `ASTER_DRAIN_CLOSE_CODE`.
    pub connections_closed: u64,
    /// New connections refused at accept.
    pub connections_refused: u64,
    /// How long the drain took, in milliseconds.
    pub elapsed_ms: u64,
    /// 1 when the deadline passed with work outstanding, else 0.
    pub deadline_reached: u32,
    pub reserved: u32,
}

impl From<aster::rpc::DrainReport> for aster_drain_report_t {
    fn from(r: aster::rpc::DrainReport) -> Self {
        Self {
            calls_in_flight: r.calls_in_flight,
            calls_completed: r.calls_completed,
            calls_cancelled: r.calls_cancelled,
            calls_refused: r.calls_refused,
            connections_closed: r.connections_closed,
            connections_refused: r.connections_refused,
            elapsed_ms: u64::try_from(r.elapsed.as_millis()).unwrap_or(u64::MAX),
            deadline_reached: u32::from(r.deadline_reached),
            reserved: 0,
        }
    }
}

/// Drain `server` (ABI 7.2): set an attached ops listener not ready, refuse
/// new connections (closed after the handshake with
/// `ASTER_DRAIN_CLOSE_CODE`) and new calls (`UNAVAILABLE`, detail
/// `aster.draining` = `true`; calls on a session that already exists still
/// run), close each connection once it has no call in flight and no live
/// session, and after `grace_ms` close every remaining connection, which
/// cancels the calls still running (the binding sees `CANCELLED{PEER}`), then
/// wait up to 2 s for them to end. Blocks until done and fills `out_report`
/// (may be NULL).
///
/// The server keeps refusing afterwards; follow with `aster_server_destroy`
/// and the node's close. A second call while a drain runs (from another
/// thread) moves its deadline earlier when its own `grace_ms` ends sooner --
/// `0` cuts the drain short -- and returns the same report; a call after the
/// drain ended returns that report again. `NOT_FOUND` for an unknown server
/// or one of another runtime; `UNSUPPORTED` on a tokio runtime thread.
///
/// # Safety
/// `out_report` is NULL or points to writable `aster_drain_report_t`.
#[no_mangle]
pub unsafe extern "C" fn aster_server_drain(
    runtime: aster_runtime_t,
    server: aster_server_t,
    grace_ms: u32,
    out_report: *mut aster_drain_report_t,
) -> i32 {
    if let Some(error) = crate::http_server::blocking_on_runtime("aster_server_drain") {
        return error;
    }
    let bridge = match load_runtime(runtime) {
        Ok(bridge) => bridge,
        Err(s) => return s as i32,
    };
    let Some(state) = SERVERS.get(server) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    if state.runtime_owner != runtime {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    }
    let Some(drainer) = state
        .handle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|handle| handle.drainer())
    else {
        set_last_error("aster_server_drain: the server is stopping");
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    };
    // Probes stop routing here before anything is refused.
    if let Some(health) = &state.health {
        health.set_ready(false);
    }
    drop(state);
    let report = bridge
        .runtime
        .block_on(drainer.drain(Duration::from_millis(u64::from(grace_ms))));
    if !out_report.is_null() {
        // SAFETY: checked non-null; the caller passes writable storage.
        unsafe { ptr::write(out_report, report.into()) };
    }
    aster_status_t::ASTER_STATUS_OK as i32
}
