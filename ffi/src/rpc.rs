//! Payload-level RPC client (ABI 3.5) over `aster::rpc::RpcConnection`.
//!
//! Design: `docs/_internal/bindings/dynamic-dispatch-design.md` §3 and §4.4.
//! Rust builds the `StreamHeader`, frames, compresses and decodes trailers;
//! the binding passes `aster_call_options_t` plus request payloads and gets
//! response payloads plus an AEv1 `RpcStatus`.
//!
//! ```text
//!   aster_rpc_unary(conn, opts, req) → resp, status     pooled, one block_on
//!   aster_rpc_open(conn, pattern, opts) → call          streaming patterns
//!     aster_call_send_payload(call, payload, end)*
//!     aster_call_recv_payload(call, timeout) → payload | trailer
//!     aster_call_cancel(call)
//!   aster_call_release(call)                            frees; cancels if unfinished
//! ```
//!
//! A call's terminal outcome is always delivered as a trailer: an AEv1
//! `RpcStatus` (code 0 on success). Client-side failures map to codes too:
//! the deadline to `DEADLINE_EXCEEDED`, a cancel to `CANCELLED`, a lost
//! connection or pool exhaustion to `UNAVAILABLE`.

use std::future::Future;
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::rpc::{
    encode_stream_header, status_from_error, CallOptions, MethodPattern, RequestSink,
    ResponseStream, RpcConnection, RpcStatus, SerializationMode, StatusCode, StreamHeader,
};
use tokio::sync::Notify;

use crate::server::{decode_kv, ASTER_TIMEOUT_INFINITE};
use crate::{
    aster_buffer_t, aster_bytes_t, aster_connection_t, aster_runtime_t, aster_status_t,
    load_runtime, set_last_error, HandleRegistry,
};

// The call handle type and buffer leases live in `call.rs`.
use crate::call::{aster_call_t, CALL_BUFFERS};

/// Per-call options for `aster_rpc_unary` / `aster_rpc_open`. 72 bytes
/// (LP64). Set `struct_size = sizeof(aster_call_options_t)`.
#[repr(C)]
pub struct aster_call_options_t {
    pub struct_size: u32,
    /// Service version (`StreamHeader.version`).
    pub version: i32,
    /// Service name (UTF-8, required).
    pub service: aster_bytes_t,
    /// Method name (UTF-8, required).
    pub method: aster_bytes_t,
    /// Extra metadata as a k/v list (`u32 LE count`, then per entry
    /// `u32 LE len` + key, `u32 LE len` + value); `{NULL, 0}` = none.
    pub metadata: aster_bytes_t,
    /// 0 = shared; otherwise a session id from
    /// `aster_connection_next_session_id` (session-scoped services). Unary
    /// calls of one session share one pooled stream.
    pub session_id: u32,
    /// Total call budget in ms; 0 = none. Enforced client-side
    /// (`DEADLINE_EXCEEDED`) and sent as whole seconds, rounded up.
    pub deadline_ms: u32,
    /// `StreamHeader.serialization_mode`: 0 = XLANG (default), 1 = NATIVE,
    /// 2 = ROW.
    pub serialization_mode: i32,
    /// `StreamHeader.call_id` (logging / request id); 0 = none.
    pub wire_call_id: i32,
}

/// Lowered options: the header to send plus the `CallOptions` Rust applies.
fn lower_options(opts: *const aster_call_options_t) -> Result<(StreamHeader, CallOptions), i32> {
    if opts.is_null() {
        return Err(invalid("options are NULL"));
    }
    // SAFETY: non-null, caller-owned for the duration of the call.
    let o = unsafe { &*opts };
    if o.struct_size as usize != std::mem::size_of::<aster_call_options_t>() {
        return Err(invalid(format!(
            "aster_call_options_t.struct_size {} != {}",
            o.struct_size,
            std::mem::size_of::<aster_call_options_t>()
        )));
    }
    let text = |b: &aster_bytes_t, what: &str| -> Result<String, i32> {
        // SAFETY: caller passes `len` readable bytes.
        let raw = unsafe { view(b) };
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| invalid(format!("{what} is not UTF-8")))
    };
    let service = text(&o.service, "service")?;
    let method = text(&o.method, "method")?;
    if service.is_empty() || method.is_empty() {
        return Err(invalid("service and method are required"));
    }
    // SAFETY: as above.
    let metadata = decode_kv(unsafe { view(&o.metadata) }).map_err(invalid)?;
    let mode = match o.serialization_mode {
        0 => SerializationMode::Xlang,
        1 => SerializationMode::Native,
        2 => SerializationMode::Row,
        other => return Err(invalid(format!("unknown serialization_mode {other}"))),
    };
    let header = StreamHeader {
        service,
        method,
        version: o.version,
        call_id: o.wire_call_id,
        session_id: 0,
        ..Default::default()
    };
    let mut options = CallOptions::new().session(o.session_id);
    if mode != SerializationMode::Xlang {
        options = options.serialization_mode(mode);
    }
    if o.deadline_ms != 0 {
        options = options.deadline(Duration::from_millis(o.deadline_ms as u64));
    }
    options.metadata = metadata;
    Ok((header, options))
}

