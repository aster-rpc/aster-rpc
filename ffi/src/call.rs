//! Client-side per-call FFI surface (the unified `aster_call_*` family).
//!
//! Pairs with the per-connection `MultiplexedStreamPool` from
//! `core::pool` and `CoreConnection::pool()`. Per-call ops here are
//! direction-agnostic in shape — server-side migration in a future
//! step will reuse the same family from the inbound dispatch path.
//!
//! See `ffi_spec/Aster-multiplexed-streams.md` §8 for the design.
//!
//! ## Lifecycle
//!
//! ```text
//!   aster_call_acquire(conn, session_id) → call handle
//!         (lazily opens or reuses a multiplexed bi-stream from the pool)
//!     │
//!     ├── aster_call_send_frame(call, framed_bytes)*    # request frames
//!     ├── aster_call_recv_frame(call, timeout) → frame*  # response frames
//!     │     (reads payload + flags; binding inspects FLAG_TRAILER for end)
//!     │
//!     ├── aster_call_buffer_release(buffer_id)*  # release recv buffers
//!     │
//!     ├── aster_call_release(call)   # success path: stream returns to pool
//!     └── aster_call_discard(call)   # error path: stream poisoned, slot freed
//! ```
//!
//! ## Concurrency contract
//!
//! - `send_frame` and `recv_frame` on the same call may run from
//!   different threads concurrently (request/response for bidi). They
//!   serialize internally on the underlying `CoreSendStream` /
//!   `CoreRecvStream` mutexes.
//! - `release` and `discard` MUST NOT be called while a `send_frame`
//!   or `recv_frame` is in flight on the same call. Same contract as
//!   the reactor surface.

use std::ptr;
use std::sync::Mutex;

use aster_transport_core::framing::{FLAG_ROW_SCHEMA, FLAG_TRAILER, MAX_FRAME_SIZE};
use aster_transport_core::pool::{AcquireError, PoolKey};
use aster_transport_core::{CoreRecvStream, CoreSendStream, MultiplexedStreamHandle};

use crate::{
    iroh_buffer_t, iroh_connection_t, iroh_runtime_t, iroh_status_t, load_runtime, BufferRegistry,
    HandleRegistry,
};

// ============================================================================
// Types
// ============================================================================

pub type aster_call_t = u64;

/// Result codes returned by `aster_call_recv_frame`. Mirrors the
/// `aster_reactor_recv_frame` triplet for consistency across surfaces.
pub const ASTER_CALL_RECV_OK: i32 = 0;
pub const ASTER_CALL_RECV_END_OF_STREAM: i32 = 1;
pub const ASTER_CALL_RECV_TIMEOUT: i32 = 2;

/// Acquire-error subcodes returned by `aster_call_acquire` as negative
/// values. Bindings can map these to typed exceptions per spec §5.
pub const ASTER_CALL_ERR_POOL_FULL: i32 = -10;
pub const ASTER_CALL_ERR_QUIC_LIMIT_REACHED: i32 = -11;
pub const ASTER_CALL_ERR_PEER_STREAM_LIMIT_TOO_LOW: i32 = -12;
pub const ASTER_CALL_ERR_STREAM_OPEN_FAILED: i32 = -13;
pub const ASTER_CALL_ERR_POOL_CLOSED: i32 = -14;

pub(crate) struct CallState {
    /// Pool handle for the multiplexed stream this call is using.
    /// Wrapped in `Mutex<Option<...>>` so that release/discard can
    /// take ownership and drive the RAII drop semantics.
    pool_handle: Mutex<Option<MultiplexedStreamHandle>>,
}

// ============================================================================
// Registries
// ============================================================================

pub(crate) static CALLS: once_cell::sync::Lazy<HandleRegistry<CallState>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

/// Buffer registry for payloads returned by `aster_call_recv_frame`.
/// Distinct from the reactor's per-instance registry so calls and
/// reactor surfaces don't share buffer-id namespaces.
pub(crate) static CALL_BUFFERS: once_cell::sync::Lazy<BufferRegistry> =
    once_cell::sync::Lazy::new(BufferRegistry::new);

// ============================================================================
// Helpers
// ============================================================================

