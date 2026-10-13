//! Raw WebTransport ABI 4.17. Byte streams are unframed and independent of RPC.
//! Inputs are copied before submission; runtime-local owners never alias Iroh handles.
use crate::upcall::{aster_upcalls_t, upcalls_for};
use crate::{
    alloc_owned_bytes, aster_bytes_t, aster_event_kind_t, aster_operation_t, aster_runtime_t,
    aster_status_t, load_runtime, set_last_error, BridgeRuntime, EventInternal,
};
use aster::binding_support::upcall::auth::{ForeignAuthenticator, ForeignAuthenticatorOptions};
use aster_transport_salvo::raw_webtransport::{RawWtConfig, RawWtListener, RawWtStream};
use std::{
    future::Future,
    ptr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

/// Runtime-local owned raw listener, session, and directional stream handles.
pub type aster_raw_wt_listener_t = u64;
pub type aster_raw_wt_session_t = u64;
pub type aster_raw_wt_stream_t = u64;

/// ABI 4.17: inputs copied before start. struct_size must equal sizeof; flags/reserved zero.
/// Exact path is not normalized. Zero limits select the shared adapter defaults.
#[repr(C)]
pub struct aster_raw_wt_config_t {
    pub struct_size: u32,
    pub flags: u32,
    pub bind_addr: aster_bytes_t,
    pub certificate_pem: aster_bytes_t,
    pub private_key_pem: aster_bytes_t,
    pub path: aster_bytes_t,
    pub max_sessions: u32,
    pub max_streams: u32,
    pub max_datagrams: u32,
    pub max_datagram_bytes: u32,
    pub max_read_bytes: u32,
    pub max_pending_auth: u32,
    pub max_auth_payload_bytes: u32,
    pub reserved: u32,
}
pub(crate) struct StreamState {
    inner: RawWtStream,
    closed: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}
impl StreamState {
    fn new(inner: RawWtStream) -> Self {
        Self {
            inner,
            closed: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}
async fn stream_wait<F, T>(v: Arc<StreamState>, future: F) -> Result<T, (aster_status_t, String)>
where
    F: Future<Output = Result<T, String>>,
{
    let closed = v.notify.notified();
    tokio::pin!(closed);
    closed.as_mut().enable();
    if v.closed.load(Ordering::Acquire) {
        return Err((
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
            "stream freed".into(),
        ));
    }
    tokio::select! {biased;_= &mut closed=>Err((aster_status_t::ASTER_STATUS_ALREADY_CLOSED,"stream freed".into())),result=future=>result.map_err(error)}
}
const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
fn fail(status: aster_status_t, message: impl ToString) -> i32 {
    set_last_error(message);
    status as i32
}
fn bridge(runtime: u64) -> Result<Arc<BridgeRuntime>, i32> {
    load_runtime(runtime).map_err(|s| s as i32)
}
unsafe fn bytes(input: aster_bytes_t) -> Result<Vec<u8>, i32> {
    if input.len > 16 * 1024 * 1024 {
        return Err(fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "raw WebTransport input exceeds 16 MiB",
        ));
    }
    if input.len == 0 {
        return Ok(Vec::new());
    }
    if input.ptr.is_null() {
        return Err(fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "NULL nonempty span",
        ));
    }
    Ok(unsafe { std::slice::from_raw_parts(input.ptr, input.len) }.to_vec())
}
unsafe fn text(input: aster_bytes_t) -> Result<String, i32> {
    String::from_utf8(unsafe { bytes(input)? })
        .map_err(|e| fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, e))
}
fn error(message: String) -> (aster_status_t, String) {
    // Adapter validation strings are local invariants, never parse remote errors.
    let status = match message.as_str() {
        "stream has no receive direction"
        | "stream has no send direction"
        | "write exceeds configured limit"
        | "read exceeds configured limit"
        | "datagram exceeds session limit" => aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
        "session closed" | "listener closed" => aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
        "send reset" | "receive stopped" => aster_status_t::ASTER_STATUS_STREAM_RESET,
        _ => aster_status_t::ASTER_STATUS_INTERNAL,
    };
    (status, message)
}
struct Completion {
    kind: aster_event_kind_t,
    handle: u64,
    flags: u32,
    data: Option<Vec<u8>>,
    discard: Option<Box<dyn FnOnce() + Send>>,
}
impl Completion {
    fn new(kind: aster_event_kind_t, handle: u64) -> Self {
        Self {
            kind,
            handle,
            flags: 0,
            data: None,
            discard: None,
        }
    }
    fn data(mut self, data: Vec<u8>) -> Self {
        self.data = Some(data);
        self
    }
    fn flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }
    fn discard(mut self, free: impl FnOnce() + Send + 'static) -> Self {
        self.discard = Some(Box::new(free));
        self
    }
}
/// Every pending future races cancellation AND runtime shutdown; admission uses the
/// runtime's bounded operation permit. Register Notify before reading atomics.
unsafe fn submit<F>(b: &Arc<BridgeRuntime>, user: u64, out: *mut u64, future: F) -> i32
where
    F: Future<Output = Result<Completion, (aster_status_t, String)>> + Send + 'static,
{
    if out.is_null() {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "NULL operation output",
        );
    }
    unsafe { ptr::write(out, 0) };
    let (id, cancelled) = match b.new_operation(user) {
        Ok(v) => v,
        Err(s) => return s as i32,
    };
    let Some(op) = b.operations.get(id) else {
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    };
    let notify = op.cancel_notify.clone();
    unsafe { ptr::write(out, id) };
    let b = b.clone();
    b.runtime.spawn({let b=b.clone();async move {
  let cancel=notify.notified();let closing=b.close_notify.notified();tokio::pin!(cancel,closing);
  cancel.as_mut().enable();closing.as_mut().enable();
  if cancelled.load(Ordering::Acquire)||b.closing.load(Ordering::Acquire){return;}
  let result=tokio::select!{biased;_= &mut cancel=>return,_=&mut closing=>return,result=future=>result};
  match result {
   Ok(done)=>{let mut event=EventInternal::new(done.kind,aster_status_t::ASTER_STATUS_OK,id,done.handle,0,user,0);event.flags=done.flags;
    if !b.complete(event,done.data){if let Some(free)=done.discard{free();}}}
   Err((status,message))=>b.emit_error_flagged(id,user,status,status as i32,0,&message),
  }
 }});
    OK
}
unsafe fn start(
    runtime: u64,
    upcalls: Option<u64>,
    config: *const aster_raw_wt_config_t,
    out: *mut u64,
) -> i32 {
    if config.is_null() || out.is_null() {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "NULL start argument",
        );
    }
    unsafe { ptr::write(out, 0) };
    if let Some(s) = crate::http_server::blocking_on_runtime("raw WebTransport start") {
        return s;
    }
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let c = unsafe { &*config };
    if c.struct_size as usize != std::mem::size_of::<aster_raw_wt_config_t>()
        || c.flags != 0
        || c.reserved != 0
    {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "invalid raw WebTransport config",
        );
    }
    if [
        c.bind_addr.len,
        c.certificate_pem.len,
        c.private_key_pem.len,
        c.path.len,
    ]
    .into_iter()
    .any(|n| n > 64 * 1024)
    {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "raw listener config span exceeds 64 KiB",
        );
    }
    let copied = unsafe {
        (
            text(c.bind_addr),
            bytes(c.certificate_pem),
            bytes(c.private_key_pem),
            text(c.path),
        )
    };
    let (addr, cert, key, path) = match copied {
        (Ok(a), Ok(c), Ok(k), Ok(p)) => (a, c, k, p),
        (Err(s), _, _, _) | (_, Err(s), _, _) | (_, _, Err(s), _) | (_, _, _, Err(s)) => return s,
    };
    let mut opts = RawWtConfig::new(addr, cert, key, path);
    if c.max_sessions != 0 {
        opts.max_sessions = c.max_sessions as usize;
    }
    if c.max_streams != 0 {
        opts.max_streams = c.max_streams as usize;
    }
    if c.max_datagrams != 0 {
        opts.max_datagrams = c.max_datagrams as usize;
    }
    if c.max_datagram_bytes != 0 {
        opts.max_datagram_bytes = c.max_datagram_bytes as usize;
    }
    if c.max_read_bytes != 0 {
        opts.max_read_bytes = c.max_read_bytes as usize;
    }
    let listener = if let Some(upcalls) = upcalls {
        let upcalls = match upcalls_for(upcalls) {
            Ok(v) if v.runtime_owner == runtime => v,
            Ok(_) => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
            Err(s) => return s,
        };
        if upcalls.upcalls.is_closed() {
            return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
        }
        let defaults = ForeignAuthenticatorOptions::default();
        let auth = match ForeignAuthenticator::with_options(
            upcalls.upcalls.clone(),
            ForeignAuthenticatorOptions {
                max_pending: if c.max_pending_auth == 0 {
                    defaults.max_pending
                } else {
                    c.max_pending_auth as usize
                },
                max_payload_bytes: if c.max_auth_payload_bytes == 0 {
                    defaults.max_payload_bytes
                } else {
                    c.max_auth_payload_bytes as usize
                },
            },
        ) {
            Ok(v) => Arc::new(v),
            Err(e) => return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, e),
        };
        b.runtime
            .block_on(RawWtListener::start_authenticated(opts, auth))
    } else {
        b.runtime.block_on(RawWtListener::start_public(opts))
    };
    let listener = match listener {
        Ok(v) => v,
        Err(e) => return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, e),
    };
    if b.closing.load(Ordering::Acquire) {
        let _ = b.runtime.block_on(listener.stop(Duration::ZERO));
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    }
    let id = b.raw_wt_listeners.insert(listener);
    if b.closing.load(Ordering::Acquire) {
        if let Some(v) = b.raw_wt_listeners.remove(id) {
            let _ = b.runtime.block_on(v.stop(Duration::ZERO));
        }
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    }
    unsafe { ptr::write(out, id) };
    OK
}
/// Bind ready TCP+QUIC listener; foreign auth runs before upgrade (kind80).
/// Blocking: call off Tokio workers. All spans copied; malformed input INVALID_ARGUMENT.
/// # Safety
/// config/out valid; config spans readable during call.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_listener_start(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
    config: *const aster_raw_wt_config_t,
    out: *mut aster_raw_wt_listener_t,
) -> i32 {
    unsafe { start(runtime, Some(upcalls), config, out) }
}
/// Explicit public raw listener; transport provides no authenticated identity.
/// # Safety
/// Same input/output contract as authenticated start.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_listener_start_public(
    runtime: aster_runtime_t,
    config: *const aster_raw_wt_config_t,
    out: *mut aster_raw_wt_listener_t,
) -> i32 {
    unsafe { start(runtime, None, config, out) }
}
/// Owned version1 JSON {tcp_addr,quic_addr,path?}; release with string_release.
/// # Safety
/// out writable for one span.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_listener_addr(
    runtime: u64,
    listener: u64,
    out: *mut aster_bytes_t,
) -> i32 {
    if out.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    unsafe { ptr::write(out, alloc_owned_bytes(Vec::new())) };
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_listeners.get(listener) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let data = serde_json::json!({"version":1,"tcp_addr":v.tcp_addr().to_string(),"quic_addr":v.quic_addr().to_string()});
    unsafe { ptr::write(out, alloc_owned_bytes(data.to_string().into_bytes())) };
    OK
}
/// Stops and joins the listener; consumed on success. Repeated stop NOT_FOUND.
/// Blocking off Tokio workers; grace milliseconds bounded by u32.
#[no_mangle]
pub extern "C" fn aster_raw_wt_listener_stop(runtime: u64, listener: u64, grace_ms: u32) -> i32 {
    if let Some(s) = crate::http_server::blocking_on_runtime("raw WebTransport stop") {
        return s;
    }
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_listeners.remove(listener) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    match b
        .runtime
        .block_on(v.stop(Duration::from_millis(grace_ms as u64)))
    {
        Ok(()) => OK,
        Err(e) => fail(aster_status_t::ASTER_STATUS_INTERNAL, e),
    }
}
/// Accept one session. SESSION event121 handle owns session; late cancelled result closes it.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_listener_accept(
    runtime: u64,
    listener: u64,
    user_data: u64,
    out: *mut aster_operation_t,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_listeners.get(listener) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = b.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            let value = v.accept().await.map_err(error)?;
            let id = owner.raw_wt_sessions.insert(value);
            let cleanup = owner.clone();
            Ok(
                Completion::new(aster_event_kind_t::ASTER_EVENT_RAW_WT_SESSION, id).discard(
                    move || {
                        if let Some(v) = cleanup.raw_wt_sessions.remove(id) {
                            let _ = v.close(0, b"cancelled accept");
                        }
                    },
                ),
            )
        })
    }
}
/// Synchronously close actual QUIC session; retains owner for closed/info until free. Child streams terminate.
/// Reason is copied before return; code is uint32 WebTransport application code.
/// # Safety
/// reason readable for length.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_close(
    runtime: u64,
    session: u64,
    code: u32,
    reason: aster_bytes_t,
) -> i32 {
    let reason = match unsafe { bytes(reason) } {
        Ok(v) => v,
        Err(s) => return s,
    };
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    match v.close(code, &reason) {
        Ok(()) => OK,
        Err(e) => fail(aster_status_t::ASTER_STATUS_INTERNAL, e),
    }
}
/// Free closes the session; repeated free NOT_FOUND (wrappers make Dispose idempotent).
#[no_mangle]
pub extern "C" fn aster_raw_wt_session_free(runtime: u64, session: u64) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.remove(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let _ = v.close(0, b"owner freed");
    OK
}
/// Free stream owner; dropping halves resets/stops unfinished directions. NOT_FOUND if repeated.
#[no_mangle]
pub extern "C" fn aster_raw_wt_stream_free(runtime: u64, stream: u64) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    if let Some(v) = b.raw_wt_streams.remove(stream) {
        v.close();
        OK
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

/// Returns one owned stream(event122); flags bit0 send, bit1 receive. Raw/unframed.
/// Cancellation discards/reset-stops an unseen stream owner.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_open_bi(
    runtime: u64,
    session: u64,
    user_data: u64,
    out: *mut aster_operation_t,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = b.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            let stream = v.open_bi().await.map_err(error)?;
            let info = stream.info();
            let flags = u32::from(info.has_send) | (u32::from(info.has_receive) << 1);
            let id = owner.raw_wt_streams.insert(StreamState::new(stream));
            let cleanup = owner.clone();
            Ok(
                Completion::new(aster_event_kind_t::ASTER_EVENT_RAW_WT_STREAM, id)
                    .flags(flags)
                    .discard(move || {
                        if let Some(v) = cleanup.raw_wt_streams.remove(id) {
                            v.close();
                        }
                    }),
            )
        })
    }
}

