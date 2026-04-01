//! Skeleton Rust implementation for the Iroh Java FFI/FFM bridge.
//! This is intentionally a scaffold: signatures and ownership rules are in place,
//! while several transport details are left as TODOs for the concrete bridge.

#![allow(clippy::missing_safety_doc)]

use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rand::rng;
use tokio::runtime::{Builder as TokioBuilder, Runtime};
use tokio::sync::mpsc;

use iroh::{Endpoint, SecretKey};

pub type iroh_runtime_t = u64;
pub type iroh_endpoint_t = u64;
pub type iroh_connection_t = u64;
pub type iroh_stream_t = u64;
pub type iroh_operation_t = u64;

pub const IROH_ABI_VERSION_MAJOR: u32 = 1;
pub const IROH_ABI_VERSION_MINOR: u32 = 0;
pub const IROH_ABI_VERSION_PATCH: u32 = 0;

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum iroh_status_t {
    IROH_STATUS_OK = 0,
    IROH_STATUS_INVALID_ARGUMENT = 1,
    IROH_STATUS_NOT_FOUND = 2,
    IROH_STATUS_ALREADY_CLOSED = 3,
    IROH_STATUS_QUEUE_FULL = 4,
    IROH_STATUS_BUFFER_TOO_SMALL = 5,
    IROH_STATUS_UNSUPPORTED = 6,
    IROH_STATUS_INTERNAL = 7,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum iroh_relay_mode_t {
    IROH_RELAY_MODE_DEFAULT = 0,
    IROH_RELAY_MODE_CUSTOM = 1,
    IROH_RELAY_MODE_DISABLED = 2,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum iroh_event_kind_t {
    IROH_EVENT_NONE = 0,
    IROH_EVENT_ENDPOINT_CREATED = 1,
    IROH_EVENT_ENDPOINT_CREATE_FAILED = 2,
    IROH_EVENT_CONNECT_SUCCEEDED = 3,
    IROH_EVENT_CONNECT_FAILED = 4,
    IROH_EVENT_INCOMING_CONNECTION = 5,
    IROH_EVENT_STREAM_OPEN_SUCCEEDED = 6,
    IROH_EVENT_STREAM_OPEN_FAILED = 7,
    IROH_EVENT_FRAME_RECEIVED = 8,
    IROH_EVENT_SEND_COMPLETED = 9,
    IROH_EVENT_STREAM_FINISHED = 10,
    IROH_EVENT_STREAM_RESET = 11,
    IROH_EVENT_OPERATION_CANCELLED = 12,
    IROH_EVENT_ERROR = 13,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_runtime_config_t {
    pub event_queue_capacity: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_endpoint_config_t {
    pub secret_key_ptr: *const u8,
    pub secret_key_len: usize,

    pub relay_url_ptrs: *const *const c_char,
    pub relay_url_count: usize,
    pub relay_mode: u32,

    pub enable_default_discovery: u32,
    pub reserved0: u32,
    pub reserved1: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_connect_request_t {
    pub remote_addr_ptr: *const u8,
    pub remote_addr_len: usize,
    pub alpn_ptr: *const c_char,
    pub alpn_len: usize,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_open_stream_request_t {
    pub connection: iroh_connection_t,
    pub bidirectional: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_send_request_t {
    pub stream: iroh_stream_t,
    pub data_ptr: *const u8,
    pub data_len: usize,
    pub app_message_id: u64,
    pub flags: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_event_t {
    pub kind: u32,
    pub status: u32,
    pub operation: u64,
    pub object: u64,
    pub related: u64,
    pub app_message_id: u64,
    pub data_ptr: *const u8,
    pub data_len: usize,
    pub error_code: i32,
    pub flags: u32,
}

#[derive(Debug)]
struct EventOwned {
    event: iroh_event_t,
    payload: Option<Arc<[u8]>>,
}

struct BridgeRuntime {
    runtime: Runtime,
    events_tx: mpsc::UnboundedSender<EventOwned>,
    events_rx: Mutex<mpsc::UnboundedReceiver<EventOwned>>,
    next_handle: AtomicU64,
    endpoints: Mutex<HashMap<iroh_endpoint_t, EndpointRecord>>,
    connections: Mutex<HashMap<iroh_connection_t, ConnectionRecord>>,
    streams: Mutex<HashMap<iroh_stream_t, StreamRecord>>,
}

struct EndpointRecord {
    endpoint: Endpoint,
    secret_key_export: Vec<u8>,
}

struct ConnectionRecord {}
struct StreamRecord {}

static RUNTIMES: std::sync::OnceLock<Mutex<HashMap<iroh_runtime_t, Arc<BridgeRuntime>>>> =
    std::sync::OnceLock::new();
static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

fn runtimes() -> &'static Mutex<HashMap<iroh_runtime_t, Arc<BridgeRuntime>>> {
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn new_handle(next: &AtomicU64) -> u64 {
    next.fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

fn load_runtime(handle: iroh_runtime_t) -> Result<Arc<BridgeRuntime>, iroh_status_t> {
    let guard = runtimes().lock().map_err(|_| iroh_status_t::IROH_STATUS_INTERNAL)?;
    guard
        .get(&handle)
        .cloned()
        .ok_or(iroh_status_t::IROH_STATUS_NOT_FOUND)
}

unsafe fn read_optional_bytes(ptr: *const u8, len: usize) -> Result<Option<Vec<u8>>> {
    if ptr.is_null() {
        if len == 0 {
            return Ok(None);
        }
        return Err(anyhow!("non-zero length with null pointer"));
    }
    Ok(Some(slice::from_raw_parts(ptr, len).to_vec()))
}

unsafe fn read_c_string(ptr: *const c_char) -> Result<String> {
    if ptr.is_null() {
        return Err(anyhow!("null string"));
    }
    Ok(CStr::from_ptr(ptr).to_str()?.to_owned())
}

unsafe fn read_relay_urls(ptrs: *const *const c_char, count: usize) -> Result<Vec<String>> {
    if ptrs.is_null() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(count);
    for raw in slice::from_raw_parts(ptrs, count) {
        out.push(read_c_string(*raw)?);
    }
    Ok(out)
}

fn emit_event(rt: &BridgeRuntime, event: EventOwned) {
    let _ = rt.events_tx.send(event);
}

fn event_with_payload(
    kind: iroh_event_kind_t,
    status: iroh_status_t,
    operation: u64,
    object: u64,
    related: u64,
    app_message_id: u64,
    payload: Option<Vec<u8>>,
    error_code: i32,
    flags: u32,
) -> EventOwned {
    let payload_arc = payload.map(|v| Arc::<[u8]>::from(v));
    let (data_ptr, data_len) = if let Some(bytes) = payload_arc.as_ref() {
        (bytes.as_ptr(), bytes.len())
    } else {
        (ptr::null(), 0)
    };
    EventOwned {
        event: iroh_event_t {
            kind: kind as u32,
            status: status as u32,
            operation,
            object,
            related,
            app_message_id,
            data_ptr,
            data_len,
            error_code,
            flags,
        },
        payload: payload_arc,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_abi_version_major() -> u32 {
    IROH_ABI_VERSION_MAJOR
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_abi_version_minor() -> u32 {
    IROH_ABI_VERSION_MINOR
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_abi_version_patch() -> u32 {
    IROH_ABI_VERSION_PATCH
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_runtime_new(
    config: *const iroh_runtime_config_t,
    out_runtime: *mut iroh_runtime_t,
) -> iroh_status_t {
    if out_runtime.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT;
    }
    let queue_capacity = unsafe { config.as_ref().map(|c| c.event_queue_capacity).unwrap_or(1024) };
    let runtime = match TokioBuilder::new_multi_thread()
        .enable_all()
        .thread_name("iroh-bridge")
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return iroh_status_t::IROH_STATUS_INTERNAL,
    };
    let (_cap_hint, (events_tx, events_rx)) = (queue_capacity, mpsc::unbounded_channel());
    let id = NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed);
    let bridge = Arc::new(BridgeRuntime {
        runtime,
        events_tx,
        events_rx: Mutex::new(events_rx),
        next_handle: AtomicU64::new(1),
        endpoints: Mutex::new(HashMap::new()),
        connections: Mutex::new(HashMap::new()),
        streams: Mutex::new(HashMap::new()),
    });
    if let Ok(mut guard) = runtimes().lock() {
        guard.insert(id, bridge);
        unsafe {
            *out_runtime = id;
        }
        iroh_status_t::IROH_STATUS_OK
    } else {
        iroh_status_t::IROH_STATUS_INTERNAL
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_runtime_close(runtime: iroh_runtime_t) -> iroh_status_t {
    match runtimes().lock() {
        Ok(mut guard) => {
            if guard.remove(&runtime).is_some() {
                iroh_status_t::IROH_STATUS_OK
            } else {
                iroh_status_t::IROH_STATUS_NOT_FOUND
            }
        }
        Err(_) => iroh_status_t::IROH_STATUS_INTERNAL,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_secret_key_generate(
    out_key_ptr: *mut u8,
    out_key_capacity: usize,
    out_key_len: *mut usize,
) -> iroh_status_t {
    if out_key_len.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT;
    }
    let key = SecretKey::generate(&mut rng());
    let bytes = key.secret().to_bytes();
    unsafe {
        *out_key_len = bytes.len();
    }
    if out_key_ptr.is_null() || out_key_capacity < bytes.len() {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL;
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), out_key_ptr, bytes.len());
    }
    iroh_status_t::IROH_STATUS_OK
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_endpoint_create(
    runtime: iroh_runtime_t,
    config: *const iroh_endpoint_config_t,
    out_operation: *mut iroh_operation_t,
) -> iroh_status_t {
    if config.is_null() || out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT;
    }
    let bridge = match load_runtime(runtime) {
        Ok(rt) => rt,
        Err(status) => return status,
    };
    let operation = new_handle(&bridge.next_handle);
    unsafe {
        *out_operation = operation;
    }
    let cfg = unsafe { *config };
    let bridge_clone = bridge.clone();
    bridge.runtime.spawn(async move {
        let result: Result<()> = async {
            let secret_key_bytes = unsafe { read_optional_bytes(cfg.secret_key_ptr, cfg.secret_key_len)? };
            let relay_urls = unsafe { read_relay_urls(cfg.relay_url_ptrs, cfg.relay_url_count)? };

            let secret_key = match secret_key_bytes {
                Some(bytes) => {
                    if bytes.len() != 32 {
                        return Err(anyhow!("expected 32 secret key bytes"));
                    }
                    SecretKey::from(bytes.as_slice().try_into().map_err(|_| anyhow!("invalid key length"))?)
                }
                None => SecretKey::generate(&mut rng()),
            };

            let mut builder = Endpoint::builder().secret_key(secret_key.clone());

            match cfg.relay_mode {
                x if x == iroh_relay_mode_t::IROH_RELAY_MODE_DEFAULT as u32 => {}
                x if x == iroh_relay_mode_t::IROH_RELAY_MODE_CUSTOM as u32 => {
                    let urls = relay_urls
                        .into_iter()
                        .map(|s| s.parse())
                        .collect::<Result<Vec<_>, _>>()
                        .context("invalid relay URL")?;
                    builder = builder.relay_mode(iroh::endpoint::RelayMode::Custom(urls));
                }
                x if x == iroh_relay_mode_t::IROH_RELAY_MODE_DISABLED as u32 => {
                    builder = builder.relay_mode(iroh::endpoint::RelayMode::Disabled);
                }
                _ => return Err(anyhow!("unsupported relay mode")),
            }

            if cfg.enable_default_discovery == 0 {
                // TODO: for the chosen iroh version, replace with the proper
                // empty/discovery-free builder path if needed.
            }

            let endpoint = builder.bind().await?;
            let endpoint_handle = new_handle(&bridge_clone.next_handle);
            let exported = secret_key.secret().to_bytes().to_vec();

            bridge_clone
                .endpoints
                .lock()
                .map_err(|_| anyhow!("endpoint lock poisoned"))?
                .insert(endpoint_handle, EndpointRecord { endpoint, secret_key_export: exported });

            emit_event(
                &bridge_clone,
                event_with_payload(
                    iroh_event_kind_t::IROH_EVENT_ENDPOINT_CREATED,
                    iroh_status_t::IROH_STATUS_OK,
                    operation,
                    endpoint_handle,
                    0,
                    0,
                    None,
                    0,
                    0,
                ),
            );
            Ok(())
        }
        .await;

        if let Err(err) = result {
            emit_event(
                &bridge_clone,
                event_with_payload(
                    iroh_event_kind_t::IROH_EVENT_ENDPOINT_CREATE_FAILED,
                    iroh_status_t::IROH_STATUS_INTERNAL,
                    operation,
                    0,
                    0,
                    0,
                    Some(err.to_string().into_bytes()),
                    -1,
                    0,
                ),
            );
        }
    });
    iroh_status_t::IROH_STATUS_OK
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_endpoint_close(
    runtime: iroh_runtime_t,
    endpoint: iroh_endpoint_t,
) -> iroh_status_t {
    let bridge = match load_runtime(runtime) {
        Ok(rt) => rt,
        Err(status) => return status,
    };
    match bridge.endpoints.lock() {
        Ok(mut guard) => {
            if guard.remove(&endpoint).is_some() {
                iroh_status_t::IROH_STATUS_OK
            } else {
                iroh_status_t::IROH_STATUS_NOT_FOUND
            }
        }
        Err(_) => iroh_status_t::IROH_STATUS_INTERNAL,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_endpoint_export_secret_key(
    runtime: iroh_runtime_t,
    endpoint: iroh_endpoint_t,
    out_key_ptr: *mut u8,
    out_key_capacity: usize,
    out_key_len: *mut usize,
) -> iroh_status_t {
    if out_key_len.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT;
    }
    let bridge = match load_runtime(runtime) {
        Ok(rt) => rt,
        Err(status) => return status,
    };
    let guard = match bridge.endpoints.lock() {
        Ok(g) => g,
        Err(_) => return iroh_status_t::IROH_STATUS_INTERNAL,
    };
    let record = match guard.get(&endpoint) {
        Some(r) => r,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND,
    };
    unsafe {
        *out_key_len = record.secret_key_export.len();
    }
    if out_key_ptr.is_null() || out_key_capacity < record.secret_key_export.len() {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL;
    }
    unsafe {
        ptr::copy_nonoverlapping(
            record.secret_key_export.as_ptr(),
            out_key_ptr,
            record.secret_key_export.len(),
        );
    }
    iroh_status_t::IROH_STATUS_OK
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_connect(
    runtime: iroh_runtime_t,
    endpoint: iroh_endpoint_t,
    request: *const iroh_connect_request_t,
    out_operation: *mut iroh_operation_t,
) -> iroh_status_t {
    if request.is_null() || out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT;
    }
    let bridge = match load_runtime(runtime) {
        Ok(rt) => rt,
        Err(status) => return status,
    };
    let operation = new_handle(&bridge.next_handle);
    unsafe {
        *out_operation = operation;
    }
    let req = unsafe { *request };
    let bridge_clone = bridge.clone();
    bridge.runtime.spawn(async move {
        let result: Result<()> = async {
            let alpn = unsafe {
                if req.alpn_ptr.is_null() {
                    return Err(anyhow!("missing ALPN"));
                }
                if req.alpn_len > 0 {
                    let bytes = slice::from_raw_parts(req.alpn_ptr as *const u8, req.alpn_len);
                    String::from_utf8(bytes.to_vec())?
                } else {
                    read_c_string(req.alpn_ptr)?
                }
            };

            let _remote = unsafe { read_optional_bytes(req.remote_addr_ptr, req.remote_addr_len)? }
                .ok_or_else(|| anyhow!("missing remote address bytes"))?;

            let endpoint_clone = {
                let guard = bridge_clone.endpoints.lock().map_err(|_| anyhow!("lock poisoned"))?;
                guard
                    .get(&endpoint)
                    .ok_or_else(|| anyhow!("endpoint not found"))?
                    .endpoint
                    .clone()
            };

            let _ = endpoint_clone;
            let _ = alpn;

            let connection_handle = new_handle(&bridge_clone.next_handle);
            bridge_clone
                .connections
                .lock()
                .map_err(|_| anyhow!("connections lock poisoned"))?
                .insert(connection_handle, ConnectionRecord {});

            emit_event(
                &bridge_clone,
                event_with_payload(
                    iroh_event_kind_t::IROH_EVENT_CONNECT_SUCCEEDED,
                    iroh_status_t::IROH_STATUS_OK,
                    operation,
                    connection_handle,
                    endpoint,
                    0,
                    None,
                    0,
                    0,
                ),
            );
            Ok(())
        }
        .await;

        if let Err(err) = result {
            emit_event(
                &bridge_clone,
                event_with_payload(
                    iroh_event_kind_t::IROH_EVENT_CONNECT_FAILED,
                    iroh_status_t::IROH_STATUS_INTERNAL,
                    operation,
                    0,
                    endpoint,
                    0,
                    Some(err.to_string().into_bytes()),
                    -1,
                    0,
                ),
            );
        }
    });
    iroh_status_t::IROH_STATUS_OK
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_accept(
    _runtime: iroh_runtime_t,
    _endpoint: iroh_endpoint_t,
    _out_operation: *mut iroh_operation_t,
) -> iroh_status_t {
    iroh_status_t::IROH_STATUS_UNSUPPORTED
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_stream_open(
    runtime: iroh_runtime_t,
    request: *const iroh_open_stream_request_t,
    out_operation: *mut iroh_operation_t,
) -> iroh_status_t {
    if request.is_null() || out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT;
    }
    let bridge = match load_runtime(runtime) {
        Ok(rt) => rt,
        Err(status) => return status,
    };
    let operation = new_handle(&bridge.next_handle);
    unsafe {
        *out_operation = operation;
    }
    let req = unsafe { *request };
    let bridge_clone = bridge.clone();
    bridge.runtime.spawn(async move {
        let result: Result<()> = async {
            let _conn = bridge_clone
                .connections
                .lock()
                .map_err(|_| anyhow!("connections lock poisoned"))?
                .get(&req.connection)
                .ok_or_else(|| anyhow!("connection not found"))?;
            let stream_handle = new_handle(&bridge_clone.next_handle);

            bridge_clone
                .streams
                .lock()
                .map_err(|_| anyhow!("streams lock poisoned"))?
                .insert(stream_handle, StreamRecord {});

            emit_event(
                &bridge_clone,
                event_with_payload(
                    iroh_event_kind_t::IROH_EVENT_STREAM_OPEN_SUCCEEDED,
                    iroh_status_t::IROH_STATUS_OK,
                    operation,
                    stream_handle,
                    req.connection,
                    0,
                    None,
                    0,
                    0,
                ),
            );
            Ok(())
        }
        .await;

        if let Err(err) = result {
            emit_event(
                &bridge_clone,
                event_with_payload(
                    iroh_event_kind_t::IROH_EVENT_STREAM_OPEN_FAILED,
                    iroh_status_t::IROH_STATUS_INTERNAL,
                    operation,
                    0,
                    req.connection,
                    0,
                    Some(err.to_string().into_bytes()),
                    -1,
                    0,
                ),
            );
        }
    });
    iroh_status_t::IROH_STATUS_OK
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_stream_send(
    runtime: iroh_runtime_t,
    request: *const iroh_send_request_t,
    out_operation: *mut iroh_operation_t,
) -> iroh_status_t {
    if request.is_null() || out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT;
    }
    let bridge = match load_runtime(runtime) {
        Ok(rt) => rt,
        Err(status) => return status,
    };
    let operation = new_handle(&bridge.next_handle);
    unsafe {
        *out_operation = operation;
    }
    let req = unsafe { *request };
    let payload = unsafe {
        if req.data_ptr.is_null() && req.data_len > 0 {
            return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT;
        }
        slice::from_raw_parts(req.data_ptr, req.data_len).to_vec()
    };
    let bridge_clone = bridge.clone();
    bridge.runtime.spawn(async move {
        let result: Result<()> = async {
            let _stream = bridge_clone
                .streams
                .lock()
                .map_err(|_| anyhow!("streams lock poisoned"))?
                .get(&req.stream)
                .ok_or_else(|| anyhow!("stream not found"))?;

            let _ = payload;

            emit_event(
                &bridge_clone,
                event_with_payload(
                    iroh_event_kind_t::IROH_EVENT_SEND_COMPLETED,
                    iroh_status_t::IROH_STATUS_OK,
                    operation,
                    req.stream,
                    0,
                    req.app_message_id,
                    None,
                    0,
                    req.flags,
                ),
            );
            Ok(())
        }
        .await;

        if let Err(err) = result {
            emit_event(
                &bridge_clone,
                event_with_payload(
                    iroh_event_kind_t::IROH_EVENT_ERROR,
                    iroh_status_t::IROH_STATUS_INTERNAL,
                    operation,
                    req.stream,
                    0,
                    req.app_message_id,
                    Some(err.to_string().into_bytes()),
                    -1,
                    req.flags,
                ),
            );
        }
    });
    iroh_status_t::IROH_STATUS_OK
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_stream_finish(
    _runtime: iroh_runtime_t,
    _stream: iroh_stream_t,
    _out_operation: *mut iroh_operation_t,
) -> iroh_status_t {
    iroh_status_t::IROH_STATUS_UNSUPPORTED
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_stream_reset(
    _runtime: iroh_runtime_t,
    _stream: iroh_stream_t,
    _error_code: u32,
    _out_operation: *mut iroh_operation_t,
) -> iroh_status_t {
    iroh_status_t::IROH_STATUS_UNSUPPORTED
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_operation_cancel(
    _runtime: iroh_runtime_t,
    _operation: iroh_operation_t,
) -> iroh_status_t {
    iroh_status_t::IROH_STATUS_UNSUPPORTED
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_poll_events(
    runtime: iroh_runtime_t,
    out_events: *mut iroh_event_t,
    max_events: usize,
    timeout_millis: u32,
) -> usize {
    if out_events.is_null() || max_events == 0 {
        return 0;
    }
    let bridge = match load_runtime(runtime) {
        Ok(rt) => rt,
        Err(_) => return 0,
    };

    let mut guard = match bridge.events_rx.lock() {
        Ok(g) => g,
        Err(_) => return 0,
    };

    let first = if timeout_millis == 0 {
        guard.try_recv().ok()
    } else {
        bridge.runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(timeout_millis as u64), guard.recv())
                .await
                .ok()
                .flatten()
        })
    };

    let mut written = 0usize;
    if let Some(event) = first {
        unsafe {
            ptr::write(out_events.add(written), event.event)
        };
        std::mem::forget(event.payload);
        written += 1;
    } else {
        return 0;
    }

    while written < max_events {
        match guard.try_recv() {
            Ok(event) => {
                unsafe {
                    ptr::write(out_events.add(written), event.event)
                };
                std::mem::forget(event.payload);
                written += 1;
            }
            Err(_) => break,
        }
    }
    written
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_release_event_data(
    _runtime: iroh_runtime_t,
    data_ptr: *const u8,
    data_len: usize,
) -> iroh_status_t {
    if data_ptr.is_null() || data_len == 0 {
        return iroh_status_t::IROH_STATUS_OK;
    }
    let _ = (data_ptr, data_len);
    iroh_status_t::IROH_STATUS_UNSUPPORTED
}

#[unsafe(no_mangle)]
pub extern "C" fn iroh_status_name(status: iroh_status_t) -> *const c_char {
    let s = match status {
        iroh_status_t::IROH_STATUS_OK => "IROH_STATUS_OK\0",
        iroh_status_t::IROH_STATUS_INVALID_ARGUMENT => "IROH_STATUS_INVALID_ARGUMENT\0",
        iroh_status_t::IROH_STATUS_NOT_FOUND => "IROH_STATUS_NOT_FOUND\0",
        iroh_status_t::IROH_STATUS_ALREADY_CLOSED => "IROH_STATUS_ALREADY_CLOSED\0",
        iroh_status_t::IROH_STATUS_QUEUE_FULL => "IROH_STATUS_QUEUE_FULL\0",
        iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL => "IROH_STATUS_BUFFER_TOO_SMALL\0",
        iroh_status_t::IROH_STATUS_UNSUPPORTED => "IROH_STATUS_UNSUPPORTED\0",
        iroh_status_t::IROH_STATUS_INTERNAL => "IROH_STATUS_INTERNAL\0",
    };
    s.as_ptr() as *const c_char
}