/// Borrow the `CoreSendStream` clone for the current pool handle.
/// Returns `None` if the call has been released/discarded.
fn clone_send(state: &CallState) -> Option<CoreSendStream> {
    let guard = state.pool_handle.lock().unwrap();
    guard.as_ref().map(|h| h.get().0.clone())
}

/// Borrow the `CoreRecvStream` clone for the current pool handle.
/// Returns `None` if the call has been released/discarded.
fn clone_recv(state: &CallState) -> Option<CoreRecvStream> {
    let guard = state.pool_handle.lock().unwrap();
    guard.as_ref().map(|h| h.get().1.clone())
}

/// Read one wire frame: 4-byte LE length prefix + flags byte + payload.
async fn read_one_frame(recv: &CoreRecvStream) -> Result<(Vec<u8>, u8), RecvErr> {
    let mut len_bytes = [0u8; 4];
    if recv.read_exact_into(&mut len_bytes).await.is_err() {
        return Err(RecvErr::Eof);
    }
    let frame_body_len = u32::from_le_bytes(len_bytes) as usize;
    if frame_body_len == 0 || frame_body_len > MAX_FRAME_SIZE as usize {
        return Err(RecvErr::Invalid);
    }
    let mut flags_buf = [0u8; 1];
    if recv.read_exact_into(&mut flags_buf).await.is_err() {
        return Err(RecvErr::Eof);
    }
    let flags = flags_buf[0];
    let payload_len = frame_body_len - 1;
    let mut payload = vec![0u8; payload_len];
    if recv.read_exact_into(&mut payload).await.is_err() {
        return Err(RecvErr::Eof);
    }
    Ok((payload, flags))
}

enum RecvErr {
    Eof,
    Timeout,
    Invalid,
}

/// Convert a `u32` session id (per spec §6: 0 = SHARED, non-zero =
/// per-session) into the pool's `PoolKey` representation.
fn pool_key_for(session_id: u32) -> PoolKey {
    if session_id == 0 {
        None
    } else {
        Some(session_id.to_le_bytes().to_vec())
    }
}

fn map_acquire_error(err: AcquireError) -> i32 {
    match err {
        AcquireError::PoolFull => ASTER_CALL_ERR_POOL_FULL,
        AcquireError::QuicLimitReached => ASTER_CALL_ERR_QUIC_LIMIT_REACHED,
        AcquireError::Timeout => ASTER_CALL_ERR_POOL_FULL,
        AcquireError::PeerStreamLimitTooLow { .. } => ASTER_CALL_ERR_PEER_STREAM_LIMIT_TOO_LOW,
        AcquireError::StreamOpenFailed(_) => ASTER_CALL_ERR_STREAM_OPEN_FAILED,
        AcquireError::Closed => ASTER_CALL_ERR_POOL_CLOSED,
        // AcquireError is non_exhaustive — future variants fall back
        // to a generic stream-open-failed mapping until the binding
        // contract is extended.
        _ => ASTER_CALL_ERR_STREAM_OPEN_FAILED,
    }
}

// ============================================================================
// C API
// ============================================================================

/// Acquire a call handle from the connection's per-connection
/// multiplexed-stream pool. `session_id == 0` selects the SHARED
/// pool; non-zero selects the session pool keyed by that id.
///
/// Blocks (on the FFI thread, via `block_on`) up to the pool's
/// configured `stream_acquire_timeout`. On success, writes a non-zero
/// call handle to `*out_call`. On failure, returns one of the
/// `ASTER_CALL_ERR_*` codes (negative) or an `iroh_status_t` code.
#[no_mangle]
pub unsafe extern "C" fn aster_call_acquire(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    session_id: u32,
    out_call: *mut aster_call_t,
) -> i32 {
    if out_call.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let key = pool_key_for(session_id);
    let acquired = bridge
        .runtime
        .handle()
        .block_on(async move { conn.acquire_stream(key).await });

    match acquired {
        Ok(handle) => {
            let state = CallState {
                pool_handle: Mutex::new(Some(handle)),
            };
            let id = CALLS.insert(state);
            unsafe { ptr::write(out_call, id) };
            iroh_status_t::IROH_STATUS_OK as i32
        }
        Err(e) => map_acquire_error(e),
    }
}

