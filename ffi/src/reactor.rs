#![allow(unused_variables)]

//! Reactor FFI — batch-drain call delivery for non-Python bindings.
//!
//! Architecture:
//!   connection tasks → [tokio mpsc] → pump task → [SPSC ring] → aster_reactor_poll
//!
//! The pump task is the single writer to the SPSC ring. The FFI consumer
//! (Java/Go/.NET poll thread) is the single reader. Buffer ownership is
//! tracked via the BridgeRuntime's BufferRegistry.

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aster_transport_core::reactor::{self, OutgoingFrame, ReactorHandle, RequestFrame};
use aster_transport_core::ring;
use tokio::sync::mpsc;

use crate::{
    iroh_node_t, iroh_runtime_t, iroh_status_t, load_runtime, BufferRegistry, HandleRegistry,
};

// ============================================================================
// Types
// ============================================================================

pub type aster_reactor_t = u64;

/// C-visible call descriptor returned by aster_reactor_poll.
#[repr(C)]
pub struct aster_reactor_call_t {
    /// Reactor-assigned call ID (for submit correlation).
    pub call_id: u64,
    /// Reactor-assigned unique ID for the QUIC bi-stream this call arrived
    /// on. Stateless calls each get a fresh stream_id (one call per stream);
    /// session-mode calls share one stream_id across multiple calls. Bindings
    /// should key session-scoped service instances on `(peer_id, stream_id,
    /// service)` so concurrent sessions from the same peer stay isolated.
    pub stream_id: u64,
    /// Pointer to header payload bytes (owned by buffer registry).
    pub header_ptr: *const u8,
    /// Length of header payload.
    pub header_len: u32,
    /// Header frame flags.
    pub header_flags: u8,
    /// Pointer to request payload bytes (owned by buffer registry).
    pub request_ptr: *const u8,
    /// Length of request payload.
    pub request_len: u32,
    /// Request frame flags.
    pub request_flags: u8,
    /// Pointer to peer ID string (UTF-8, not null-terminated, owned by buffer registry).
    pub peer_ptr: *const u8,
    /// Length of peer ID string.
    pub peer_len: u32,
    /// 1 if this is a session call, 0 otherwise.
    pub is_session_call: u8,
    /// Buffer ID for the header payload (release after processing).
    pub header_buffer: u64,
    /// Buffer ID for the request payload.
    pub request_buffer: u64,
    /// Buffer ID for the peer ID string.
    pub peer_buffer: u64,
}

/// Internal ring slot: holds an IncomingCall with buffer IDs already assigned.
struct RingCall {
    call_id: u64,
    stream_id: u64,
    header_buffer: u64,
    header_ptr: *const u8,
    header_len: u32,
    header_flags: u8,
    request_buffer: u64,
    request_ptr: *const u8,
    request_len: u32,
    request_flags: u8,
    peer_buffer: u64,
    peer_ptr: *const u8,
    peer_len: u32,
    is_session_call: bool,
    response_sender: Option<mpsc::UnboundedSender<OutgoingFrame>>,
    /// Per-call receiver for ADDITIONAL request frames (after the first).
    /// Stashed into `ReactorState::request_receivers` on `aster_reactor_poll`
    /// and consumed by `aster_reactor_recv_frame`. For unary / server-stream
    /// calls the binding never reaches in here and the receiver is dropped
    /// when the call is cleaned up, which closes the per-call request
    /// channel.
    request_receiver: Option<mpsc::UnboundedReceiver<RequestFrame>>,
}

// Safety: RingCall is only moved between the pump task and the poll thread.
// The raw pointers point to Arc<[u8]> data held alive by BufferRegistry.
unsafe impl Send for RingCall {}