unsafe fn view<'a>(b: &aster_bytes_t) -> &'a [u8] {
    if b.ptr.is_null() || b.len == 0 {
        &[]
    } else {
        // SAFETY: caller-provided length.
        unsafe { std::slice::from_raw_parts(b.ptr, b.len) }
    }
}

fn invalid(msg: impl ToString) -> i32 {
    set_last_error(msg);
    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
}

/// The status a failed call reports: an `Error::Rpc` as sent (or as the
/// client produced it: deadline, cancel, pool), a lost connection as
/// `UNAVAILABLE`, anything else as `INTERNAL`.
fn error_status(e: &aster::Error) -> RpcStatus {
    match e {
        aster::Error::Rpc { .. } => status_from_error(e),
        aster::Error::Connection(_) | aster::Error::Transport(_) => {
            RpcStatus::error(StatusCode::Unavailable, e.to_string())
        }
        aster::Error::InvalidArgument(m) => RpcStatus::error(StatusCode::InvalidArgument, m),
        _ => status_from_error(e),
    }
}

fn status_bytes(st: &RpcStatus) -> Vec<u8> {
    aster::rpc::codec::encode_rpc_status(st).unwrap_or_default()
}

unsafe fn lease_out(
    bytes: Vec<u8>,
    out_ptr: *mut *const u8,
    out_len: *mut u32,
    out_buffer: *mut aster_buffer_t,
) {
    // SAFETY: the caller checked every out pointer.
    unsafe {
        if bytes.is_empty() {
            ptr::write(out_ptr, ptr::null());
            ptr::write(out_len, 0);
            ptr::write(out_buffer, 0);
        } else {
            let (id, arc) = CALL_BUFFERS.insert(bytes);
            ptr::write(out_ptr, arc.as_ptr());
            ptr::write(out_len, arc.len() as u32);
            ptr::write(out_buffer, id);
        }
    }
}

// ============================================================================
// Unary
// ============================================================================