/// Push a frame on a call's request side.
///
/// `frame_ptr` / `frame_len` point to already-framed bytes (including
/// the 4-byte little-endian length prefix and 1-byte flags). The
/// binding is responsible for framing — this matches
/// `aster_reactor_submit_frame`'s contract.
///
/// Blocks on the FFI thread until the underlying QUIC `write_all`
/// completes.
#[no_mangle]
pub unsafe extern "C" fn aster_call_send_frame(
    runtime: iroh_runtime_t,
    call: aster_call_t,
    frame_ptr: *const u8,
    frame_len: u32,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let state = match CALLS.get(call) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let send = match clone_send(&state) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let frame = if frame_ptr.is_null() || frame_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(frame_ptr, frame_len as usize).to_vec() }
    };

    if frame.is_empty() {
        return iroh_status_t::IROH_STATUS_OK as i32;
    }

    let result = bridge
        .runtime
        .handle()
        .block_on(async move { send.write_all(frame).await });

    if result.is_err() {
        return iroh_status_t::IROH_STATUS_INTERNAL as i32;
    }
    iroh_status_t::IROH_STATUS_OK as i32
}

/// Pull the next response frame on a call. Blocks up to `timeout_ms`
/// waiting for the next frame on the recv side.
///
/// On `ASTER_CALL_RECV_OK` (return value 0), writes:
/// - `*out_payload_ptr` — pointer to payload bytes (owned by
///   `CALL_BUFFERS`; release via `aster_call_buffer_release`).
/// - `*out_payload_len` — payload length in bytes.
/// - `*out_flags` — frame flags byte. Bindings inspect `FLAG_TRAILER`
///   to detect end-of-call.
/// - `*out_buffer_id` — buffer registry id for releasing the payload.
///
/// On `ASTER_CALL_RECV_TIMEOUT` (return value 2), no out parameters
/// are touched and the call remains valid for further recv attempts.
///
/// On `ASTER_CALL_RECV_END_OF_STREAM` (return value 1), the underlying
/// QUIC recv side has closed; further recvs will keep returning EOF.
/// The binding should release/discard the call.
///
/// Negative return values are `iroh_status_t` error codes.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_call_recv_frame(
    runtime: iroh_runtime_t,
    call: aster_call_t,
    timeout_ms: u32,
    out_payload_ptr: *mut *const u8,
    out_payload_len: *mut u32,
    out_flags: *mut u8,
    out_buffer_id: *mut iroh_buffer_t,
) -> i32 {
    if out_payload_ptr.is_null()
        || out_payload_len.is_null()
        || out_flags.is_null()
        || out_buffer_id.is_null()
    {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let state = match CALLS.get(call) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let recv = match clone_recv(&state) {
        Some(r) => r,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let result = bridge.runtime.handle().block_on(async move {
        if timeout_ms == 0 {
            read_one_frame(&recv).await
        } else {
            let timeout = std::time::Duration::from_millis(timeout_ms as u64);
            tokio::select! {
                r = read_one_frame(&recv) => r,
                _ = tokio::time::sleep(timeout) => Err(RecvErr::Timeout),
            }
        }
    });

    match result {
        Ok((payload, flags)) => {
            let (buf_id, arc) = CALL_BUFFERS.insert(payload);
            unsafe {
                ptr::write(out_payload_ptr, arc.as_ptr());
                ptr::write(out_payload_len, arc.len() as u32);
                ptr::write(out_flags, flags);
                ptr::write(out_buffer_id, buf_id);
            }
            ASTER_CALL_RECV_OK
        }
        Err(RecvErr::Eof) => ASTER_CALL_RECV_END_OF_STREAM,
        Err(RecvErr::Timeout) => ASTER_CALL_RECV_TIMEOUT,
        Err(RecvErr::Invalid) => iroh_status_t::IROH_STATUS_INTERNAL as i32,
    }
}

/// Release a call on the success path. The underlying multiplexed
/// stream returns to the connection's pool (LIFO) for reuse by the
/// next acquire.
///
/// MUST be called only when no concurrent `send_frame` / `recv_frame`
/// is in flight on this call. Calling release with leftover unread
/// bytes on the recv side would corrupt the next call that reuses
/// the stream — the binding MUST drain to the trailer first.
#[no_mangle]
pub unsafe extern "C" fn aster_call_release(runtime: iroh_runtime_t, call: aster_call_t) -> i32 {
    let _ = runtime;
    match CALLS.remove(call) {
        Some(state) => {
            // Drop the handle. RAII returns the stream to the pool
            // unless poisoned.
            let _ = state.pool_handle.lock().unwrap().take();
            iroh_status_t::IROH_STATUS_OK as i32
        }
        None => iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    }
}

/// Discard a call on the error path. The underlying multiplexed
/// stream is dropped (not returned to the pool); the pool slot is
/// freed and any blocked waiter is woken to either reuse a freed
/// slot or surface the same transport error.
///
/// Use this when:
/// - A `send_frame` or `recv_frame` returned an error.
/// - The call was cancelled mid-flight (FLAG_CANCEL).
/// - The recv side returned `ASTER_CALL_RECV_END_OF_STREAM` before
///   the trailer was seen.
#[no_mangle]
pub unsafe extern "C" fn aster_call_discard(runtime: iroh_runtime_t, call: aster_call_t) -> i32 {
    let _ = runtime;
    match CALLS.remove(call) {
        Some(state) => {
            if let Some(handle) = state.pool_handle.lock().unwrap().take() {
                handle.discard();
            }
            iroh_status_t::IROH_STATUS_OK as i32
        }
        None => iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    }
}

/// Release a buffer obtained from `aster_call_recv_frame`. Each
/// buffer id returned by recv MUST be released exactly once.
#[no_mangle]
pub unsafe extern "C" fn aster_call_buffer_release(
    runtime: iroh_runtime_t,
    buffer: iroh_buffer_t,
) -> i32 {
    let _ = runtime;
    CALL_BUFFERS.release(buffer);
    iroh_status_t::IROH_STATUS_OK as i32
}

/// Unary fast-path (spec §8). Collapses
/// `acquire` + `send_frame(header)` + `send_frame(request)` +
/// `recv_frame(response)` + `recv_frame(trailer)` + `release` into a
/// single FFI entry point and a single `block_on`. Bindings whose
/// hot-path is dominated by FFI roundtrips (e.g. Java FFM, where every
/// `block_on` parks a platform thread) see ~5-6× fewer FFI hops per
/// unary call.
///
/// `request_pair_ptr` / `request_pair_len` is a single buffer holding
/// the already-framed StreamHeader frame *concatenated with* the
/// already-framed request frame:
///
/// ```text
///   [4B LE len][1B FLAG_HEADER][header_payload]
///   [4B LE len][1B request_flags][request_payload]
/// ```
///
/// The Rust side writes the whole buffer in one `write_all` so Quinn's
/// flow control sees one logical write.
///
/// On success, populates four sets of out parameters:
/// - `out_response_*` — the FIRST non-row-schema response frame on the
///   stream, or zero-length if the dispatcher only sent a trailer.
/// - `out_trailer_*` — the trailer frame's payload (an encoded
///   `RpcStatus`). Always populated on success; zero-length if the
///   dispatcher sent an empty OK trailer.
///
/// Each populated payload comes with its own buffer id; bindings MUST
/// call `aster_call_buffer_release` for every non-zero buffer id
/// returned. Skipped row-schema frames are released internally.
///
/// On error returns the appropriate `ASTER_CALL_ERR_*` (acquire failure)
/// or `IROH_STATUS_INTERNAL` (transport / framing failure); no out
/// parameters are populated. The pool slot is freed on every error path.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_call_unary(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    session_id: u32,
    request_pair_ptr: *const u8,
    request_pair_len: u32,
    out_response_ptr: *mut *const u8,
    out_response_len: *mut u32,
    out_response_flags: *mut u8,
    out_response_buffer: *mut iroh_buffer_t,
    out_trailer_ptr: *mut *const u8,
    out_trailer_len: *mut u32,
    out_trailer_buffer: *mut iroh_buffer_t,
) -> i32 {
    if out_response_ptr.is_null()
        || out_response_len.is_null()
        || out_response_flags.is_null()
        || out_response_buffer.is_null()
        || out_trailer_ptr.is_null()
        || out_trailer_len.is_null()
        || out_trailer_buffer.is_null()
    {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let request_pair = if request_pair_ptr.is_null() || request_pair_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(request_pair_ptr, request_pair_len as usize).to_vec() }
    };

    if request_pair.is_empty() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let key = pool_key_for(session_id);

    let result: Result<(Option<(Vec<u8>, u8)>, Vec<u8>), UnaryErr> =
        bridge.runtime.handle().block_on(async move {
            let handle = match conn.acquire_stream(key).await {
                Ok(h) => h,
                Err(e) => return Err(UnaryErr::Acquire(e)),
            };
            // Pull the send/recv refs before any awaits so handle stays
            // owned by this scope (RAII drop returns to pool / discards
            // depending on which terminator path we take below).
            let (send, recv) = {
                let pair = handle.get();
                (pair.0.clone(), pair.1.clone())
            };

            // 1. Send the framed header+request as one write_all.
            if send.write_all(request_pair).await.is_err() {
                handle.discard();
                return Err(UnaryErr::Transport);
            }

            // 2. Drain frames until trailer. Capture the first non-row-schema
            //    data frame as the response; ignore extras (server contract
            //    for unary is "exactly one"; binding can validate from
            //    out_trailer status if it cares).
            let mut response: Option<(Vec<u8>, u8)> = None;
            let trailer_payload = loop {
                match read_one_frame(&recv).await {
                    Ok((payload, flags)) => {
                        if flags & FLAG_TRAILER != 0 {
                            break payload;
                        }
                        if flags & FLAG_ROW_SCHEMA != 0 {
                            // Skip row-schema metadata frames silently.
                            continue;
                        }
                        if response.is_none() {
                            response = Some((payload, flags));
                        }
                        // Extra response frames (>1) are ignored — the trailer
                        // is what closes the call.
                    }
                    Err(_) => {
                        handle.discard();
                        return Err(UnaryErr::Transport);
                    }
                }
            };

            // 3. Stream returns to the pool on drop (release path).
            drop(handle);
            Ok((response, trailer_payload))
        });

    match result {
        Ok((response, trailer_payload)) => {
            // Response payload (may be empty if dispatcher sent only a
            // trailer — typical for non-OK error trailers).
            let (resp_buf_id, resp_flags) = match response {
                Some((payload, flags)) => {
                    let (buf_id, arc) = CALL_BUFFERS.insert(payload);
                    unsafe {
                        ptr::write(out_response_ptr, arc.as_ptr());
                        ptr::write(out_response_len, arc.len() as u32);
                    }
                    (buf_id, flags)
                }
                None => {
                    unsafe {
                        ptr::write(out_response_ptr, ptr::null());
                        ptr::write(out_response_len, 0);
                    }
                    (0, 0u8)
                }
            };
            unsafe {
                ptr::write(out_response_flags, resp_flags);
                ptr::write(out_response_buffer, resp_buf_id);
            }

            // Trailer payload (may be empty for clean OK).
            if trailer_payload.is_empty() {
                unsafe {
                    ptr::write(out_trailer_ptr, ptr::null());
                    ptr::write(out_trailer_len, 0);
                    ptr::write(out_trailer_buffer, 0);
                }
            } else {
                let (trailer_buf_id, arc) = CALL_BUFFERS.insert(trailer_payload);
                unsafe {
                    ptr::write(out_trailer_ptr, arc.as_ptr());
                    ptr::write(out_trailer_len, arc.len() as u32);
                    ptr::write(out_trailer_buffer, trailer_buf_id);
                }
            }
            iroh_status_t::IROH_STATUS_OK as i32
        }
        Err(UnaryErr::Acquire(e)) => map_acquire_error(e),
        Err(UnaryErr::Transport) => iroh_status_t::IROH_STATUS_INTERNAL as i32,
    }
}

enum UnaryErr {
    Acquire(AcquireError),
    Transport,
}