/// Reactor state owned by the FFI layer.
pub(crate) struct ReactorState {
    /// Consumer side of the SPSC ring (read by aster_reactor_poll).
    consumer: Mutex<ring::Consumer<RingCall>>,
    /// Pending response senders keyed by call_id. Removing a sender drops
    /// the last reference to its mpsc channel, which unblocks the reactor's
    /// `recv().await` loop and finishes the stream.
    response_senders:
        Mutex<std::collections::HashMap<u64, mpsc::UnboundedSender<OutgoingFrame>>>,
    /// Pending per-call request receivers keyed by call_id. The binding
    /// pulls additional request frames via `aster_reactor_recv_frame`,
    /// which locks the per-call receiver and `block_on`s a single
    /// `rx.recv()` with a timeout. Inner mutex is std (not tokio) because
    /// the lock is held only across a synchronous `block_on` call from
    /// the binding's poll thread.
    request_receivers:
        Mutex<std::collections::HashMap<u64, Mutex<mpsc::UnboundedReceiver<RequestFrame>>>>,
    /// Signal to stop the pump task.
    stopped: Arc<AtomicBool>,
    /// Buffer registry for payload lifetime management.
    buffers: Arc<BufferRegistry>,
    /// Tokio runtime handle, captured at reactor creation time. Used by
    /// `aster_reactor_recv_frame` to `block_on` the per-call receiver from
    /// the binding's (non-runtime) poll thread.
    rt_handle: tokio::runtime::Handle,
}

// ============================================================================
// Registry (lives on BridgeRuntime)
// ============================================================================

pub(crate) static REACTORS: once_cell::sync::Lazy<HandleRegistry<ReactorState>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

// ============================================================================
// Pump task
// ============================================================================