/// Returns one owned stream(event122); flags bit0 send, bit1 receive. Raw/unframed.
/// Cancellation discards/reset-stops an unseen stream owner.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_open_uni(
    runtime: u64,
    session: u64,
    user_data: u64,
    out: *mut aster_operation_t,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = b.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            let stream = v.open_uni().await.map_err(error)?;
            let info = stream.info();
            let flags = u32::from(info.has_send) | (u32::from(info.has_receive) << 1);
            let id = owner.raw_wt_streams.insert(StreamState::new(stream));
            let cleanup = owner.clone();
            Ok(
                Completion::new(aster_event_kind_t::ASTER_EVENT_RAW_WT_STREAM, id)
                    .flags(flags)
                    .discard(move || {
                        if let Some(v) = cleanup.raw_wt_streams.remove(id) {
                            v.close();
                        }
                    }),
            )
        })
    }
}

/// Returns one owned stream(event122); flags bit0 send, bit1 receive. Raw/unframed.
/// Cancellation discards/reset-stops an unseen stream owner.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_accept_bi(
    runtime: u64,
    session: u64,
    user_data: u64,
    out: *mut aster_operation_t,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = b.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            let stream = v.accept_bi().await.map_err(error)?;
            let info = stream.info();
            let flags = u32::from(info.has_send) | (u32::from(info.has_receive) << 1);
            let id = owner.raw_wt_streams.insert(StreamState::new(stream));
            let cleanup = owner.clone();
            Ok(
                Completion::new(aster_event_kind_t::ASTER_EVENT_RAW_WT_STREAM, id)
                    .flags(flags)
                    .discard(move || {
                        if let Some(v) = cleanup.raw_wt_streams.remove(id) {
                            v.close();
                        }
                    }),
            )
        })
    }
}

