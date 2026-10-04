//! Client call handles: release and buffer leases.
//!
//! Since ABI 4.0 the only client calls are the payload-level ones from
//! `aster_rpc_open` (`rpc.rs`); the framed `aster_call_{acquire,
//! acquire_streaming,send_frame,recv_frame,unary}` family was removed with
//! the reactor. What remains here is shared by `rpc.rs`:
//!
//! - `aster_call_t`, the call handle type;
//! - `aster_call_release` / `aster_call_discard`, which free an
//!   `aster_rpc_open` call (an unfinished one is cancelled);
//! - `aster_call_buffer_release` and the buffer registry behind the leases
//!   that `aster_rpc_unary` and `aster_call_recv_payload` hand out.

use crate::{aster_buffer_t, aster_runtime_t, aster_status_t, BufferRegistry};

pub type aster_call_t = u64;

/// Buffer registry for payload / trailer leases returned by `aster_rpc_unary`
/// and `aster_call_recv_payload`.
pub(crate) static CALL_BUFFERS: once_cell::sync::Lazy<BufferRegistry> =
    once_cell::sync::Lazy::new(BufferRegistry::new);

/// Free a call from `aster_rpc_open`. An unfinished call is cancelled
/// (`FLAG_CANCEL` + reset). Returns `NOT_FOUND` for an unknown or already
/// freed handle. Must not race another operation on the same call.
#[no_mangle]
pub unsafe extern "C" fn aster_call_release(runtime: aster_runtime_t, call: aster_call_t) -> i32 {
    let _ = runtime;
    if crate::rpc::release(call) {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

/// Identical to `aster_call_release` (kept for bindings that distinguish the
/// error path; since 4.0 there is no pooled stream to poison).
#[no_mangle]
pub unsafe extern "C" fn aster_call_discard(runtime: aster_runtime_t, call: aster_call_t) -> i32 {
    // SAFETY: same contract as `aster_call_release`.
    unsafe { aster_call_release(runtime, call) }
}

/// Release a buffer lease from `aster_rpc_unary` / `aster_call_recv_payload`.
/// Each non-zero buffer id must be released exactly once; 0 is a no-op.
#[no_mangle]
pub unsafe extern "C" fn aster_call_buffer_release(
    runtime: aster_runtime_t,
    buffer: aster_buffer_t,
) -> i32 {
    let _ = runtime;
    CALL_BUFFERS.release(buffer);
    aster_status_t::ASTER_STATUS_OK as i32
}
