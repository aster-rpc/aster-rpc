//! Iroh Transport FFI - C ABI Bridge
//!
//! This module provides a C-compatible FFI layer for iroh transport capabilities.
//! The design follows the completion queue architecture for async operations.

#![allow(clippy::missing_safety_doc)]
#![allow(non_camel_case_types)] // FFI types intentionally use snake_case
#![allow(nonstandard_style)] // FFI types intentionally use snake_case

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, CString};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;

use aster_transport_core::*;

// ============================================================================
// ABI Version
// ============================================================================

pub const IROH_ABI_VERSION_MAJOR: u32 = 1;
pub const IROH_ABI_VERSION_MINOR: u32 = 0;
pub const IROH_ABI_VERSION_PATCH: u32 = 0;

// ============================================================================
// ABI Types (FFI-safe C-compatible structs)
// ============================================================================

/// Opaque handle types
pub type iroh_runtime_t = u64;
pub type iroh_endpoint_t = u64;
pub type iroh_connection_t = u64;
pub type iroh_send_stream_t = u64;
pub type iroh_recv_stream_t = u64;
pub type iroh_node_t = u64;
pub type iroh_operation_t = u64;
pub type iroh_buffer_t = u64;
pub type iroh_hook_invocation_t = u64; // Phase 1b: identifies a pending hook invocation

/// Status codes
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
    IROH_STATUS_TIMEOUT = 8,
    IROH_STATUS_CANCELLED = 9,
    IROH_STATUS_CONNECTION_REFUSED = 10,
    IROH_STATUS_STREAM_RESET = 11,
}

/// Relay mode
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum iroh_relay_mode_t {
    IROH_RELAY_MODE_DEFAULT = 0,
    IROH_RELAY_MODE_CUSTOM = 1,
    IROH_RELAY_MODE_DISABLED = 2,
}

/// Event kinds
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum iroh_event_kind_t {
    IROH_EVENT_NONE = 0,

    // Lifecycle
    IROH_EVENT_NODE_CREATED = 1,
    IROH_EVENT_NODE_CREATE_FAILED = 2,
    IROH_EVENT_ENDPOINT_CREATED = 3,
    IROH_EVENT_ENDPOINT_CREATE_FAILED = 4,
    IROH_EVENT_CLOSED = 5,

    // Connections
    IROH_EVENT_CONNECTED = 10,
    IROH_EVENT_CONNECT_FAILED = 11,
    IROH_EVENT_CONNECTION_ACCEPTED = 12,
    IROH_EVENT_CONNECTION_CLOSED = 13,

    // Streams
    IROH_EVENT_STREAM_OPENED = 20,
    IROH_EVENT_STREAM_ACCEPTED = 21,
    IROH_EVENT_FRAME_RECEIVED = 22,
    IROH_EVENT_SEND_COMPLETED = 23,
    IROH_EVENT_STREAM_FINISHED = 24,
    IROH_EVENT_STREAM_RESET = 25,

    // Blobs
    IROH_EVENT_BLOB_ADDED = 30,
    IROH_EVENT_BLOB_READ = 31,
    IROH_EVENT_BLOB_DOWNLOADED = 32,
    IROH_EVENT_BLOB_TICKET_CREATED = 33,
    IROH_EVENT_BLOB_COLLECTION_ADDED = 34,
    IROH_EVENT_BLOB_COLLECTION_TICKET_CREATED = 35,

    /// Emitted by iroh_blobs_observe_complete when the blob is fully available locally.
    IROH_EVENT_BLOB_OBSERVE_COMPLETE = 56,

    // Tags (Phase 1c)
    IROH_EVENT_TAG_SET = 36,
    IROH_EVENT_TAG_GET = 37,
    IROH_EVENT_TAG_DELETED = 38,
    IROH_EVENT_TAG_LIST = 39,

    // Docs
    IROH_EVENT_DOC_CREATED = 40,
    IROH_EVENT_DOC_JOINED = 41,
    IROH_EVENT_DOC_SET = 42,
    IROH_EVENT_DOC_GET = 43,
    IROH_EVENT_DOC_SHARED = 44,
    IROH_EVENT_DOC_QUERY = 46,
    IROH_EVENT_AUTHOR_CREATED = 45,
    IROH_EVENT_DOC_SUBSCRIBED = 47,
    IROH_EVENT_DOC_EVENT = 48,
    /// join_and_subscribe: event.handle = doc_handle, event.related = receiver_handle
    IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED = 49,

    // Gossip
    IROH_EVENT_GOSSIP_SUBSCRIBED = 50,
    IROH_EVENT_GOSSIP_BROADCAST_DONE = 51,
    IROH_EVENT_GOSSIP_RECEIVED = 52,
    IROH_EVENT_GOSSIP_NEIGHBOR_UP = 53,
    IROH_EVENT_GOSSIP_NEIGHBOR_DOWN = 54,
    IROH_EVENT_GOSSIP_LAGGED = 55,

    // Datagrams (Phase 1b)
    IROH_EVENT_DATAGRAM_RECEIVED = 60,

    // Hooks (Phase 1b)
    IROH_EVENT_HOOK_BEFORE_CONNECT = 70,
    IROH_EVENT_HOOK_AFTER_CONNECT = 71,
    IROH_EVENT_HOOK_INVOCATION_RELEASED = 72,

    // Aster custom-ALPN (Phase 1e)
    IROH_EVENT_ASTER_ACCEPTED = 65,

    // Generic
    IROH_EVENT_STRING_RESULT = 90,
    IROH_EVENT_BYTES_RESULT = 91,
    IROH_EVENT_UNIT_RESULT = 92,
    IROH_EVENT_OPERATION_CANCELLED = 98,
    IROH_EVENT_ERROR = 99,
}

/// Hook decision
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum iroh_hook_decision_t {
    IROH_HOOK_DECISION_ALLOW = 0,
    IROH_HOOK_DECISION_DENY = 1,
}

// ============================================================================
// C Structs (#[repr(C)])
// ============================================================================

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_runtime_config_t {
    pub struct_size: u32,
    pub worker_threads: u32,
    pub event_queue_capacity: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_endpoint_config_t {
    pub struct_size: u32,
    pub relay_mode: u32,           // 0=default, 1=custom, 2=disabled, 3=staging
    pub secret_key: iroh_bytes_t,
    pub alpns: iroh_bytes_list_t,
    pub relay_urls: iroh_bytes_list_t,
    pub enable_discovery: u32,
    pub enable_hooks: u32,         // Phase 1b: 0=disabled, 1=enabled
    pub hook_timeout_ms: u64,      // Phase 1b: 0 = use default (5000ms)
    // Phase 1d: endpoint builder gaps
    pub bind_addr: iroh_bytes_t,           // socket addr string; empty = use default
    pub clear_ip_transports: u32,          // 1 = relay-only mode
    pub clear_relay_transports: u32,       // 1 = direct-IP-only mode
    pub portmapper_config: u32,            // 0 = enabled (default), 1 = disabled
    pub proxy_url: iroh_bytes_t,           // HTTP/SOCKS proxy URL string; empty = none
    pub proxy_from_env: u32,               // 1 = read HTTP_PROXY/HTTPS_PROXY from env
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_connect_config_t {
    pub struct_size: u32,
    pub flags: u32,
    pub node_id: iroh_bytes_t, // hex string
    pub alpn: iroh_bytes_t,
    pub addr: *const iroh_node_addr_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_node_addr_t {
    pub endpoint_id: iroh_bytes_t,
    pub relay_url: iroh_bytes_t,
    pub direct_addresses: iroh_bytes_list_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_bytes_t {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_bytes_list_t {
    pub items: *const iroh_bytes_t,
    pub len: usize,
}

/// C-compatible event structure for FFI output
#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_event_t {
    pub struct_size: u32,
    pub kind: u32,
    pub status: u32,
    pub operation: u64,
    pub handle: u64,
    pub related: u64,
    pub user_data: u64,
    pub data_ptr: *const u8,
    pub data_len: usize,
    pub buffer: u64,
    pub error_code: i32,
    pub flags: u32,
}

// ============================================================================
// Thread-local Error Storage
// ============================================================================

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: impl ToString) -> i32 {
    let mut s = msg.to_string();
    if s.contains('\0') {
        s = s.replace('\0', " ");
    }
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(s).ok();
    });
    iroh_status_t::IROH_STATUS_INTERNAL as i32
}

#[allow(dead_code)]
fn get_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(ptr::null())
    })
}

// ============================================================================
// Handle Registry - Arc-backed safe handle storage
// ============================================================================

struct HandleRegistry<T> {
    next_id: AtomicU64,
    items: RwLock<HashMap<u64, Arc<T>>>,
}

impl<T> HandleRegistry<T> {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            items: RwLock::new(HashMap::new()),
        }
    }

    fn insert(&self, value: T) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let arc = Arc::new(value);
        self.items.write().unwrap().insert(id, arc);
        id
    }

    fn get(&self, id: u64) -> Option<Arc<T>> {
        self.items.read().unwrap().get(&id).cloned()
    }

    fn remove(&self, id: u64) -> Option<Arc<T>> {
        self.items.write().unwrap().remove(&id)
    }

    #[allow(dead_code)]
    fn count(&self) -> usize {
        self.items.read().unwrap().len()
    }
}

// ============================================================================
// Event System - Send-safe internal events
// ============================================================================

/// Internal event for crossing thread boundary (no raw pointers - Send-safe)
#[derive(Clone)]
struct EventInternal {
    struct_size: u32,
    kind: u32,
    status: u32,
    operation: u64,
    handle: u64,
    related: u64,
    user_data: u64,
    data_len: usize,
    buffer_id: u64,
    error_code: i32,
    flags: u32,
}

impl Default for EventInternal {
    fn default() -> Self {
        Self {
            struct_size: std::mem::size_of::<iroh_event_t>() as u32,
            kind: iroh_event_kind_t::IROH_EVENT_NONE as u32,
            status: iroh_status_t::IROH_STATUS_OK as u32,
            operation: 0,
            handle: 0,
            related: 0,
            user_data: 0,
            data_len: 0,
            buffer_id: 0,
            error_code: 0,
            flags: 0,
        }
    }
}