/// Returns one owned stream(event122); flags bit0 send, bit1 receive. Raw/unframed.
/// Cancellation discards/reset-stops an unseen stream owner.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_accept_uni(
    runtime: u64,
    session: u64,
    user_data: u64,
    out: *mut aster_operation_t,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = b.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            let stream = v.accept_uni().await.map_err(error)?;
            let info = stream.info();
            let flags = u32::from(info.has_send) | (u32::from(info.has_receive) << 1);
            let id = owner.raw_wt_streams.insert(StreamState::new(stream));
            let cleanup = owner.clone();
            Ok(
                Completion::new(aster_event_kind_t::ASTER_EVENT_RAW_WT_STREAM, id)
                    .flags(flags)
                    .discard(move || {
                        if let Some(v) = cleanup.raw_wt_streams.remove(id) {
                            v.close();
                        }
                    }),
            )
        })
    }
}

/// Version1 JSON snapshot; owned span released with string_release. No native pointer fields.
/// # Safety
/// out writable for one span.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_info(
    runtime: u64,
    session: u64,
    out: *mut aster_bytes_t,
) -> i32 {
    if out.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    unsafe { ptr::write(out, alloc_owned_bytes(Vec::new())) };
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let mut json = match serde_json::to_value(v.info()) {
        Ok(v) => v,
        Err(e) => return fail(aster_status_t::ASTER_STATUS_INTERNAL, e),
    };
    json["version"] = serde_json::json!(1);
    unsafe { ptr::write(out, alloc_owned_bytes(json.to_string().into_bytes())) };
    OK
}