async fn pump_task(
    mut handle: ReactorHandle,
    mut producer: ring::Producer<RingCall>,
    buffers: Arc<BufferRegistry>,
    stopped: Arc<AtomicBool>,
) {
    while !stopped.load(Ordering::Relaxed) {
        let call = match handle.next_call().await {
            Some(c) => c,
            None => break, // reactor shut down
        };

        // Register payloads in BufferRegistry so they stay alive while the
        // FFI consumer holds pointers to them.
        let (header_buf_id, header_arc) = buffers.insert(call.header_payload);
        let (request_buf_id, request_arc) = buffers.insert(call.request_payload);
        let (peer_buf_id, peer_arc) = buffers.insert(call.peer_id.into_bytes());

        let ring_call = RingCall {
            call_id: call.call_id,
            stream_id: call.stream_id,
            header_buffer: header_buf_id,
            header_ptr: header_arc.as_ptr(),
            header_len: header_arc.len() as u32,
            header_flags: call.header_flags,
            request_buffer: request_buf_id,
            request_ptr: request_arc.as_ptr(),
            request_len: request_arc.len() as u32,
            request_flags: call.request_flags,
            peer_buffer: peer_buf_id,
            peer_ptr: peer_arc.as_ptr(),
            peer_len: peer_arc.len() as u32,
            is_session_call: call.is_session_call,
            response_sender: Some(call.response_sender),
            request_receiver: Some(call.request_receiver),
        };

        // Spin-try push with backpressure.
        let mut slot = ring_call;
        loop {
            match producer.try_push(slot) {
                Ok(()) => break,
                Err(returned) => {
                    slot = returned;
                    if stopped.load(Ordering::Relaxed) {
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            }
        }
    }
}

// ============================================================================
// C API
// ============================================================================

/// Create a reactor attached to a node. The reactor starts accepting
/// connections and delivering calls via the SPSC ring.
///
/// `ring_capacity` is rounded up to the next power of two.
/// `out_reactor` receives the reactor handle on success.
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_create(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    ring_capacity: u32,
    out_reactor: *mut aster_reactor_t,
) -> i32 {
    if out_reactor.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(_) => return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32,
    };

    let core_node = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let capacity = if ring_capacity > 0 {
        ring_capacity as usize
    } else {
        256
    };

    // Create the core reactor (owns the accept loop).
    let rt_handle = bridge.runtime.handle().clone();
    let reactor_handle = reactor::start_reactor_on(&rt_handle, (*core_node).clone(), capacity);

    // Create the SPSC ring.
    let (producer, consumer) = ring::spsc::<RingCall>(capacity);

    let stopped = Arc::new(AtomicBool::new(false));
    let buffers = Arc::new(BufferRegistry::new());

    // Spawn the pump task.
    let pump_stopped = stopped.clone();
    let pump_buffers = buffers.clone();
    rt_handle.spawn(pump_task(
        reactor_handle,
        producer,
        pump_buffers,
        pump_stopped,
    ));

    let state = ReactorState {
        consumer: Mutex::new(consumer),
        response_senders: Mutex::new(std::collections::HashMap::new()),
        request_receivers: Mutex::new(std::collections::HashMap::new()),
        stopped,
        buffers,
        rt_handle: rt_handle.clone(),
    };

    let handle_id = REACTORS.insert(state);
    unsafe { ptr::write(out_reactor, handle_id) };

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Destroy a reactor. Stops the pump task and releases resources.
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_destroy(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
) -> i32 {
    match REACTORS.remove(reactor) {
        Some(state) => {
            state.stopped.store(true, Ordering::Relaxed);
            iroh_status_t::IROH_STATUS_OK as i32
        }
        None => iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    }
}

/// Poll for incoming calls. Drains up to `max_calls` from the ring buffer
/// into the caller-provided `out_calls` array. Returns the number of calls
/// written.
///
/// If `timeout_ms` is 0, returns immediately (non-blocking).
/// If `timeout_ms` > 0, blocks up to that duration waiting for at least one call.
///
/// The caller must release each call's buffers via `iroh_buffer_release` after
/// processing (using the reactor's buffer registry, not the runtime's).
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_poll(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
    out_calls: *mut aster_reactor_call_t,
    max_calls: u32,
    timeout_ms: u32,
) -> u32 {
    if out_calls.is_null() || max_calls == 0 {
        return 0;
    }

    let state = match REACTORS.get(reactor) {
        Some(s) => s,
        None => return 0,
    };

    let mut consumer = match state.consumer.lock() {
        Ok(c) => c,
        Err(_) => return 0,
    };

    let mut written = 0u32;

    // Try draining without waiting first.
    consumer.drain(max_calls as usize, |ring_call| {
        let desc = aster_reactor_call_t {
            call_id: ring_call.call_id,
            stream_id: ring_call.stream_id,
            header_ptr: ring_call.header_ptr,
            header_len: ring_call.header_len,
            header_flags: ring_call.header_flags,
            request_ptr: ring_call.request_ptr,
            request_len: ring_call.request_len,
            request_flags: ring_call.request_flags,
            peer_ptr: ring_call.peer_ptr,
            peer_len: ring_call.peer_len,
            is_session_call: if ring_call.is_session_call { 1 } else { 0 },
            header_buffer: ring_call.header_buffer,
            request_buffer: ring_call.request_buffer,
            peer_buffer: ring_call.peer_buffer,
        };
        unsafe {
            ptr::write(out_calls.add(written as usize), desc);
        }

        // Stash the response sender + request receiver for later submit /
        // recv_frame calls. Both are owned by the FFI state until the
        // call's terminal trailer (or destroy).
        if let Some(sender) = ring_call.response_sender {
            state
                .response_senders
                .lock()
                .unwrap()
                .insert(ring_call.call_id, sender);
        }
        if let Some(receiver) = ring_call.request_receiver {
            state
                .request_receivers
                .lock()
                .unwrap()
                .insert(ring_call.call_id, Mutex::new(receiver));
        }

        written += 1;
    });

    if written > 0 || timeout_ms == 0 {
        return written;
    }

    // No calls available and timeout > 0: spin-poll with yields.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);

    loop {
        if consumer.available() > 0 {
            consumer.drain(max_calls as usize, |ring_call| {
                let desc = aster_reactor_call_t {
                    call_id: ring_call.call_id,
                    stream_id: ring_call.stream_id,
                    header_ptr: ring_call.header_ptr,
                    header_len: ring_call.header_len,
                    header_flags: ring_call.header_flags,
                    request_ptr: ring_call.request_ptr,
                    request_len: ring_call.request_len,
                    request_flags: ring_call.request_flags,
                    peer_ptr: ring_call.peer_ptr,
                    peer_len: ring_call.peer_len,
                    is_session_call: if ring_call.is_session_call { 1 } else { 0 },
                    header_buffer: ring_call.header_buffer,
                    request_buffer: ring_call.request_buffer,
                    peer_buffer: ring_call.peer_buffer,
                };
                unsafe {
                    ptr::write(out_calls.add(written as usize), desc);
                }
                if let Some(sender) = ring_call.response_sender {
                    state
                        .response_senders
                        .lock()
                        .unwrap()
                        .insert(ring_call.call_id, sender);
                }
                if let Some(receiver) = ring_call.request_receiver {
                    state
                        .request_receivers
                        .lock()
                        .unwrap()
                        .insert(ring_call.call_id, Mutex::new(receiver));
                }
                written += 1;
            });
            return written;
        }

        if std::time::Instant::now() >= deadline {
            return 0;
        }
        std::thread::yield_now();
    }
}

/// Submit a unary response (single response frame + trailer) in one FFI call.
///
/// Convenience wrapper over `aster_reactor_submit_frame` + `aster_reactor_submit_trailer`
/// for the unary path. For server-streaming or bidi, call `submit_frame` N times
/// then `submit_trailer` once.
///
/// After this call, the call_id is no longer valid.
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_submit(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
    call_id: u64,
    response_ptr: *const u8,
    response_len: u32,
    trailer_ptr: *const u8,
    trailer_len: u32,
) -> i32 {
    let state = match REACTORS.get(reactor) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    // Remove the sender from the map — this is the terminal submit, callers
    // that still hold the call_id afterwards would get NOT_FOUND.
    let sender = match state.response_senders.lock().unwrap().remove(&call_id) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let response_frame = if response_ptr.is_null() || response_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(response_ptr, response_len as usize).to_vec() }
    };

    let trailer_frame = if trailer_ptr.is_null() || trailer_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(trailer_ptr, trailer_len as usize).to_vec() }
    };

    if !response_frame.is_empty() {
        let _ = sender.send(OutgoingFrame::Frame(response_frame));
    }
    let _ = sender.send(OutgoingFrame::Trailer(trailer_frame));
    // `sender` goes out of scope here — the map already dropped its copy,
    // so this closes the mpsc channel and lets the reactor finish the stream.

    // Drop the per-call request receiver too (terminal cleanup).
    state.request_receivers.lock().unwrap().remove(&call_id);

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Submit one streaming response frame for a call. May be called multiple
/// times per call; each call enqueues one Frame on the reactor's mpsc channel.
/// The call_id remains valid until `aster_reactor_submit_trailer` is called.
///
/// `frame_ptr`/`frame_len` point to already-framed bytes (including the 4-byte
/// length prefix and 1-byte flags). The binding is responsible for framing.
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_submit_frame(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
    call_id: u64,
    frame_ptr: *const u8,
    frame_len: u32,
) -> i32 {
    let state = match REACTORS.get(reactor) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    // Clone the sender without removing it — the call remains open for more
    // frames and the eventual trailer.
    let sender = {
        let map = state.response_senders.lock().unwrap();
        match map.get(&call_id) {
            Some(s) => s.clone(),
            None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
        }
    };

    let frame = if frame_ptr.is_null() || frame_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(frame_ptr, frame_len as usize).to_vec() }
    };

    if sender.send(OutgoingFrame::Frame(frame)).is_err() {
        return iroh_status_t::IROH_STATUS_NOT_FOUND as i32;
    }
    iroh_status_t::IROH_STATUS_OK as i32
}