impl EventInternal {
    fn new(
        kind: iroh_event_kind_t,
        status: iroh_status_t,
        operation: u64,
        handle: u64,
        related: u64,
        user_data: u64,
        error_code: i32,
    ) -> Self {
        Self {
            struct_size: std::mem::size_of::<iroh_event_t>() as u32,
            kind: kind as u32,
            status: status as u32,
            operation,
            handle,
            related,
            user_data,
            data_len: 0,
            buffer_id: 0,
            error_code,
            flags: 0,
        }
    }

    #[allow(dead_code)]
    fn with_data(mut self, data: Vec<u8>, buffers: &BufferRegistry) -> Self {
        let (buf_id, arc) = buffers.insert(data);
        self.data_len = arc.len();
        self.buffer_id = buf_id;
        self
    }

    fn with_buffer(mut self, buf_id: u64, data_len: usize) -> Self {
        self.buffer_id = buf_id;
        self.data_len = data_len;
        self
    }
}

/// EventOwned wraps EventInternal with its data payload for crossing thread boundary
/// This is Send-safe because EventInternal has no raw pointers
#[derive(Default)]
struct EventOwned {
    event: EventInternal,
    payload: Option<Arc<[u8]>>,
}

// ============================================================================
// Buffer Registry - Track allocated buffers
// ============================================================================

struct BufferRegistry {
    next_id: AtomicU64,
    buffers: RwLock<HashMap<u64, Arc<[u8]>>>,
}

impl BufferRegistry {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            buffers: RwLock::new(HashMap::new()),
        }
    }

    fn insert(&self, data: Vec<u8>) -> (u64, Arc<[u8]>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let arc: Arc<[u8]> = data.into();
        self.buffers.write().unwrap().insert(id, arc.clone());
        (id, arc)
    }

    fn get(&self, id: u64) -> Option<Arc<[u8]>> {
        self.buffers.read().unwrap().get(&id).cloned()
    }

    fn remove(&self, id: u64) -> Option<Arc<[u8]>> {
        self.buffers.write().unwrap().remove(&id)
    }
}

// ============================================================================
// Hook Invocation State - stores pending hook reply senders (Phase 1b)
// ============================================================================

/// Wraps the oneshot reply sender for a pending hook invocation.
enum HookSender {
    BeforeConnect(tokio::sync::oneshot::Sender<bool>),
    AfterConnect(tokio::sync::oneshot::Sender<CoreAfterHandshakeDecision>),
}

struct HookInvocationState {
    /// Consumed exactly once when the caller responds.
    sender: std::sync::Mutex<Option<HookSender>>,
}

// ============================================================================
// Bridge Runtime - Main FFI runtime
// ============================================================================

struct BridgeRuntime {
    runtime: tokio::runtime::Runtime,
    events_tx: tokio::sync::mpsc::UnboundedSender<EventOwned>,
    events_rx: Mutex<tokio::sync::mpsc::UnboundedReceiver<EventOwned>>,

    // Handle registries
    nodes: HandleRegistry<CoreNode>,
    endpoints: HandleRegistry<CoreNetClient>,
    connections: HandleRegistry<CoreConnection>,
    send_streams: HandleRegistry<CoreSendStream>,
    recv_streams: HandleRegistry<CoreRecvStream>,
    #[allow(dead_code)]
    blobs_clients: HandleRegistry<CoreBlobsClient>,
    #[allow(dead_code)]
    docs_clients: HandleRegistry<CoreDocsClient>,
    docs: HandleRegistry<CoreDoc>,
    #[allow(dead_code)]
    gossip_clients: HandleRegistry<CoreGossipClient>,
    gossip_topics: HandleRegistry<CoreGossipTopic>,

    // Operation registry (for cancellation)
    operations: HandleRegistry<OperationState>,

    // Hook invocation registry (Phase 1b)
    hook_invocations: HandleRegistry<HookInvocationState>,

    // Doc event receiver registry (Phase 1c.4)
    doc_event_receivers: HandleRegistry<aster_transport_core::CoreDocEventReceiver>,

    // Buffer registry
    buffers: BufferRegistry,
}

struct OperationState {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl BridgeRuntime {
    fn new(worker_threads: u32, queue_capacity: u32) -> Result<Self> {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();

        if worker_threads > 0 {
            builder.worker_threads(worker_threads as usize);
        }

        builder.thread_name("iroh-ffi");

        let runtime = builder.build()?;

        let _capacity = if queue_capacity > 0 {
            queue_capacity
        } else {
            4096
        };
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();

        Ok(Self {
            runtime,
            events_tx,
            events_rx: Mutex::new(events_rx),
            nodes: HandleRegistry::new(),
            endpoints: HandleRegistry::new(),
            connections: HandleRegistry::new(),
            send_streams: HandleRegistry::new(),
            recv_streams: HandleRegistry::new(),
            blobs_clients: HandleRegistry::new(),
            docs_clients: HandleRegistry::new(),
            docs: HandleRegistry::new(),
            gossip_clients: HandleRegistry::new(),
            gossip_topics: HandleRegistry::new(),
            operations: HandleRegistry::new(),
            hook_invocations: HandleRegistry::new(),
            doc_event_receivers: HandleRegistry::new(),
            buffers: BufferRegistry::new(),
        })
    }

    fn emit(&self, event: EventOwned) {
        let _ = self.events_tx.send(event);
    }