/// Version1 JSON snapshot; owned span released with string_release. No native pointer fields.
/// # Safety
/// out writable for one span.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_stats(
    runtime: u64,
    session: u64,
    out: *mut aster_bytes_t,
) -> i32 {
    if out.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    unsafe { ptr::write(out, alloc_owned_bytes(Vec::new())) };
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let mut json = match serde_json::to_value(v.stats()) {
        Ok(v) => v,
        Err(e) => return fail(aster_status_t::ASTER_STATUS_INTERNAL, e),
    };
    json["version"] = serde_json::json!(1);
    unsafe { ptr::write(out, alloc_owned_bytes(json.to_string().into_bytes())) };
    OK
}

/// Wait session close; event125 owns version1 JSON {kind,code?,reason} payload.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_closed(
    runtime: u64,
    session: u64,
    user_data: u64,
    out: *mut u64,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    unsafe {
        submit(&b, user_data, out, async move {
            let mut json =
                serde_json::to_value(v.closed().await).map_err(|e| error(e.to_string()))?;
            json["version"] = serde_json::json!(1);
            Ok(
                Completion::new(aster_event_kind_t::ASTER_EVENT_RAW_WT_CLOSED, session)
                    .data(json.to_string().into_bytes()),
            )
        })
    }
}
/// Send datagram synchronously; errors never truncate. Empty datagrams are valid.
/// # Safety
/// data span readable during call.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_send_datagram(
    runtime: u64,
    session: u64,
    data: aster_bytes_t,
) -> i32 {
    let data = match unsafe { bytes(data) } {
        Ok(v) => v,
        Err(s) => return s,
    };
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    match v.send_datagram(&data) {
        Ok(()) => OK,
        Err(e) => {
            let (status, message) = error(e);
            fail(status, message)
        }
    }
}
/// One datagram per event124; payload may be empty; release nonzero event.buffer always.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_receive_datagram(
    runtime: u64,
    session: u64,
    user_data: u64,
    out: *mut u64,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    unsafe {
        submit(&b, user_data, out, async move {
            Ok(
                Completion::new(aster_event_kind_t::ASTER_EVENT_RAW_WT_DATAGRAM, session)
                    .data(v.receive_datagram().await.map_err(error)?),
            )
        })
    }
}
/// Current maximum datagram payload; is_some0 means unavailable, size0. Never encode None as zero-size support.
/// # Safety
/// both outputs writable.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_session_max_datagram_size(
    runtime: u64,
    session: u64,
    out_size: *mut u64,
    out_is_some: *mut u32,
) -> i32 {
    if out_size.is_null() || out_is_some.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    unsafe {
        ptr::write(out_size, 0);
        ptr::write(out_is_some, 0)
    };
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_sessions.get(session) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    if let Some(size) = v.max_datagram_size() {
        unsafe {
            ptr::write(out_size, size as u64);
            ptr::write(out_is_some, 1)
        }
    }
    OK
}
/// Read at most max_bytes, nonzero and bounded by configured max_read_bytes. Event123 payload raw.
/// flags bit0 EOF; empty payload+EOF distinct from an empty datagram. No framing or read-to-end allocation.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_stream_read(
    runtime: u64,
    stream: u64,
    max_bytes: u32,
    user_data: u64,
    out: *mut u64,
) -> i32 {
    if max_bytes == 0 {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "zero read limit",
        );
    }
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_streams.get(stream) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = v.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            let data = stream_wait(v, owner.inner.read(max_bytes as usize)).await?;
            let eof = u32::from(data.is_empty());
            Ok(
                Completion::new(aster_event_kind_t::ASTER_EVENT_RAW_WT_READ, stream)
                    .flags(eof)
                    .data(data),
            )
        })
    }
}
/// Raw write-all; 16 MiB absolute input cap before copying, configured max_read_bytes transmission cap.
/// Input copied before submission. Cancellation can leave a written prefix; never retry automatically.
/// # Safety
/// input span readable during call; out writable.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_stream_write(
    runtime: u64,
    stream: u64,
    data: aster_bytes_t,
    user_data: u64,
    out: *mut u64,
) -> i32 {
    let data = match unsafe { bytes(data) } {
        Ok(v) => v,
        Err(s) => return s,
    };
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_streams.get(stream) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = v.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            stream_wait(v, owner.inner.write(&data)).await?;
            Ok(Completion::new(
                aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                stream,
            ))
        })
    }
}