/// One unary call on `connection`'s pooled streams: one write of header +
/// request, read to the trailer.
///
/// Returns `OK` once the call is over, whatever its outcome:
/// - `*out_status_code == 0`: `*out_resp_*` is the response (`{NULL,0}`
///   with buffer 0 for an empty one); no status bytes;
/// - otherwise the call failed: `*out_status_*` is the AEv1 `RpcStatus`
///   (message, details); no response.
///
/// Release non-zero buffers with `aster_call_buffer_release`. Bad options →
/// `INVALID_ARGUMENT`; unknown connection → `NOT_FOUND`. Blocks the calling
/// thread.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_rpc_unary(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    options: *const aster_call_options_t,
    request: *const u8,
    request_len: u32,
    out_resp_ptr: *mut *const u8,
    out_resp_len: *mut u32,
    out_resp_buffer: *mut aster_buffer_t,
    out_status_code: *mut i32,
    out_status_ptr: *mut *const u8,
    out_status_len: *mut u32,
    out_status_buffer: *mut aster_buffer_t,
) -> i32 {
    // SAFETY: forwarded caller contract.
    unsafe {
        unary_impl(
            runtime,
            connection,
            options,
            None,
            request,
            request_len,
            out_resp_ptr,
            out_resp_len,
            out_resp_buffer,
            out_status_code,
            out_status_ptr,
            out_status_len,
            out_status_buffer,
        )
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn unary_impl(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    options: *const aster_call_options_t,
    cancel: Option<Arc<CancelToken>>,
    request: *const u8,
    request_len: u32,
    out_resp_ptr: *mut *const u8,
    out_resp_len: *mut u32,
    out_resp_buffer: *mut aster_buffer_t,
    out_status_code: *mut i32,
    out_status_ptr: *mut *const u8,
    out_status_len: *mut u32,
    out_status_buffer: *mut aster_buffer_t,
) -> i32 {
    if out_resp_ptr.is_null()
        || out_resp_len.is_null()
        || out_resp_buffer.is_null()
        || out_status_code.is_null()
        || out_status_ptr.is_null()
        || out_status_len.is_null()
        || out_status_buffer.is_null()
    {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let Some(conn) = bridge.connections.get(connection) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let (header, opts) = match lower_options(options) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let request = if request.is_null() || request_len == 0 {
        Vec::new()
    } else {
        // SAFETY: caller passes `request_len` readable bytes.
        unsafe { std::slice::from_raw_parts(request, request_len as usize).to_vec() }
    };
    let rpc = RpcConnection::from_core_connection((*conn).clone());
    let result = match cancel {
        None => bridge
            .runtime
            .block_on(async move { rpc.unary_with(&header, request, &opts).await }),
        // Cancelled before it started: nothing goes on the wire.
        Some(t) if t.is_cancelled() => Err(cancelled_error()),
        Some(t) => bridge.runtime.block_on(async move {
            // Cancel wins a tie. Losing drops the call's future here, inside
            // the runtime, so `PooledCall`'s drop discards the pooled stream
            // and writes FLAG_CANCEL + reset from a task: exactly what
            // dropping the future does in Rust.
            tokio::select! {
                biased;
                () = t.cancelled() => Err(cancelled_error()),
                r = rpc.unary_with(&header, request, &opts) => r,
            }
        }),
    };
    // SAFETY: out pointers checked above.
    unsafe {
        match result {
            Ok(resp) => {
                ptr::write(out_status_code, 0);
                lease_out(resp, out_resp_ptr, out_resp_len, out_resp_buffer);
                lease_out(
                    Vec::new(),
                    out_status_ptr,
                    out_status_len,
                    out_status_buffer,
                );
            }
            Err(e) => {
                let st = error_status(&e);
                ptr::write(out_status_code, st.code);
                lease_out(Vec::new(), out_resp_ptr, out_resp_len, out_resp_buffer);
                lease_out(
                    status_bytes(&st),
                    out_status_ptr,
                    out_status_len,
                    out_status_buffer,
                );
            }
        }
    }
    aster_status_t::ASTER_STATUS_OK as i32
}

fn cancelled_error() -> aster::Error {
    aster::Error::rpc(StatusCode::Cancelled, "call cancelled")
}

// ============================================================================
// Cancellable unary (ABI 4.5)
// ============================================================================

/// A cancel token for one `aster_rpc_unary_cancellable` call (ABI 4.5).
/// Opaque, never reused; 0 = none.
pub type aster_cancel_token_t = u64;

/// One call's cancel signal. `cancel` may run before, during or after the
/// call; the flag makes a cancel that lands before the call starts stick.
pub(crate) struct CancelToken {
    cancelled: AtomicBool,
    /// Set by the call that took the token (a token serves one call).
    taken: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            taken: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            self.notify.notify_waiters();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Resolves once cancelled (at once if it already was). The waiter is
    /// registered before the flag is read, so a concurrent `cancel` (flag,
    /// then `notify_waiters`) is never missed.
    async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

type TokenShard = Mutex<std::collections::HashMap<u64, Arc<CancelToken>>>;

/// Live tokens, sharded so concurrent calls rarely meet on a lock: a call
/// costs one insert (`aster_cancel_token_new`), one lookup and one remove,
/// each on its own shard's mutex.
struct TokenTable {
    shards: Box<[TokenShard]>,
}

const TOKEN_SHARDS: usize = 64;

impl TokenTable {
    fn new() -> Self {
        Self {
            shards: (0..TOKEN_SHARDS).map(|_| Mutex::default()).collect(),
        }
    }

    fn shard(&self, id: u64) -> &TokenShard {
        &self.shards[(id as usize) % TOKEN_SHARDS]
    }

    fn insert(&self) -> u64 {
        let id = crate::next_handle_id();
        self.shard(id)
            .lock()
            .unwrap()
            .insert(id, Arc::new(CancelToken::new()));
        id
    }

    fn get(&self, id: u64) -> Option<Arc<CancelToken>> {
        self.shard(id).lock().unwrap().get(&id).cloned()
    }

    fn remove(&self, id: u64) -> Option<Arc<CancelToken>> {
        self.shard(id).lock().unwrap().remove(&id)
    }
}

static CANCEL_TOKENS: once_cell::sync::Lazy<TokenTable> =
    once_cell::sync::Lazy::new(TokenTable::new);

/// A new cancel token for one `aster_rpc_unary_cancellable` call. The call
/// consumes it (whatever its outcome); a token never passed to a call is
/// freed with `aster_cancel_token_release`. Never blocks.
#[no_mangle]
pub unsafe extern "C" fn aster_cancel_token_new(
    runtime: aster_runtime_t,
    out_token: *mut aster_cancel_token_t,
) -> i32 {
    let _ = runtime;
    if out_token.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    // SAFETY: checked non-null.
    unsafe { ptr::write(out_token, CANCEL_TOKENS.insert()) };
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Cancel the call `token` was (or will be) passed to. From any thread,
/// never blocks, idempotent.
///
/// - Before the call starts: the call returns `CANCELLED` without sending.
/// - While it runs: the call is abandoned like a dropped Rust future -- its
///   pooled stream is discarded and cancelled (`FLAG_CANCEL`, then a stream
///   reset), so the server sees the cancel; the connection and its pool stay
///   usable. The blocked `aster_rpc_unary_cancellable` returns `OK` with
///   `*out_status_code` = `CANCELLED` (or with the call's own outcome if
///   that came first: exactly one outcome either way).
///
/// `OK` while the token is live; `NOT_FOUND` once its call has returned (or
/// for a released / unknown token): nothing is left to cancel.
#[no_mangle]
pub extern "C" fn aster_cancel_token_cancel(
    runtime: aster_runtime_t,
    token: aster_cancel_token_t,
) -> i32 {
    let _ = runtime;
    match CANCEL_TOKENS.get(token) {
        Some(t) => {
            t.cancel();
            aster_status_t::ASTER_STATUS_OK as i32
        }
        None => aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    }
}

/// Free a token that was never passed to `aster_rpc_unary_cancellable`
/// (the call frees its own). `NOT_FOUND` if it is already gone.
#[no_mangle]
pub extern "C" fn aster_cancel_token_release(
    runtime: aster_runtime_t,
    token: aster_cancel_token_t,
) -> i32 {
    let _ = runtime;
    match CANCEL_TOKENS.remove(token) {
        Some(_) => aster_status_t::ASTER_STATUS_OK as i32,
        None => aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    }
}

/// Frees the token when the call returns, on every path.
struct TokenGuard(aster_cancel_token_t);

impl Drop for TokenGuard {
    fn drop(&mut self) {
        CANCEL_TOKENS.remove(self.0);
    }
}

/// `aster_rpc_unary` that `aster_cancel_token_cancel(token)` can abort (ABI
/// 4.5). Same pooled single-write path and results; `token` 0 makes it
/// exactly `aster_rpc_unary`.
///
/// The call consumes `token` on every return path (argument errors too),
/// except when it refuses the token itself: a token already taken by another
/// call, or unknown, is `INVALID_ARGUMENT` and stays as it was.
/// A cancelled call returns `OK` with `*out_status_code` = `CANCELLED` and
/// the AEv1 status "call cancelled". No completion event is involved: the
/// outcome is in the out-parameters, as for `aster_rpc_unary`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_rpc_unary_cancellable(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    options: *const aster_call_options_t,
    token: aster_cancel_token_t,
    request: *const u8,
    request_len: u32,
    out_resp_ptr: *mut *const u8,
    out_resp_len: *mut u32,
    out_resp_buffer: *mut aster_buffer_t,
    out_status_code: *mut i32,
    out_status_ptr: *mut *const u8,
    out_status_len: *mut u32,
    out_status_buffer: *mut aster_buffer_t,
) -> i32 {
    // The guard is armed only once this call owns the token: a call refused
    // for reusing a token must not free it under the call that owns it.
    let (cancel, _guard) = if token == 0 {
        (None, None)
    } else {
        match CANCEL_TOKENS.get(token) {
            Some(t) if !t.taken.swap(true, Ordering::AcqRel) => (Some(t), Some(TokenGuard(token))),
            Some(_) => return invalid("cancel token already taken by another call"),
            None => return invalid("unknown cancel token"),
        }
    };
    // SAFETY: forwarded caller contract.
    unsafe {
        unary_impl(
            runtime,
            connection,
            options,
            cancel,
            request,
            request_len,
            out_resp_ptr,
            out_resp_len,
            out_resp_buffer,
            out_status_code,
            out_status_ptr,
            out_status_len,
            out_status_buffer,
        )
    }
}

/// A fresh session id from `connection`'s counter (shared with every client
/// on the connection, Rust or binding, so ids never collide). The server
/// creates the session on the first call carrying it.
#[no_mangle]
pub unsafe extern "C" fn aster_connection_next_session_id(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_session_id: *mut u32,
) -> i32 {
    if out_session_id.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let Some(conn) = bridge.connections.get(connection) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    match conn.next_session_id() {
        Some(id) => {
            // SAFETY: checked non-null.
            unsafe { ptr::write(out_session_id, id) };
            aster_status_t::ASTER_STATUS_OK as i32
        }
        None => {
            set_last_error("session ids exhausted on this connection");
            aster_status_t::ASTER_STATUS_QUEUE_FULL as i32
        }
    }
}

// ============================================================================
// Streaming calls
// ============================================================================

type NextFuture =
    Pin<Box<dyn Future<Output = (ResponseStream, Option<aster::Result<Vec<u8>>>)> + Send>>;

#[derive(Default)]
struct Reader {
    stream: Option<ResponseStream>,
    /// A read that timed out, kept so the next `recv` resumes it (a frame
    /// read is not cancel-safe).
    pending: Option<NextFuture>,
    /// The trailer was handed out.
    done: bool,
}

pub(crate) struct RpcCall {
    rt: tokio::runtime::Handle,
    conn: RpcConnection,
    pattern: MethodPattern,
    /// Server-stream: the header waits for the request (one write).
    deferred: Mutex<Option<(StreamHeader, CallOptions)>>,
    /// Client-stream / bidi. An `Arc` so a cancel never waits behind a
    /// blocked send.
    sink: Mutex<Option<Arc<RequestSink>>>,
    reader: tokio::sync::Mutex<Reader>,
    /// The first failure seen by a send, a read or a cancel: what the
    /// trailer reports (a cancel triggered by a deadline still says
    /// `DEADLINE_EXCEEDED`).
    failure: Mutex<Option<RpcStatus>>,
    cancelled: AtomicBool,
    cancel: Notify,
}

impl RpcCall {
    fn fail_with(&self, st: RpcStatus) -> RpcStatus {
        self.failure.lock().unwrap().get_or_insert(st).clone()
    }
}

pub(crate) static RPC_CALLS: once_cell::sync::Lazy<HandleRegistry<RpcCall>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

/// Open a streaming call (`pattern` = `aster_method_pattern_t`: server-,
/// client- or bidi-stream). Client-stream and bidi open the stream and send
/// the header now; server-stream sends header + request together on the
/// first `aster_call_send_payload`. Release with `aster_call_release`.
#[no_mangle]
pub unsafe extern "C" fn aster_rpc_open(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    pattern: u8,
    options: *const aster_call_options_t,
    out_call: *mut aster_call_t,
) -> i32 {
    if out_call.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let Some(conn) = bridge.connections.get(connection) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let pattern = match pattern {
        1 => MethodPattern::ServerStream,
        2 => MethodPattern::ClientStream,
        3 => MethodPattern::BidiStream,
        _ => return invalid("aster_rpc_open: pattern must be a streaming pattern (1..=3)"),
    };
    let (header, opts) = match lower_options(options) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let rpc = RpcConnection::from_core_connection((*conn).clone());
    let rt = bridge.runtime.handle().clone();
    let call = RpcCall {
        rt: rt.clone(),
        conn: rpc.clone(),
        pattern,
        deferred: Mutex::new(None),
        sink: Mutex::new(None),
        reader: tokio::sync::Mutex::new(Reader::default()),
        failure: Mutex::new(None),
        cancelled: AtomicBool::new(false),
        cancel: Notify::new(),
    };
    if pattern == MethodPattern::ServerStream {
        *call.deferred.lock().unwrap() = Some((header, opts));
    } else {
        // Validate the header before opening (an encode failure is the
        // caller's fault, not a call outcome).
        if let Err(e) = encode_stream_header(&header) {
            return invalid(e);
        }
        match rt.block_on(rpc.bidi_open_with(&header, &opts)) {
            Ok((sink, stream)) => {
                *call.sink.lock().unwrap() = Some(Arc::new(sink));
                call.reader.try_lock().expect("unshared").stream = Some(stream);
            }
            Err(e) => {
                // Opening failed: the call exists, and its trailer says why.
                call.fail_with(error_status(&e));
            }
        }
    }
    let id = RPC_CALLS.insert(call);
    // SAFETY: checked non-null.
    unsafe { ptr::write(out_call, id) };
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Send one request payload. `end_stream` = 1 ends the request stream with
/// it; an empty payload with `end_stream` = 1 just ends it (client-stream /
/// bidi). Server-stream: exactly one payload (its `end_stream` is implied).
///
/// `OK`; `ALREADY_CLOSED` when the call is over (failed, cancelled or
/// answered; `aster_call_recv_payload` returns the trailer that says why);
/// `INVALID_ARGUMENT` for misuse (empty payload without end, a second
/// server-stream request, sending after the end). Blocks while the stream
/// is flow-controlled.
#[no_mangle]
pub unsafe extern "C" fn aster_call_send_payload(
    runtime: aster_runtime_t,
    call: aster_call_t,
    payload: *const u8,
    payload_len: u32,
    end_stream: u8,
) -> i32 {
    let _ = runtime;
    let Some(c) = RPC_CALLS.get(call) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let payload = if payload.is_null() || payload_len == 0 {
        Vec::new()
    } else {
        // SAFETY: caller passes `payload_len` readable bytes.
        unsafe { std::slice::from_raw_parts(payload, payload_len as usize).to_vec() }
    };
    if c.cancelled.load(Ordering::Acquire) || c.failure.lock().unwrap().is_some() {
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    }
    if c.pattern == MethodPattern::ServerStream {
        let Some((header, opts)) = c.deferred.lock().unwrap().take() else {
            return invalid("server-stream call already has its request");
        };
        let conn = c.conn.clone();
        let call = c.clone();
        return c.rt.block_on(async move {
            let opened = conn.server_stream_with(&header, payload, &opts).await;
            match opened {
                Ok(stream) => {
                    call.reader.lock().await.stream = Some(stream);
                    aster_status_t::ASTER_STATUS_OK as i32
                }
                Err(e) => {
                    call.fail_with(error_status(&e));
                    aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32
                }
            }
        });
    }
    let end = end_stream != 0;
    if payload.is_empty() && !end {
        return invalid("empty request payload (only allowed with end_stream)");
    }
    let Some(sink) = c.sink.lock().unwrap().clone() else {
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    };
    let call = c.clone();
    c.rt.block_on(async move {
        let sent = match (payload.is_empty(), end) {
            (true, _) => sink.finish().await,
            (false, true) => sink.send_last(payload).await,
            (false, false) => sink.send(payload).await,
        };
        match sent {
            Ok(()) => aster_status_t::ASTER_STATUS_OK as i32,
            Err(aster::Error::InvalidArgument(m)) if m.contains("already finished") => invalid(m),
            Err(e) => {
                call.fail_with(error_status(&e));
                aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32
            }
        }
    })
}

/// Send `count` request payloads (`payloads[i]` = `aster_bytes_t`) framed
/// into as few writes as possible; `end_stream` = 1 ends the request stream
/// with the last. Client-stream / bidi only. Results as for
/// `aster_call_send_payload`.
#[no_mangle]
pub unsafe extern "C" fn aster_call_send_payloads(
    runtime: aster_runtime_t,
    call: aster_call_t,
    payloads: *const aster_bytes_t,
    count: u32,
    end_stream: u8,
) -> i32 {
    let _ = runtime;
    let Some(c) = RPC_CALLS.get(call) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    if c.pattern == MethodPattern::ServerStream {
        return invalid("aster_call_send_payloads: not for server-stream calls");
    }
    if count > 0 && payloads.is_null() {
        return invalid("payloads is NULL");
    }
    let items: Vec<Vec<u8>> = (0..count as usize)
        // SAFETY: the caller passes `count` descriptors of readable bytes.
        .map(|i| unsafe { view(&*payloads.add(i)) }.to_vec())
        .collect();
    if c.cancelled.load(Ordering::Acquire) || c.failure.lock().unwrap().is_some() {
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    }
    let Some(sink) = c.sink.lock().unwrap().clone() else {
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    };
    let call = c.clone();
    c.rt.block_on(async move {
        match sink.send_many(items, end_stream != 0).await {
            Ok(()) => aster_status_t::ASTER_STATUS_OK as i32,
            Err(aster::Error::InvalidArgument(m)) => invalid(m),
            Err(e) => {
                call.fail_with(error_status(&e));
                aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32
            }
        }
    })
}

/// The next response payload, or the trailer. Waits up to `timeout_ms`
/// (0 = do not wait, `ASTER_TIMEOUT_INFINITE` = no limit; the call's own
/// deadline applies either way).
///
/// - `OK`, `*out_is_trailer == 0`: a payload (release `*out_buffer` with
///   `aster_call_buffer_release`);
/// - `OK`, `*out_is_trailer == 1`: the call is over; the bytes are its AEv1
///   `RpcStatus` (code 0 = success). Further reads → `ALREADY_CLOSED`;
/// - `TIMEOUT`: nothing yet (the read resumes on the next call);
/// - `INVALID_ARGUMENT`: a server-stream call whose request was not sent.
#[no_mangle]
pub unsafe extern "C" fn aster_call_recv_payload(
    runtime: aster_runtime_t,
    call: aster_call_t,
    timeout_ms: u32,
    out_ptr: *mut *const u8,
    out_len: *mut u32,
    out_buffer: *mut aster_buffer_t,
    out_is_trailer: *mut u8,
) -> i32 {
    let _ = runtime;
    if out_ptr.is_null() || out_len.is_null() || out_buffer.is_null() || out_is_trailer.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let Some(c) = RPC_CALLS.get(call) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    if c.pattern == MethodPattern::ServerStream && c.deferred.lock().unwrap().is_some() {
        return invalid("server-stream call: send the request first");
    }
    enum Out {
        Data(Vec<u8>),
        Trailer(RpcStatus),
        Timeout,
        Closed,
    }
    let call = c.clone();
    let out = c.rt.block_on(async move {
        let mut reader = call.reader.lock().await;
        if reader.done {
            return Out::Closed;
        }
        let cancelled = call.cancel.notified();
        tokio::pin!(cancelled);
        cancelled.as_mut().enable();
        let stopped = call.cancelled.load(Ordering::Acquire)
            || (reader.stream.is_none() && reader.pending.is_none());
        if stopped {
            reader.done = true;
            reader.pending = None;
            reader.stream = None;
            let st = call
                .failure
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| RpcStatus::error(StatusCode::Cancelled, "call cancelled"));
            return Out::Trailer(st);
        }
        // No timeout and nothing resumed: read in place (no boxed future).
        // Only a cancel interrupts it, and that ends the call anyway.
        if timeout_ms == ASTER_TIMEOUT_INFINITE && reader.pending.is_none() {
            let mut stream = reader.stream.take().expect("stream present");
            let item = tokio::select! {
                biased;
                r = stream.next() => r,
                () = &mut cancelled => {
                    drop(stream);
                    reader.done = true;
                    let st = call
                        .failure
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| RpcStatus::error(StatusCode::Cancelled, "call cancelled"));
                    return Out::Trailer(st);
                }
            };
            reader.stream = Some(stream);
            return match item {
                Some(Ok(payload)) => Out::Data(payload),
                None => {
                    reader.done = true;
                    Out::Trailer(RpcStatus::ok())
                }
                Some(Err(e)) => {
                    reader.done = true;
                    Out::Trailer(call.fail_with(error_status(&e)))
                }
            };
        }
        let mut next = match reader.pending.take() {
            Some(f) => f,
            None => {
                let mut stream = reader.stream.take().expect("stream present");
                Box::pin(async move {
                    let r = stream.next().await;
                    (stream, r)
                }) as NextFuture
            }
        };
        let limit = match timeout_ms {
            ASTER_TIMEOUT_INFINITE => None,
            ms => Some(Duration::from_millis(ms as u64)),
        };
        let polled = tokio::select! {
            biased;
            r = &mut next => Some(r),
            () = &mut cancelled => {
                // Dropping the read and the stream cancels the call.
                drop(next);
                reader.done = true;
                let st = call
                    .failure
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| RpcStatus::error(StatusCode::Cancelled, "call cancelled"));
                return Out::Trailer(st);
            }
            () = sleep_for(limit) => None,
        };
        let Some((stream, item)) = polled else {
            reader.pending = Some(next);
            return Out::Timeout;
        };
        reader.stream = Some(stream);
        match item {
            Some(Ok(payload)) => Out::Data(payload),
            None => {
                reader.done = true;
                Out::Trailer(RpcStatus::ok())
            }
            Some(Err(e)) => {
                reader.done = true;
                Out::Trailer(call.fail_with(error_status(&e)))
            }
        }
    });
    // SAFETY: out pointers checked above.
    unsafe {
        match out {
            Out::Data(payload) => {
                ptr::write(out_is_trailer, 0);
                if payload.is_empty() {
                    // Keep "payload" distinguishable: a real (empty) lease.
                    let (id, _) = CALL_BUFFERS.insert(Vec::new());
                    ptr::write(out_ptr, ptr::null());
                    ptr::write(out_len, 0);
                    ptr::write(out_buffer, id);
                } else {
                    lease_out(payload, out_ptr, out_len, out_buffer);
                }
                aster_status_t::ASTER_STATUS_OK as i32
            }
            Out::Trailer(st) => {
                ptr::write(out_is_trailer, 1);
                lease_out(status_bytes(&st), out_ptr, out_len, out_buffer);
                aster_status_t::ASTER_STATUS_OK as i32
            }
            Out::Timeout => aster_status_t::ASTER_STATUS_TIMEOUT as i32,
            Out::Closed => aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32,
        }
    }
}

async fn sleep_for(limit: Option<Duration>) {
    match limit {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

/// Cancel the call: `FLAG_CANCEL`, then a stream reset. A blocked
/// `aster_call_recv_payload` returns the `CANCELLED` trailer; later sends
/// return `ALREADY_CLOSED`. A no-op once the call is over.
#[no_mangle]
pub extern "C" fn aster_call_cancel(runtime: aster_runtime_t, call: aster_call_t) -> i32 {
    let _ = runtime;
    let Some(c) = RPC_CALLS.get(call) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    cancel(&c);
    aster_status_t::ASTER_STATUS_OK as i32
}

fn cancel(c: &Arc<RpcCall>) {
    if c.cancelled.swap(true, Ordering::AcqRel) {
        return;
    }
    c.fail_with(RpcStatus::error(StatusCode::Cancelled, "call cancelled"));
    c.deferred.lock().unwrap().take();
    c.cancel.notify_waiters();
    let call = c.clone();
    let sink = c.sink.lock().unwrap().clone();
    c.rt.block_on(async move {
        if let Some(sink) = sink {
            sink.cancel().await;
        }
        let mut reader = call.reader.lock().await;
        reader.pending = None;
        if let Some(mut stream) = reader.stream.take() {
            stream.cancel().await;
        }
    });
}

/// Free a streaming call (from `aster_call_release` / `aster_call_discard`).
/// Unfinished calls are cancelled. Returns false for a handle that is not an
/// RPC call.
pub(crate) fn release(call: aster_call_t) -> bool {
    let Some(c) = RPC_CALLS.remove(call) else {
        return false;
    };
    let rt = c.rt.clone();
    // Dropping an unfinished sink / stream cancels from a runtime task.
    let _enter = rt.enter();
    drop(c);
    true
}