    fn emit_error(&self, operation: u64, user_data: u64, message: &str) {
        let data = message.as_bytes().to_vec();
        let (buf_id, _) = self.buffers.insert(data);
        let event = EventInternal::new(
            iroh_event_kind_t::IROH_EVENT_ERROR,
            iroh_status_t::IROH_STATUS_INTERNAL,
            operation,
            0,
            0,
            user_data,
            -1,
        )
        .with_buffer(buf_id, message.len());
        self.emit(EventOwned {
            event,
            payload: None,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_simple(
        &self,
        kind: iroh_event_kind_t,
        status: iroh_status_t,
        operation: u64,
        handle: u64,
        related: u64,
        user_data: u64,
        error_code: i32,
    ) {
        let event = EventInternal::new(
            kind, status, operation, handle, related, user_data, error_code,
        );
        self.emit(EventOwned {
            event,
            payload: None,
        });
    }

    fn emit_with_data(&self, event: EventInternal, data: Vec<u8>) {
        let (buf_id, arc) = self.buffers.insert(data);
        let event = event.with_buffer(buf_id, arc.len());
        self.emit(EventOwned {
            event,
            payload: Some(arc),
        });
    }

    fn new_operation(&self) -> (u64, Arc<std::sync::atomic::AtomicBool>) {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let op = OperationState {
            cancelled: cancelled.clone(),
        };
        let id = self.operations.insert(op);
        (id, cancelled)
    }

    fn cancel_operation(&self, op_id: u64) -> bool {
        if let Some(op) = self.operations.get(op_id) {
            op.cancelled.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
}

// ============================================================================
// Global Runtime Registry
// ============================================================================

static RUNTIMES: std::sync::OnceLock<Mutex<HashMap<iroh_runtime_t, Arc<BridgeRuntime>>>> =
    std::sync::OnceLock::new();

fn runtimes() -> &'static Mutex<HashMap<iroh_runtime_t, Arc<BridgeRuntime>>> {
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn load_runtime(handle: iroh_runtime_t) -> Result<Arc<BridgeRuntime>, iroh_status_t> {
    let guard = runtimes()
        .lock()
        .map_err(|_| iroh_status_t::IROH_STATUS_INTERNAL)?;
    guard
        .get(&handle)
        .cloned()
        .ok_or(iroh_status_t::IROH_STATUS_NOT_FOUND)
}

// ============================================================================
// Utility Functions
// ============================================================================

fn new_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

unsafe fn read_bytes(b: &iroh_bytes_t) -> Vec<u8> {
    if b.ptr.is_null() || b.len == 0 {
        Vec::new()
    } else {
        slice::from_raw_parts(b.ptr, b.len).to_vec()
    }
}

unsafe fn read_bytes_opt(b: &iroh_bytes_t) -> Option<Vec<u8>> {
    if b.ptr.is_null() || b.len == 0 {
        None
    } else {
        Some(slice::from_raw_parts(b.ptr, b.len).to_vec())
    }
}

unsafe fn read_string_opt(b: &iroh_bytes_t) -> Option<String> {
    read_bytes_opt(b).and_then(|v| String::from_utf8(v).ok())
}

unsafe fn read_string(b: &iroh_bytes_t) -> Result<String, i32> {
    let bytes = read_bytes(b);
    String::from_utf8(bytes).map_err(set_last_error)
}

unsafe fn read_string_list(list: &iroh_bytes_list_t) -> Vec<String> {
    if list.items.is_null() || list.len == 0 {
        Vec::new()
    } else {
        let items = slice::from_raw_parts(list.items, list.len);
        items.iter().filter_map(|b| read_string(b).ok()).collect()
    }
}

fn alloc_string(s: String) -> iroh_bytes_t {
    let mut bytes = s.into_bytes();
    let len = bytes.len();
    bytes.push(0); // null terminator
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    iroh_bytes_t { ptr, len }
}

fn alloc_bytes(bytes: Vec<u8>) -> iroh_bytes_t {
    let len = bytes.len();
    let ptr = Box::into_raw(bytes.into_boxed_slice()) as *mut u8;
    iroh_bytes_t { ptr, len }
}

/// Encode a before_connect hook payload as:
/// [4 bytes LE: remote_id_len][remote_id bytes][4 bytes LE: alpn_len][alpn bytes]
fn encode_hook_connect_payload(info: &CoreHookConnectInfo) -> Vec<u8> {
    let id = info.remote_endpoint_id.as_bytes();
    let alpn = &info.alpn;
    let mut buf = Vec::with_capacity(8 + id.len() + alpn.len());
    buf.extend_from_slice(&(id.len() as u32).to_le_bytes());
    buf.extend_from_slice(id);
    buf.extend_from_slice(&(alpn.len() as u32).to_le_bytes());
    buf.extend_from_slice(alpn);
    buf
}

/// Encode an after_handshake hook payload as:
/// [4 bytes LE: remote_id_len][remote_id bytes][4 bytes LE: alpn_len][alpn bytes][1 byte: is_alive]
fn encode_hook_handshake_payload(info: &CoreHookHandshakeInfo) -> Vec<u8> {
    let id = info.remote_endpoint_id.as_bytes();
    let alpn = &info.alpn;
    let mut buf = Vec::with_capacity(9 + id.len() + alpn.len());
    buf.extend_from_slice(&(id.len() as u32).to_le_bytes());
    buf.extend_from_slice(id);
    buf.extend_from_slice(&(alpn.len() as u32).to_le_bytes());
    buf.extend_from_slice(alpn);
    buf.push(info.is_alive as u8);
    buf
}

// Helper to check if operation was cancelled
fn check_cancelled(
    cancelled: &Arc<std::sync::atomic::AtomicBool>,
    bridge: &Arc<BridgeRuntime>,
    op_id: u64,
    user_data: u64,
) -> bool {
    if cancelled.load(Ordering::SeqCst) {
        bridge.emit_simple(
            iroh_event_kind_t::IROH_EVENT_OPERATION_CANCELLED,
            iroh_status_t::IROH_STATUS_CANCELLED,
            op_id,
            0,
            0,
            user_data,
            iroh_status_t::IROH_STATUS_CANCELLED as i32,
        );
        true
    } else {
        false
    }
}

// ============================================================================
// C FFI Functions - Versioning
// ============================================================================

#[no_mangle]
pub extern "C" fn iroh_abi_version_major() -> u32 {
    IROH_ABI_VERSION_MAJOR
}

#[no_mangle]
pub extern "C" fn iroh_abi_version_minor() -> u32 {
    IROH_ABI_VERSION_MINOR
}

#[no_mangle]
pub extern "C" fn iroh_abi_version_patch() -> u32 {
    IROH_ABI_VERSION_PATCH
}

// ============================================================================
// C FFI Functions - Runtime
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_runtime_new(
    config: *const iroh_runtime_config_t,
    out_runtime: *mut iroh_runtime_t,
) -> i32 {
    if out_runtime.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let worker_threads = config.as_ref().map(|c| c.worker_threads).unwrap_or(0);
    let queue_capacity = config
        .as_ref()
        .map(|c| c.event_queue_capacity)
        .unwrap_or(4096);

    let bridge = match BridgeRuntime::new(worker_threads, queue_capacity) {
        Ok(b) => b,
        Err(e) => return set_last_error(e),
    };

    let runtime_id = new_handle();

    if let Ok(mut guard) = runtimes().lock() {
        guard.insert(runtime_id, Arc::new(bridge));
        *out_runtime = runtime_id;
        iroh_status_t::IROH_STATUS_OK as i32
    } else {
        iroh_status_t::IROH_STATUS_INTERNAL as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_runtime_close(runtime: iroh_runtime_t) -> i32 {
    match runtimes().lock() {
        Ok(mut guard) => {
            if guard.remove(&runtime).is_some() {
                iroh_status_t::IROH_STATUS_OK as i32
            } else {
                iroh_status_t::IROH_STATUS_NOT_FOUND as i32
            }
        }
        Err(_) => iroh_status_t::IROH_STATUS_INTERNAL as i32,
    }
}

// ============================================================================
// C FFI Functions - Secret Key
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_secret_key_generate(
    out_key_ptr: *mut u8,
    out_key_capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let mut key = [0u8; 32];
    if getrandom::getrandom(&mut key).is_err() {
        return iroh_status_t::IROH_STATUS_INTERNAL as i32;
    }
    let len = key.len();

    *out_len = len;

    if out_key_ptr.is_null() || out_key_capacity < len {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
    }

    unsafe {
        ptr::copy_nonoverlapping(key.as_ptr(), out_key_ptr, len);
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Event Polling
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_poll_events(
    runtime: iroh_runtime_t,
    out_events: *mut iroh_event_t,
    max_events: usize,
    timeout_ms: u32,
) -> usize {
    if out_events.is_null() || max_events == 0 {
        return 0;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(_) => return 0,
    };

    let mut guard = match bridge.events_rx.lock() {
        Ok(g) => g,
        Err(_) => return 0,
    };

    let first = if timeout_ms == 0 {
        guard.try_recv().ok()
    } else {
        bridge.runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(timeout_ms as u64), guard.recv())
                .await
                .ok()
                .flatten()
        })
    };

    let mut written = 0usize;

    if let Some(event_owned) = first {
        let ev = to_iroh_event(&event_owned.event, &bridge.buffers);
        unsafe {
            ptr::write(out_events.add(written), ev);
        }
        std::mem::forget(event_owned.payload);
        written += 1;
    } else {
        return 0;
    }

    while written < max_events {
        match guard.try_recv() {
            Ok(event_owned) => {
                let ev = to_iroh_event(&event_owned.event, &bridge.buffers);
                unsafe {
                    ptr::write(out_events.add(written), ev);
                }
                std::mem::forget(event_owned.payload);
                written += 1;
            }
            Err(_) => break,
        }
    }

    written
}

/// Convert internal EventInternal to C-compatible iroh_event_t
fn to_iroh_event(event: &EventInternal, buffers: &BufferRegistry) -> iroh_event_t {
    let (data_ptr, data_len) = if event.buffer_id != 0 {
        if let Some(buf) = buffers.get(event.buffer_id) {
            (buf.as_ptr(), buf.len())
        } else {
            (ptr::null(), 0)
        }
    } else {
        (ptr::null(), 0)
    };

    iroh_event_t {
        struct_size: event.struct_size,
        kind: event.kind,
        status: event.status,
        operation: event.operation,
        handle: event.handle,
        related: event.related,
        user_data: event.user_data,
        data_ptr,
        data_len,
        buffer: event.buffer_id,
        error_code: event.error_code,
        flags: event.flags,
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_buffer_release(runtime: iroh_runtime_t, buffer: u64) -> i32 {
    if buffer == 0 {
        return iroh_status_t::IROH_STATUS_OK as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(_) => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    if bridge.buffers.remove(buffer).is_some() {
        iroh_status_t::IROH_STATUS_OK as i32
    } else {
        iroh_status_t::IROH_STATUS_NOT_FOUND as i32
    }
}

// ============================================================================
// C FFI Functions - Operation Cancellation
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_operation_cancel(
    runtime: iroh_runtime_t,
    operation: iroh_operation_t,
) -> i32 {
    if operation == 0 {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.cancel_operation(operation) {
        bridge.emit_simple(
            iroh_event_kind_t::IROH_EVENT_OPERATION_CANCELLED,
            iroh_status_t::IROH_STATUS_CANCELLED,
            operation,
            0,
            0,
            0,
            iroh_status_t::IROH_STATUS_CANCELLED as i32,
        );
        iroh_status_t::IROH_STATUS_OK as i32
    } else {
        iroh_status_t::IROH_STATUS_NOT_FOUND as i32
    }
}

// ============================================================================
// C FFI Functions - Error
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_last_error_message(buffer: *mut u8, capacity: usize) -> usize {
    let msg = LAST_ERROR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|s| s.to_bytes().to_vec())
            .unwrap_or_default()
    });

    let len = msg.len().min(capacity);
    if !buffer.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(msg.as_ptr(), buffer, len);
        }
    }

    msg.len()
}

#[no_mangle]
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
        iroh_status_t::IROH_STATUS_TIMEOUT => "IROH_STATUS_TIMEOUT\0",
        iroh_status_t::IROH_STATUS_CANCELLED => "IROH_STATUS_CANCELLED\0",
        iroh_status_t::IROH_STATUS_CONNECTION_REFUSED => "IROH_STATUS_CONNECTION_REFUSED\0",
        iroh_status_t::IROH_STATUS_STREAM_RESET => "IROH_STATUS_STREAM_RESET\0",
    };
    s.as_ptr() as *const c_char
}

// ============================================================================
// C FFI Functions - Node
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_node_memory(
    runtime: iroh_runtime_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match CoreNode::memory().await {
            Ok(node) => {
                let handle = bridge2.nodes.insert(node);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_NODE_CREATED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_node_persistent(
    runtime: iroh_runtime_t,
    path: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let path_str = match unsafe { read_string(&path) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match CoreNode::persistent(path_str).await {
            Ok(node) => {
                let handle = bridge2.nodes.insert(node);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_NODE_CREATED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_node_close(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        node_arc.close().await;
        bridge2.nodes.remove(node);
        bridge2.emit_simple(
            iroh_event_kind_t::IROH_EVENT_CLOSED,
            iroh_status_t::IROH_STATUS_OK,
            op_id,
            node,
            0,
            user_data,
            0,
        );
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_node_id(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let id = node_arc.node_id();
    let len = id.len();

    *out_len = len;

    if capacity < len {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(id.as_ptr(), out_buf, len);
        }
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_node_addr_info(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    out_buf: *mut u8,
    buf_capacity: usize,
    out_addr: *mut iroh_node_addr_t,
) -> i32 {
    if out_addr.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let addr = node_arc.node_addr_info();

    // Pack into scratch buffer: endpoint_id, relay_url, direct_addresses
    let mut offset = 0;

    // endpoint_id
    if offset + addr.endpoint_id.len() + 1 > buf_capacity {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
    }
    let ep_id_offset = offset;
    unsafe {
        ptr::copy_nonoverlapping(
            addr.endpoint_id.as_ptr(),
            out_buf.add(offset),
            addr.endpoint_id.len(),
        );
    }
    offset += addr.endpoint_id.len();
    unsafe {
        *out_buf.add(offset) = 0;
    }
    offset += 1;

    // relay_url
    let relay_offset = if let Some(ref url) = addr.relay_url {
        if offset + url.len() + 1 > buf_capacity {
            return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
        }
        let rel_off = offset;
        unsafe {
            ptr::copy_nonoverlapping(url.as_ptr(), out_buf.add(offset), url.len());
        }
        offset += url.len();
        unsafe {
            *out_buf.add(offset) = 0;
        }
        Some(rel_off)
    } else {
        unsafe {
            *out_buf.add(offset) = 0;
        }
        None
    };

    *out_addr = iroh_node_addr_t {
        endpoint_id: iroh_bytes_t {
            ptr: unsafe { out_buf.add(ep_id_offset) },
            len: addr.endpoint_id.len(),
        },
        relay_url: iroh_bytes_t {
            ptr: relay_offset
                .map(|o| unsafe { out_buf.add(o) })
                .unwrap_or(ptr::null_mut()),
            len: addr.relay_url.as_ref().map(|s| s.len()).unwrap_or(0),
        },
        direct_addresses: iroh_bytes_list_t {
            items: ptr::null(),
            len: 0,
        },
    };

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_node_export_secret_key(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let key = node_arc.export_secret_key();
    let len = key.len();

    *out_len = len;

    if capacity < len {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(key.as_ptr(), out_buf, len);
        }
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Endpoint
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_endpoint_create(
    runtime: iroh_runtime_t,
    config: *const iroh_endpoint_config_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if config.is_null() || out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let cfg = unsafe { *config };

    let enable_hooks = cfg.enable_hooks != 0;
    let hook_timeout_ms = if cfg.hook_timeout_ms > 0 {
        cfg.hook_timeout_ms
    } else {
        5000
    };

    let core_config = CoreEndpointConfig {
        relay_mode: match cfg.relay_mode {
            0 => None,
            1 => Some("custom".to_string()),
            2 => Some("disabled".to_string()),
            3 => Some("staging".to_string()),
            _ => None,
        },
        relay_urls: unsafe { read_string_list(&cfg.relay_urls) },
        alpns: if cfg.alpns.items.is_null() || cfg.alpns.len == 0 {
            Vec::new()
        } else {
            unsafe {
                slice::from_raw_parts(cfg.alpns.items, cfg.alpns.len)
                    .iter()
                    .map(|b| read_bytes(b))
                    .collect()
            }
        },
        secret_key: unsafe { read_bytes_opt(&cfg.secret_key) },
        enable_discovery: cfg.enable_discovery != 0,
        enable_monitoring: true, // Always enable monitoring for FFI endpoints
        enable_hooks,
        hook_timeout_ms,
        // Phase 1d fields: read from the C struct (added in iroh_endpoint_config_t).
        bind_addr: unsafe { read_string_opt(&cfg.bind_addr) },
        clear_ip_transports: cfg.clear_ip_transports != 0,
        clear_relay_transports: cfg.clear_relay_transports != 0,
        portmapper_config: match cfg.portmapper_config {
            0 => None,           // 0 = enabled (default) → let build_endpoint_config use its default
            1 => Some("disabled".to_string()),
            _ => None,
        },
        proxy_url: unsafe { read_string_opt(&cfg.proxy_url) },
        proxy_from_env: cfg.proxy_from_env != 0,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match CoreNetClient::create_with_config(core_config).await {
            Ok(endpoint) => {
                // Take the hook receiver before inserting endpoint into the registry.
                let hook_receiver = if enable_hooks {
                    endpoint.take_hook_receiver()
                } else {
                    None
                };

                let handle = bridge2.endpoints.insert(endpoint);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_ENDPOINT_CREATED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    0,
                    user_data,
                    0,
                );

                // Spawn background tasks to drain hook events and emit them to the queue.
                if let Some(receiver) = hook_receiver {
                    let CoreHookReceiver {
                        before_connect_rx,
                        after_handshake_rx,
                    } = receiver;

                    // before_connect drainer
                    let bridge3 = bridge2.clone();
                    tokio::spawn(async move {
                        let mut rx = before_connect_rx;
                        while let Some((info, reply_tx)) = rx.recv().await {
                            let state = HookInvocationState {
                                sender: std::sync::Mutex::new(Some(HookSender::BeforeConnect(
                                    reply_tx,
                                ))),
                            };
                            let invocation_id = bridge3.hook_invocations.insert(state);
                            let payload = encode_hook_connect_payload(&info);
                            let event = EventInternal::new(
                                iroh_event_kind_t::IROH_EVENT_HOOK_BEFORE_CONNECT,
                                iroh_status_t::IROH_STATUS_OK,
                                0,
                                handle,
                                invocation_id,
                                user_data,
                                0,
                            );
                            bridge3.emit_with_data(event, payload);
                        }
                    });

                    // after_handshake drainer
                    let bridge4 = bridge2.clone();
                    tokio::spawn(async move {
                        let mut rx = after_handshake_rx;
                        while let Some((info, reply_tx)) = rx.recv().await {
                            let state = HookInvocationState {
                                sender: std::sync::Mutex::new(Some(HookSender::AfterConnect(
                                    reply_tx,
                                ))),
                            };
                            let invocation_id = bridge4.hook_invocations.insert(state);
                            let payload = encode_hook_handshake_payload(&info);
                            let event = EventInternal::new(
                                iroh_event_kind_t::IROH_EVENT_HOOK_AFTER_CONNECT,
                                iroh_status_t::IROH_STATUS_OK,
                                0,
                                handle,
                                invocation_id,
                                user_data,
                                0,
                            );
                            bridge4.emit_with_data(event, payload);
                        }
                    });
                }
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_endpoint_close(
    runtime: iroh_runtime_t,
    endpoint: iroh_endpoint_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = match bridge.endpoints.get(endpoint) {
        Some(e) => e,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        ep_arc.close().await;
        bridge2.endpoints.remove(endpoint);
        bridge2.emit_simple(
            iroh_event_kind_t::IROH_EVENT_CLOSED,
            iroh_status_t::IROH_STATUS_OK,
            op_id,
            endpoint,
            0,
            user_data,
            0,
        );
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_endpoint_id(
    runtime: iroh_runtime_t,
    endpoint: iroh_endpoint_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = match bridge.endpoints.get(endpoint) {
        Some(e) => e,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let id = ep_arc.endpoint_id();
    let len = id.len();

    *out_len = len;

    if capacity < len {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(id.as_ptr(), out_buf, len);
        }
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_endpoint_addr_info(
    runtime: iroh_runtime_t,
    endpoint: iroh_endpoint_t,
    out_buf: *mut u8,
    buf_capacity: usize,
    out_addr: *mut iroh_node_addr_t,
) -> i32 {
    if out_addr.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = match bridge.endpoints.get(endpoint) {
        Some(e) => e,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let addr = ep_arc.endpoint_addr_info();

    // Pack into scratch buffer: endpoint_id, relay_url, direct_addresses
    let mut offset = 0;

    // endpoint_id
    if offset + addr.endpoint_id.len() + 1 > buf_capacity {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
    }
    let ep_id_offset = offset;
    unsafe {
        ptr::copy_nonoverlapping(
            addr.endpoint_id.as_ptr(),
            out_buf.add(offset),
            addr.endpoint_id.len(),
        );
    }
    offset += addr.endpoint_id.len();
    unsafe {
        *out_buf.add(offset) = 0;
    }
    offset += 1;

    // relay_url
    let relay_offset = if let Some(ref url) = addr.relay_url {
        if offset + url.len() + 1 > buf_capacity {
            return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
        }
        let rel_off = offset;
        unsafe {
            ptr::copy_nonoverlapping(url.as_ptr(), out_buf.add(offset), url.len());
        }
        offset += url.len();
        unsafe {
            *out_buf.add(offset) = 0;
        }
        offset += 1;
        Some(rel_off)
    } else {
        unsafe {
            *out_buf.add(offset) = 0;
        }
        offset += 1;
        None
    };

    // direct_addresses - pack as additional null-terminated strings
    let mut addr_offsets = Vec::new();
    for direct_addr in &addr.direct_addresses {
        if offset + direct_addr.len() + 1 > buf_capacity {
            return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
        }
        let addr_off = offset;
        unsafe {
            ptr::copy_nonoverlapping(direct_addr.as_ptr(), out_buf.add(offset), direct_addr.len());
        }
        offset += direct_addr.len();
        unsafe {
            *out_buf.add(offset) = 0;
        }
        offset += 1;
        addr_offsets.push(addr_off);
    }

    // Create the bytes list for direct addresses
    let direct_addrs_ptr: *const iroh_bytes_t = if addr_offsets.is_empty() {
        ptr::null()
    } else {
        // Allocate space for the list at the end
        let list_offset = offset;
        offset += addr_offsets.len() * std::mem::size_of::<iroh_bytes_t>();
        if offset > buf_capacity {
            return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
        }

        for (i, &addr_off) in addr_offsets.iter().enumerate() {
            let item_ptr = unsafe {
                out_buf
                    .add(list_offset)
                    .add(i * std::mem::size_of::<iroh_bytes_t>())
                    as *mut iroh_bytes_t
            };
            let addr_len = addr.direct_addresses[i].len();
            unsafe {
                *item_ptr = iroh_bytes_t {
                    ptr: out_buf.add(addr_off),
                    len: addr_len,
                };
            }
        }
        unsafe { out_buf.add(list_offset) as *const iroh_bytes_t }
    };

    *out_addr = iroh_node_addr_t {
        endpoint_id: iroh_bytes_t {
            ptr: unsafe { out_buf.add(ep_id_offset) },
            len: addr.endpoint_id.len(),
        },
        relay_url: iroh_bytes_t {
            ptr: relay_offset
                .map(|o| unsafe { out_buf.add(o) })
                .unwrap_or(ptr::null_mut()),
            len: addr.relay_url.as_ref().map(|s| s.len()).unwrap_or(0),
        },
        direct_addresses: iroh_bytes_list_t {
            items: direct_addrs_ptr,
            len: addr.direct_addresses.len(),
        },
    };

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_stream_stop(
    runtime: iroh_runtime_t,
    recv_stream: iroh_recv_stream_t,
    error_code: u32,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    if let Err(e) = stream_arc.stop(error_code as u64) {
        return set_last_error(e);
    }

    bridge.recv_streams.remove(recv_stream);
    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Connections
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_connect(
    runtime: iroh_runtime_t,
    endpoint_or_node: u64,
    config: *const iroh_connect_config_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if config.is_null() || out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    // Try to get as endpoint first, then as node
    let ep_arc = bridge.endpoints.get(endpoint_or_node).or_else(|| {
        bridge
            .nodes
            .get(endpoint_or_node)
            .map(|n| n.net_client())
            .map(Arc::new)
    });

    let ep = match ep_arc {
        Some(e) => e,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let cfg = unsafe { *config };
    let node_id = match unsafe { read_string(&cfg.node_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let alpn = unsafe { read_bytes(&cfg.alpn) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match ep.connect(node_id, alpn).await {
            Ok(conn) => {
                let handle = bridge2.connections.insert(conn);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_CONNECTED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    endpoint_or_node,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_accept(
    runtime: iroh_runtime_t,
    endpoint_or_node: u64,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = bridge.endpoints.get(endpoint_or_node).or_else(|| {
        bridge
            .nodes
            .get(endpoint_or_node)
            .map(|n| n.net_client())
            .map(Arc::new)
    });

    let ep = match ep_arc {
        Some(e) => e,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match ep.accept().await {
            Ok(conn) => {
                let handle = bridge2.connections.insert(conn);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_CONNECTION_ACCEPTED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    endpoint_or_node,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// Phase 1e: Unified Aster Node — custom ALPNs on the shared iroh Router
// ============================================================================

/// Create an in-memory node with blobs/docs/gossip + custom aster ALPNs.
/// Emits IROH_EVENT_NODE_CREATED on success.
#[no_mangle]
pub unsafe extern "C" fn iroh_node_memory_with_alpns(
    runtime: iroh_runtime_t,
    alpns: *const *const u8,
    alpn_lens: *const usize,
    alpn_count: usize,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    // Extract ALPN byte slices.
    let aster_alpns: Vec<Vec<u8>> = (0..alpn_count)
        .map(|i| unsafe {
            let ptr = *alpns.add(i);
            let len = *alpn_lens.add(i);
            std::slice::from_raw_parts(ptr, len).to_vec()
        })
        .collect();

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match aster_transport_core::CoreNode::memory_with_alpns(aster_alpns, None).await {
            Ok(node) => {
                let handle = bridge2.nodes.insert(node);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_NODE_CREATED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Pull the next incoming aster-ALPN connection from the node's queue.
/// Long-poll: spawns a tokio task, emits IROH_EVENT_ASTER_ACCEPTED when a
/// connection arrives. event.handle = connection_handle, event.data_ptr/len
/// = ALPN bytes, event.buffer = lease to release via iroh_buffer_release.
#[no_mangle]
pub unsafe extern "C" fn iroh_node_accept_aster(
    runtime: iroh_runtime_t,
    node: u64,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match node_arc.accept_aster().await {
            Ok((alpn, conn)) => {
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
                // ALPN bytes go into the event payload (data_ptr/data_len).
                bridge2.emit_with_data(event, alpn);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_connection_remote_id(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let id = conn_arc.remote_id();
    let len = id.len();

    *out_len = len;

    if capacity < len {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(id.as_ptr(), out_buf, len);
        }
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_connection_close(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    error_code: u32,
    reason: iroh_bytes_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let reason_bytes = unsafe { read_bytes(&reason) };

    if let Err(e) = conn_arc.close(error_code as u64, reason_bytes) {
        return set_last_error(e);
    }

    bridge.connections.remove(connection);
    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_connection_closed(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let closed_info = conn_arc.closed().await;
        // Serialize closed info: kind + error_code + reason
        let mut payload = Vec::new();
        payload.extend_from_slice(closed_info.kind.as_bytes());
        payload.push(0); // null separator
        if let Some(reason) = &closed_info.reason {
            payload.extend_from_slice(reason);
        }

        let error_code = closed_info.code.map(|c| c as i32).unwrap_or(-1);

        let event = EventInternal::new(
            iroh_event_kind_t::IROH_EVENT_CONNECTION_CLOSED,
            iroh_status_t::IROH_STATUS_OK,
            op_id,
            connection,
            0,
            user_data,
            error_code,
        );
        bridge2.emit_with_data(event, payload);
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_connection_send_datagram(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    data: iroh_bytes_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let data_bytes = unsafe { read_bytes(&data) };

    if let Err(e) = conn_arc.send_datagram(data_bytes) {
        return set_last_error(e);
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_connection_read_datagram(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match conn_arc.read_datagram().await {
            Ok(data) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_BYTES_RESULT,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    connection,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Streams
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_open_bi(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match conn_arc.open_bi().await {
            Ok((send, recv)) => {
                let send_handle = bridge2.send_streams.insert(send);
                let recv_handle = bridge2.recv_streams.insert(recv);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_STREAM_OPENED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    send_handle,
                    recv_handle,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_accept_bi(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match conn_arc.accept_bi().await {
            Ok((send, recv)) => {
                let send_handle = bridge2.send_streams.insert(send);
                let recv_handle = bridge2.recv_streams.insert(recv);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_STREAM_ACCEPTED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    send_handle,
                    recv_handle,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_open_uni(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match conn_arc.open_uni().await {
            Ok(send) => {
                let handle = bridge2.send_streams.insert(send);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_STREAM_OPENED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_accept_uni(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match conn_arc.accept_uni().await {
            Ok(recv) => {
                let handle = bridge2.recv_streams.insert(recv);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_STREAM_ACCEPTED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_stream_write(
    runtime: iroh_runtime_t,
    send_stream: iroh_send_stream_t,
    data: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.send_streams.get(send_stream) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let data_bytes = unsafe { read_bytes(&data) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.write_all(data_bytes).await {
            Ok(()) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_SEND_COMPLETED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    send_stream,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_stream_finish(
    runtime: iroh_runtime_t,
    send_stream: iroh_send_stream_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.send_streams.get(send_stream) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.finish().await {
            Ok(()) => {
                bridge2.send_streams.remove(send_stream);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_STREAM_FINISHED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    send_stream,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_stream_read(
    runtime: iroh_runtime_t,
    recv_stream: iroh_recv_stream_t,
    max_len: usize,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.read(max_len).await {
            Ok(Some(data)) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_FRAME_RECEIVED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    recv_stream,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Ok(None) => {
                bridge2.recv_streams.remove(recv_stream);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_STREAM_FINISHED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    recv_stream,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_stream_read_to_end(
    runtime: iroh_runtime_t,
    recv_stream: iroh_recv_stream_t,
    max_size: usize,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.read_to_end(max_size).await {
            Ok(data) => {
                bridge2.recv_streams.remove(recv_stream);
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_BYTES_RESULT,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    recv_stream,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_stream_read_exact(
    runtime: iroh_runtime_t,
    recv_stream: iroh_recv_stream_t,
    exact_len: usize,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.read_exact(exact_len).await {
            Ok(data) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_BYTES_RESULT,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    recv_stream,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_stream_stopped(
    runtime: iroh_runtime_t,
    send_stream: iroh_send_stream_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.send_streams.get(send_stream) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.stopped().await {
            Ok(Some(error_code)) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_STREAM_RESET,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    send_stream,
                    0,
                    user_data,
                    error_code as i32,
                );
            }
            Ok(None) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_STREAM_FINISHED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    send_stream,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_endpoint_export_secret_key(
    runtime: iroh_runtime_t,
    endpoint: iroh_endpoint_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = match bridge.endpoints.get(endpoint) {
        Some(e) => e,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let key = ep_arc.export_secret_key();
    let len = key.len();

    *out_len = len;

    if capacity < len {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(key.as_ptr(), out_buf, len);
        }
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_add_node_addr(
    runtime: iroh_runtime_t,
    endpoint: iroh_endpoint_t,
    addr: iroh_node_addr_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let ep_arc = match bridge.endpoints.get(endpoint) {
        Some(e) => e,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    // Build CoreNodeAddr from FFI struct
    let endpoint_id = match unsafe { read_string(&addr.endpoint_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let relay_url = if addr.relay_url.ptr.is_null() || addr.relay_url.len == 0 {
        None
    } else {
        match unsafe { read_string(&addr.relay_url) } {
            Ok(s) => Some(s),
            Err(e) => return e,
        }
    };

    let mut direct_addresses = Vec::new();
    if !addr.direct_addresses.items.is_null() && addr.direct_addresses.len > 0 {
        let items = unsafe {
            slice::from_raw_parts(addr.direct_addresses.items, addr.direct_addresses.len)
        };
        for item in items {
            match unsafe { read_string(item) } {
                Ok(s) => direct_addresses.push(s),
                Err(e) => return e,
            }
        }
    }

    let core_addr = aster_transport_core::CoreNodeAddr {
        endpoint_id,
        relay_url,
        direct_addresses,
    };

    // Convert to EndpointAddr and add to the endpoint's address lookup
    match aster_transport_core::core_to_endpoint_addr(&core_addr) {
        Ok(endpoint_addr) => {
            if let Ok(lookup) = ep_arc.endpoint.address_lookup() {
                let mem = iroh::address_lookup::memory::MemoryLookup::new();
                mem.add_endpoint_info(endpoint_addr);
                lookup.add(mem);
            }
        }
        Err(e) => return set_last_error(e),
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Handle Free (typed, one per handle kind)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_node_free(runtime: iroh_runtime_t, node: iroh_node_t) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.nodes.remove(node).is_some() {
        iroh_status_t::IROH_STATUS_OK as i32
    } else {
        iroh_status_t::IROH_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_endpoint_free(
    runtime: iroh_runtime_t,
    endpoint: iroh_endpoint_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.endpoints.remove(endpoint).is_some() {
        iroh_status_t::IROH_STATUS_OK as i32
    } else {
        iroh_status_t::IROH_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_connection_free(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.connections.remove(connection).is_some() {
        iroh_status_t::IROH_STATUS_OK as i32
    } else {
        iroh_status_t::IROH_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_send_stream_free(
    runtime: iroh_runtime_t,
    stream: iroh_send_stream_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.send_streams.remove(stream).is_some() {
        iroh_status_t::IROH_STATUS_OK as i32
    } else {
        iroh_status_t::IROH_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_recv_stream_free(
    runtime: iroh_runtime_t,
    stream: iroh_recv_stream_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.recv_streams.remove(stream).is_some() {
        iroh_status_t::IROH_STATUS_OK as i32
    } else {
        iroh_status_t::IROH_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_doc_free(runtime: iroh_runtime_t, doc: u64) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.docs.remove(doc).is_some() {
        iroh_status_t::IROH_STATUS_OK as i32
    } else {
        iroh_status_t::IROH_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_gossip_topic_free(runtime: iroh_runtime_t, topic: u64) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.gossip_topics.remove(topic).is_some() {
        iroh_status_t::IROH_STATUS_OK as i32
    } else {
        iroh_status_t::IROH_STATUS_NOT_FOUND as i32
    }
}

// ============================================================================
// C FFI Functions - Blobs (simplified for Phase 1)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_add_bytes(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    data: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let data_bytes = unsafe { read_bytes(&data) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.add_bytes(data_bytes).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_BLOB_ADDED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, hash.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_read(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    hash_hex: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let hash = match unsafe { read_string(&hash_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.read_to_bytes(hash).await {
            Ok(data) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_BLOB_READ,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Blobs: Collection (sendme-compatible)
// ============================================================================

/// Store bytes as a single-file Collection (HashSeq), compatible with sendme.
/// Emits IROH_EVENT_BLOB_ADDED with the collection hash in the payload.
#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_add_bytes_as_collection(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    name: iroh_bytes_t,
    data: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let name_str = match unsafe { read_string(&name) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let data_bytes = unsafe { read_bytes(&data) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.add_bytes_as_collection(name_str, data_bytes).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_BLOB_ADDED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, hash.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Create a ticket for a Collection (HashSeq format), compatible with sendme.
/// Writes the ticket string into the caller-provided buffer.
#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_create_collection_ticket(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    hash_hex: iroh_bytes_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let hash = match unsafe { read_string(&hash_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let ticket = match blobs.create_collection_ticket(hash) {
        Ok(t) => t,
        Err(e) => return set_last_error(e),
    };

    let len = ticket.len();
    *out_len = len;

    if capacity < len {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(ticket.as_ptr(), out_buf, len);
        }
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Blobs: Ticket & Download
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_create_ticket(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    hash_hex: iroh_bytes_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let hash = match unsafe { read_string(&hash_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let ticket = match blobs.create_ticket(hash) {
        Ok(t) => t,
        Err(e) => return set_last_error(e),
    };

    let len = ticket.len();
    *out_len = len;

    if capacity < len {
        return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(ticket.as_ptr(), out_buf, len);
        }
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_download(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    ticket: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let ticket_str = match unsafe { read_string(&ticket) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.download_blob(ticket_str).await {
            Ok(data) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_BLOB_DOWNLOADED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Blob Status / Has (Phase 1c.3)
// ============================================================================

/// Check the status of a blob in the local store. Synchronous.
///
/// Writes to `out_status`: 0 = not_found, 1 = partial, 2 = complete.
/// Writes to `out_size`: byte size (0 if not_found).
#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_status(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_status: *mut u32,
    out_size: *mut u64,
) -> i32 {
    if hash_hex_ptr.is_null() || out_status.is_null() || out_size.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();

    let blobs = node_arc.blobs_client();
    let result = bridge
        .runtime
        .block_on(async move { blobs.blob_status(hash_hex).await });

    match result {
        Ok(status) => {
            let (code, size) = match status {
                aster_transport_core::CoreBlobStatus::NotFound => (0u32, 0u64),
                aster_transport_core::CoreBlobStatus::Partial { size } => (1u32, size),
                aster_transport_core::CoreBlobStatus::Complete { size } => (2u32, size),
            };
            unsafe {
                *out_status = code;
                *out_size = size;
            }
            iroh_status_t::IROH_STATUS_OK as i32
        }
        Err(_) => iroh_status_t::IROH_STATUS_INTERNAL as i32,
    }
}

/// Check if a blob is fully stored locally. Synchronous.
///
/// Writes to `out_has`: 1 = complete/present, 0 = not complete.
#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_has(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_has: *mut u32,
) -> i32 {
    if hash_hex_ptr.is_null() || out_has.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();

    let blobs = node_arc.blobs_client();
    let result = bridge
        .runtime
        .block_on(async move { blobs.blob_has(hash_hex).await });

    match result {
        Ok(has) => {
            unsafe {
                *out_has = if has { 1 } else { 0 };
            }
            iroh_status_t::IROH_STATUS_OK as i32
        }
        Err(_) => iroh_status_t::IROH_STATUS_INTERNAL as i32,
    }
}

// ============================================================================
// C FFI Functions - Blob Transfer Observability (Phase 1d)
// ============================================================================

/// Snapshot of the current bitfield for a blob.
/// Fills `out_is_complete` (1=complete, 0=partial/not-found) and `out_size` (total bytes, 0 if unknown).
/// Returns IROH_STATUS_NOT_FOUND if the node handle is unknown.
/// Returns IROH_STATUS_INVALID_ARGUMENT if any pointer is null.
#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_observe_snapshot(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_is_complete: *mut u32,
    out_size: *mut u64,
) -> i32 {
    if hash_hex_ptr.is_null() || out_is_complete.is_null() || out_size.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();
    let blobs = node_arc.blobs_client();

    let result = bridge
        .runtime
        .block_on(async move { blobs.blob_observe_snapshot(hash_hex).await });

    match result {
        Ok(r) => {
            unsafe {
                *out_is_complete = if r.is_complete { 1 } else { 0 };
                *out_size = r.size;
            }
            iroh_status_t::IROH_STATUS_OK as i32
        }
        Err(_) => iroh_status_t::IROH_STATUS_INTERNAL as i32,
    }
}

/// Wait until a blob is fully downloaded locally.
/// Emits IROH_EVENT_BLOB_OBSERVE_COMPLETE (via IROH_EVENT_UNIT_RESULT) when complete,
/// or an error event if the observation stream ends without completion.
#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_observe_complete(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if hash_hex_ptr.is_null() || out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let blobs = node_arc.blobs_client();
        match blobs.blob_observe_complete(hash_hex).await {
            Ok(()) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_BLOB_OBSERVE_COMPLETE,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Check local availability of a blob using the Remote API.
/// Fills `out_is_complete` (1=complete, 0=partial) and `out_local_bytes` (bytes we have locally).
/// Returns IROH_STATUS_NOT_FOUND if the node handle is unknown.
/// Returns IROH_STATUS_INVALID_ARGUMENT if any pointer is null.
#[no_mangle]
pub unsafe extern "C" fn iroh_blobs_local_info(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_is_complete: *mut u32,
    out_local_bytes: *mut u64,
) -> i32 {
    if hash_hex_ptr.is_null() || out_is_complete.is_null() || out_local_bytes.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();
    let blobs = node_arc.blobs_client();

    let result = bridge
        .runtime
        .block_on(async move { blobs.blob_local_info(hash_hex).await });

    match result {
        Ok(r) => {
            unsafe {
                *out_is_complete = if r.is_complete { 1 } else { 0 };
                *out_local_bytes = r.local_bytes;
            }
            iroh_status_t::IROH_STATUS_OK as i32
        }
        Err(_) => iroh_status_t::IROH_STATUS_INTERNAL as i32,
    }
}

// ============================================================================
// C FFI Functions - Tags (Phase 1c)
// ============================================================================

/// Encode a CoreTagInfo as a null-separated UTF-8 payload: name\0hash\0format\0
fn encode_tag_payload(t: &aster_transport_core::CoreTagInfo) -> Vec<u8> {
    let mut buf = Vec::with_capacity(t.name.len() + t.hash.len() + t.format.len() + 3);
    buf.extend_from_slice(t.name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(t.hash.as_bytes());
    buf.push(0);
    buf.extend_from_slice(t.format.as_bytes());
    buf.push(0);
    buf
}

/// Encode a list of CoreTagInfo as concatenated null-separated records.
/// `flags` in the event holds the count.
fn encode_tag_list_payload(tags: &[aster_transport_core::CoreTagInfo]) -> Vec<u8> {
    tags.iter().flat_map(encode_tag_payload).collect()
}

/// Set a named tag. format: 0 = raw, 1 = hash_seq. Emits IROH_EVENT_TAG_SET.
#[no_mangle]
pub unsafe extern "C" fn iroh_tags_set(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    name_ptr: *const u8,
    name_len: usize,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    format: u32,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() || name_ptr.is_null() || hash_hex_ptr.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let name = String::from_utf8(unsafe { slice::from_raw_parts(name_ptr, name_len).to_vec() })
        .unwrap_or_default();
    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();
    let format_str = if format == 1 {
        "hash_seq".to_string()
    } else {
        "raw".to_string()
    };

    let blobs = node_arc.blobs_client();
    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        match blobs.tag_set(name, hash_hex, format_str).await {
            Ok(()) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_TAG_SET,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Get a tag by name. Emits IROH_EVENT_TAG_GET with payload on found, NOT_FOUND status if absent.
#[no_mangle]
pub unsafe extern "C" fn iroh_tags_get(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    name_ptr: *const u8,
    name_len: usize,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() || name_ptr.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let name = String::from_utf8(unsafe { slice::from_raw_parts(name_ptr, name_len).to_vec() })
        .unwrap_or_default();

    let blobs = node_arc.blobs_client();
    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        match blobs.tag_get(name).await {
            Ok(Some(tag)) => {
                let payload = encode_tag_payload(&tag);
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_TAG_GET,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, payload);
            }
            Ok(None) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_TAG_GET,
                    iroh_status_t::IROH_STATUS_NOT_FOUND,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Delete a tag by name. Emits IROH_EVENT_TAG_DELETED with count in event.flags.
#[no_mangle]
pub unsafe extern "C" fn iroh_tags_delete(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    name_ptr: *const u8,
    name_len: usize,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() || name_ptr.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let name = String::from_utf8(unsafe { slice::from_raw_parts(name_ptr, name_len).to_vec() })
        .unwrap_or_default();

    let blobs = node_arc.blobs_client();
    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        match blobs.tag_delete(name).await {
            Ok(count) => {
                let mut event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_TAG_DELETED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                event.flags = count as u32;
                bridge2.emit(EventOwned {
                    event,
                    payload: None,
                });
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// List tags matching a prefix (empty prefix = all tags).
/// Emits IROH_EVENT_TAG_LIST with packed tag records in payload; event.flags = count.
#[no_mangle]
pub unsafe extern "C" fn iroh_tags_list_prefix(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    prefix_ptr: *const u8,
    prefix_len: usize,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let prefix = if prefix_ptr.is_null() || prefix_len == 0 {
        String::new()
    } else {
        String::from_utf8(unsafe { slice::from_raw_parts(prefix_ptr, prefix_len).to_vec() })
            .unwrap_or_default()
    };

    let blobs = node_arc.blobs_client();
    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        let result = if prefix.is_empty() {
            blobs.tag_list().await
        } else {
            blobs.tag_list_prefix(prefix).await
        };
        match result {
            Ok(tags) => {
                let count = tags.len() as u32;
                let payload = encode_tag_list_payload(&tags);
                let mut event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_TAG_LIST,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                event.flags = count;
                bridge2.emit_with_data(event, payload);
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Docs (simplified for Phase 1)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_docs_create(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let docs = node_arc.docs_client();

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match docs.create().await {
            Ok(doc) => {
                let handle = bridge2.docs.insert(doc);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_DOC_CREATED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    node,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_docs_create_author(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let docs = node_arc.docs_client();

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match docs.create_author().await {
            Ok(author_id) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_AUTHOR_CREATED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, author_id.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_docs_join(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    ticket: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let docs = node_arc.docs_client();
    let ticket_str = match unsafe { read_string(&ticket) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match docs.join(ticket_str).await {
            Ok(doc) => {
                let handle = bridge2.docs.insert(doc);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_DOC_JOINED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    node,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_doc_set_bytes(
    runtime: iroh_runtime_t,
    doc: u64,
    author_hex: iroh_bytes_t,
    key: iroh_bytes_t,
    value: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let key_bytes = unsafe { read_bytes(&key) };
    let value_bytes = unsafe { read_bytes(&value) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.set_bytes(author, key_bytes, value_bytes).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_DOC_SET,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, hash.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_doc_get_exact(
    runtime: iroh_runtime_t,
    doc: u64,
    author_hex: iroh_bytes_t,
    key: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let key_bytes = unsafe { read_bytes(&key) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.get_exact(author, key_bytes).await {
            Ok(Some(data)) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_DOC_GET,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Ok(None) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_DOC_GET,
                    iroh_status_t::IROH_STATUS_NOT_FOUND,
                    op_id,
                    doc,
                    0,
                    user_data,
                    iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_doc_share(
    runtime: iroh_runtime_t,
    doc: u64,
    mode: u32,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let mode_str = if mode == 0 {
        "read".to_string()
    } else {
        "write".to_string()
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.share(mode_str).await {
            Ok(ticket) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_DOC_SHARED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, ticket.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Docs: Query (key_exact / key_prefix without author)
// ============================================================================

/// Query mode for doc queries
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum iroh_doc_query_mode_t {
    IROH_DOC_QUERY_KEY_EXACT = 0,
    IROH_DOC_QUERY_KEY_PREFIX = 1,
}

#[no_mangle]
pub unsafe extern "C" fn iroh_doc_query(
    runtime: iroh_runtime_t,
    doc: u64,
    mode: u32,
    key: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let key_bytes = unsafe { read_bytes(&key) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let result = if mode == iroh_doc_query_mode_t::IROH_DOC_QUERY_KEY_PREFIX as u32 {
            doc_arc.query_key_prefix(key_bytes).await
        } else {
            doc_arc.query_key_exact(key_bytes).await
        };

        match result {
            Ok(entries) => {
                // Serialize entries into a packed binary payload
                let mut payload = Vec::new();
                let entry_count = entries.len() as u32;

                for entry in &entries {
                    // author_id
                    let author_bytes = entry.author_id.as_bytes();
                    payload.extend_from_slice(&(author_bytes.len() as u32).to_le_bytes());
                    payload.extend_from_slice(author_bytes);
                    // key
                    payload.extend_from_slice(&(entry.key.len() as u32).to_le_bytes());
                    payload.extend_from_slice(&entry.key);
                    // content_hash
                    let hash_bytes = entry.content_hash.as_bytes();
                    payload.extend_from_slice(&(hash_bytes.len() as u32).to_le_bytes());
                    payload.extend_from_slice(hash_bytes);
                    // content_len
                    payload.extend_from_slice(&entry.content_len.to_le_bytes());
                    // timestamp
                    payload.extend_from_slice(&entry.timestamp.to_le_bytes());
                }

                let mut event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_DOC_QUERY,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                event.flags = entry_count;
                bridge2.emit_with_data(event, payload);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Read the content bytes for a doc entry given its content hash.
#[no_mangle]
pub unsafe extern "C" fn iroh_doc_read_entry_content(
    runtime: iroh_runtime_t,
    doc: u64,
    content_hash_hex: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let hash_hex = match unsafe { read_string(&content_hash_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.read_entry_content(hash_hex).await {
            Ok(data) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_BLOB_READ,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Doc Sync Lifecycle (Phase 1c.5)
// ============================================================================

/// Start syncing a document with the specified peers (endpoint-ID hex strings).
/// Emits IROH_EVENT_UNIT_RESULT on completion.
#[no_mangle]
pub unsafe extern "C" fn iroh_doc_start_sync(
    runtime: iroh_runtime_t,
    doc: u64,
    peers: iroh_bytes_list_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let peer_strs = unsafe { read_string_list(&peers) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.start_sync(peer_strs).await {
            Ok(()) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_UNIT_RESULT,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Leave (stop syncing) a document.
/// Emits IROH_EVENT_UNIT_RESULT on completion.
#[no_mangle]
pub unsafe extern "C" fn iroh_doc_leave(
    runtime: iroh_runtime_t,
    doc: u64,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.leave().await {
            Ok(()) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_UNIT_RESULT,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Doc Subscribe (Phase 1c.4)
// ============================================================================

/// Encode a CoreDocEntry into the packed binary format used by DOC_QUERY payloads.
fn encode_entry_payload(entry: &aster_transport_core::CoreDocEntry) -> Vec<u8> {
    let mut payload = Vec::new();
    let author_bytes = entry.author_id.as_bytes();
    payload.extend_from_slice(&(author_bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(author_bytes);
    payload.extend_from_slice(&(entry.key.len() as u32).to_le_bytes());
    payload.extend_from_slice(&entry.key);
    let hash_bytes = entry.content_hash.as_bytes();
    payload.extend_from_slice(&(hash_bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(hash_bytes);
    payload.extend_from_slice(&entry.content_len.to_le_bytes());
    payload.extend_from_slice(&entry.timestamp.to_le_bytes());
    payload
}

/// Encode a CoreDocEvent into (subtype: u32, payload: Vec<u8>).
///
/// Subtype: 0=InsertLocal, 1=InsertRemote, 2=ContentReady, 3=PendingContentReady,
///          4=NeighborUp, 5=NeighborDown, 6=SyncFinished
fn encode_doc_event(ev: &aster_transport_core::CoreDocEvent) -> (u32, Vec<u8>) {
    use aster_transport_core::CoreDocEvent::*;
    match ev {
        InsertLocal { entry } => (0, encode_entry_payload(entry)),
        InsertRemote { from, entry } => {
            let mut payload = encode_entry_payload(entry);
            let from_bytes = from.as_bytes();
            payload.extend_from_slice(&(from_bytes.len() as u32).to_le_bytes());
            payload.extend_from_slice(from_bytes);
            (1, payload)
        }
        ContentReady { hash } => (2, hash.as_bytes().to_vec()),
        PendingContentReady => (3, Vec::new()),
        NeighborUp { peer } => (4, peer.as_bytes().to_vec()),
        NeighborDown { peer } => (5, peer.as_bytes().to_vec()),
        SyncFinished { peer } => (6, peer.as_bytes().to_vec()),
    }
}

/// Subscribe to live document events.
/// Emits IROH_EVENT_DOC_SUBSCRIBED (status=OK, handle=receiver_handle) on success.
#[no_mangle]
pub unsafe extern "C" fn iroh_doc_subscribe(
    runtime: iroh_runtime_t,
    doc: u64,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.subscribe().await {
            Ok(receiver) => {
                let handle = bridge2.doc_event_receivers.insert(receiver);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_DOC_SUBSCRIBED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    doc,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Receive the next live document event (long-poll, like gossip_recv).
/// Emits IROH_EVENT_DOC_EVENT with event.subtype indicating the event kind and
/// packed data in the payload.  When the subscription ends, emits with status=NOT_FOUND.
#[no_mangle]
pub unsafe extern "C" fn iroh_doc_event_recv(
    runtime: iroh_runtime_t,
    receiver: u64,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let receiver_arc = match bridge.doc_event_receivers.get(receiver) {
        Some(r) => r,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match receiver_arc.recv().await {
            Ok(Some(ev)) => {
                let (subtype, payload) = encode_doc_event(&ev);
                let mut event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_DOC_EVENT,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    receiver,
                    0,
                    user_data,
                    subtype as i32,
                );
                event.flags = subtype;
                bridge2.emit_with_data(event, payload);
            }
            Ok(None) => {
                // Subscription ended cleanly.
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_DOC_EVENT,
                    iroh_status_t::IROH_STATUS_NOT_FOUND,
                    op_id,
                    receiver,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Docs: Download Policy, Share with Addr, Join+Subscribe (Phase 1c.6-1c.8)
// ============================================================================

/// Download policy mode for iroh_doc_set_download_policy.
/// 0 = everything (download all entries, no prefixes needed)
/// 1 = nothing_except (only download entries matching the given prefixes)
/// 2 = everything_except (download all except entries matching the given prefixes)
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum iroh_download_policy_mode_t {
    IROH_DOWNLOAD_POLICY_EVERYTHING = 0,
    IROH_DOWNLOAD_POLICY_NOTHING_EXCEPT = 1,
    IROH_DOWNLOAD_POLICY_EVERYTHING_EXCEPT = 2,
}

/// Set the download policy for a document.
/// prefixes: list of byte-string prefixes (used for NOTHING_EXCEPT / EVERYTHING_EXCEPT modes).
/// Emits IROH_EVENT_UNIT_RESULT on success.
#[no_mangle]
pub unsafe extern "C" fn iroh_doc_set_download_policy(
    runtime: iroh_runtime_t,
    doc: u64,
    mode: u32,
    prefixes: iroh_bytes_list_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let prefix_list: Vec<Vec<u8>> = {
        let n = prefixes.len;
        if n == 0 || prefixes.items.is_null() {
            vec![]
        } else {
            let slice = unsafe { std::slice::from_raw_parts(prefixes.items, n) };
            slice.iter().map(|b| unsafe { read_bytes(b) }).collect()
        }
    };

    let policy = match mode {
        0 => aster_transport_core::CoreDownloadPolicy::Everything,
        1 => aster_transport_core::CoreDownloadPolicy::NothingExcept {
            prefixes: prefix_list,
        },
        2 => aster_transport_core::CoreDownloadPolicy::EverythingExcept {
            prefixes: prefix_list,
        },
        _ => return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.set_download_policy(policy).await {
            Ok(()) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_UNIT_RESULT,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Share a document with full relay+address info (AddrInfoOptions::RelayAndAddresses).
/// mode: 0 = read, 1 = write.
/// Emits IROH_EVENT_DOC_SHARED with the ticket string in the payload.
#[no_mangle]
pub unsafe extern "C" fn iroh_doc_share_with_addr(
    runtime: iroh_runtime_t,
    doc: u64,
    mode: u32,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let mode_str = if mode == 0 {
        "read".to_string()
    } else {
        "write".to_string()
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.share_with_addr(mode_str).await {
            Ok(ticket) => {
                let event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_DOC_SHARED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, ticket.into_bytes());
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

/// Join a document and subscribe to live events atomically.
/// Emits IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED:
///   event.handle = doc handle
///   event.flags  = event-receiver handle
#[no_mangle]
pub unsafe extern "C" fn iroh_docs_join_and_subscribe(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    ticket: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let docs = node_arc.docs_client();
    let ticket_str = match unsafe { read_string(&ticket) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match docs.join_and_subscribe(ticket_str).await {
            Ok((doc, receiver)) => {
                let doc_handle = bridge2.docs.insert(doc);
                let receiver_handle = bridge2.doc_event_receivers.insert(receiver);
                // event.handle = doc_handle, event.related = receiver_handle
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    doc_handle,
                    receiver_handle,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Gossip (simplified for Phase 1)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_gossip_subscribe(
    runtime: iroh_runtime_t,
    node: iroh_node_t,
    topic: iroh_bytes_t,
    peers: iroh_bytes_list_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let gossip = node_arc.gossip_client();
    let topic_bytes = unsafe { read_bytes(&topic) };
    let peers_strs = unsafe { read_string_list(&peers) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match gossip.subscribe(topic_bytes, peers_strs).await {
            Ok(topic_handle) => {
                let handle = bridge2.gossip_topics.insert(topic_handle);
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_GOSSIP_SUBSCRIBED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    handle,
                    node,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_gossip_broadcast(
    runtime: iroh_runtime_t,
    topic: u64,
    data: iroh_bytes_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let topic_arc = match bridge.gossip_topics.get(topic) {
        Some(t) => t,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let data_bytes = unsafe { read_bytes(&data) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match topic_arc.broadcast(data_bytes).await {
            Ok(()) => {
                bridge2.emit_simple(
                    iroh_event_kind_t::IROH_EVENT_GOSSIP_BROADCAST_DONE,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    topic,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_gossip_recv(
    runtime: iroh_runtime_t,
    topic: u64,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let topic_arc = match bridge.gossip_topics.get(topic) {
        Some(t) => t,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match topic_arc.recv().await {
            Ok(event) => {
                let event_bytes = event.data.unwrap_or_default();
                let internal_event = EventInternal::new(
                    iroh_event_kind_t::IROH_EVENT_GOSSIP_RECEIVED,
                    iroh_status_t::IROH_STATUS_OK,
                    op_id,
                    topic,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(internal_event, event_bytes);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// Phase 1b: Datagram Completion FFI Functions
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn iroh_connection_max_datagram_size(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    out_size: *mut u64,
    out_is_some: *mut u32,
) -> i32 {
    if out_size.is_null() || out_is_some.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    match conn_arc.max_datagram_size() {
        Some(size) => unsafe {
            *out_size = size as u64;
            *out_is_some = 1;
        },
        None => unsafe {
            *out_size = 0;
            *out_is_some = 0;
        },
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_connection_datagram_send_buffer_space(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    out_bytes: *mut u64,
) -> i32 {
    if out_bytes.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let space = conn_arc.datagram_send_buffer_space();
    unsafe {
        *out_bytes = space as u64;
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// Phase 1b: Remote-Info FFI Functions
// ============================================================================

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_remote_info_t {
    pub struct_size: u32,
    pub node_id: iroh_bytes_t,
    pub is_connected: u32,
    pub connection_type: u32, // 0=none, 1=connecting, 2=udp_direct, 3=udp_relay
    pub relay_url: iroh_bytes_t,
    pub last_handshake_ns: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iroh_connection_info_t {
    pub struct_size: u32,
    pub connection_type: u32, // 2=udp_direct, 3=udp_relay, etc.
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub rtt_ns: u64,
    pub alpn: iroh_bytes_t,
    pub is_connected: u32,
}

#[no_mangle]
pub unsafe extern "C" fn iroh_endpoint_remote_info(
    runtime: iroh_runtime_t,
    endpoint_or_node: u64,
    node_id: iroh_bytes_t,
    out_info: *mut iroh_remote_info_t,
) -> i32 {
    if out_info.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = bridge.endpoints.get(endpoint_or_node).or_else(|| {
        bridge
            .nodes
            .get(endpoint_or_node)
            .map(|n| Arc::new(n.net_client()))
    });

    let ep = match ep_arc {
        Some(e) => e,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let node_id_str = match unsafe { read_string(&node_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let info = match ep.remote_info(&node_id_str) {
        Some(info) => info,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    // Map connection type
    let conn_type = match info.connection_type {
        aster_transport_core::ConnectionType::NotConnected => 0,
        aster_transport_core::ConnectionType::Connecting => 1,
        aster_transport_core::ConnectionType::Connected(detail) => match detail {
            aster_transport_core::ConnectionTypeDetail::UdpDirect => 2,
            aster_transport_core::ConnectionTypeDetail::UdpRelay => 3,
            aster_transport_core::ConnectionTypeDetail::Other(_) => 4,
        },
    };

    // Get relay_url offset (we'll use placeholder since this is a single call)
    let relay_url_str = info.relay_url.unwrap_or_default();
    let relay_url_bytes = alloc_string(relay_url_str);

    // Pack node_id into buffer (simplified - caller should use separate buffer)
    let node_id_bytes = alloc_string(info.node_id);

    unsafe {
        *out_info = iroh_remote_info_t {
            struct_size: std::mem::size_of::<iroh_remote_info_t>() as u32,
            node_id: node_id_bytes,
            is_connected: if info.is_connected { 1 } else { 0 },
            connection_type: conn_type,
            relay_url: relay_url_bytes,
            last_handshake_ns: info.last_handshake_ns.unwrap_or(0),
            bytes_sent: info.bytes_sent,
            bytes_received: info.bytes_received,
        };
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_endpoint_remote_info_list(
    runtime: iroh_runtime_t,
    endpoint_or_node: u64,
    out_infos: *mut iroh_remote_info_t,
    max_infos: usize,
    out_count: *mut usize,
) -> i32 {
    if out_infos.is_null() || out_count.is_null() || max_infos == 0 {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = bridge.endpoints.get(endpoint_or_node).or_else(|| {
        bridge
            .nodes
            .get(endpoint_or_node)
            .map(|n| Arc::new(n.net_client()))
    });

    let ep = match ep_arc {
        Some(e) => e,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let infos = ep.remote_info_iter();
    let count = infos.len().min(max_infos);

    // Note: In a full implementation, we'd need to handle buffer allocation properly
    // For now, we return the count and let the caller allocate buffers
    unsafe {
        *out_count = count;
    }

    // Pack first 'count' items
    for (i, info) in infos.iter().take(count).enumerate() {
        let conn_type = match &info.connection_type {
            aster_transport_core::ConnectionType::NotConnected => 0,
            aster_transport_core::ConnectionType::Connecting => 1,
            aster_transport_core::ConnectionType::Connected(detail) => match detail {
                aster_transport_core::ConnectionTypeDetail::UdpDirect => 2,
                aster_transport_core::ConnectionTypeDetail::UdpRelay => 3,
                aster_transport_core::ConnectionTypeDetail::Other(_) => 4,
            },
        };

        let relay_url_str = info.relay_url.clone().unwrap_or_default();
        let node_id_str = info.node_id.clone();

        unsafe {
            *out_infos.add(i) = iroh_remote_info_t {
                struct_size: std::mem::size_of::<iroh_remote_info_t>() as u32,
                node_id: alloc_string(node_id_str),
                is_connected: if info.is_connected { 1 } else { 0 },
                connection_type: conn_type,
                relay_url: alloc_string(relay_url_str),
                last_handshake_ns: info.last_handshake_ns.unwrap_or(0),
                bytes_sent: info.bytes_sent,
                bytes_received: info.bytes_received,
            };
        }
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn iroh_connection_info(
    runtime: iroh_runtime_t,
    connection: iroh_connection_t,
    out_info: *mut iroh_connection_info_t,
) -> i32 {
    if out_info.is_null() {
        return iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let info = conn_arc.connection_info();

    let conn_type = match info.connection_type {
        aster_transport_core::ConnectionTypeDetail::UdpDirect => 2,
        aster_transport_core::ConnectionTypeDetail::UdpRelay => 3,
        aster_transport_core::ConnectionTypeDetail::Other(_) => 4,
    };

    let alpn_bytes = alloc_bytes(info.alpn);

    unsafe {
        *out_info = iroh_connection_info_t {
            struct_size: std::mem::size_of::<iroh_connection_info_t>() as u32,
            connection_type: conn_type,
            bytes_sent: info.bytes_sent,
            bytes_received: info.bytes_received,
            rtt_ns: info.rtt_ns.unwrap_or(0),
            alpn: alpn_bytes,
            is_connected: if info.is_connected { 1 } else { 0 },
        };
    }

    iroh_status_t::IROH_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Hook Replies (Phase 1b)
// ============================================================================

/// Respond to a pending before_connect hook invocation.
///
/// `decision` is IROH_HOOK_DECISION_ALLOW (0) or IROH_HOOK_DECISION_DENY (1).
/// Calling this consumes the invocation; a second call returns NOT_FOUND.
#[no_mangle]
pub unsafe extern "C" fn iroh_hook_before_connect_respond(
    runtime: iroh_runtime_t,
    invocation: iroh_hook_invocation_t,
    decision: iroh_hook_decision_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let state = match bridge.hook_invocations.remove(invocation) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let sender = match state.sender.lock().ok().and_then(|mut g| g.take()) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    match sender {
        HookSender::BeforeConnect(tx) => {
            let allow = matches!(decision, iroh_hook_decision_t::IROH_HOOK_DECISION_ALLOW);
            // Ignore send error — the hook may have timed out already.
            let _ = tx.send(allow);
            iroh_status_t::IROH_STATUS_OK as i32
        }
        HookSender::AfterConnect(_) => {
            // Wrong invocation type.
            iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32
        }
    }
}

/// Respond to a pending after_connect hook invocation (always accepts).
///
/// Calling this consumes the invocation; a second call returns NOT_FOUND.
#[no_mangle]
pub unsafe extern "C" fn iroh_hook_after_connect_respond(
    runtime: iroh_runtime_t,
    invocation: iroh_hook_invocation_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let state = match bridge.hook_invocations.remove(invocation) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    let sender = match state.sender.lock().ok().and_then(|mut g| g.take()) {
        Some(s) => s,
        None => return iroh_status_t::IROH_STATUS_NOT_FOUND as i32,
    };

    match sender {
        HookSender::AfterConnect(tx) => {
            let _ = tx.send(CoreAfterHandshakeDecision::Accept);
            iroh_status_t::IROH_STATUS_OK as i32
        }
        HookSender::BeforeConnect(_) => {
            // Wrong invocation type.
            iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32
        }
    }
}