/// Async stream finish; direction errors arrive as ERROR99. UNIT92 on success.
/// FIN retains send owner for transport_acked or reset; never free on FIN.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_stream_finish(
    runtime: u64,
    stream: u64,
    user_data: u64,
    out: *mut u64,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_streams.get(stream) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = v.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            stream_wait(v, owner.inner.finish()).await?;
            Ok(Completion::new(
                aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                stream,
            ))
        })
    }
}

/// Async stream reset; direction errors arrive as ERROR99. UNIT92 on success.
/// FIN retains send owner for transport_acked or reset; never free on FIN.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_stream_reset(
    runtime: u64,
    stream: u64,
    code: u32,
    user_data: u64,
    out: *mut u64,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_streams.get(stream) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = v.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            stream_wait(v, owner.inner.reset(code)).await?;
            Ok(Completion::new(
                aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                stream,
            ))
        })
    }
}

/// Async stream stop; direction errors arrive as ERROR99. UNIT92 on success.
/// FIN retains send owner for transport_acked or reset; never free on FIN.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_stream_stop(
    runtime: u64,
    stream: u64,
    code: u32,
    user_data: u64,
    out: *mut u64,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_streams.get(stream) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = v.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            stream_wait(v, owner.inner.stop(code)).await?;
            Ok(Completion::new(
                aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                stream,
            ))
        })
    }
}