/// Submit the trailer for a call and close the stream. After this call, the
/// `call_id` is no longer valid. The trailer payload may be empty — the
/// reactor still finishes the send side cleanly.
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_submit_trailer(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
    call_id: u64,
    trailer_ptr: *const u8,
    trailer_len: u32,
) -> i32 {
    let state = match REACTORS.get(reactor) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let sender = match state.response_senders.lock().unwrap().remove(&call_id) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let trailer = if trailer_ptr.is_null() || trailer_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(trailer_ptr, trailer_len as usize).to_vec() }
    };

    let _ = sender.send(OutgoingFrame::Trailer(trailer));
    // `sender` dropped here — channel closes once the reactor drains it.

    // Drop the per-call request receiver too (terminal cleanup).
    state.request_receivers.lock().unwrap().remove(&call_id);

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Result codes returned by `aster_reactor_recv_frame` in addition to the
/// usual `iroh_status_t` codes for error cases.
pub const ASTER_RECV_FRAME_OK: i32 = 0;
/// Per-call request channel was closed cleanly (peer signalled END_STREAM
/// or its recv stream EOF'd). No frame was written; the binding should
/// stop calling recv_frame for this call_id.
pub const ASTER_RECV_FRAME_END_OF_STREAM: i32 = 1;
/// Timeout expired with no frame available. The binding may retry.
pub const ASTER_RECV_FRAME_TIMEOUT: i32 = 2;

