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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(any(test, feature = "test-hooks"))]
use std::sync::atomic::AtomicUsize;

use aster_transport_core::reactor::{
    self, OutgoingFrame, ReactorEvent, RequestFrame, ResponsePermit, ResponseReserveError,
    ResponseSender,
};
use aster_transport_core::ring;
use tokio::sync::mpsc;

use crate::{
    iroh_node_t, iroh_runtime_t, iroh_status_t, load_runtime, BufferRegistry, HandleRegistry,
};

// ============================================================================
// Types
// ============================================================================

pub type aster_reactor_t = u64;

/// Event-kind discriminator for `aster_reactor_call_t` (spec §7.5).
/// Bindings MUST check `event_kind` before reading any other field — a
/// `ConnectionClosed` event populates only `event_kind`, `connection_id`,
/// `peer_*` (peer_id of the closed connection); all other fields are zero
/// or NULL.
pub const ASTER_EVENT_KIND_CALL: u8 = 0;
pub const ASTER_EVENT_KIND_CONNECTION_CLOSED: u8 = 1;
/// A previously-full response channel has reserved capacity for `call_id`.
/// No payload buffers are present; the binding must retry the same submit.
pub const ASTER_EVENT_KIND_RESPONSE_WRITABLE: u8 = 2;
/// A pending response-capacity reservation closed before becoming writable.
/// No payload buffers are present; the binding must stop producing this call.
pub const ASTER_EVENT_KIND_RESPONSE_CLOSED: u8 = 3;

/// C-visible event descriptor returned by aster_reactor_poll.
///
/// Despite the historical `aster_reactor_call_t` name, this struct carries
/// inbound calls, connection-close notifications, and response-capacity
/// notifications. Capacity notifications populate only `event_kind` and
/// `call_id`; every pointer and buffer id is zero. Bindings MUST branch on
/// `event_kind` before reading the remaining fields.
#[repr(C)]
pub struct aster_reactor_call_t {
    /// Event-kind discriminator. See `ASTER_EVENT_KIND_*` constants.
    pub event_kind: u8,
    /// Reactor-assigned call ID (for submit correlation). Zero on
    /// ConnectionClosed events.
    pub call_id: u64,
    /// Reactor-assigned unique ID for the QUIC connection this event
    /// belongs to (spec §7.5). Same id is reused across every call from
    /// this connection and emitted with the terminal ConnectionClosed
    /// event so the binding can drop per-connection state. A peer that
    /// disconnects and reconnects gets a fresh `connection_id`.
    pub connection_id: u64,
    /// Reactor-assigned unique ID for the QUIC bi-stream this call arrived
    /// on. Multiplexed streams (spec §6) carry many calls per bi-stream,
    /// so DO NOT key per-session state on `stream_id` — use
    /// `(connection_id, StreamHeader.sessionId)` from the binding-decoded
    /// header instead. Zero on ConnectionClosed events.
    pub stream_id: u64,
    /// Pointer to header payload bytes (owned by buffer registry).
    /// NULL on ConnectionClosed events.
    pub header_ptr: *const u8,
    /// Length of header payload. Zero on ConnectionClosed events.
    pub header_len: u32,
    /// Header frame flags. Zero on ConnectionClosed events.
    pub header_flags: u8,
    /// Pointer to request payload bytes (owned by buffer registry).
    /// NULL on ConnectionClosed events.
    pub request_ptr: *const u8,
    /// Length of request payload. Zero on ConnectionClosed events.
    pub request_len: u32,
    /// Request frame flags. Zero on ConnectionClosed events.
    pub request_flags: u8,
    /// Pointer to peer ID string (UTF-8, not null-terminated, owned by
    /// buffer registry). Populated on both Call and ConnectionClosed.
    pub peer_ptr: *const u8,
    /// Length of peer ID string. Populated on both Call and
    /// ConnectionClosed.
    pub peer_len: u32,
    /// Buffer ID for the header payload (release after processing). Zero
    /// on ConnectionClosed events.
    pub header_buffer: u64,
    /// Buffer ID for the request payload. Zero on ConnectionClosed events.
    pub request_buffer: u64,
    /// Buffer ID for the peer ID string. Populated on both Call and
    /// ConnectionClosed.
    pub peer_buffer: u64,
    /// Borrowed `iroh_connection_t` handle for the QUIC connection this
    /// event arrived on. Populated on both Call and ConnectionClosed.
    /// Same handle for every event from the same `connection_id`; freed
    /// internally by the reactor when ConnectionClosed is consumed.
    /// Bindings MUST NOT call `iroh_connection_free` on this handle —
    /// the reactor owns its lifetime.
    pub connection_handle: u64,
}