/// Async stream set_priority; direction errors arrive as ERROR99. UNIT92 on success.
/// FIN retains send owner for transport_acked or reset; never free on FIN.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_stream_set_priority(
    runtime: u64,
    stream: u64,
    priority: i32,
    user_data: u64,
    out: *mut u64,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_streams.get(stream) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = v.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            stream_wait(v, owner.inner.set_priority(priority)).await?;
            Ok(Completion::new(
                aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                stream,
            ))
        })
    }
}

/// Priority BYTES91 payload signed i32 LE when flags1; flags0 empty means unavailable.
/// Higher signed priority is scheduled first; negative priorities are valid.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_stream_priority(
    runtime: u64,
    stream: u64,
    user_data: u64,
    out: *mut u64,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_streams.get(stream) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = v.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            let priority = stream_wait(v, owner.inner.priority()).await?;
            Ok(
                Completion::new(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT, stream)
                    .flags(u32::from(priority.is_some()))
                    .data(
                        priority
                            .map(|v| v.to_le_bytes().to_vec())
                            .unwrap_or_default(),
                    ),
            )
        })
    }
}
/// ACKED126 flags1 all sent data+FIN acknowledged; flags0 reset/connection loss. No timeout implied.
/// # Safety
/// out writable for operation ID.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_stream_transport_acked(
    runtime: u64,
    stream: u64,
    user_data: u64,
    out: *mut u64,
) -> i32 {
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_streams.get(stream) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let owner = v.clone();
    unsafe {
        submit(&b, user_data, out, async move {
            let acked = stream_wait(v, owner.inner.transport_acked()).await?;
            Ok(
                Completion::new(aster_event_kind_t::ASTER_EVENT_RAW_WT_ACKED, stream)
                    .flags(u32::from(acked)),
            )
        })
    }
}