/// Pull the next ADDITIONAL request frame for a client-streaming or
/// bidi-streaming call. Blocks up to `timeout_ms` waiting for a frame.
///
/// On `ASTER_RECV_FRAME_OK` (return value 0), the function writes:
/// - `*out_payload_ptr`: pointer to payload bytes (owned by the reactor's
///   buffer registry — must be released via `aster_reactor_buffer_release`)
/// - `*out_payload_len`: payload length in bytes
/// - `*out_flags`: frame flags byte (peer may set `FLAG_END_STREAM` here on
///   the last frame; the binding may use that as a hint but is not required
///   to — the reactor will also surface end-of-stream as
///   `ASTER_RECV_FRAME_END_OF_STREAM` on the next call)
/// - `*out_buffer_id`: registry id for releasing the payload buffer
///
/// On `ASTER_RECV_FRAME_END_OF_STREAM` (return value 1) or
/// `ASTER_RECV_FRAME_TIMEOUT` (return value 2), no out parameters are
/// touched and the call_id remains valid for further recv attempts (in the
/// timeout case) or is now drained (end-of-stream case — further calls
/// will continue to return end-of-stream).
///
/// Negative return values are `iroh_status_t` error codes (e.g.
/// `IROH_STATUS_NOT_FOUND` if the call_id is unknown).
///
/// The first request frame is delivered inline via `aster_reactor_poll`'s
/// `request_ptr` / `request_len` / `request_flags` fields. This function
/// is for SUBSEQUENT frames only.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_reactor_recv_frame(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
    call_id: u64,
    timeout_ms: u32,
    out_payload_ptr: *mut *const u8,
    out_payload_len: *mut u32,
    out_flags: *mut u8,
    out_buffer_id: *mut u64,
) -> i32 {
    if out_payload_ptr.is_null()
        || out_payload_len.is_null()
        || out_flags.is_null()
        || out_buffer_id.is_null()
    {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let state = match REACTORS.get(reactor) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    // Take the receiver out of the map for the duration of the recv. This
    // means concurrent `recv_frame` calls on the same call_id will see
    // NOT_FOUND temporarily — that's fine, the contract is single-consumer
    // per call_id.
    let receiver_mutex = match state.request_receivers.lock().unwrap().remove(&call_id) {
        Some(r) => r,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let mut receiver = match receiver_mutex.into_inner() {
        Ok(r) => r,
        Err(_) => return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32,
    };

    let timeout = std::time::Duration::from_millis(timeout_ms as u64);
    let recv_result = state.rt_handle.block_on(async {
        if timeout_ms == 0 {
            // Non-blocking poll
            match receiver.try_recv() {
                Ok(frame) => Ok(Some(frame)),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(None),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => Err(()),
            }
        } else {
            tokio::select! {
                frame = receiver.recv() => match frame {
                    Some(f) => Ok(Some(f)),
                    None => Err(()),
                },
                _ = tokio::time::sleep(timeout) => Ok(None),
            }
        }
    });

    match recv_result {
        Ok(Some(frame)) => {
            // Stash the receiver back so subsequent recv_frame calls work.
            state
                .request_receivers
                .lock()
                .unwrap()
                .insert(call_id, Mutex::new(receiver));

            let (buf_id, arc) = state.buffers.insert(frame.payload);
            unsafe {
                ptr::write(out_payload_ptr, arc.as_ptr());
                ptr::write(out_payload_len, arc.len() as u32);
                ptr::write(out_flags, frame.flags);
                ptr::write(out_buffer_id, buf_id);
            }
            ASTER_RECV_FRAME_OK
        }
        Ok(None) => {
            // Timeout — put the receiver back so the binding can retry.
            state
                .request_receivers
                .lock()
                .unwrap()
                .insert(call_id, Mutex::new(receiver));
            ASTER_RECV_FRAME_TIMEOUT
        }
        Err(()) => {
            // Channel closed — drop the receiver permanently. Subsequent
            // recv_frame calls for this call_id will get NOT_FOUND, which
            // the binding can interpret as "already drained".
            ASTER_RECV_FRAME_END_OF_STREAM
        }
    }
}

/// Release a buffer obtained from a reactor call descriptor.
/// Each buffer ID from aster_reactor_call_t must be released exactly once.
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_buffer_release(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
    buffer: u64,
) -> i32 {
    let state = match REACTORS.get(reactor) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    state.buffers.release(buffer);
    iroh_status_t::IROH_STATUS_OK as i32
}