/// Internal ring slot: holds either an IncomingCall (event_kind=0) or a
/// ConnectionClosed event (event_kind=1). For ConnectionClosed slots only
/// `event_kind`, `connection_id`, and the `peer_*` fields are populated;
/// the rest are zero/NULL.
struct RingCall {
    event_kind: u8,
    call_id: u64,
    connection_id: u64,
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
    connection_handle: u64,
    response_sender: Option<ResponseSender>,
    /// Per-call receiver for ADDITIONAL request frames (after the first).
    /// Stashed into `ReactorState::request_receivers` on `aster_reactor_poll`
    /// and consumed by `aster_reactor_recv_frame`. For unary / server-stream
    /// calls the binding never reaches in here and the receiver is dropped
    /// when the call is cleaned up, which closes the per-call request
    /// channel.
    request_receiver: Option<mpsc::Receiver<RequestFrame>>,
    /// Per-call cancellation flag. Set by the reactor when a `FLAG_CANCEL`
    /// frame arrives on the wire (or the stream errors mid-call). Stashed
    /// into `ReactorState::cancelled_flags` on poll so the binding can
    /// query it via `aster_reactor_check_cancelled`.
    cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl RingCall {
    fn response_capacity(call_id: u64, writable: bool) -> Self {
        Self {
            event_kind: if writable {
                ASTER_EVENT_KIND_RESPONSE_WRITABLE
            } else {
                ASTER_EVENT_KIND_RESPONSE_CLOSED
            },
            call_id,
            connection_id: 0,
            stream_id: 0,
            header_buffer: 0,
            header_ptr: ptr::null(),
            header_len: 0,
            header_flags: 0,
            request_buffer: 0,
            request_ptr: ptr::null(),
            request_len: 0,
            request_flags: 0,
            peer_buffer: 0,
            peer_ptr: ptr::null(),
            peer_len: 0,
            connection_handle: 0,
            response_sender: None,
            request_receiver: None,
            cancelled: None,
        }
    }
}

// Safety: RingCall is only moved between the pump task and the poll thread.
// The raw pointers point to Arc<[u8]> data held alive by BufferRegistry.
unsafe impl Send for RingCall {}

enum ResponseReservation {
    Pending {
        token: u64,
        cancel: tokio::sync::oneshot::Sender<()>,
    },
    Ready {
        permit: ResponsePermit,
    },
}

/// Reactor state owned by the FFI layer.
pub(crate) struct ReactorState {
    /// Consumer side of the SPSC ring (read by aster_reactor_poll).
    consumer: Mutex<ring::Consumer<RingCall>>,
    /// Pending response senders keyed by call_id. Removing a sender drops
    /// the last reference to its mpsc channel, which unblocks the reactor's
    /// `recv().await` loop and finishes the stream.
    response_senders: Mutex<std::collections::HashMap<u64, ResponseSender>>,
    /// At most one async capacity reservation per call. Java waits for the
    /// corresponding ring event, so its virtual thread parks without either
    /// blocking in native code or polling `try_reserve`.
    response_reservations: Mutex<std::collections::HashMap<u64, ResponseReservation>>,
    next_reservation_token: AtomicU64,
    response_capacity_tx: mpsc::UnboundedSender<RingCall>,
    /// Pending per-call request receivers keyed by call_id. The binding
    /// pulls additional request frames via `aster_reactor_recv_frame`,
    /// which locks the per-call receiver and `block_on`s a single
    /// `rx.recv()` with a timeout. Inner mutex is std (not tokio) because
    /// the lock is held only across a synchronous `block_on` call from
    /// the binding's poll thread.
    request_receivers: Mutex<std::collections::HashMap<u64, Mutex<mpsc::Receiver<RequestFrame>>>>,
    /// Per-call cancellation flags. Populated on poll, removed on
    /// terminal submit. Streaming dispatchers query this via
    /// `aster_reactor_check_cancelled` to break out of long-running
    /// loops when the peer signals cancellation.
    cancelled_flags: Mutex<std::collections::HashMap<u64, Arc<std::sync::atomic::AtomicBool>>>,
    /// Signal to stop the pump task.
    stopped: Arc<AtomicBool>,
    /// Number of polls that reached the wait-after-initial-drain path.
    /// Test-only proof that an arriving call exercised the second drain.
    #[cfg(any(test, feature = "test-hooks"))]
    blocking_poll_entries: AtomicUsize,
    /// Buffer registry for payload lifetime management.
    buffers: Arc<BufferRegistry>,
    /// Tokio runtime handle, captured at reactor creation time. Used by
    /// `aster_reactor_recv_frame` to `block_on` the per-call receiver from
    /// the binding's (non-runtime) poll thread.
    rt_handle: tokio::runtime::Handle,
    /// Non-rpc aster-ALPN connections awaiting dispatch. Drained by
    /// `aster_reactor_accept_non_rpc`. Tokio mutex because the binding polls
    /// it through an async await inside a spawned task.
    non_rpc_rx: tokio::sync::Mutex<
        tokio::sync::mpsc::Receiver<(Vec<u8>, aster_transport_core::CoreConnection)>,
    >,
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
    mut event_rx: tokio::sync::mpsc::Receiver<ReactorEvent>,
    mut response_capacity_rx: mpsc::UnboundedReceiver<RingCall>,
    mut producer: ring::Producer<RingCall>,
    buffers: Arc<BufferRegistry>,
    bridge: Arc<crate::BridgeRuntime>,
    stopped: Arc<AtomicBool>,
) {
    // Per-reactor cache mapping core reactor `connection_id` → bridge
    // `iroh_connection_t` handle. The reactor's `connection_id` is its own
    // counter (not a bridge handle); we register the `CoreConnection` once
    // per connection here so every Call slot from the same connection
    // shares one borrowed handle, then evict + free on ConnectionClosed.
    // Bindings see a stable handle for the connection's lifetime and never
    // need to manage its lifecycle themselves.
    let mut handle_cache: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();

    while !stopped.load(Ordering::Relaxed) {
        enum PumpInput {
            Reactor(ReactorEvent),
            ResponseCapacity(RingCall),
        }

        let input = tokio::select! {
            event = event_rx.recv() => match event {
                Some(event) => PumpInput::Reactor(event),
                None => break, // reactor shut down
            },
            capacity = response_capacity_rx.recv() => match capacity {
                Some(capacity) => PumpInput::ResponseCapacity(capacity),
                None => break, // reactor state was destroyed
            },
        };

        let ring_call = match input {
            PumpInput::ResponseCapacity(capacity) => capacity,
            PumpInput::Reactor(ReactorEvent::Call(call)) => {
                let (header_buf_id, header_arc) = buffers.insert(call.header_payload);
                let (request_buf_id, request_arc) = buffers.insert(call.request_payload);
                let (peer_buf_id, peer_arc) = buffers.insert(call.peer_id.into_bytes());

                let conn_handle = *handle_cache
                    .entry(call.connection_id)
                    .or_insert_with(|| bridge.connections.insert(call.connection.clone()));

                RingCall {
                    event_kind: ASTER_EVENT_KIND_CALL,
                    call_id: call.call_id,
                    connection_id: call.connection_id,
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
                    connection_handle: conn_handle,
                    response_sender: Some(call.response_sender),
                    request_receiver: Some(call.request_receiver),
                    cancelled: Some(call.cancelled),
                }
            }
            PumpInput::Reactor(ReactorEvent::ConnectionClosed {
                peer_id,
                connection_id,
                ..
            }) => {
                let (peer_buf_id, peer_arc) = buffers.insert(peer_id.into_bytes());

                // Evict + drop the bridge-side Arc. After this the binding
                // can still see `connection_handle` on the closed event but
                // any FFI call against it will return NOT_FOUND, which is
                // the correct semantic for "the connection is gone".
                let conn_handle = handle_cache.remove(&connection_id).unwrap_or(0);
                if conn_handle != 0 {
                    bridge.connections.remove(conn_handle);
                }

                RingCall {
                    event_kind: ASTER_EVENT_KIND_CONNECTION_CLOSED,
                    call_id: 0,
                    connection_id,
                    stream_id: 0,
                    header_buffer: 0,
                    header_ptr: ptr::null(),
                    header_len: 0,
                    header_flags: 0,
                    request_buffer: 0,
                    request_ptr: ptr::null(),
                    request_len: 0,
                    request_flags: 0,
                    peer_buffer: peer_buf_id,
                    peer_ptr: peer_arc.as_ptr(),
                    peer_len: peer_arc.len() as u32,
                    connection_handle: conn_handle,
                    response_sender: None,
                    request_receiver: None,
                    cancelled: None,
                }
            }
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

    // Split the handle: the pump task owns the RPC event stream; ReactorState
    // keeps the non-rpc receiver so the binding can long-poll for admission /
    // manifest connections via `aster_reactor_accept_non_rpc`.
    let (event_rx, non_rpc_rx) = reactor_handle.into_parts();

    // Create the SPSC ring.
    let (producer, consumer) = ring::spsc::<RingCall>(capacity);
    let (response_capacity_tx, response_capacity_rx) = mpsc::unbounded_channel();

    let stopped = Arc::new(AtomicBool::new(false));
    let buffers = Arc::new(BufferRegistry::new());

    // Spawn the pump task.
    let pump_stopped = stopped.clone();
    let pump_buffers = buffers.clone();
    let pump_bridge = bridge.clone();
    rt_handle.spawn(pump_task(
        event_rx,
        response_capacity_rx,
        producer,
        pump_buffers,
        pump_bridge,
        pump_stopped,
    ));

    let state = ReactorState {
        consumer: Mutex::new(consumer),
        response_senders: Mutex::new(std::collections::HashMap::new()),
        response_reservations: Mutex::new(std::collections::HashMap::new()),
        next_reservation_token: AtomicU64::new(1),
        response_capacity_tx,
        request_receivers: Mutex::new(std::collections::HashMap::new()),
        cancelled_flags: Mutex::new(std::collections::HashMap::new()),
        stopped,
        #[cfg(any(test, feature = "test-hooks"))]
        blocking_poll_entries: AtomicUsize::new(0),
        buffers,
        rt_handle: rt_handle.clone(),
        non_rpc_rx: tokio::sync::Mutex::new(non_rpc_rx),
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

/// Long-poll for the next non-rpc aster-ALPN connection accepted by the reactor.
/// Fires `IROH_EVENT_ASTER_ACCEPTED` when a connection arrives, with the same
/// payload shape as `iroh_node_accept_aster`: `event.handle` = connection handle,
/// `event.data_ptr/len` = ALPN bytes, `event.buffer` = lease to release via
/// `iroh_buffer_release`. Returns `IROH_STATUS_OK` on successful dispatch of the
/// async wait; the caller awaits completion via the usual operation registry.
///
/// When the reactor is shut down (its non_rpc sender dropped) the async task
/// emits an error event instead of blocking forever.
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_accept_non_rpc(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
    user_data: u64,
    out_operation: *mut crate::iroh_operation_t,
) -> i32 {
    use crate::{iroh_event_kind_t, iroh_status_t, EventInternal};
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match crate::load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let state = match REACTORS.get(reactor) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    let state = state.clone();
    bridge.runtime.spawn(async move {
        if crate::check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        let mut rx = state.non_rpc_rx.lock().await;
        match rx.recv().await {
            Some((alpn, conn)) => {
                let conn_handle = bridge2.connections.insert(conn);
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_ASTER_ACCEPTED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    conn_handle,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, alpn);
            }
            None => {
                bridge2.emit_error(op_id, user_data, "reactor non_rpc channel closed");
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
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
    consumer.drain(max_calls as usize, |mut ring_call| {
        let desc = aster_reactor_call_t {
            event_kind: ring_call.event_kind,
            call_id: ring_call.call_id,
            connection_id: ring_call.connection_id,
            stream_id: ring_call.stream_id,
            header_ptr: ring_call.header_ptr,
            header_len: ring_call.header_len,
            header_flags: ring_call.header_flags,
            request_ptr: ring_call.request_ptr,
            request_len: ring_call.request_len,
            request_flags: ring_call.request_flags,
            peer_ptr: ring_call.peer_ptr,
            peer_len: ring_call.peer_len,
            header_buffer: ring_call.header_buffer,
            request_buffer: ring_call.request_buffer,
            peer_buffer: ring_call.peer_buffer,
            connection_handle: ring_call.connection_handle,
        };
        unsafe {
            ptr::write(out_calls.add(written as usize), desc);
        }

        // Stash the response sender + request receiver for later submit /
        // recv_frame calls. Both are owned by the FFI state until the
        // call's terminal trailer (or destroy).
        install_call_state(&state, reactor, &mut ring_call);

        written += 1;
    });

    if written > 0 || timeout_ms == 0 {
        return written;
    }

    // No calls available and timeout > 0: spin-poll with yields.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);

    #[cfg(any(test, feature = "test-hooks"))]
    state.blocking_poll_entries.fetch_add(1, Ordering::Release);

    loop {
        if consumer.available() > 0 {
            consumer.drain(max_calls as usize, |mut ring_call| {
                let desc = aster_reactor_call_t {
                    event_kind: ring_call.event_kind,
                    call_id: ring_call.call_id,
                    connection_id: ring_call.connection_id,
                    stream_id: ring_call.stream_id,
                    header_ptr: ring_call.header_ptr,
                    header_len: ring_call.header_len,
                    header_flags: ring_call.header_flags,
                    request_ptr: ring_call.request_ptr,
                    request_len: ring_call.request_len,
                    request_flags: ring_call.request_flags,
                    peer_ptr: ring_call.peer_ptr,
                    peer_len: ring_call.peer_len,
                    header_buffer: ring_call.header_buffer,
                    request_buffer: ring_call.request_buffer,
                    peer_buffer: ring_call.peer_buffer,
                    connection_handle: ring_call.connection_handle,
                };
                unsafe {
                    ptr::write(out_calls.add(written as usize), desc);
                }
                install_call_state(&state, reactor, &mut ring_call);
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

/// Sizes of the three per-call maps, for tests.
///
/// Exists because the previous cleanup tests modelled these maps instead of
/// observing them, so two production races -- a watcher armed before the
/// entries were inserted, and a drain path with no watcher at all -- were
/// invisible. `doc(hidden)` and Rust-only: this is not part of the C ABI.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn debug_call_state_len(reactor: aster_reactor_t) -> Option<(usize, usize, usize)> {
    let state = REACTORS.get(reactor)?;
    let s = (
        state.response_senders.lock().unwrap().len(),
        state.request_receivers.lock().unwrap().len(),
        state.cancelled_flags.lock().unwrap().len(),
    );
    Some(s)
}

/// Number of calls to `aster_reactor_poll` that found the ring empty on their
/// initial drain and entered the blocking wait path.
///
/// A live integration test uses this to prove its call arrived through the
/// second drain, which previously omitted `install_call_state` entirely.
#[doc(hidden)]
#[cfg(any(test, feature = "test-hooks"))]
pub fn debug_blocking_poll_entries(reactor: aster_reactor_t) -> Option<usize> {
    REACTORS
        .get(reactor)
        .map(|state| state.blocking_poll_entries.load(Ordering::Acquire))
}

/// Install one call's per-call state, then arm its lifecycle watcher.
///
/// Both drain paths go through here, and the ORDER matters twice over:
///
///  * all three entries are inserted BEFORE the watcher is spawned. Spawning
///    first raced a call that was already cancelled -- `closed()` could fire
///    against empty maps, and the insert that followed then leaked exactly the
///    entries the watcher existed to remove.
///  * the watcher is armed for every call, not just those from the immediate
///    path. The blocking-poll drain had none at all, which is the path a
///    binding waiting for work actually takes.
fn install_call_state(state: &ReactorState, reactor: aster_reactor_t, ring_call: &mut RingCall) {
    let sender = ring_call.response_sender.take();
    if let Some(receiver) = ring_call.request_receiver.take() {
        state
            .request_receivers
            .lock()
            .unwrap()
            .insert(ring_call.call_id, Mutex::new(receiver));
    }
    if let Some(flag) = ring_call.cancelled.take() {
        state
            .cancelled_flags
            .lock()
            .unwrap()
            .insert(ring_call.call_id, flag);
    }
    if let Some(sender) = sender {
        let watcher = sender.clone();
        let call_id = ring_call.call_id;
        state
            .response_senders
            .lock()
            .unwrap()
            .insert(call_id, sender);
        // Armed only now that every entry is present, so a cancellation that
        // already happened reaps a complete set rather than an empty one.
        state.rt_handle.spawn(async move {
            watcher.closed().await;
            if let Some(state) = REACTORS.get(reactor) {
                reap_call(&state, call_id);
            }
        });
    }
}

/// Drop every per-call entry for `call_id`.
///
/// One function so the terminal-success path and the closed-channel path cannot
/// diverge: they used to, and the closed path leaked. Idempotent, so calling it
/// twice (a retry that races a cancellation) is harmless.
fn reap_call(state: &ReactorState, call_id: u64) {
    state.response_senders.lock().unwrap().remove(&call_id);
    if let Some(reservation) = state.response_reservations.lock().unwrap().remove(&call_id) {
        if let ResponseReservation::Pending { cancel, .. } = reservation {
            let _ = cancel.send(());
        }
        // Wake a binding parked on this reservation. A stale CLOSED after a
        // WRITABLE notification is harmless: the binding removes its waiter
        // exactly once and its retry observes NOT_FOUND.
        let _ = state
            .response_capacity_tx
            .send(RingCall::response_capacity(call_id, false));
    }
    state.request_receivers.lock().unwrap().remove(&call_id);
    state.cancelled_flags.lock().unwrap().remove(&call_id);
}

enum CapacitySend {
    Sent,
    Full,
    Closed,
    Invalid,
}

/// Send one response only after capacity is owned, constructing/copying its
/// bytes after that point. A Java capacity wait leaves a combined item/byte
/// permit here; other bindings continue to use the non-blocking `try_reserve`
/// path.
fn send_with_capacity(
    state: &ReactorState,
    call_id: u64,
    sender: &ResponseSender,
    encoded_len: usize,
    make_frame: impl FnOnce() -> OutgoingFrame,
) -> CapacitySend {
    let held_permit = {
        let mut reservations = state.response_reservations.lock().unwrap();
        match reservations.entry(call_id) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                if matches!(entry.get(), ResponseReservation::Pending { .. }) {
                    // Keep the pending entry in place. Removing and reinserting
                    // it creates a window in which a concurrent submit can
                    // reserve independently and violate the one-producer
                    // contract that the reservation is meant to enforce.
                    return CapacitySend::Full;
                }
                if matches!(
                    entry.get(),
                    ResponseReservation::Ready { permit }
                        if permit.encoded_len() != encoded_len
                ) {
                    return CapacitySend::Invalid;
                }
                let ResponseReservation::Ready { permit } = entry.remove() else {
                    unreachable!("pending reservation returned above")
                };
                Some(permit)
            }
            std::collections::hash_map::Entry::Vacant(_) => None,
        }
    };

    match held_permit {
        Some(permit) => {
            if permit.send(make_frame()).is_ok() {
                CapacitySend::Sent
            } else {
                CapacitySend::Invalid
            }
        }
        None => match sender.try_reserve(encoded_len) {
            Ok(permit) => {
                if permit.send(make_frame()).is_ok() {
                    CapacitySend::Sent
                } else {
                    CapacitySend::Invalid
                }
            }
            Err(ResponseReserveError::Full) => CapacitySend::Full,
            Err(ResponseReserveError::Closed) => CapacitySend::Closed,
            Err(ResponseReserveError::TooLarge) => CapacitySend::Invalid,
        },
    }
}

/// Begin an asynchronous reservation after a submit returned `QUEUE_FULL`.
///
/// `encoded_len` is the exact combined framed-byte length of the failed submit.
/// This call itself never blocks. Once Tokio owns that byte charge, one item
/// slot, and queue capacity, `aster_reactor_poll` emits
/// `ASTER_EVENT_KIND_RESPONSE_WRITABLE` for the call. The binding then retries
/// the exact submit; that retry consumes the held combined permit and therefore
/// cannot return `QUEUE_FULL`. If cancellation or disconnect closes the channel
/// first, poll emits
/// `ASTER_EVENT_KIND_RESPONSE_CLOSED` instead.
///
/// Only one reservation may be pending per call, matching the ABI's
/// single-producer-per-call rule.
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_reserve_response_capacity(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
    call_id: u64,
    encoded_len: u64,
) -> i32 {
    if load_runtime(runtime).is_err() {
        return iroh_status_t::IROH_STATUS_NOT_FOUND as i32;
    }
    let state = match REACTORS.get(reactor) {
        Some(state) => state,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };
    let sender = {
        let senders = state.response_senders.lock().unwrap();
        match senders.get(&call_id) {
            Some(sender) => sender.clone(),
            None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
        }
    };
    let Ok(encoded_len) = usize::try_from(encoded_len) else {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    };
    if encoded_len > sender.byte_limit() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let token = state.next_reservation_token.fetch_add(1, Ordering::Relaxed);
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    {
        let mut reservations = state.response_reservations.lock().unwrap();
        if reservations.contains_key(&call_id) {
            return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
        }
        reservations.insert(
            call_id,
            ResponseReservation::Pending {
                token,
                cancel: cancel_tx,
            },
        );
    }

    let reserved_state = state.clone();
    state.rt_handle.spawn(async move {
        let permit = tokio::select! {
            permit = sender.reserve(encoded_len) => permit.ok(),
            _ = cancel_rx => None,
        };
        let Some(permit) = permit else {
            // `reap_call` owns notification when it cancelled the wait. An
            // ordinary closed receiver still has the Pending entry and is
            // reaped below, which emits RESPONSE_CLOSED.
            let still_pending = matches!(
                reserved_state
                    .response_reservations
                    .lock()
                    .unwrap()
                    .get(&call_id),
                Some(ResponseReservation::Pending { token: current, .. }) if *current == token
            );
            if still_pending {
                reap_call(&reserved_state, call_id);
            }
            return;
        };

        let installed = {
            let mut reservations = reserved_state.response_reservations.lock().unwrap();
            match reservations.remove(&call_id) {
                Some(ResponseReservation::Pending { token: current, .. }) if current == token => {
                    reservations.insert(call_id, ResponseReservation::Ready { permit });
                    true
                }
                Some(other) => {
                    reservations.insert(call_id, other);
                    false
                }
                None => false,
            }
        };
        if installed {
            let _ = reserved_state
                .response_capacity_tx
                .send(RingCall::response_capacity(call_id, true));
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Cancel a response-capacity wait whose binding thread was interrupted.
/// Drops either the pending reservation request or an already-held permit.
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_cancel_response_reservation(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
    call_id: u64,
) -> i32 {
    if load_runtime(runtime).is_err() {
        return iroh_status_t::IROH_STATUS_NOT_FOUND as i32;
    }
    let state = match REACTORS.get(reactor) {
        Some(state) => state,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };
    let reservation = state.response_reservations.lock().unwrap().remove(&call_id);
    match reservation {
        Some(ResponseReservation::Pending { cancel, .. }) => {
            let _ = cancel.send(());
            iroh_status_t::IROH_STATUS_OK as i32
        }
        Some(ResponseReservation::Ready { .. }) => iroh_status_t::IROH_STATUS_OK as i32,
        None => iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    }
}

/// Submit a unary response (single response frame + trailer) in one FFI call.
///
/// This is a convenience wrapper over `aster_reactor_submit_frame` plus
/// `aster_reactor_submit_trailer`. For server-streaming or bidi, call
/// `submit_frame` N times and then `submit_trailer` once.
///
/// After a successful call, `call_id` is no longer valid.
///
/// BACKPRESSURE: the per-call response channel is bounded, so this may return
/// `IROH_STATUS_QUEUE_FULL`. Nothing was copied, sent, or removed; the caller
/// retains its buffers and MUST retry the exact submit. This function never
/// blocks. A binding may use bounded backoff, or call
/// `aster_reactor_reserve_response_capacity` and wait for the corresponding
/// reactor-poll notification before retrying. A caller that ignores
/// `IROH_STATUS_QUEUE_FULL` drops its own response frame.
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

    // Clone rather than remove, and reserve before copying: identical
    // reasoning to `submit_trailer`. Removing first would make `QUEUE_FULL`
    // unretryable, turning a momentarily full channel into a lost response.
    let sender = {
        let map = state.response_senders.lock().unwrap();
        match map.get(&call_id) {
            Some(s) => s.clone(),
            None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
        }
    };

    let Some(encoded_len) = (response_len as usize).checked_add(trailer_len as usize) else {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    };
    let sent = send_with_capacity(&state, call_id, &sender, encoded_len, || {
        // Unary fast-path (mirrors the client-side `aster_call_unary` write):
        // concatenate `[response frame][trailer frame]` and ship as one
        // `CompleteUnary` message so the reactor task writes them with a
        // single `send.write_all` + one select iteration instead of two. Copy
        // directly into the reserved-size allocation: constructing two
        // temporary Vecs first would transiently pin twice the byte budget.
        let mut bundled = Vec::with_capacity(encoded_len);
        if !response_ptr.is_null() && response_len != 0 {
            bundled.extend_from_slice(unsafe {
                std::slice::from_raw_parts(response_ptr, response_len as usize)
            });
        }
        if !trailer_ptr.is_null() && trailer_len != 0 {
            bundled.extend_from_slice(unsafe {
                std::slice::from_raw_parts(trailer_ptr, trailer_len as usize)
            });
        }
        OutgoingFrame::CompleteUnary(bundled)
    });
    match sent {
        CapacitySend::Full => return iroh_status_t::IROH_STATUS_QUEUE_FULL as i32,
        CapacitySend::Closed => {
            reap_call(&state, call_id);
            return iroh_status_t::IROH_STATUS_NOT_FOUND as i32;
        }
        CapacitySend::Invalid => {
            return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
        }
        CapacitySend::Sent => {}
    }

    // Terminal: dropping the map's sender closes the channel once drained.
    reap_call(&state, call_id);

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Submit one streaming response frame for a call. May be called multiple
/// times per call; each call enqueues one Frame on the reactor's mpsc channel.
/// The call_id remains valid until `aster_reactor_submit_trailer` is called.
///
/// May return `IROH_STATUS_QUEUE_FULL`; see `aster_reactor_submit` for the
/// exact-retry and asynchronous-reservation contract. Never blocks.
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

    // Reserve BEFORE copying. On a full channel this returns without
    // allocating, without copying the caller's bytes, and without touching any
    // per-call state -- so `QUEUE_FULL` leaves the call exactly as it was and
    // the caller can retry with the same pointer it still owns.
    //
    // Never blocks. This runs on the caller's thread, which for the Java
    // binding is a virtual thread: blocking here would pin its carrier.
    let sent = send_with_capacity(&state, call_id, &sender, frame_len as usize, || {
        let frame = if frame_ptr.is_null() || frame_len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(frame_ptr, frame_len as usize).to_vec() }
        };
        OutgoingFrame::Frame(frame)
    });
    match sent {
        CapacitySend::Full => return iroh_status_t::IROH_STATUS_QUEUE_FULL as i32,
        CapacitySend::Closed => {
            // The call is over -- cancelled, or the peer went away. Reap this
            // call's state HERE: the maps are otherwise cleaned only on a
            // successful terminal submit, so a cancelled handler that never
            // gets to send a trailer would leave its sender, request receiver
            // and cancellation flag retained for the process's lifetime.
            reap_call(&state, call_id);
            return iroh_status_t::IROH_STATUS_NOT_FOUND as i32;
        }
        CapacitySend::Invalid => {
            return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
        }
        CapacitySend::Sent => {}
    }
    iroh_status_t::IROH_STATUS_OK as i32
}

/// Submit the trailer for a call and close the stream. After this call, the
/// `call_id` is no longer valid after a SUCCESSFUL call; on `QUEUE_FULL` it
/// remains valid and must be retried. The trailer payload may be empty — the
/// reactor still finishes the send side cleanly.
///
/// May return `IROH_STATUS_QUEUE_FULL`; see `aster_reactor_submit` for the
/// exact-retry and asynchronous-reservation contract. Terminal state is
/// removed only after capacity is owned, so a retry cannot turn into a
/// truncated stream.
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

    // CLONE rather than remove, then reserve. Removing first would make
    // `QUEUE_FULL` unretryable: the sender would be gone and the retry would
    // see NOT_FOUND, so a full channel would silently truncate the stream
    // instead of finishing it. Terminal state is torn down only once the
    // permit is in hand and the trailer is certain to go.
    let sender = {
        let map = state.response_senders.lock().unwrap();
        match map.get(&call_id) {
            Some(s) => s.clone(),
            None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
        }
    };

    let sent = send_with_capacity(&state, call_id, &sender, trailer_len as usize, || {
        let trailer = if trailer_ptr.is_null() || trailer_len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(trailer_ptr, trailer_len as usize).to_vec() }
        };
        OutgoingFrame::Trailer(trailer)
    });
    match sent {
        CapacitySend::Full => return iroh_status_t::IROH_STATUS_QUEUE_FULL as i32,
        CapacitySend::Closed => {
            reap_call(&state, call_id);
            return iroh_status_t::IROH_STATUS_NOT_FOUND as i32;
        }
        CapacitySend::Invalid => {
            return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
        }
        CapacitySend::Sent => {}
    }

    // Only now is the call finished: dropping the map's sender closes the
    // channel once drained.
    reap_call(&state, call_id);

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

/// Check whether the given call has been cancelled by the peer (or by a
/// stream-level error). Streaming dispatchers should poll this from any
/// long-running emit loop and stop early when it returns 1.
///
/// Returns:
///   0 — call is alive (no cancellation observed yet)
///   1 — call has been cancelled (peer sent FLAG_CANCEL or stream errored)
///   IROH_STATUS_NOT_FOUND (2) — the call_id is unknown: already completed, or
///       cancelled and reaped. A caller MUST treat this as terminal and stop,
///       exactly as it treats 1. It is NOT "alive".
///  other `iroh_status_t` codes — error. Note these are POSITIVE values; an
///       earlier version of this comment said error codes are <0, which led
///       callers to test `status == 1` and read NOT_FOUND as "keep going".
#[no_mangle]
pub unsafe extern "C" fn aster_reactor_check_cancelled(
    runtime: iroh_runtime_t,
    reactor: aster_reactor_t,
    call_id: u64,
) -> i32 {
    let state = match REACTORS.get(reactor) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };
    let map = state.cancelled_flags.lock().unwrap();
    // An unknown call_id is NOT_FOUND, as documented. Returning 0 -- "alive,
    // not cancelled" -- for a call that no longer exists told a polling emit
    // loop to keep going forever on a call that had already been reaped.
    match map.get(&call_id) {
        Some(flag) => i32::from(flag.load(std::sync::atomic::Ordering::Acquire)),
        None => iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{iroh_runtime_close, iroh_runtime_new};

    struct ReservationRig {
        runtime: iroh_runtime_t,
        reactor: aster_reactor_t,
        bridge: Arc<crate::BridgeRuntime>,
        response_rx: Option<reactor::ResponseReceiver>,
        _core_events: mpsc::Sender<ReactorEvent>,
        _non_rpc: mpsc::Sender<(Vec<u8>, aster_transport_core::CoreConnection)>,
    }

    impl ReservationRig {
        unsafe fn start_full(call_id: u64) -> Self {
            unsafe { Self::start_with_queued(call_id, 1, 1024, 1) }
        }

        unsafe fn start_with_queued(
            call_id: u64,
            item_limit: usize,
            byte_limit: usize,
            queued_len: usize,
        ) -> Self {
            let mut runtime = 0;
            assert_eq!(
                unsafe { iroh_runtime_new(ptr::null(), &mut runtime) },
                iroh_status_t::IROH_STATUS_OK as i32
            );
            let bridge = load_runtime(runtime).expect("runtime exists");
            let rt_handle = bridge.runtime.handle().clone();

            let (core_events, event_rx) = mpsc::channel(1);
            let (capacity_tx, capacity_rx) = mpsc::unbounded_channel();
            let (producer, consumer) = ring::spsc::<RingCall>(8);
            let buffers = Arc::new(BufferRegistry::new());
            let stopped = Arc::new(AtomicBool::new(false));
            rt_handle.spawn(pump_task(
                event_rx,
                capacity_rx,
                producer,
                buffers.clone(),
                bridge.clone(),
                stopped.clone(),
            ));

            let (response_tx, response_rx) =
                reactor::response_channel_with_limits(item_limit, byte_limit);
            response_tx
                .try_reserve(queued_len)
                .expect("reserve the initial capacity")
                .send(OutgoingFrame::Frame(vec![0; queued_len]))
                .expect("fill the response queue");
            let mut response_senders = std::collections::HashMap::new();
            response_senders.insert(call_id, response_tx);
            let (non_rpc, non_rpc_rx) = mpsc::channel(1);

            let reactor = REACTORS.insert(ReactorState {
                consumer: Mutex::new(consumer),
                response_senders: Mutex::new(response_senders),
                response_reservations: Mutex::new(std::collections::HashMap::new()),
                next_reservation_token: AtomicU64::new(1),
                response_capacity_tx: capacity_tx,
                request_receivers: Mutex::new(std::collections::HashMap::new()),
                cancelled_flags: Mutex::new(std::collections::HashMap::new()),
                stopped,
                blocking_poll_entries: AtomicUsize::new(0),
                buffers,
                rt_handle,
                non_rpc_rx: tokio::sync::Mutex::new(non_rpc_rx),
            });

            assert_eq!(
                REACTORS
                    .get(reactor)
                    .expect("reactor exists")
                    .response_senders
                    .lock()
                    .unwrap()
                    .get(&call_id)
                    .expect("sender exists")
                    .try_reserve(queued_len)
                    .err(),
                Some(ResponseReserveError::Full),
                "setup must exhaust at least one capacity dimension",
            );

            Self {
                runtime,
                reactor,
                bridge,
                response_rx: Some(response_rx),
                _core_events: core_events,
                _non_rpc: non_rpc,
            }
        }

        fn drain_response(&mut self) -> OutgoingFrame {
            let item = self
                .bridge
                .runtime
                .block_on(self.response_rx.as_mut().expect("receiver present").recv())
                .expect("response frame");
            let (frame, _lease) = item.into_parts();
            frame
        }

        unsafe fn poll_capacity(&self) -> aster_reactor_call_t {
            let mut event: aster_reactor_call_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { aster_reactor_poll(self.runtime, self.reactor, &mut event, 1, 10_000) },
                1,
                "capacity reservation did not produce a reactor event"
            );
            event
        }

        unsafe fn close(self) {
            assert_eq!(
                unsafe { aster_reactor_destroy(self.runtime, self.reactor) },
                iroh_status_t::IROH_STATUS_OK as i32
            );
            assert_eq!(
                unsafe { iroh_runtime_close(self.runtime) },
                iroh_status_t::IROH_STATUS_OK as i32
            );
        }
    }

    #[test]
    fn item_and_byte_refusals_happen_before_the_copy_closure() {
        unsafe {
            const CALL_ID: u64 = 40;
            for (item_limit, byte_limit, queued_len) in [(1, 1024, 1), (2, 4, 4)] {
                let rig =
                    ReservationRig::start_with_queued(CALL_ID, item_limit, byte_limit, queued_len);
                let state = REACTORS.get(rig.reactor).expect("reactor exists");
                let sender = state
                    .response_senders
                    .lock()
                    .unwrap()
                    .get(&CALL_ID)
                    .expect("sender exists")
                    .clone();
                let copied = AtomicBool::new(false);

                assert!(matches!(
                    send_with_capacity(&state, CALL_ID, &sender, 1, || {
                        copied.store(true, Ordering::Release);
                        OutgoingFrame::Frame(vec![9])
                    }),
                    CapacitySend::Full,
                ));
                assert!(
                    !copied.load(Ordering::Acquire),
                    "neither an exhausted item slot nor exhausted bytes may run the copy closure",
                );
                rig.close();
            }
        }
    }

    #[test]
    fn async_capacity_reservation_holds_a_real_permit_for_the_exact_retry() {
        unsafe {
            const CALL_ID: u64 = 41;
            let mut rig = ReservationRig::start_full(CALL_ID);
            assert_eq!(
                aster_reactor_reserve_response_capacity(rig.runtime, rig.reactor, CALL_ID, 3),
                iroh_status_t::IROH_STATUS_OK as i32
            );

            assert!(matches!(rig.drain_response(), OutgoingFrame::Frame(bytes) if bytes == [0]));
            let event = rig.poll_capacity();
            assert_eq!(event.event_kind, ASTER_EVENT_KIND_RESPONSE_WRITABLE);
            assert_eq!(event.call_id, CALL_ID);
            assert_eq!(
                (event.header_buffer, event.request_buffer, event.peer_buffer),
                (0, 0, 0)
            );

            let frame = [7u8, 8, 9];
            assert_eq!(
                aster_reactor_submit_frame(
                    rig.runtime,
                    rig.reactor,
                    CALL_ID,
                    frame.as_ptr(),
                    frame.len() as u32,
                ),
                iroh_status_t::IROH_STATUS_OK as i32,
                "the retry must consume the held permit, not race back to QUEUE_FULL"
            );
            assert!(matches!(rig.drain_response(), OutgoingFrame::Frame(bytes) if bytes == frame));
            rig.close();
        }
    }

    #[test]
    fn closed_channel_wakes_an_async_capacity_wait_as_closed() {
        unsafe {
            const CALL_ID: u64 = 42;
            let mut rig = ReservationRig::start_full(CALL_ID);
            assert_eq!(
                aster_reactor_reserve_response_capacity(rig.runtime, rig.reactor, CALL_ID, 1),
                iroh_status_t::IROH_STATUS_OK as i32
            );

            drop(rig.response_rx.take());
            let event = rig.poll_capacity();
            assert_eq!(event.event_kind, ASTER_EVENT_KIND_RESPONSE_CLOSED);
            assert_eq!(event.call_id, CALL_ID);
            assert_eq!(
                aster_reactor_submit_frame(rig.runtime, rig.reactor, CALL_ID, ptr::null(), 0),
                iroh_status_t::IROH_STATUS_NOT_FOUND as i32
            );
            rig.close();
        }
    }

    #[test]
    fn cancelling_a_capacity_wait_drops_it_without_consuming_channel_capacity() {
        unsafe {
            const CALL_ID: u64 = 43;
            let mut rig = ReservationRig::start_full(CALL_ID);
            assert_eq!(
                aster_reactor_reserve_response_capacity(rig.runtime, rig.reactor, CALL_ID, 2),
                iroh_status_t::IROH_STATUS_OK as i32
            );
            assert_eq!(
                aster_reactor_cancel_response_reservation(rig.runtime, rig.reactor, CALL_ID),
                iroh_status_t::IROH_STATUS_OK as i32
            );

            assert!(matches!(rig.drain_response(), OutgoingFrame::Frame(bytes) if bytes == [0]));
            let frame = [10u8, 11];
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let status = aster_reactor_submit_frame(
                    rig.runtime,
                    rig.reactor,
                    CALL_ID,
                    frame.as_ptr(),
                    frame.len() as u32,
                );
                if status == iroh_status_t::IROH_STATUS_OK as i32 {
                    break;
                }
                assert_eq!(
                    status,
                    iroh_status_t::IROH_STATUS_QUEUE_FULL as i32,
                    "a cancelled reservation may be briefly in flight, but must not fail otherwise"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "cancelling the wait leaked its pending reservation"
                );
                std::thread::yield_now();
            }
            assert!(matches!(rig.drain_response(), OutgoingFrame::Frame(bytes) if bytes == frame));
            rig.close();
        }
    }
}