/// Owned version1 JSON stream_id/has_send/has_receive. Release with string_release.
/// # Safety
/// out points to writable span storage.
#[no_mangle]
pub unsafe extern "C" fn aster_raw_wt_stream_info(
    runtime: aster_runtime_t,
    stream: aster_raw_wt_stream_t,
    out: *mut aster_bytes_t,
) -> i32 {
    if out.is_null() {
        return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, "NULL output");
    }
    unsafe {
        ptr::write(
            out,
            aster_bytes_t {
                ptr: ptr::null(),
                len: 0,
            },
        )
    };
    let b = match bridge(runtime) {
        Ok(v) => v,
        Err(s) => return s,
    };
    let Some(v) = b.raw_wt_streams.get(stream) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let mut json = serde_json::to_value(v.inner.info()).expect("stream info serializes");
    json["version"] = serde_json::json!(1);
    unsafe {
        ptr::write(
            out,
            alloc_owned_bytes(serde_json::to_vec(&json).expect("JSON serializes")),
        )
    };
    OK
}
pub(crate) fn shutdown(b: &BridgeRuntime) {
    for v in b.raw_wt_streams.items.read().unwrap().values() {
        v.close();
    }
    for v in b.raw_wt_sessions.items.read().unwrap().values() {
        let _ = v.close(0, b"runtime closed");
    }
    for v in b.raw_wt_listeners.items.read().unwrap().values() {
        v.close();
    }
    b.raw_wt_streams.clear();
    b.raw_wt_sessions.clear();
    b.raw_wt_listeners.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Dropped(std::sync::mpsc::Sender<()>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    #[test]
    fn cancelled_pending_future_is_dropped() {
        unsafe {
            let mut runtime = 0;
            assert_eq!(
                crate::aster_runtime_new(crate::ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime),
                0
            );
            let b = bridge(runtime).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let guard = Dropped(tx);
            let mut op = 0;
            assert_eq!(
                submit(&b, 0, &mut op, async move {
                    let _guard = guard;
                    std::future::pending::<Result<Completion, (aster_status_t, String)>>().await
                }),
                0
            );
            assert_eq!(crate::aster_operation_cancel(runtime, op), 0);
            rx.recv_timeout(Duration::from_secs(5))
                .expect("cancel must drop future and captured resources");
            assert_eq!(crate::aster_runtime_close(runtime), 0);
        }
    }
    #[test]
    fn oversized_span_is_rejected_before_pointer_read() {
        unsafe {
            let input = aster_bytes_t {
                ptr: ptr::null(),
                len: 16 * 1024 * 1024 + 1,
            };
            assert_eq!(
                bytes(input).unwrap_err(),
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
            );
            crate::LAST_ERROR.with(|message| {
                assert!(message
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .contains("16 MiB"))
            });
        }
    }
}
