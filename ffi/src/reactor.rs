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

use aster_transport_core::reactor::{self, OutgoingResponse, ReactorHandle};
use aster_transport_core::ring;
use tokio::sync::oneshot;

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
    response_sender: Option<oneshot::Sender<OutgoingResponse>>,
}

// Safety: RingCall is only moved between the pump task and the poll thread.
// The raw pointers point to Arc<[u8]> data held alive by BufferRegistry.
unsafe impl Send for RingCall {}

/// Reactor state owned by the FFI layer.
pub(crate) struct ReactorState {
    /// Consumer side of the SPSC ring (read by aster_reactor_poll).
    consumer: Mutex<ring::Consumer<RingCall>>,
    /// Pending response senders keyed by call_id.
    response_senders: Mutex<std::collections::HashMap<u64, oneshot::Sender<OutgoingResponse>>>,
    /// Signal to stop the pump task.
    stopped: Arc<AtomicBool>,
    /// Buffer registry for payload lifetime management.
    buffers: Arc<BufferRegistry>,
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
        stopped,
        buffers,
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

        // Stash the response sender for later submit.
        if let Some(sender) = ring_call.response_sender {
            state
                .response_senders
                .lock()
                .unwrap()
                .insert(ring_call.call_id, sender);
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

/// Submit a response for a call received via aster_reactor_poll.
///
/// `response_ptr`/`response_len` is the response frame bytes.
/// `trailer_ptr`/`trailer_len` is the trailer frame bytes.
///
/// The reactor writes both frames to the QUIC stream and finishes it.
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

    let _ = sender.send(OutgoingResponse {
        response_frame,
        trailer_frame,
    });

    iroh_status_t::IROH_STATUS_OK as i32
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
