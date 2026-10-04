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

pub mod call;
pub mod data;
pub mod envelope;
mod layout;
pub mod node;
pub mod probe;
pub mod reactor;

// ============================================================================
// ABI Version
// ============================================================================

// History:
// - 2.0.0: `aster_reactor_submit`, `..._submit_frame` and `..._submit_trailer`
//   can return `ASTER_STATUS_QUEUE_FULL` (behavioural break, see the retry
//   contract in `aster_ffi.h` and `docs/_internal/reactor-ffi-guide.md`).
// - 3.0.0: whole C ABI renamed to the single `aster_` / `ASTER_` prefix;
//   `aster_abi_version()` replaces the three per-component getters;
//   `aster_runtime_new` takes `expected_abi_major` and refuses a mismatch
//   with `ASTER_STATUS_ABI_MISMATCH`; reactor event-kind constants renamed
//   `ASTER_REACTOR_EVENT_*`; every enum is exported to the header.
// - 3.1.0 (additive): AEv1 RPC envelope codec (`aster_{stream_header,
//   call_header,rpc_status}_{encode,decode,release}` + their structs) and
//   zstd payload compression (`aster_zstd_maybe_compress`,
//   `aster_zstd_decompress`); see `envelope.rs`.
// - 3.2.0 (additive): nodes from `aster_config_t` on `aster::Node`, Gate 0,
//   admission respond, Prometheus metrics; see `node.rs`.
// - 3.3.0 (additive): data-plane parity -- multi-provider blob fetch with a
//   fetch report, ranged reads, tag prefix delete, one-shot GC; docs
//   authority/retire/open/import/durable/entry_exists/content status;
//   connection paths and send-stream priority; see `data.rs`.
pub const ASTER_ABI_VERSION_MAJOR: u32 = 3;
pub const ASTER_ABI_VERSION_MINOR: u32 = 3;
pub const ASTER_ABI_VERSION_PATCH: u32 = 0;
/// Packed ABI version: `(major << 16) | (minor << 8) | patch`.
/// This is the value `aster_abi_version()` returns.
pub const ASTER_ABI_VERSION: u32 =
    (ASTER_ABI_VERSION_MAJOR << 16) | (ASTER_ABI_VERSION_MINOR << 8) | ASTER_ABI_VERSION_PATCH;

// ============================================================================
// ABI Types (FFI-safe C-compatible structs)
// ============================================================================

/// Opaque handle types
pub type aster_runtime_t = u64;
pub type aster_endpoint_t = u64;
pub type aster_connection_t = u64;
pub type aster_send_stream_t = u64;
pub type aster_recv_stream_t = u64;
pub type aster_node_t = u64;
pub type aster_operation_t = u64;
pub type aster_buffer_t = u64;
pub type aster_hook_invocation_t = u64; // Phase 1b: identifies a pending hook invocation

/// Status codes
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_status_t {
    ASTER_STATUS_OK = 0,
    ASTER_STATUS_INVALID_ARGUMENT = 1,
    ASTER_STATUS_NOT_FOUND = 2,
    ASTER_STATUS_ALREADY_CLOSED = 3,
    ASTER_STATUS_QUEUE_FULL = 4,
    ASTER_STATUS_BUFFER_TOO_SMALL = 5,
    ASTER_STATUS_UNSUPPORTED = 6,
    ASTER_STATUS_INTERNAL = 7,
    ASTER_STATUS_TIMEOUT = 8,
    ASTER_STATUS_CANCELLED = 9,
    ASTER_STATUS_CONNECTION_REFUSED = 10,
    ASTER_STATUS_STREAM_RESET = 11,
    /// `aster_runtime_new` was called with an `expected_abi_major` that does
    /// not match the loaded library's `ASTER_ABI_VERSION_MAJOR`.
    ASTER_STATUS_ABI_MISMATCH = 12,
}

/// Relay mode
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_relay_mode_t {
    ASTER_RELAY_MODE_DEFAULT = 0,
    ASTER_RELAY_MODE_CUSTOM = 1,
    ASTER_RELAY_MODE_DISABLED = 2,
    ASTER_RELAY_MODE_STAGING = 3,
}

/// Hook fallback behavior when callback processing is unavailable.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_hook_failure_mode_t {
    ASTER_HOOK_FAILURE_FAIL_OPEN = 0,
    ASTER_HOOK_FAILURE_FAIL_CLOSED = 1,
}

/// Event kinds
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_event_kind_t {
    ASTER_EVENT_NONE = 0,

    // Lifecycle
    ASTER_EVENT_NODE_CREATED = 1,
    ASTER_EVENT_NODE_CREATE_FAILED = 2,
    ASTER_EVENT_ENDPOINT_CREATED = 3,
    ASTER_EVENT_ENDPOINT_CREATE_FAILED = 4,
    ASTER_EVENT_CLOSED = 5,

    // Connections
    ASTER_EVENT_CONNECTED = 10,
    ASTER_EVENT_CONNECT_FAILED = 11,
    ASTER_EVENT_CONNECTION_ACCEPTED = 12,
    ASTER_EVENT_CONNECTION_CLOSED = 13,

    // Streams
    ASTER_EVENT_STREAM_OPENED = 20,
    ASTER_EVENT_STREAM_ACCEPTED = 21,
    ASTER_EVENT_FRAME_RECEIVED = 22,
    ASTER_EVENT_SEND_COMPLETED = 23,
    ASTER_EVENT_STREAM_FINISHED = 24,
    ASTER_EVENT_STREAM_RESET = 25,

    // Blobs
    ASTER_EVENT_BLOB_ADDED = 30,
    ASTER_EVENT_BLOB_READ = 31,
    ASTER_EVENT_BLOB_DOWNLOADED = 32,
    ASTER_EVENT_BLOB_TICKET_CREATED = 33,
    ASTER_EVENT_BLOB_COLLECTION_ADDED = 34,
    ASTER_EVENT_BLOB_COLLECTION_TICKET_CREATED = 35,

    /// Emitted by aster_blobs_observe_complete when the blob is fully available locally.
    ASTER_EVENT_BLOB_OBSERVE_COMPLETE = 56,

    // Tags (Phase 1c)
    ASTER_EVENT_TAG_SET = 36,
    ASTER_EVENT_TAG_GET = 37,
    ASTER_EVENT_TAG_DELETED = 38,
    ASTER_EVENT_TAG_LIST = 39,

    // Docs
    ASTER_EVENT_DOC_CREATED = 40,
    ASTER_EVENT_DOC_JOINED = 41,
    ASTER_EVENT_DOC_SET = 42,
    ASTER_EVENT_DOC_GET = 43,
    ASTER_EVENT_DOC_SHARED = 44,
    ASTER_EVENT_DOC_QUERY = 46,
    ASTER_EVENT_AUTHOR_CREATED = 45,
    ASTER_EVENT_DOC_SUBSCRIBED = 47,
    ASTER_EVENT_DOC_EVENT = 48,
    /// join_and_subscribe: event.handle = doc_handle, event.related = receiver_handle
    ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED = 49,
    /// event.payload = removed count as little-endian u64.
    ASTER_EVENT_DOC_DEL = 57,

    // Gossip
    ASTER_EVENT_GOSSIP_SUBSCRIBED = 50,
    ASTER_EVENT_GOSSIP_BROADCAST_DONE = 51,
    ASTER_EVENT_GOSSIP_RECEIVED = 52,
    ASTER_EVENT_GOSSIP_NEIGHBOR_UP = 53,
    ASTER_EVENT_GOSSIP_NEIGHBOR_DOWN = 54,
    ASTER_EVENT_GOSSIP_LAGGED = 55,

    // Datagrams (Phase 1b)
    ASTER_EVENT_DATAGRAM_RECEIVED = 60,

    // Hooks (Phase 1b)
    ASTER_EVENT_HOOK_BEFORE_CONNECT = 70,
    ASTER_EVENT_HOOK_AFTER_CONNECT = 71,
    ASTER_EVENT_HOOK_INVOCATION_RELEASED = 72,

    // Aster custom-ALPN (Phase 1e)
    ASTER_EVENT_ASTER_ACCEPTED = 65,

    // Registry (§11.9 — async doc-backed ops)
    ASTER_EVENT_REGISTRY_RESOLVED = 80,
    ASTER_EVENT_REGISTRY_PUBLISHED = 81,
    ASTER_EVENT_REGISTRY_RENEWED = 82,
    ASTER_EVENT_REGISTRY_ACL_UPDATED = 83,
    ASTER_EVENT_REGISTRY_ACL_LISTED = 84,

    // Generic
    ASTER_EVENT_STRING_RESULT = 90,
    ASTER_EVENT_BYTES_RESULT = 91,
    ASTER_EVENT_UNIT_RESULT = 92,
    ASTER_EVENT_OPERATION_CANCELLED = 98,
    ASTER_EVENT_ERROR = 99,

    // Data plane (ABI 3.3, data.rs)
    /// `aster_blobs_download_hash_from`: payload = fetch report v1, then the
    /// blob bytes unless `ASTER_FETCH_TO_STORE`; `flags` = provider count.
    ASTER_EVENT_BLOB_FETCHED = 100,
    /// `flags` = `aster_local_access_t`; payload = namespace id (32) +
    /// authority fingerprint (32).
    ASTER_EVENT_DOC_AUTHORITY_REPORT = 101,
    /// `flags` = `aster_retire_outcome_t`; payload = u32 access, u32 reserved,
    /// u64 open references.
    ASTER_EVENT_DOC_RETIRE_OUTCOME = 102,
    /// `handle` = doc, `related` = node; status NOT_FOUND (handle 0) when
    /// `aster_docs_open` finds no such namespace.
    ASTER_EVENT_DOC_OPENED = 103,
    /// `flags` = 1 if the entry (or its tombstone) exists, else 0.
    ASTER_EVENT_DOC_ENTRY_EXISTS = 104,
    /// `flags` = `aster_blob_status_t`; payload = size (u64 LE).
    ASTER_EVENT_DOC_CONTENT_STATUS = 105,
    /// payload = priority (i32 LE); `flags` = its bit pattern.
    ASTER_EVENT_STREAM_PRIORITY = 106,
}

/// Value of `connection_type` in `aster_remote_info_t` and
/// `aster_connection_info_t` (carried as `uint32_t`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_connection_type_t {
    ASTER_CONNECTION_TYPE_NOT_CONNECTED = 0,
    ASTER_CONNECTION_TYPE_CONNECTING = 1,
    ASTER_CONNECTION_TYPE_UDP_DIRECT = 2,
    ASTER_CONNECTION_TYPE_UDP_RELAY = 3,
    ASTER_CONNECTION_TYPE_OTHER = 4,
}

/// Value of `path_kind` in `aster_transport_snapshot_t` (carried as `uint32_t`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_path_kind_t {
    /// No path selected (connection dropped or not yet negotiated).
    ASTER_PATH_KIND_NONE = 0,
    /// Direct UDP: `peer_host` and `peer_port` are populated.
    ASTER_PATH_KIND_DIRECT = 1,
    /// Relay: `relay_url` is populated.
    ASTER_PATH_KIND_RELAY = 2,
    /// A custom or future transport (ABI 3.3, `aster_path_info_t.other`).
    ASTER_PATH_KIND_OTHER = 3,
}

/// `mode` argument of `aster_doc_share` / `aster_doc_share_with_addr`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_doc_share_mode_t {
    ASTER_DOC_SHARE_READ = 0,
    ASTER_DOC_SHARE_WRITE = 1,
}

/// `kind` argument of `aster_registry_key`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_registry_key_kind_t {
    /// `contract_key(arg1)`
    ASTER_REGISTRY_KEY_CONTRACT = 0,
    /// `version_key(arg1, arg2 as int)`
    ASTER_REGISTRY_KEY_VERSION = 1,
    /// `channel_key(arg1, arg2)`
    ASTER_REGISTRY_KEY_CHANNEL = 2,
    /// `lease_key(arg1, arg2, arg3)`
    ASTER_REGISTRY_KEY_LEASE = 3,
    /// `lease_prefix(arg1, arg2)`
    ASTER_REGISTRY_KEY_LEASE_PREFIX = 4,
    /// `acl_key(arg1)`
    ASTER_REGISTRY_KEY_ACL = 5,
}

/// Hook decision
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_hook_decision_t {
    ASTER_HOOK_DECISION_ALLOW = 0,
    ASTER_HOOK_DECISION_DENY = 1,
}

// ============================================================================
// C Structs (#[repr(C)])
// ============================================================================

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_runtime_config_t {
    pub struct_size: u32,
    pub worker_threads: u32,
    pub event_queue_capacity: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_endpoint_config_t {
    pub struct_size: u32,
    /// One of `aster_relay_mode_t`. Unknown values are rejected.
    pub relay_mode: u32,
    pub secret_key: aster_bytes_t,
    pub alpns: aster_bytes_list_t,
    pub relay_urls: aster_bytes_list_t,
    pub enable_discovery: u32,
    pub enable_hooks: u32,    // Phase 1b: 0=disabled, 1=enabled
    pub hook_timeout_ms: u64, // Phase 1b: 0 = use default (5000ms)
    // Phase 1d: endpoint builder gaps
    pub bind_addr: aster_bytes_t, // socket addr string; empty = use default
    pub clear_ip_transports: u32, // 1 = relay-only mode
    pub clear_relay_transports: u32, // 1 = direct-IP-only mode
    pub portmapper_config: u32,   // 0 = enabled (default), 1 = disabled
    pub proxy_url: aster_bytes_t, // HTTP/SOCKS proxy URL string; empty = none
    pub proxy_from_env: u32,      // 1 = read HTTP_PROXY/HTTPS_PROXY from env
    pub data_dir_utf8: aster_bytes_t, // Node data directory; empty = no persistent state
    /// One of `aster_hook_failure_mode_t` (default 0 = fail-open).
    pub hook_failure_mode: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_connect_config_t {
    pub struct_size: u32,
    pub flags: u32,
    pub node_id: aster_bytes_t, // hex string
    pub alpn: aster_bytes_t,
    pub addr: *const aster_node_addr_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_node_addr_t {
    pub endpoint_id: aster_bytes_t,
    pub relay_url: aster_bytes_t,
    pub direct_addresses: aster_bytes_list_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_bytes_t {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_bytes_list_t {
    pub items: *const aster_bytes_t,
    pub len: usize,
}

/// C-compatible event structure for FFI output
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_event_t {
    /// `sizeof(aster_event_t)`; written by the library.
    pub struct_size: u32,
    /// One of `aster_event_kind_t`.
    pub kind: u32,
    /// One of `aster_status_t`.
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
    aster_status_t::ASTER_STATUS_INTERNAL as i32
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

pub(crate) struct HandleRegistry<T> {
    items: RwLock<HashMap<u64, Arc<T>>>,
}

/// One id space for every handle registry, so a node handle can never equal an
/// endpoint (or any other) handle. Functions that accept "endpoint or node"
/// (`aster_connect`, `aster_accept`, remote info, transport metrics) rely on it.
static NEXT_HANDLE_ID: AtomicU64 = AtomicU64::new(1);

impl<T> HandleRegistry<T> {
    pub(crate) fn new() -> Self {
        Self {
            items: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn insert(&self, value: T) -> u64 {
        let id = NEXT_HANDLE_ID.fetch_add(1, Ordering::Relaxed);
        let arc = Arc::new(value);
        self.items.write().unwrap().insert(id, arc);
        id
    }

    pub(crate) fn get(&self, id: u64) -> Option<Arc<T>> {
        self.items.read().unwrap().get(&id).cloned()
    }

    pub(crate) fn remove(&self, id: u64) -> Option<Arc<T>> {
        self.items.write().unwrap().remove(&id)
    }

    #[allow(dead_code)]
    pub(crate) fn count(&self) -> usize {
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
            struct_size: std::mem::size_of::<aster_event_t>() as u32,
            kind: aster_event_kind_t::ASTER_EVENT_NONE as u32,
            status: aster_status_t::ASTER_STATUS_OK as u32,
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
        kind: aster_event_kind_t,
        status: aster_status_t,
        operation: u64,
        handle: u64,
        related: u64,
        user_data: u64,
        error_code: i32,
    ) -> Self {
        Self {
            struct_size: std::mem::size_of::<aster_event_t>() as u32,
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

pub(crate) struct BufferRegistry {
    next_id: AtomicU64,
    buffers: RwLock<HashMap<u64, Arc<[u8]>>>,
}

impl BufferRegistry {
    pub(crate) fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            buffers: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn insert(&self, data: Vec<u8>) -> (u64, Arc<[u8]>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let arc: Arc<[u8]> = data.into();
        self.buffers.write().unwrap().insert(id, arc.clone());
        (id, arc)
    }

    #[allow(dead_code)]
    pub(crate) fn get(&self, id: u64) -> Option<Arc<[u8]>> {
        self.buffers.read().unwrap().get(&id).cloned()
    }

    pub(crate) fn release(&self, id: u64) -> bool {
        self.buffers.write().unwrap().remove(&id).is_some()
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

pub(crate) struct BridgeRuntime {
    // Dropped by hand in `Drop for BridgeRuntime`: spawned tasks hold
    // `Arc<BridgeRuntime>` clones, so the last reference can go away on one of
    // this runtime's own worker threads, where dropping a Runtime panics.
    pub(crate) runtime: std::mem::ManuallyDrop<tokio::runtime::Runtime>,
    events_tx: tokio::sync::mpsc::UnboundedSender<EventOwned>,
    events_rx: Mutex<tokio::sync::mpsc::UnboundedReceiver<EventOwned>>,

    // Handle registries
    pub(crate) nodes: HandleRegistry<CoreNode>,
    endpoints: HandleRegistry<CoreNetClient>,
    pub(crate) connections: HandleRegistry<CoreConnection>,
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

    // ABI 3.2: Gate-0 policies and the gate slot of each hooked node.
    gates: HandleRegistry<node::GateEntry>,
    node_gates: node::NodeGates,

    // Doc event receiver registry (Phase 1c.4)
    doc_event_receivers: HandleRegistry<aster_transport_core::CoreDocEventReceiver>,

    // Buffer registry
    buffers: BufferRegistry,

    // Endpoint secret keys: keyed by endpoint handle, stores the 32-byte secret key seed
    endpoint_secret_keys: Mutex<HashMap<aster_endpoint_t, Vec<u8>>>,

    // Registry resolution state (§11.9): persistent round-robin counters + monotonic
    // lease_seq cache, shared across every aster_registry_resolve call so rotation
    // and stale rejection survive call boundaries.
    registry_state: aster_transport_core::registry::ResolveState,

    // Per-doc RegistryAcl, keyed by doc handle. Lazily created in open mode on first
    // touch via `registry_acl_for_doc`.
    registry_acls: Mutex<HashMap<u64, Arc<aster_transport_core::registry::RegistryAcl>>>,
}

impl Drop for BridgeRuntime {
    fn drop(&mut self) {
        // SAFETY: `runtime` is never touched again after this.
        let runtime = unsafe { std::mem::ManuallyDrop::take(&mut self.runtime) };
        if tokio::runtime::Handle::try_current().is_ok() {
            // Inside an async context -- possibly a worker of this very
            // runtime, which a blocking drop would wait on forever. Hand the
            // drop to a detached OS thread; it completes once the current
            // task returns to the scheduler.
            std::thread::spawn(move || drop(runtime));
        } else {
            drop(runtime);
        }
    }
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
            runtime: std::mem::ManuallyDrop::new(runtime),
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
            gates: HandleRegistry::new(),
            node_gates: Mutex::new(HashMap::new()),
            doc_event_receivers: HandleRegistry::new(),
            buffers: BufferRegistry::new(),
            endpoint_secret_keys: Mutex::new(HashMap::new()),
            registry_state: aster_transport_core::registry::ResolveState::new(),
            registry_acls: Mutex::new(HashMap::new()),
        })
    }

    fn emit(&self, event: EventOwned) {
        let _ = self.events_tx.send(event);
    }

    fn emit_error(&self, operation: u64, user_data: u64, message: &str) {
        let data = message.as_bytes().to_vec();
        let (buf_id, _) = self.buffers.insert(data);
        let event = EventInternal::new(
            aster_event_kind_t::ASTER_EVENT_ERROR,
            aster_status_t::ASTER_STATUS_INTERNAL,
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
        kind: aster_event_kind_t,
        status: aster_status_t,
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

    /// Return the per-doc RegistryAcl, lazily creating one in open mode on first
    /// touch. Returns the same Arc on subsequent calls so add/remove writer state
    /// persists for the life of the bridge.
    fn registry_acl_for_doc(&self, doc: u64) -> Arc<aster_transport_core::registry::RegistryAcl> {
        let mut guard = self.registry_acls.lock().unwrap();
        guard
            .entry(doc)
            .or_insert_with(|| Arc::new(aster_transport_core::registry::RegistryAcl::new()))
            .clone()
    }

    /// Return the per-doc RegistryAcl if one has been created, without creating one.
    fn registry_acl_lookup(
        &self,
        doc: u64,
    ) -> Option<Arc<aster_transport_core::registry::RegistryAcl>> {
        self.registry_acls.lock().unwrap().get(&doc).cloned()
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

static RUNTIMES: std::sync::OnceLock<Mutex<HashMap<aster_runtime_t, Arc<BridgeRuntime>>>> =
    std::sync::OnceLock::new();

fn runtimes() -> &'static Mutex<HashMap<aster_runtime_t, Arc<BridgeRuntime>>> {
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn load_runtime(handle: aster_runtime_t) -> Result<Arc<BridgeRuntime>, aster_status_t> {
    let guard = runtimes()
        .lock()
        .map_err(|_| aster_status_t::ASTER_STATUS_INTERNAL)?;
    guard
        .get(&handle)
        .cloned()
        .ok_or(aster_status_t::ASTER_STATUS_NOT_FOUND)
}

// ============================================================================
// Utility Functions
// ============================================================================

fn new_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

unsafe fn read_bytes(b: &aster_bytes_t) -> Vec<u8> {
    if b.ptr.is_null() || b.len == 0 {
        Vec::new()
    } else {
        slice::from_raw_parts(b.ptr, b.len).to_vec()
    }
}

unsafe fn read_bytes_opt(b: &aster_bytes_t) -> Option<Vec<u8>> {
    if b.ptr.is_null() || b.len == 0 {
        None
    } else {
        Some(slice::from_raw_parts(b.ptr, b.len).to_vec())
    }
}

unsafe fn read_string_opt(b: &aster_bytes_t) -> Option<String> {
    read_bytes_opt(b).and_then(|v| String::from_utf8(v).ok())
}

unsafe fn read_string(b: &aster_bytes_t) -> Result<String, i32> {
    let bytes = read_bytes(b);
    String::from_utf8(bytes).map_err(set_last_error)
}

unsafe fn read_string_list(list: &aster_bytes_list_t) -> Vec<String> {
    if list.items.is_null() || list.len == 0 {
        Vec::new()
    } else {
        let items = slice::from_raw_parts(list.items, list.len);
        items.iter().filter_map(|b| read_string(b).ok()).collect()
    }
}

/// Hand a Rust-owned byte buffer to the caller as an `aster_bytes_t` that the
/// caller frees with `aster_string_release(ptr, len)`.
///
/// Allocation contract (must stay in lock-step with `aster_string_release`):
/// - empty input -> `{ ptr: NULL, len: 0 }`, nothing is allocated;
/// - otherwise exactly `len + 1` bytes are allocated as a `Box<[u8]>` whose
///   last byte is a NUL terminator (so C callers may treat text as a C
///   string), and `len` excludes that terminator.
///
/// `into_boxed_slice` shrinks the allocation to exactly `len + 1`, so the
/// release side can rebuild the identical `Box<[u8]>` from `(ptr, len + 1)`.
fn alloc_owned_bytes(mut bytes: Vec<u8>) -> aster_bytes_t {
    if bytes.is_empty() {
        return aster_bytes_t {
            ptr: std::ptr::null(),
            len: 0,
        };
    }
    let len = bytes.len();
    bytes.reserve_exact(1);
    bytes.push(0);
    let boxed: Box<[u8]> = bytes.into_boxed_slice();
    debug_assert_eq!(boxed.len(), len + 1);
    let ptr = Box::into_raw(boxed) as *mut u8 as *const u8;
    aster_bytes_t { ptr, len }
}

fn alloc_string(s: String) -> aster_bytes_t {
    alloc_owned_bytes(s.into_bytes())
}

fn alloc_bytes(bytes: Vec<u8>) -> aster_bytes_t {
    alloc_owned_bytes(bytes)
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
            aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED,
            aster_status_t::ASTER_STATUS_CANCELLED,
            op_id,
            0,
            0,
            user_data,
            aster_status_t::ASTER_STATUS_CANCELLED as i32,
        );
        true
    } else {
        false
    }
}

// ============================================================================
// C FFI Functions - Versioning
// ============================================================================

/// Packed ABI version of the loaded library:
/// `(major << 16) | (minor << 8) | patch` (see `ASTER_ABI_VERSION`).
/// Unpack with `v >> 16`, `(v >> 8) & 0xff`, `v & 0xff`. A binding must
/// refuse to run when the major differs from the one it was written
/// against; `aster_runtime_new` enforces this as well.
#[no_mangle]
pub extern "C" fn aster_abi_version() -> u32 {
    ASTER_ABI_VERSION
}

/// Exact Aster build version as a process-lifetime, NUL-terminated UTF-8 string.
/// The returned pointer is borrowed and must not be freed.
#[no_mangle]
pub extern "C" fn aster_version() -> *const c_char {
    static VERSION: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            CString::new(aster_transport_core::VERSION)
                .expect("ASTER_BUILD_VERSION cannot contain a NUL byte")
        })
        .as_ptr()
}

// ============================================================================
// C FFI Functions - Runtime
// ============================================================================

/// Create a runtime.
///
/// `expected_abi_major` is the `ASTER_ABI_VERSION_MAJOR` the caller was
/// written against. A mismatch returns `ASTER_STATUS_ABI_MISMATCH` (and sets
/// the last-error message) without creating anything.
///
/// `config` may be NULL for defaults. When non-NULL, `config->struct_size`
/// must equal `sizeof(aster_runtime_config_t)`, otherwise
/// `ASTER_STATUS_INVALID_ARGUMENT` is returned.
#[no_mangle]
pub unsafe extern "C" fn aster_runtime_new(
    expected_abi_major: u32,
    config: *const aster_runtime_config_t,
    out_runtime: *mut aster_runtime_t,
) -> i32 {
    if expected_abi_major != ASTER_ABI_VERSION_MAJOR {
        set_last_error(format!(
            "ABI major mismatch: caller expects {expected_abi_major}, library provides {ASTER_ABI_VERSION_MAJOR}.{ASTER_ABI_VERSION_MINOR}.{ASTER_ABI_VERSION_PATCH}"
        ));
        return aster_status_t::ASTER_STATUS_ABI_MISMATCH as i32;
    }
    if out_runtime.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    if let Some(c) = config.as_ref() {
        let expected = std::mem::size_of::<aster_runtime_config_t>() as u32;
        if c.struct_size != expected {
            set_last_error(format!(
                "aster_runtime_config_t.struct_size is {}, expected {expected}",
                c.struct_size
            ));
            return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
        }
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
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_INTERNAL as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_runtime_close(runtime: aster_runtime_t) -> i32 {
    let arc = match runtimes().lock() {
        Ok(mut guard) => match guard.remove(&runtime) {
            Some(arc) => arc,
            None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
        },
        Err(_) => return aster_status_t::ASTER_STATUS_INTERNAL as i32,
    };

    // In-flight tasks may still hold clones; whichever reference is last
    // shuts the runtime down safely (see `Drop for BridgeRuntime`).
    drop(arc);

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Secret Key
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_secret_key_generate(
    out_key_ptr: *mut u8,
    out_key_capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let mut key = [0u8; 32];
    if getrandom::getrandom(&mut key).is_err() {
        return aster_status_t::ASTER_STATUS_INTERNAL as i32;
    }
    let len = key.len();

    *out_len = len;

    if out_key_ptr.is_null() || out_key_capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    unsafe {
        ptr::copy_nonoverlapping(key.as_ptr(), out_key_ptr, len);
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Event Polling
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_poll_events(
    runtime: aster_runtime_t,
    out_events: *mut aster_event_t,
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

/// Convert internal EventInternal to C-compatible aster_event_t
fn to_iroh_event(event: &EventInternal, buffers: &BufferRegistry) -> aster_event_t {
    let (data_ptr, data_len) = if event.buffer_id != 0 {
        if let Some(buf) = buffers.get(event.buffer_id) {
            (buf.as_ptr(), buf.len())
        } else {
            (ptr::null(), 0)
        }
    } else {
        (ptr::null(), 0)
    };

    aster_event_t {
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
pub unsafe extern "C" fn aster_buffer_release(runtime: aster_runtime_t, buffer: u64) -> i32 {
    if buffer == 0 {
        return aster_status_t::ASTER_STATUS_OK as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(_) => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    if bridge.buffers.release(buffer) {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

/// Release a Rust-owned buffer returned in an `aster_bytes_t` field of an
/// out-struct: `aster_remote_info_t.node_id` / `.relay_url` (from
/// `aster_endpoint_remote_info` and each entry of
/// `aster_endpoint_remote_info_list`), `aster_connection_info_t.alpn`, and
/// `aster_transport_snapshot_t.peer_host` / `.relay_url`, and the `out`
/// buffer of `aster_{stream_header,call_header,rpc_status}_encode`,
/// `aster_zstd_maybe_compress` and `aster_zstd_decompress`.
///
/// Pass back exactly the `ptr` and `len` the library returned. Non-empty
/// buffers are additionally NUL-terminated (the terminator is not counted in
/// `len`). Empty buffers are returned as `{NULL, 0}`; releasing a NULL `ptr`
/// or a `len` of 0 is a no-op that returns `ASTER_STATUS_OK`.
///
/// # Safety
/// - A non-NULL `ptr` with `len > 0` must be a buffer returned by this
///   library through one of the fields above, with its original `len`.
/// - Must be called at most once for each buffer.
#[no_mangle]
pub unsafe extern "C" fn aster_string_release(ptr: *const u8, len: usize) -> i32 {
    if ptr.is_null() || len == 0 {
        return aster_status_t::ASTER_STATUS_OK as i32;
    }
    let Some(alloc_len) = len.checked_add(1) else {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    };
    // SAFETY: the caller guarantees (ptr, len) came from `alloc_owned_bytes`,
    // which allocated exactly `len + 1` bytes as a `Box<[u8]>`. Rebuild that
    // identical boxed slice so the deallocation layout matches.
    let _boxed: Box<[u8]> = unsafe {
        Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            ptr as *mut u8,
            alloc_len,
        ))
    };
    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Operation Cancellation
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_operation_cancel(
    runtime: aster_runtime_t,
    operation: aster_operation_t,
) -> i32 {
    if operation == 0 {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.cancel_operation(operation) {
        bridge.emit_simple(
            aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED,
            aster_status_t::ASTER_STATUS_CANCELLED,
            operation,
            0,
            0,
            0,
            aster_status_t::ASTER_STATUS_CANCELLED as i32,
        );
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

// ============================================================================
// C FFI Functions - Error
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_last_error_message(buffer: *mut u8, capacity: usize) -> usize {
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

/// Static, NUL-terminated name for an `aster_status_t` value (passed as the
/// raw `int32_t` that every function returns). Unknown values (including the
/// negative `ASTER_CALL_ERR_*` sub-codes) yield `"ASTER_STATUS_UNKNOWN"`.
#[no_mangle]
pub extern "C" fn aster_status_name(status: i32) -> *const c_char {
    let s = match status {
        x if x == aster_status_t::ASTER_STATUS_OK as i32 => "ASTER_STATUS_OK\0",
        x if x == aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32 => {
            "ASTER_STATUS_INVALID_ARGUMENT\0"
        }
        x if x == aster_status_t::ASTER_STATUS_NOT_FOUND as i32 => "ASTER_STATUS_NOT_FOUND\0",
        x if x == aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32 => {
            "ASTER_STATUS_ALREADY_CLOSED\0"
        }
        x if x == aster_status_t::ASTER_STATUS_QUEUE_FULL as i32 => "ASTER_STATUS_QUEUE_FULL\0",
        x if x == aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32 => {
            "ASTER_STATUS_BUFFER_TOO_SMALL\0"
        }
        x if x == aster_status_t::ASTER_STATUS_UNSUPPORTED as i32 => "ASTER_STATUS_UNSUPPORTED\0",
        x if x == aster_status_t::ASTER_STATUS_INTERNAL as i32 => "ASTER_STATUS_INTERNAL\0",
        x if x == aster_status_t::ASTER_STATUS_TIMEOUT as i32 => "ASTER_STATUS_TIMEOUT\0",
        x if x == aster_status_t::ASTER_STATUS_CANCELLED as i32 => "ASTER_STATUS_CANCELLED\0",
        x if x == aster_status_t::ASTER_STATUS_CONNECTION_REFUSED as i32 => {
            "ASTER_STATUS_CONNECTION_REFUSED\0"
        }
        x if x == aster_status_t::ASTER_STATUS_STREAM_RESET as i32 => "ASTER_STATUS_STREAM_RESET\0",
        x if x == aster_status_t::ASTER_STATUS_ABI_MISMATCH as i32 => "ASTER_STATUS_ABI_MISMATCH\0",
        _ => "ASTER_STATUS_UNKNOWN\0",
    };
    s.as_ptr() as *const c_char
}

// ============================================================================
// C FFI Functions - Node
// ============================================================================

/// Create an in-memory node with default endpoint settings.
///
/// **Deprecated (ABI 3.2, removed in 4.0):** use `aster_node_start` with an
/// `aster_config_t`.
#[no_mangle]
pub unsafe extern "C" fn aster_node_memory(
    runtime: aster_runtime_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
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
                    aster_event_kind_t::ASTER_EVENT_NODE_CREATED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Create a persistent node at `path` with default endpoint settings.
///
/// **Deprecated (ABI 3.2, removed in 4.0):** use `aster_node_start` with an
/// `aster_config_t`.
#[no_mangle]
pub unsafe extern "C" fn aster_node_persistent(
    runtime: aster_runtime_t,
    path: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
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
                    aster_event_kind_t::ASTER_EVENT_NODE_CREATED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_node_close(
    runtime: aster_runtime_t,
    node: aster_node_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
        node::forget_node(&bridge2, node);
        bridge2.nodes.remove(node);
        bridge2.emit_simple(
            aster_event_kind_t::ASTER_EVENT_CLOSED,
            aster_status_t::ASTER_STATUS_OK,
            op_id,
            node,
            0,
            user_data,
            0,
        );
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_node_id(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let id = node_arc.node_id();
    let len = id.len();

    *out_len = len;

    if capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(id.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_node_addr_info(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out_buf: *mut u8,
    buf_capacity: usize,
    out_addr: *mut aster_node_addr_t,
) -> i32 {
    if out_addr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let addr = node_arc.node_addr_info();
    pack_node_addr(&addr, out_buf, buf_capacity, out_addr)
}

#[no_mangle]
pub unsafe extern "C" fn aster_node_export_secret_key(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let key = node_arc.export_secret_key();
    let len = key.len();

    *out_len = len;

    if capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(key.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Pack `addr` into the caller's scratch buffer and point `*out_addr` into it:
/// NUL-terminated endpoint id, relay URL and direct addresses, then an aligned
/// `aster_bytes_t` array for the direct addresses. Shared by
/// `aster_node_addr_info` and `aster_endpoint_addr_info`.
unsafe fn pack_node_addr(
    addr: &CoreNodeAddr,
    out_buf: *mut u8,
    buf_capacity: usize,
    out_addr: *mut aster_node_addr_t,
) -> i32 {
    if out_buf.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    // Pack into scratch buffer: endpoint_id, relay_url, direct_addresses
    let mut offset = 0;

    // endpoint_id
    if offset + addr.endpoint_id.len() + 1 > buf_capacity {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
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
            return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
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
            return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
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
    let direct_addrs_ptr: *const aster_bytes_t = if addr_offsets.is_empty() {
        ptr::null()
    } else {
        // Allocate space for the list at the end, aligned for `aster_bytes_t`
        // (the packed strings before it leave `offset` at an arbitrary byte
        // boundary; writing the structs there unaligned is UB).
        let pad = out_buf
            .wrapping_add(offset)
            .align_offset(std::mem::align_of::<aster_bytes_t>());
        let list_offset = offset.saturating_add(pad);
        offset =
            list_offset.saturating_add(addr_offsets.len() * std::mem::size_of::<aster_bytes_t>());
        if offset > buf_capacity {
            return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
        }

        for (i, &addr_off) in addr_offsets.iter().enumerate() {
            let item_ptr = unsafe {
                out_buf
                    .add(list_offset)
                    .add(i * std::mem::size_of::<aster_bytes_t>())
                    as *mut aster_bytes_t
            };
            let addr_len = addr.direct_addresses[i].len();
            unsafe {
                *item_ptr = aster_bytes_t {
                    ptr: out_buf.add(addr_off),
                    len: addr_len,
                };
            }
        }
        unsafe { out_buf.add(list_offset) as *const aster_bytes_t }
    };

    *out_addr = aster_node_addr_t {
        endpoint_id: aster_bytes_t {
            ptr: unsafe { out_buf.add(ep_id_offset) },
            len: addr.endpoint_id.len(),
        },
        relay_url: aster_bytes_t {
            ptr: relay_offset
                .map(|o| unsafe { out_buf.add(o) })
                .unwrap_or(ptr::null_mut()),
            len: addr.relay_url.as_ref().map(|s| s.len()).unwrap_or(0),
        },
        direct_addresses: aster_bytes_list_t {
            items: direct_addrs_ptr,
            len: addr.direct_addresses.len(),
        },
    };

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Endpoint
// ============================================================================

/// Create a bare endpoint (no blobs/docs/gossip).
///
/// **Deprecated (ABI 3.2, removed in 4.0):** use `aster_node_start`; a node
/// covers every endpoint use (connect, accept, hooks, metrics).
#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_create(
    runtime: aster_runtime_t,
    config: *const aster_endpoint_config_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if config.is_null() || out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let expected_struct_size = std::mem::size_of::<aster_endpoint_config_t>() as u32;
    let actual_struct_size = unsafe { (*config).struct_size };
    if actual_struct_size != expected_struct_size {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
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
    let hook_failure_mode = match cfg.hook_failure_mode {
        0 => aster_transport_core::trust::HookFailureMode::FailOpen,
        1 => aster_transport_core::trust::HookFailureMode::FailClosed,
        _ => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
    };

    // Extract secret_key before building core_config so we can store it later
    let secret_key = unsafe { read_bytes_opt(&cfg.secret_key) };

    let core_config = CoreEndpointConfig {
        relay_mode: match cfg.relay_mode {
            0 => None,
            1 => Some("custom".to_string()),
            2 => Some("disabled".to_string()),
            3 => Some("staging".to_string()),
            _ => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
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
        secret_key: secret_key.clone(),
        enable_discovery: cfg.enable_discovery != 0,
        enable_monitoring: true, // Always enable monitoring for FFI endpoints
        enable_hooks,
        hook_timeout_ms,
        hook_failure_mode,
        // Phase 1d fields: read from the C struct (added in aster_endpoint_config_t).
        bind_addr: unsafe { read_string_opt(&cfg.bind_addr) },
        clear_ip_transports: cfg.clear_ip_transports != 0,
        clear_relay_transports: cfg.clear_relay_transports != 0,
        portmapper_config: match cfg.portmapper_config {
            0 => None, // 0 = enabled (default) → let build_endpoint_config use its default
            1 => Some("disabled".to_string()),
            _ => None,
        },
        proxy_url: unsafe { read_string_opt(&cfg.proxy_url) },
        proxy_from_env: cfg.proxy_from_env != 0,
        transport_max_concurrent_bidi_streams: None,
        transport_max_concurrent_uni_streams: None,
        transport_stream_receive_window: None,
        transport_receive_window: None,
        transport_send_window: None,
        transport_max_idle_timeout_ms: None,
        transport_keep_alive_interval_ms: None,
        transport_initial_mtu: None,
        transport_datagram_receive_buffer_size: None,
        transport_datagram_send_buffer_size: None,
        transport_send_fairness: None,
        transport_enable_segmentation_offload: None,
        data_dir: unsafe { read_string_opt(&cfg.data_dir_utf8) },
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

                // Store the secret key seed for later export via aster_endpoint_export_secret_key
                if let Some(ref key) = secret_key {
                    let mut keys = bridge2.endpoint_secret_keys.lock().unwrap();
                    keys.insert(handle, key.clone());
                }

                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_ENDPOINT_CREATED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    handle,
                    0,
                    user_data,
                    0,
                );

                // Drain hook events onto the completion queue.
                if let Some(receiver) = hook_receiver {
                    node::spawn_hook_drainers(&bridge2, handle, user_data, receiver, None);
                }
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_close(
    runtime: aster_runtime_t,
    endpoint: aster_endpoint_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = match bridge.endpoints.get(endpoint) {
        Some(e) => e,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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

        // Remove the secret key from the registry
        bridge2
            .endpoint_secret_keys
            .lock()
            .unwrap()
            .remove(&endpoint);

        bridge2.emit_simple(
            aster_event_kind_t::ASTER_EVENT_CLOSED,
            aster_status_t::ASTER_STATUS_OK,
            op_id,
            endpoint,
            0,
            user_data,
            0,
        );
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_id(
    runtime: aster_runtime_t,
    endpoint: aster_endpoint_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = match bridge.endpoints.get(endpoint) {
        Some(e) => e,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let id = ep_arc.endpoint_id();
    let len = id.len();

    *out_len = len;

    if capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(id.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_export_secret_key(
    runtime: aster_runtime_t,
    endpoint: aster_endpoint_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    // Check endpoint exists to return NOT_FOUND (keys are removed when endpoint is closed)
    let _ep = match bridge.endpoints.get(endpoint) {
        Some(e) => e,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let keys = bridge.endpoint_secret_keys.lock().unwrap();
    let key = match keys.get(&endpoint) {
        Some(k) => k,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let len = key.len();

    *out_len = len;

    if capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(key.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_addr_info(
    runtime: aster_runtime_t,
    endpoint: aster_endpoint_t,
    out_buf: *mut u8,
    buf_capacity: usize,
    out_addr: *mut aster_node_addr_t,
) -> i32 {
    if out_addr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = match bridge.endpoints.get(endpoint) {
        Some(e) => e,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let addr = ep_arc.endpoint_addr_info();
    pack_node_addr(&addr, out_buf, buf_capacity, out_addr)
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_stop(
    runtime: aster_runtime_t,
    recv_stream: aster_recv_stream_t,
    error_code: u32,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    if let Err(e) = stream_arc.stop(error_code as u64) {
        return set_last_error(e);
    }

    bridge.recv_streams.remove(recv_stream);
    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Connections
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_connect(
    runtime: aster_runtime_t,
    endpoint_or_node: u64,
    config: *const aster_connect_config_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if config.is_null() || out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
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
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let cfg = unsafe { *config };
    let node_id = match unsafe { read_string(&cfg.node_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let alpn = unsafe { read_bytes(&cfg.alpn) };

    // If addr is provided, copy it out before the async block to avoid lifetime issues
    let connect_addr = if !cfg.addr.is_null() {
        let addr_ref = unsafe { &*cfg.addr };
        let addr_node_id = match unsafe { read_string(&addr_ref.endpoint_id) } {
            Ok(s) => s,
            Err(e) => return e,
        };
        let relay_url = if addr_ref.relay_url.ptr.is_null() || addr_ref.relay_url.len == 0 {
            None
        } else {
            match unsafe { read_string(&addr_ref.relay_url) } {
                Ok(s) => Some(s),
                Err(e) => return e,
            }
        };
        let mut direct_addresses = Vec::new();
        if !addr_ref.direct_addresses.items.is_null() && addr_ref.direct_addresses.len > 0 {
            let items = unsafe {
                slice::from_raw_parts(
                    addr_ref.direct_addresses.items,
                    addr_ref.direct_addresses.len,
                )
            };
            for item in items {
                match unsafe { read_string(item) } {
                    Ok(s) => direct_addresses.push(s),
                    Err(e) => return e,
                }
            }
        }
        Some(CoreNodeAddr {
            endpoint_id: addr_node_id,
            relay_url,
            direct_addresses,
        })
    } else {
        None
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

        let connect_result = if let Some(addr) = connect_addr {
            ep.connect_node_addr(addr, alpn).await
        } else {
            ep.connect(node_id, alpn).await
        };

        match connect_result {
            Ok(conn) => {
                let handle = bridge2.connections.insert(conn);
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_CONNECTED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_accept(
    runtime: aster_runtime_t,
    endpoint_or_node: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
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
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_CONNECTION_ACCEPTED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// Phase 1e: Unified Aster Node — custom ALPNs on the shared iroh Router
// ============================================================================

/// Create an in-memory node with blobs/docs/gossip + custom aster ALPNs.
/// Emits ASTER_EVENT_NODE_CREATED on success.
///
/// **Deprecated (ABI 3.2, removed in 4.0):** use `aster_node_start` with an
/// `aster_config_t`.
#[no_mangle]
pub unsafe extern "C" fn aster_node_memory_with_alpns(
    runtime: aster_runtime_t,
    alpns: *const *const u8,
    alpn_lens: *const usize,
    alpn_count: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
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
                    aster_event_kind_t::ASTER_EVENT_NODE_CREATED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Pull the next incoming aster-ALPN connection from the node's queue.
/// Long-poll: spawns a tokio task, emits ASTER_EVENT_ASTER_ACCEPTED when a
/// connection arrives. event.handle = connection_handle, event.data_ptr/len
/// = ALPN bytes, event.buffer = lease to release via aster_buffer_release.
#[no_mangle]
pub unsafe extern "C" fn aster_node_accept_aster(
    runtime: aster_runtime_t,
    node: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_ASTER_ACCEPTED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_connection_remote_id(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let id = conn_arc.remote_id();
    let len = id.len();

    *out_len = len;

    if capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(id.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_close(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    error_code: u32,
    reason: aster_bytes_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let reason_bytes = unsafe { read_bytes(&reason) };

    if let Err(e) = conn_arc.close(error_code as u64, reason_bytes) {
        return set_last_error(e);
    }

    bridge.connections.remove(connection);
    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_closed(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
            aster_event_kind_t::ASTER_EVENT_CONNECTION_CLOSED,
            aster_status_t::ASTER_STATUS_OK,
            op_id,
            connection,
            0,
            user_data,
            error_code,
        );
        bridge2.emit_with_data(event, payload);
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_send_datagram(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    data: aster_bytes_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let data_bytes = unsafe { read_bytes(&data) };

    if let Err(e) = conn_arc.send_datagram(data_bytes) {
        return set_last_error(e);
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_read_datagram(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_DATAGRAM_RECEIVED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Streams
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_open_bi(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_STREAM_OPENED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_accept_bi(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_STREAM_ACCEPTED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// Tunneling — see ffi_spec/Aster-tunneling.md
// ============================================================================

/// Synchronous: authorize a TCP tunnel on `connection` to `host:port`.
/// On success, writes the 32-byte ticket bytes to `out_ticket` (must
/// point to ≥32 bytes). The bytes go into the RPC response payload
/// the handler returns to the peer.
///
/// `host` is read as a UTF-8 string from `aster_bytes_t`.
/// `ttl_secs == 0` uses the node default (30s).
#[no_mangle]
pub unsafe extern "C" fn aster_connection_authorize_tunnel_tcp(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    host: aster_bytes_t,
    port: u16,
    ttl_secs: u32,
    out_ticket: *mut u8,
) -> i32 {
    if out_ticket.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };
    let host_str = match unsafe { read_string(&host) } {
        Ok(s) => s,
        Err(s) => return s,
    };
    let addr_str = if host_str.contains(':') && !host_str.starts_with('[') {
        format!("[{host_str}]:{port}")
    } else {
        format!("{host_str}:{port}")
    };
    let addr: std::net::SocketAddr = match addr_str.parse() {
        Ok(a) => a,
        Err(e) => return set_last_error(format!("bad address: {e}")),
    };
    let ticket = match conn_arc.authorize_tunnel(
        vec![aster_transport_core::tunnel::TunnelTarget::Tcp { addr }],
        Duration::from_secs(ttl_secs as u64),
    ) {
        Ok(t) => t,
        Err(e) => return set_last_error(e),
    };
    unsafe {
        ptr::copy_nonoverlapping(ticket.as_bytes().as_ptr(), out_ticket, 32);
    }
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Async: redeem a 32-byte tunnel ticket on `connection`. Emits
/// ASTER_EVENT_STREAM_OPENED with the (send, recv) pair on success.
/// The streams are raw — bytes after this carry application data.
///
/// `ticket` must point to exactly 32 readable bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_connection_open_tunnel(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    ticket: *const u8,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() || ticket.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let mut ticket_bytes = [0u8; 32];
    unsafe { ptr::copy_nonoverlapping(ticket, ticket_bytes.as_mut_ptr(), 32) };
    let ticket = aster_transport_core::tunnel::TunnelTicket(ticket_bytes);

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        match conn_arc.open_tunnel(ticket).await {
            Ok((send, recv)) => {
                let send_handle = bridge2.send_streams.insert(send);
                let recv_handle = bridge2.recv_streams.insert(recv);
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_STREAM_OPENED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_open_uni(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_STREAM_OPENED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_accept_uni(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_STREAM_ACCEPTED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_write(
    runtime: aster_runtime_t,
    send_stream: aster_send_stream_t,
    data: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.send_streams.get(send_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_SEND_COMPLETED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_finish(
    runtime: aster_runtime_t,
    send_stream: aster_send_stream_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.send_streams.get(send_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_STREAM_FINISHED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_read(
    runtime: aster_runtime_t,
    recv_stream: aster_recv_stream_t,
    max_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_FRAME_RECEIVED,
                    aster_status_t::ASTER_STATUS_OK,
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
                    aster_event_kind_t::ASTER_EVENT_STREAM_FINISHED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_read_to_end(
    runtime: aster_runtime_t,
    recv_stream: aster_recv_stream_t,
    max_size: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_BYTES_RESULT,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_read_exact(
    runtime: aster_runtime_t,
    recv_stream: aster_recv_stream_t,
    exact_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_BYTES_RESULT,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_stopped(
    runtime: aster_runtime_t,
    send_stream: aster_send_stream_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.send_streams.get(send_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_STREAM_RESET,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    send_stream,
                    0,
                    user_data,
                    error_code as i32,
                );
            }
            Ok(None) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_STREAM_FINISHED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Add a peer address to the address lookup of an endpoint **or node**
/// (nodes accepted since ABI 3.3), so later dials and blob/doc fetches by
/// node id can reach it.
#[no_mangle]
pub unsafe extern "C" fn aster_add_node_addr(
    runtime: aster_runtime_t,
    endpoint: aster_endpoint_t,
    addr: aster_node_addr_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let ep_arc = match bridge
        .endpoints
        .get(endpoint)
        .or_else(|| bridge.nodes.get(endpoint).map(|n| Arc::new(n.net_client())))
    {
        Some(e) => e,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Handle Free (typed, one per handle kind)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_node_free(runtime: aster_runtime_t, node: aster_node_t) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    node::forget_node(&bridge, node);
    if bridge.nodes.remove(node).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_free(
    runtime: aster_runtime_t,
    endpoint: aster_endpoint_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.endpoints.remove(endpoint).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_free(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.connections.remove(connection).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_send_stream_free(
    runtime: aster_runtime_t,
    stream: aster_send_stream_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.send_streams.remove(stream).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_recv_stream_free(
    runtime: aster_runtime_t,
    stream: aster_recv_stream_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.recv_streams.remove(stream).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_doc_free(runtime: aster_runtime_t, doc: u64) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.docs.remove(doc).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

/// Synchronous: write the 64-char hex namespace id of the doc to `out_buf`.
///
/// Mirrors `CoreDoc::doc_id()` — used by consumer-admission responses to
/// carry the registry namespace so a client can `join_and_subscribe_namespace`
/// the doc and fetch published contract manifests.
///
/// On BUFFER_TOO_SMALL, sets `*out_len` to required size.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_id(
    runtime: aster_runtime_t,
    doc: u64,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };
    let id_hex = doc_arc.doc_id();
    write_to_caller_buf(id_hex.as_bytes(), out_buf, out_len) as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_gossip_topic_free(runtime: aster_runtime_t, topic: u64) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.gossip_topics.remove(topic).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

// ============================================================================
// C FFI Functions - Blobs (simplified for Phase 1)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_blobs_add_bytes(
    runtime: aster_runtime_t,
    node: aster_node_t,
    data: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_BLOB_ADDED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_blobs_read(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_BLOB_READ,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Import a file by path into the blob store (Copy mode, never TryReference).
/// On success emits `ASTER_EVENT_BLOB_ADDED` carrying the hash hex as the event
/// payload. `path` is the UTF-8 filesystem path.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_add_path(
    runtime: aster_runtime_t,
    node: aster_node_t,
    path: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
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

        match blobs.add_path(path_str).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_ADDED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Import a file by path and set a persistent named tag in one step. On success
/// emits `ASTER_EVENT_BLOB_ADDED` carrying the hash hex as the event payload.
/// `path` and `tag_name` are UTF-8.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_add_path_with_named_tag(
    runtime: aster_runtime_t,
    node: aster_node_t,
    path: aster_bytes_t,
    tag_name: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let path_str = match unsafe { read_string(&path) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let tag_str = match unsafe { read_string(&tag_name) } {
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

        match blobs.add_path_with_named_tag(path_str, tag_str).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_ADDED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Blobs: Collection (sendme-compatible)
// ============================================================================

/// Store bytes as a single-file Collection (HashSeq), compatible with sendme.
/// Emits ASTER_EVENT_BLOB_ADDED with the collection hash in the payload.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_add_bytes_as_collection(
    runtime: aster_runtime_t,
    node: aster_node_t,
    name: aster_bytes_t,
    data: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_BLOB_ADDED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Store a multi-file collection (HashSeq).
///
/// `entries_json` is a UTF-8 JSON string: `[["name1","base64data1"],["name2","base64data2"]]`
/// Each entry's data is base64-encoded. The result (collection hash hex) is emitted as
/// `ASTER_EVENT_BLOB_COLLECTION_ADDED` with the hash string in the event data buffer.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_add_collection(
    runtime: aster_runtime_t,
    node: aster_node_t,
    entries_json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let json_str = match unsafe { read_string(&entries_json) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    // Parse JSON array of [name, base64_data] pairs
    let parsed: Vec<(String, String)> = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(_) => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
    };

    use base64::Engine;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(parsed.len());
    for (name, b64_data) in parsed {
        match base64::engine::general_purpose::STANDARD.decode(&b64_data) {
            Ok(data) => entries.push((name, data)),
            Err(_) => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
        }
    }

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.add_collection(entries).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_COLLECTION_ADDED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// List entries from a stored collection.
///
/// `hash_hex` is the collection hash. The result is emitted as
/// `ASTER_EVENT_BLOB_READ` with a JSON string:
/// `[["name1","hash_hex1",size1],["name2","hash_hex2",size2]]`
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_list_collection(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let hash_str = match unsafe { read_string(&hash_hex) } {
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

        match blobs.list_collection(hash_str).await {
            Ok(entries) => {
                let json = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string());
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_READ,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, json.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Create a ticket for a Collection (HashSeq format), compatible with sendme.
/// Writes the ticket string into the caller-provided buffer.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_create_collection_ticket(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(ticket.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Blobs: Ticket & Download
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_blobs_create_ticket(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(ticket.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_blobs_download(
    runtime: aster_runtime_t,
    node: aster_node_t,
    ticket: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_BLOB_DOWNLOADED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Blob Status / Has (Phase 1c.3)
// ============================================================================

/// Check the status of a blob in the local store. Synchronous.
///
/// Writes to `out_status`: 0 = not_found, 1 = partial, 2 = complete.
/// Writes to `out_size`: byte size (0 if not_found).
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_status(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_status: *mut u32,
    out_size: *mut u64,
) -> i32 {
    if hash_hex_ptr.is_null() || out_status.is_null() || out_size.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(_) => aster_status_t::ASTER_STATUS_INTERNAL as i32,
    }
}

/// Check if a blob is fully stored locally. Synchronous.
///
/// Writes to `out_has`: 1 = complete/present, 0 = not complete.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_has(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_has: *mut u32,
) -> i32 {
    if hash_hex_ptr.is_null() || out_has.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(_) => aster_status_t::ASTER_STATUS_INTERNAL as i32,
    }
}

// ============================================================================
// C FFI Functions - Blob Transfer Observability (Phase 1d)
// ============================================================================

/// Snapshot of the current bitfield for a blob.
/// Fills `out_is_complete` (1=complete, 0=partial/not-found) and `out_size` (total bytes, 0 if unknown).
/// Returns ASTER_STATUS_NOT_FOUND if the node handle is unknown.
/// Returns ASTER_STATUS_INVALID_ARGUMENT if any pointer is null.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_observe_snapshot(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_is_complete: *mut u32,
    out_size: *mut u64,
) -> i32 {
    if hash_hex_ptr.is_null() || out_is_complete.is_null() || out_size.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(_) => aster_status_t::ASTER_STATUS_INTERNAL as i32,
    }
}

/// Wait until a blob is fully downloaded locally.
/// Emits ASTER_EVENT_BLOB_OBSERVE_COMPLETE (via ASTER_EVENT_UNIT_RESULT) when complete,
/// or an error event if the observation stream ends without completion.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_observe_complete(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if hash_hex_ptr.is_null() || out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_BLOB_OBSERVE_COMPLETE,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Check local availability of a blob using the Remote API.
/// Fills `out_is_complete` (1=complete, 0=partial) and `out_local_bytes` (bytes we have locally).
/// Returns ASTER_STATUS_NOT_FOUND if the node handle is unknown.
/// Returns ASTER_STATUS_INVALID_ARGUMENT if any pointer is null.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_local_info(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_is_complete: *mut u32,
    out_local_bytes: *mut u64,
) -> i32 {
    if hash_hex_ptr.is_null() || out_is_complete.is_null() || out_local_bytes.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(_) => aster_status_t::ASTER_STATUS_INTERNAL as i32,
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

/// Set a named tag. format: 0 = raw, 1 = hash_seq. Emits ASTER_EVENT_TAG_SET.
#[no_mangle]
pub unsafe extern "C" fn aster_tags_set(
    runtime: aster_runtime_t,
    node: aster_node_t,
    name_ptr: *const u8,
    name_len: usize,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    format: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() || name_ptr.is_null() || hash_hex_ptr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_TAG_SET,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Get a tag by name. Emits ASTER_EVENT_TAG_GET with payload on found, NOT_FOUND status if absent.
#[no_mangle]
pub unsafe extern "C" fn aster_tags_get(
    runtime: aster_runtime_t,
    node: aster_node_t,
    name_ptr: *const u8,
    name_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() || name_ptr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_TAG_GET,
                    aster_status_t::ASTER_STATUS_OK,
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
                    aster_event_kind_t::ASTER_EVENT_TAG_GET,
                    aster_status_t::ASTER_STATUS_NOT_FOUND,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Delete a tag by name. Emits ASTER_EVENT_TAG_DELETED with count in event.flags.
#[no_mangle]
pub unsafe extern "C" fn aster_tags_delete(
    runtime: aster_runtime_t,
    node: aster_node_t,
    name_ptr: *const u8,
    name_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() || name_ptr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_TAG_DELETED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// List tags matching a prefix (empty prefix = all tags).
/// Emits ASTER_EVENT_TAG_LIST with packed tag records in payload; event.flags = count.
#[no_mangle]
pub unsafe extern "C" fn aster_tags_list_prefix(
    runtime: aster_runtime_t,
    node: aster_node_t,
    prefix_ptr: *const u8,
    prefix_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_TAG_LIST,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Docs (simplified for Phase 1)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_docs_create(
    runtime: aster_runtime_t,
    node: aster_node_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_CREATED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_docs_create_author(
    runtime: aster_runtime_t,
    node: aster_node_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_AUTHOR_CREATED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_docs_join(
    runtime: aster_runtime_t,
    node: aster_node_t,
    ticket: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_JOINED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_doc_set_bytes(
    runtime: aster_runtime_t,
    doc: u64,
    author_hex: aster_bytes_t,
    key: aster_bytes_t,
    value: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_SET,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Delete all entries for an author whose key starts with `prefix`.
///
/// Emits ASTER_EVENT_DOC_DEL with an 8-byte little-endian u64 payload containing
/// the number of removed entries.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_del(
    runtime: aster_runtime_t,
    doc: u64,
    author_hex: aster_bytes_t,
    prefix: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let prefix_bytes = unsafe { read_bytes(&prefix) };

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.del(author, prefix_bytes).await {
            Ok(removed) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_DOC_DEL,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, removed.to_le_bytes().to_vec());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_doc_get_exact(
    runtime: aster_runtime_t,
    doc: u64,
    author_hex: aster_bytes_t,
    key: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_GET,
                    aster_status_t::ASTER_STATUS_OK,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_GET,
                    aster_status_t::ASTER_STATUS_NOT_FOUND,
                    op_id,
                    doc,
                    0,
                    user_data,
                    aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Share a document. `mode`: one of `aster_doc_share_mode_t`.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_share(
    runtime: aster_runtime_t,
    doc: u64,
    mode: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    // `mode` is one of `aster_doc_share_mode_t`.
    let mode_str = match mode {
        x if x == aster_doc_share_mode_t::ASTER_DOC_SHARE_READ as u32 => "read".to_string(),
        x if x == aster_doc_share_mode_t::ASTER_DOC_SHARE_WRITE as u32 => "write".to_string(),
        _ => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_SHARED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Docs: Query (key_exact / key_prefix without author)
// ============================================================================

/// Query mode for doc queries
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_doc_query_mode_t {
    ASTER_DOC_QUERY_KEY_EXACT = 0,
    ASTER_DOC_QUERY_KEY_PREFIX = 1,
    /// Return only the latest entry for the key (across all authors), or nothing if the
    /// latest write was a deletion. Always emits 0 or 1 entries; `limit` is ignored.
    ASTER_DOC_QUERY_LATEST_EXACT = 2,
    /// List the latest entry for each distinct key under the prefix (across all authors),
    /// omitting keys whose latest write was a deletion. `limit` caps the number of keys.
    ASTER_DOC_QUERY_LATEST_PREFIX = 3,
}

/// Query a doc by key.
///
/// `limit` caps the number of entries returned by a prefix (`ASTER_DOC_QUERY_KEY_PREFIX`)
/// or exact (`ASTER_DOC_QUERY_KEY_EXACT`) query; pass `0` for an unbounded result. It is
/// ignored by `ASTER_DOC_QUERY_LATEST_EXACT`, which always returns at most one entry (the
/// latest writer for the key, or none if the latest write was a deletion).
///
/// `mode`: one of `aster_doc_query_mode_t`.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_query(
    runtime: aster_runtime_t,
    doc: u64,
    mode: u32,
    key: aster_bytes_t,
    limit: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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

        let limit = if limit == 0 { None } else { Some(limit) };
        let result = if mode == aster_doc_query_mode_t::ASTER_DOC_QUERY_KEY_PREFIX as u32 {
            doc_arc.query_key_prefix(key_bytes, limit).await
        } else if mode == aster_doc_query_mode_t::ASTER_DOC_QUERY_LATEST_PREFIX as u32 {
            doc_arc.query_latest_prefix(key_bytes, limit).await
        } else if mode == aster_doc_query_mode_t::ASTER_DOC_QUERY_LATEST_EXACT as u32 {
            doc_arc
                .query_latest_exact(key_bytes)
                .await
                .map(|opt| opt.into_iter().collect::<Vec<_>>())
        } else {
            doc_arc.query_key_exact(key_bytes, limit).await
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
                    aster_event_kind_t::ASTER_EVENT_DOC_QUERY,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Read the content bytes for a doc entry given its content hash.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_read_entry_content(
    runtime: aster_runtime_t,
    doc: u64,
    content_hash_hex: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_BLOB_READ,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Doc Sync Lifecycle (Phase 1c.5)
// ============================================================================

/// Start syncing a document with the specified peers (endpoint-ID hex strings).
/// Emits ASTER_EVENT_UNIT_RESULT on completion.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_start_sync(
    runtime: aster_runtime_t,
    doc: u64,
    peers: aster_bytes_list_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Leave (stop syncing) a document.
/// Emits ASTER_EVENT_UNIT_RESULT on completion.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_leave(
    runtime: aster_runtime_t,
    doc: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
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
/// Emits ASTER_EVENT_DOC_SUBSCRIBED (status=OK, handle=receiver_handle) on success.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_subscribe(
    runtime: aster_runtime_t,
    doc: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_SUBSCRIBED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Receive the next live document event (long-poll, like gossip_recv).
/// Emits ASTER_EVENT_DOC_EVENT with event.subtype indicating the event kind and
/// packed data in the payload.  When the subscription ends, emits with status=NOT_FOUND.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_event_recv(
    runtime: aster_runtime_t,
    receiver: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let receiver_arc = match bridge.doc_event_receivers.get(receiver) {
        Some(r) => r,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_EVENT,
                    aster_status_t::ASTER_STATUS_OK,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_EVENT,
                    aster_status_t::ASTER_STATUS_NOT_FOUND,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Docs: Download Policy, Share with Addr, Join+Subscribe (Phase 1c.6-1c.8)
// ============================================================================

/// Download policy mode for aster_doc_set_download_policy.
/// 0 = everything (download all entries, no prefixes needed)
/// 1 = nothing_except (only download entries matching the given prefixes)
/// 2 = everything_except (download all except entries matching the given prefixes)
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_download_policy_mode_t {
    ASTER_DOWNLOAD_POLICY_EVERYTHING = 0,
    ASTER_DOWNLOAD_POLICY_NOTHING_EXCEPT = 1,
    ASTER_DOWNLOAD_POLICY_EVERYTHING_EXCEPT = 2,
}

/// Set the download policy for a document.
/// `mode`: one of `aster_download_policy_mode_t`.
/// prefixes: list of byte-string prefixes (used for NOTHING_EXCEPT / EVERYTHING_EXCEPT modes).
/// Emits ASTER_EVENT_UNIT_RESULT on success.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_set_download_policy(
    runtime: aster_runtime_t,
    doc: u64,
    mode: u32,
    prefixes: aster_bytes_list_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
        _ => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
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
                    aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Share a document with full relay+address info (AddrInfoOptions::RelayAndAddresses).
/// `mode`: one of `aster_doc_share_mode_t`.
/// Emits ASTER_EVENT_DOC_SHARED with the ticket string in the payload.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_share_with_addr(
    runtime: aster_runtime_t,
    doc: u64,
    mode: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    // `mode` is one of `aster_doc_share_mode_t`.
    let mode_str = match mode {
        x if x == aster_doc_share_mode_t::ASTER_DOC_SHARE_READ as u32 => "read".to_string(),
        x if x == aster_doc_share_mode_t::ASTER_DOC_SHARE_WRITE as u32 => "write".to_string(),
        _ => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_SHARED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Join a document and subscribe to live events atomically.
/// Emits ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED:
///   event.handle = doc handle
///   event.flags  = event-receiver handle
#[no_mangle]
pub unsafe extern "C" fn aster_docs_join_and_subscribe(
    runtime: aster_runtime_t,
    node: aster_node_t,
    ticket: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Gossip (simplified for Phase 1)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_gossip_subscribe(
    runtime: aster_runtime_t,
    node: aster_node_t,
    topic: aster_bytes_t,
    peers: aster_bytes_list_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_GOSSIP_SUBSCRIBED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_gossip_broadcast(
    runtime: aster_runtime_t,
    topic: u64,
    data: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let topic_arc = match bridge.gossip_topics.get(topic) {
        Some(t) => t,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_GOSSIP_BROADCAST_DONE,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_gossip_recv(
    runtime: aster_runtime_t,
    topic: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let topic_arc = match bridge.gossip_topics.get(topic) {
        Some(t) => t,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
                    aster_event_kind_t::ASTER_EVENT_GOSSIP_RECEIVED,
                    aster_status_t::ASTER_STATUS_OK,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// Phase 1b: Datagram Completion FFI Functions
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_connection_max_datagram_size(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_size: *mut u64,
    out_is_some: *mut u32,
) -> i32 {
    if out_size.is_null() || out_is_some.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_datagram_send_buffer_space(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_bytes: *mut u64,
) -> i32 {
    if out_bytes.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let space = conn_arc.datagram_send_buffer_space();
    unsafe {
        *out_bytes = space as u64;
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// Phase 1b: Remote-Info FFI Functions
// ============================================================================

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_remote_info_t {
    pub struct_size: u32,
    pub node_id: aster_bytes_t,
    pub is_connected: u32,
    /// One of `aster_connection_type_t`.
    pub connection_type: u32,
    pub relay_url: aster_bytes_t,
    pub last_handshake_ns: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_connection_info_t {
    pub struct_size: u32,
    /// One of `aster_connection_type_t`.
    pub connection_type: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub rtt_ns: u64,
    pub alpn: aster_bytes_t,
    pub is_connected: u32,
}

/// Snapshot of the currently selected QUIC path for a connection.
/// `path_kind` discriminates the populated fields:
///   0 = no path selected (connection dropped or not yet negotiated)
///   1 = direct UDP — `peer_host` and `peer_port` are populated
///   2 = relay     — `relay_url` is populated
/// `rtt_micros` is `UINT64_MAX` when RTT has not yet been measured.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_transport_snapshot_t {
    pub struct_size: u32,
    /// One of `aster_path_kind_t`.
    pub path_kind: u32,
    pub peer_host: aster_bytes_t,
    pub peer_port: u16,
    pub _pad: u16,
    pub relay_url: aster_bytes_t,
    pub rtt_micros: u64,
}

/// On success the caller owns `out_info.node_id` and `out_info.relay_url`
/// and must free each with `aster_string_release(ptr, len)`.
#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_remote_info(
    runtime: aster_runtime_t,
    endpoint_or_node: u64,
    node_id: aster_bytes_t,
    out_info: *mut aster_remote_info_t,
) -> i32 {
    if out_info.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
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
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let node_id_str = match unsafe { read_string(&node_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let info = match ep.remote_info(&node_id_str) {
        Some(info) => info,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    // Map connection type
    let conn_type = match info.connection_type {
        aster_transport_core::ConnectionType::NotConnected => {
            aster_connection_type_t::ASTER_CONNECTION_TYPE_NOT_CONNECTED as u32
        }
        aster_transport_core::ConnectionType::Connecting => {
            aster_connection_type_t::ASTER_CONNECTION_TYPE_CONNECTING as u32
        }
        aster_transport_core::ConnectionType::Connected(detail) => match detail {
            aster_transport_core::ConnectionTypeDetail::UdpDirect => {
                aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_DIRECT as u32
            }
            aster_transport_core::ConnectionTypeDetail::UdpRelay => {
                aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_RELAY as u32
            }
            aster_transport_core::ConnectionTypeDetail::Other(_) => {
                aster_connection_type_t::ASTER_CONNECTION_TYPE_OTHER as u32
            }
        },
    };

    // Get relay_url offset (we'll use placeholder since this is a single call)
    let relay_url_str = info.relay_url.unwrap_or_default();
    let relay_url_bytes = alloc_string(relay_url_str);

    // Pack node_id into buffer (simplified - caller should use separate buffer)
    let node_id_bytes = alloc_string(info.node_id);

    unsafe {
        *out_info = aster_remote_info_t {
            struct_size: std::mem::size_of::<aster_remote_info_t>() as u32,
            node_id: node_id_bytes,
            is_connected: if info.is_connected { 1 } else { 0 },
            connection_type: conn_type,
            relay_url: relay_url_bytes,
            last_handshake_ns: info.last_handshake_ns.unwrap_or(0),
            bytes_sent: info.bytes_sent,
            bytes_received: info.bytes_received,
        };
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

/// On success the caller owns `node_id` and `relay_url` of each of the first
/// `*out_count` entries and must free each with `aster_string_release(ptr, len)`.
#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_remote_info_list(
    runtime: aster_runtime_t,
    endpoint_or_node: u64,
    out_infos: *mut aster_remote_info_t,
    max_infos: usize,
    out_count: *mut usize,
) -> i32 {
    if out_infos.is_null() || out_count.is_null() || max_infos == 0 {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
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
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
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
            aster_transport_core::ConnectionType::NotConnected => {
                aster_connection_type_t::ASTER_CONNECTION_TYPE_NOT_CONNECTED as u32
            }
            aster_transport_core::ConnectionType::Connecting => {
                aster_connection_type_t::ASTER_CONNECTION_TYPE_CONNECTING as u32
            }
            aster_transport_core::ConnectionType::Connected(detail) => match detail {
                aster_transport_core::ConnectionTypeDetail::UdpDirect => {
                    aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_DIRECT as u32
                }
                aster_transport_core::ConnectionTypeDetail::UdpRelay => {
                    aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_RELAY as u32
                }
                aster_transport_core::ConnectionTypeDetail::Other(_) => {
                    aster_connection_type_t::ASTER_CONNECTION_TYPE_OTHER as u32
                }
            },
        };

        let relay_url_str = info.relay_url.clone().unwrap_or_default();
        let node_id_str = info.node_id.clone();

        unsafe {
            *out_infos.add(i) = aster_remote_info_t {
                struct_size: std::mem::size_of::<aster_remote_info_t>() as u32,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// On success the caller owns `out_info.alpn` and must free it with
/// `aster_string_release(ptr, len)`.
#[no_mangle]
pub unsafe extern "C" fn aster_connection_info(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_info: *mut aster_connection_info_t,
) -> i32 {
    if out_info.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let info = conn_arc.connection_info();

    let conn_type = match info.connection_type {
        aster_transport_core::ConnectionTypeDetail::UdpDirect => {
            aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_DIRECT as u32
        }
        aster_transport_core::ConnectionTypeDetail::UdpRelay => {
            aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_RELAY as u32
        }
        aster_transport_core::ConnectionTypeDetail::Other(_) => {
            aster_connection_type_t::ASTER_CONNECTION_TYPE_OTHER as u32
        }
    };

    let alpn_bytes = alloc_bytes(info.alpn);

    unsafe {
        *out_info = aster_connection_info_t {
            struct_size: std::mem::size_of::<aster_connection_info_t>() as u32,
            connection_type: conn_type,
            bytes_sent: info.bytes_sent,
            bytes_received: info.bytes_received,
            rtt_ns: info.rtt_ns.unwrap_or(0),
            alpn: alpn_bytes,
            is_connected: if info.is_connected { 1 } else { 0 },
        };
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Snapshot the currently selected QUIC path for `connection`. See
/// `aster_transport_snapshot_t` for the discriminator semantics. The
/// caller must free `peer_host` and `relay_url` via `aster_string_release(ptr, len)`
/// (empty buffers — `len == 0` — are safe no-ops).
#[no_mangle]
pub unsafe extern "C" fn aster_connection_transport_snapshot(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_snapshot: *mut aster_transport_snapshot_t,
) -> i32 {
    if out_snapshot.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let snap = conn_arc.transport_snapshot();

    let empty = aster_bytes_t {
        ptr: std::ptr::null(),
        len: 0,
    };
    let (path_kind, peer_host, peer_port, relay_url) = match (snap.peer_addr, snap.relay_url) {
        (Some(addr), _) => (
            aster_path_kind_t::ASTER_PATH_KIND_DIRECT as u32,
            alloc_string(addr.ip().to_string()),
            addr.port(),
            empty,
        ),
        (None, Some(url)) => (
            aster_path_kind_t::ASTER_PATH_KIND_RELAY as u32,
            empty,
            0u16,
            alloc_string(url),
        ),
        (None, None) => (
            aster_path_kind_t::ASTER_PATH_KIND_NONE as u32,
            empty,
            0u16,
            empty,
        ),
    };

    unsafe {
        *out_snapshot = aster_transport_snapshot_t {
            struct_size: std::mem::size_of::<aster_transport_snapshot_t>() as u32,
            path_kind,
            peer_host,
            peer_port,
            _pad: 0,
            relay_url,
            rtt_micros: snap.rtt_micros.unwrap_or(u64::MAX),
        };
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Hook Replies (Phase 1b)
// ============================================================================

/// Respond to a pending before_connect hook invocation.
///
/// `decision` is ASTER_HOOK_DECISION_ALLOW (0) or ASTER_HOOK_DECISION_DENY (1).
/// Calling this consumes the invocation; a second call returns NOT_FOUND.
#[no_mangle]
pub unsafe extern "C" fn aster_hook_before_connect_respond(
    runtime: aster_runtime_t,
    invocation: aster_hook_invocation_t,
    decision: u32,
) -> i32 {
    // `decision` is one of `aster_hook_decision_t`; taken as a raw integer so
    // an out-of-range value from C is rejected instead of being UB.
    let allow = match decision {
        x if x == aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32 => true,
        x if x == aster_hook_decision_t::ASTER_HOOK_DECISION_DENY as u32 => false,
        _ => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
    };
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let state = match bridge.hook_invocations.remove(invocation) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let sender = match state.sender.lock().ok().and_then(|mut g| g.take()) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    match sender {
        HookSender::BeforeConnect(tx) => {
            // Ignore send error — the hook may have timed out already.
            let _ = tx.send(allow);
            aster_status_t::ASTER_STATUS_OK as i32
        }
        HookSender::AfterConnect(_) => {
            // Wrong invocation type.
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        }
    }
}

/// Respond to a pending after_connect hook invocation (always accepts).
/// `aster_admission_respond` can also reject with a code and reason.
///
/// Calling this consumes the invocation; a second call returns NOT_FOUND.
#[no_mangle]
pub unsafe extern "C" fn aster_hook_after_connect_respond(
    runtime: aster_runtime_t,
    invocation: aster_hook_invocation_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let state = match bridge.hook_invocations.remove(invocation) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let sender = match state.sender.lock().ok().and_then(|mut g| g.take()) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    match sender {
        HookSender::AfterConnect(tx) => {
            let _ = tx.send(CoreAfterHandshakeDecision::Accept);
            aster_status_t::ASTER_STATUS_OK as i32
        }
        HookSender::BeforeConnect(_) => {
            // Wrong invocation type.
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        }
    }
}

// ============================================================================
// Cross-language contract identity, framing & signing
// ============================================================================
//
// Synchronous functions. No runtime handle required. No event queue.
// Pure computation: data in → result out → status code returned.

/// Helper: write `src` into caller-provided buffer `(out_buf, out_len)`.
///
/// On success sets `*out_len` to the number of bytes written and returns OK.
/// If the buffer is too small, sets `*out_len` to the required size and
/// returns BUFFER_TOO_SMALL.
///
/// Returns INVALID_ARGUMENT when any pointer is null.
unsafe fn write_to_caller_buf(src: &[u8], out_buf: *mut u8, out_len: *mut usize) -> aster_status_t {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT;
    }
    let capacity = *out_len;
    if src.len() > capacity || out_buf.is_null() {
        *out_len = src.len();
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(src.as_ptr(), out_buf, src.len());
    *out_len = src.len();
    aster_status_t::ASTER_STATUS_OK
}

// ============================================================================
// Phase 1g: Transport Metrics FFI
// ============================================================================

/// Transport-layer metrics snapshot from the iroh endpoint.
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct aster_transport_metrics_t {
    pub struct_size: u32,
    pub send_ipv4: u64,
    pub send_ipv6: u64,
    pub send_relay: u64,
    pub recv_data_ipv4: u64,
    pub recv_data_ipv6: u64,
    pub recv_data_relay: u64,
    pub recv_datagrams: u64,
    pub num_conns_direct: u64,
    pub num_conns_opened: u64,
    pub num_conns_closed: u64,
    pub paths_direct: u64,
    pub paths_relay: u64,
    pub holepunch_attempts: u64,
    pub relay_home_change: u64,
    pub net_reports: u64,
    pub net_reports_full: u64,
}

/// Snapshot current transport metrics from the iroh endpoint.
///
/// `endpoint_or_node` may be either an endpoint handle or a node handle.
/// Writes the metrics into `out_metrics`.
#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_transport_metrics(
    runtime: aster_runtime_t,
    endpoint_or_node: u64,
    out_metrics: *mut aster_transport_metrics_t,
) -> i32 {
    if out_metrics.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
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

    let client = match ep_arc {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let m = client.transport_metrics();
    let out = &mut *out_metrics;
    out.struct_size = std::mem::size_of::<aster_transport_metrics_t>() as u32;
    out.send_ipv4 = m.send_ipv4;
    out.send_ipv6 = m.send_ipv6;
    out.send_relay = m.send_relay;
    out.recv_data_ipv4 = m.recv_data_ipv4;
    out.recv_data_ipv6 = m.recv_data_ipv6;
    out.recv_data_relay = m.recv_data_relay;
    out.recv_datagrams = m.recv_datagrams;
    out.num_conns_direct = m.num_conns_direct;
    out.num_conns_opened = m.num_conns_opened;
    out.num_conns_closed = m.num_conns_closed;
    out.paths_direct = m.paths_direct;
    out.paths_relay = m.paths_relay;
    out.holepunch_attempts = m.holepunch_attempts;
    out.relay_home_change = m.relay_home_change;
    out.net_reports = m.net_reports;
    out.net_reports_full = m.net_reports_full;

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// Phase 1f: Cross-Language Contract Identity, Framing & Signing
// ============================================================================

/// Compute contract_id from a ServiceContract JSON string.
/// Writes 64-byte hex string (no null terminator) to out_buf.
/// On BUFFER_TOO_SMALL, sets `*out_len` to required size.
#[no_mangle]
pub unsafe extern "C" fn aster_contract_id(
    json_ptr: *const u8,
    json_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if json_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let json_bytes = slice::from_raw_parts(json_ptr, json_len);
    let json_str = match std::str::from_utf8(json_bytes) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid UTF-8 in JSON input: {e}")),
    };
    match aster_transport_core::contract::compute_contract_id_from_json(json_str) {
        Ok(hex_id) => write_to_caller_buf(hex_id.as_bytes(), out_buf, out_len) as i32,
        Err(e) => set_last_error(e),
    }
}

/// Compute canonical bytes for a named type from JSON.
/// `type_name`: `"ServiceContract"`, `"TypeDef"`, or `"MethodDef"`.
#[no_mangle]
pub unsafe extern "C" fn aster_canonical_bytes(
    type_name_ptr: *const u8,
    type_name_len: usize,
    json_ptr: *const u8,
    json_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if type_name_ptr.is_null() || json_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let type_name_bytes = slice::from_raw_parts(type_name_ptr, type_name_len);
    let type_name = match std::str::from_utf8(type_name_bytes) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid UTF-8 in type_name: {e}")),
    };
    let json_bytes = slice::from_raw_parts(json_ptr, json_len);
    let json_str = match std::str::from_utf8(json_bytes) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid UTF-8 in JSON input: {e}")),
    };
    match aster_transport_core::contract::canonical_bytes_from_json(type_name, json_str) {
        Ok(bytes) => write_to_caller_buf(&bytes, out_buf, out_len) as i32,
        Err(e) => set_last_error(e),
    }
}

/// BLAKE3 hash of arbitrary bytes → 64-char lowercase hex.
///
/// General-purpose primitive for bindings that need to hash canonical output from
/// [`aster_canonical_bytes`] (e.g. per-TypeDef hashing when assembling a
/// [`ServiceContract`](aster_transport_core::contract::ServiceContract) for
/// [`aster_contract_id`]). Mirrors what PyO3 already exposes to Python as
/// `_native.contract.compute_type_hash`.
///
/// Writes 64 bytes of hex to `out_buf`. On `BUFFER_TOO_SMALL`, sets `*out_len` to
/// the required size (64). `bytes_len == 0` is valid and hashes the empty input.
#[no_mangle]
pub unsafe extern "C" fn aster_blake3_hex(
    bytes_ptr: *const u8,
    bytes_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    if bytes_ptr.is_null() && bytes_len != 0 {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let input = if bytes_len == 0 {
        &[][..]
    } else {
        slice::from_raw_parts(bytes_ptr, bytes_len)
    };
    let hex = hex::encode(aster_transport_core::contract::compute_type_hash(input));
    write_to_caller_buf(hex.as_bytes(), out_buf, out_len) as i32
}

/// Compute canonical signing bytes from credential JSON.
///
/// The JSON must contain a `"kind"` field (`"producer"` or `"consumer"`)
/// to dispatch to the correct signing bytes format.
#[no_mangle]
pub unsafe extern "C" fn aster_signing_bytes(
    json_ptr: *const u8,
    json_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if json_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let json_bytes = slice::from_raw_parts(json_ptr, json_len);
    let json_str = match std::str::from_utf8(json_bytes) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid UTF-8 in JSON input: {e}")),
    };
    match aster_transport_core::signing::canonical_signing_bytes_from_json(json_str) {
        Ok(bytes) => write_to_caller_buf(&bytes, out_buf, out_len) as i32,
        Err(e) => set_last_error(e),
    }
}

/// Canonical JSON normalization: parse, sort all keys recursively, re-serialize compact.
///
/// This is a general-purpose canonical form — not specific to credentials or contracts.
/// Useful for computing deterministic hashes of arbitrary JSON payloads.
#[no_mangle]
pub unsafe extern "C" fn aster_canonical_json(
    json_ptr: *const u8,
    json_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if json_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let json_bytes = slice::from_raw_parts(json_ptr, json_len);
    let json_str = match std::str::from_utf8(json_bytes) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid UTF-8 in JSON input: {e}")),
    };
    // Parse into serde_json::Value, which uses BTreeMap for objects (sorted keys).
    // Then re-serialize compact.
    let value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return set_last_error(format!("invalid JSON: {e}")),
    };
    let canonical = sort_json_value(&value);
    let output = match serde_json::to_string(&canonical) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("JSON re-serialization failed: {e}")),
    };
    write_to_caller_buf(output.as_bytes(), out_buf, out_len) as i32
}

// ============================================================================
// AsterTicket — compact ticket encode/decode
// ============================================================================

/// Encode an AsterTicket to a base58 string (``aster1<base58>``).
///
/// # Parameters
/// - `endpoint_id_hex`: 64-char hex endpoint ID
/// - `relay_addr_ptr/len`: relay "ip:port" string (NULL/0 for none)
/// - `direct_addrs_json_ptr/len`: JSON array of "ip:port" strings (NULL/0 for none)
/// - `credential_type_ptr/len`: credential type string: "open", "consumer_rcan",
///   "enrollment", "registry_read" (NULL/0 for none)
/// - `credential_data_ptr/len`: credential payload bytes (NULL/0 for none)
/// - `out_buf/out_len`: output buffer for the aster1... string
#[no_mangle]
pub unsafe extern "C" fn aster_ticket_encode(
    endpoint_id_hex_ptr: *const u8,
    endpoint_id_hex_len: usize,
    relay_addr_ptr: *const u8,
    relay_addr_len: usize,
    direct_addrs_json_ptr: *const u8,
    direct_addrs_json_len: usize,
    credential_type_ptr: *const u8,
    credential_type_len: usize,
    credential_data_ptr: *const u8,
    credential_data_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if endpoint_id_hex_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let hex_str = match std::str::from_utf8(slice::from_raw_parts(
        endpoint_id_hex_ptr,
        endpoint_id_hex_len,
    )) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid endpoint_id UTF-8: {e}")),
    };
    let id_bytes = match hex::decode(hex_str) {
        Ok(b) => b,
        Err(e) => return set_last_error(format!("invalid endpoint_id hex: {e}")),
    };
    if id_bytes.len() != 32 {
        return set_last_error("endpoint_id must be 32 bytes (64 hex chars)");
    }
    let mut endpoint_id = [0u8; 32];
    endpoint_id.copy_from_slice(&id_bytes);

    let relay: Option<std::net::SocketAddr> = if !relay_addr_ptr.is_null() && relay_addr_len > 0 {
        let s = match std::str::from_utf8(slice::from_raw_parts(relay_addr_ptr, relay_addr_len)) {
            Ok(s) => s,
            Err(e) => return set_last_error(format!("invalid relay addr UTF-8: {e}")),
        };
        match s.parse() {
            Ok(a) => Some(a),
            Err(e) => return set_last_error(format!("invalid relay addr: {e}")),
        }
    } else {
        None
    };

    let direct_addrs: Vec<std::net::SocketAddr> =
        if !direct_addrs_json_ptr.is_null() && direct_addrs_json_len > 0 {
            let s = match std::str::from_utf8(slice::from_raw_parts(
                direct_addrs_json_ptr,
                direct_addrs_json_len,
            )) {
                Ok(s) => s,
                Err(e) => return set_last_error(format!("invalid direct_addrs UTF-8: {e}")),
            };
            let arr: Vec<String> = match serde_json::from_str(s) {
                Ok(a) => a,
                Err(e) => return set_last_error(format!("invalid direct_addrs JSON: {e}")),
            };
            let mut addrs = Vec::with_capacity(arr.len());
            for a in &arr {
                match a.parse() {
                    Ok(sa) => addrs.push(sa),
                    Err(e) => return set_last_error(format!("bad direct addr '{}': {e}", a)),
                }
            }
            addrs
        } else {
            vec![]
        };

    let credential = if !credential_type_ptr.is_null() && credential_type_len > 0 {
        let ctype = match std::str::from_utf8(slice::from_raw_parts(
            credential_type_ptr,
            credential_type_len,
        )) {
            Ok(s) => s,
            Err(e) => return set_last_error(format!("invalid credential_type UTF-8: {e}")),
        };
        let cdata = if !credential_data_ptr.is_null() && credential_data_len > 0 {
            slice::from_raw_parts(credential_data_ptr, credential_data_len).to_vec()
        } else {
            vec![]
        };
        use aster_transport_core::ticket::TicketCredential;
        match ctype {
            "open" => Some(TicketCredential::Open),
            "consumer_rcan" => Some(TicketCredential::ConsumerRcan(cdata)),
            "enrollment" => Some(TicketCredential::Enrollment(cdata)),
            "registry_read" => {
                if cdata.len() != 32 {
                    return set_last_error("registry_read credential requires exactly 32 bytes");
                }
                let mut ns = [0u8; 32];
                ns.copy_from_slice(&cdata[..32]);
                Some(TicketCredential::RegistryRead(ns))
            }
            _ => return set_last_error(format!("unknown credential type '{ctype}'")),
        }
    } else {
        None
    };

    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id,
        relay,
        direct_addrs,
        credential,
    };

    match ticket.to_base58_string() {
        Ok(s) => write_to_caller_buf(s.as_bytes(), out_buf, out_len) as i32,
        Err(e) => set_last_error(e),
    }
}

/// Decode an AsterTicket from a base58 string (``aster1<base58>``).
///
/// Writes a JSON object to `out_buf`:
/// ```json
/// {
///   "endpoint_id": "hex...",
///   "relay_addr": "ip:port" | null,
///   "direct_addrs": ["ip:port", ...],
///   "credential_type": "open" | "consumer_rcan" | "enrollment" | "registry_read" | null,
///   "credential_data_hex": "hex..." | null
/// }
/// ```
#[no_mangle]
pub unsafe extern "C" fn aster_ticket_decode(
    ticket_ptr: *const u8,
    ticket_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if ticket_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let ticket_str = match std::str::from_utf8(slice::from_raw_parts(ticket_ptr, ticket_len)) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid ticket UTF-8: {e}")),
    };
    let ticket = match aster_transport_core::ticket::AsterTicket::from_base58_str(ticket_str) {
        Ok(t) => t,
        Err(e) => return set_last_error(format!("ticket decode failed: {e}")),
    };

    use aster_transport_core::ticket::TicketCredential;
    let (cred_type, cred_hex): (Option<&str>, Option<String>) = match &ticket.credential {
        None => (None, None),
        Some(TicketCredential::Open) => (Some("open"), None),
        Some(TicketCredential::ConsumerRcan(v)) => (Some("consumer_rcan"), Some(hex::encode(v))),
        Some(TicketCredential::Enrollment(v)) => (Some("enrollment"), Some(hex::encode(v))),
        Some(TicketCredential::RegistryRead(namespace_id)) => {
            (Some("registry_read"), Some(hex::encode(namespace_id)))
        }
    };

    let json = serde_json::json!({
        "endpoint_id": hex::encode(ticket.endpoint_id),
        "relay_addr": ticket.relay.map(|a| a.to_string()),
        "direct_addrs": ticket.direct_addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        "credential_type": cred_type,
        "credential_data_hex": cred_hex,
    });

    let output = json.to_string();
    write_to_caller_buf(output.as_bytes(), out_buf, out_len) as i32
}

/// Recursively sort JSON object keys for canonical form.
fn sort_json_value(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let sorted: serde_json::Map<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), sort_json_value(v)))
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .collect();
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_json_value).collect()),
        other => other.clone(),
    }
}

// ─── Public Soak Test API ───────────────────────────────────────────────

/// Run the soak test for the specified duration in seconds.
///
/// This is exposed publicly so the `soak_test_runner` binary can invoke it
/// without duplicating the test logic.
pub fn run_soak_test(duration_secs: u64) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // ─── Metrics ────────────────────────────────────────────────────────────
    struct SoakMetrics {
        cycles_completed: AtomicUsize,
        ops_submitted: AtomicUsize,
        ops_completed: AtomicUsize,
        ops_cancelled: AtomicUsize,
        ops_errored: AtomicUsize,
        max_pending_ops: AtomicUsize,
        current_pending_ops: AtomicUsize,
    }

    impl SoakMetrics {
        fn new() -> Self {
            Self {
                cycles_completed: AtomicUsize::new(0),
                ops_submitted: AtomicUsize::new(0),
                ops_completed: AtomicUsize::new(0),
                ops_cancelled: AtomicUsize::new(0),
                ops_errored: AtomicUsize::new(0),
                max_pending_ops: AtomicUsize::new(0),
                current_pending_ops: AtomicUsize::new(0),
            }
        }

        fn record_submit(&self) {
            self.ops_submitted.fetch_add(1, Ordering::Relaxed);
            let pending = self.current_pending_ops.fetch_add(1, Ordering::Relaxed) + 1;
            loop {
                let current_max = self.max_pending_ops.load(Ordering::Relaxed);
                if pending <= current_max {
                    break;
                }
                if self
                    .max_pending_ops
                    .compare_exchange(current_max, pending, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            }
        }

        fn record_complete(&self) {
            self.ops_completed.fetch_add(1, Ordering::Relaxed);
            self.current_pending_ops.fetch_sub(1, Ordering::Relaxed);
        }

        fn record_error(&self) {
            self.ops_errored.fetch_add(1, Ordering::Relaxed);
            self.current_pending_ops.fetch_sub(1, Ordering::Relaxed);
        }

        fn record_cycle(&self) {
            self.cycles_completed.fetch_add(1, Ordering::Relaxed);
        }
    }

    // ─── Helpers ─────────────────────────────────────────────────────────────

    fn rand_u8() -> u8 {
        let nanos = std::time::Instant::now().elapsed().as_nanos();
        nanos as u8
    }

    fn drain_all_events(runtime: aster_runtime_t) {
        loop {
            let mut events = unsafe { [std::mem::zeroed::<aster_event_t>(); 16] };
            let count = unsafe { aster_poll_events(runtime, events.as_mut_ptr(), 16, 0) };
            if count == 0 {
                break;
            }
        }
    }

    fn poll_for_event(runtime: aster_runtime_t, kind: aster_event_kind_t, timeout_ms: u32) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            if Instant::now() >= deadline {
                return false;
            }
            let mut events = unsafe { [std::mem::zeroed::<aster_event_t>(); 4] };
            let count = unsafe { aster_poll_events(runtime, events.as_mut_ptr(), 4, 50) };
            for ev in events.iter().take(count) {
                if ev.kind == kind as u32 {
                    return true;
                }
            }
        }
    }

    fn run_soak_cycle(runtime: aster_runtime_t, metrics: &Arc<SoakMetrics>) -> bool {
        drain_all_events(runtime);

        let alpns = [b"aster".as_ptr()];
        let alpn_lens = [5];

        let mut node_op: aster_operation_t = 0;
        let status = unsafe {
            aster_node_memory_with_alpns(
                runtime,
                alpns.as_ptr(),
                alpn_lens.as_ptr(),
                1,
                0,
                &mut node_op,
            )
        };
        if status != aster_status_t::ASTER_STATUS_OK as i32 {
            return false;
        }

        let node_created =
            poll_for_event(runtime, aster_event_kind_t::ASTER_EVENT_NODE_CREATED, 2000);
        if !node_created {
            return false;
        }

        let mut accept_op: aster_operation_t = 0;
        let status = unsafe { aster_node_accept_aster(runtime, 1, 0, &mut accept_op) };
        if status != aster_status_t::ASTER_STATUS_OK as i32 {
            return false;
        }
        metrics.record_submit();

        let should_cancel = rand_u8() < 25;
        if should_cancel {
            std::thread::sleep(Duration::from_millis(5));
            let _ = unsafe { aster_operation_cancel(runtime, accept_op) };
        }

        loop {
            let mut events = unsafe { [std::mem::zeroed::<aster_event_t>(); 8] };
            let count = unsafe { aster_poll_events(runtime, events.as_mut_ptr(), 8, 100) };
            if count == 0 {
                break;
            }
            for ev in events.iter().take(count).copied() {
                if ev.operation == accept_op {
                    if ev.status == aster_status_t::ASTER_STATUS_OK as u32 {
                        metrics.record_complete();
                    } else {
                        metrics.record_error();
                    }
                }
            }
        }

        let mut close_op: aster_operation_t = 0;
        let status = unsafe { aster_node_close(runtime, 1, 0, &mut close_op) };
        if status == aster_status_t::ASTER_STATUS_OK as i32 {
            let _closed = poll_for_event(runtime, aster_event_kind_t::ASTER_EVENT_CLOSED, 2000);
        }

        metrics.record_cycle();
        true
    }

    // ─── Main ────────────────────────────────────────────────────────────────

    println!(
        "Starting soak test for {} seconds ({} hours)",
        duration_secs,
        duration_secs / 3600
    );
    println!("Churn pattern: node_create → accept → cancel/close → node_close (10% cancel rate)");
    println!();

    let start = Instant::now();
    let deadline = start + Duration::from_secs(duration_secs);

    let mut runtime: aster_runtime_t = 0;
    let status =
        unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, std::ptr::null(), &mut runtime) };
    assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

    let metrics = Arc::new(SoakMetrics::new());
    let mut cycle_count = 0usize;
    let print_interval = 100;

    while Instant::now() < deadline {
        let cycle_start = Instant::now();

        let success = run_soak_cycle(runtime, &metrics);

        let _cycle_duration = cycle_start.elapsed();
        cycle_count += 1;

        if cycle_count.is_multiple_of(print_interval) || !success {
            let elapsed = start.elapsed();
            let ops_sub = metrics.ops_submitted.load(Ordering::Relaxed);
            let ops_comp = metrics.ops_completed.load(Ordering::Relaxed);
            let ops_cancel = metrics.ops_cancelled.load(Ordering::Relaxed);
            let ops_err = metrics.ops_errored.load(Ordering::Relaxed);
            let max_pending = metrics.max_pending_ops.load(Ordering::Relaxed);
            let current_pending = metrics.current_pending_ops.load(Ordering::Relaxed);

            println!(
                "[{:?}] cycle {} — submitted={}, completed={}, cancelled={}, errored={}, max_pending={}, current_pending={}",
                elapsed,
                cycle_count,
                ops_sub,
                ops_comp,
                ops_cancel,
                ops_err,
                max_pending,
                current_pending
            );
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    let elapsed = start.elapsed();

    let ops_sub = metrics.ops_submitted.load(Ordering::Relaxed);
    let ops_comp = metrics.ops_completed.load(Ordering::Relaxed);
    let ops_cancel = metrics.ops_cancelled.load(Ordering::Relaxed);
    let ops_err = metrics.ops_errored.load(Ordering::Relaxed);
    let max_pending = metrics.max_pending_ops.load(Ordering::Relaxed);
    let final_pending = metrics.current_pending_ops.load(Ordering::Relaxed);

    println!();
    println!("=== Soak Test Results ===");
    println!("Duration: {:?}", elapsed);
    println!("Cycles: {}", cycle_count);
    println!("Ops submitted: {}", ops_sub);
    println!("Ops completed: {}", ops_comp);
    println!("Ops cancelled: {}", ops_cancel);
    println!("Ops errored: {}", ops_err);
    println!("Max pending ops: {}", max_pending);
    println!("Final pending ops: {}", final_pending);
    println!();

    assert_eq!(
        final_pending, 0,
        "Final pending ops should be 0 (leaked ops detected)"
    );
    assert!(
        max_pending < 100,
        "Max pending ops should be bounded (was {})",
        max_pending
    );

    let status = unsafe { aster_runtime_close(runtime) };
    assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

    println!("Soak test PASSED — no leaks detected");
}

// ============================================================================
// Registry FFI — centralized filter + rank logic (§11.9)
//
// These are synchronous pure-function FFI entry points that take JSON input
// and return JSON output. All language bindings use their existing doc-read
// FFI to fetch lease entries, then call into this layer to apply the
// mandatory filters and ranking strategy, so there is exactly one copy of the
// resolution logic across all five bindings.
//
// Async doc-read + publish integrations are available on the Rust core
// (`core::registry::resolve`, `publish_lease`, `publish_artifact`,
// `renew_lease`) and will be exposed through the event-based FFI model in a
// follow-up pass.
// ============================================================================

/// Return the current wall-clock epoch-millis as seen by the Rust runtime.
/// Single shared clock across all bindings prevents time-skew in lease freshness checks.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_now_epoch_ms() -> i64 {
    aster_transport_core::registry::now_epoch_ms()
}

/// Report whether a lease is still fresh given the lease duration window.
/// `lease_json` is a single EndpointLease JSON object.
/// Returns 1 = fresh, 0 = expired, negative = error (see set_last_error).
#[no_mangle]
pub unsafe extern "C" fn aster_registry_is_fresh(
    lease_json_ptr: *const u8,
    lease_json_len: usize,
    lease_duration_s: i32,
) -> i32 {
    if lease_json_ptr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bytes = slice::from_raw_parts(lease_json_ptr, lease_json_len);
    let lease: aster_transport_core::registry::EndpointLease = match serde_json::from_slice(bytes) {
        Ok(l) => l,
        Err(e) => return set_last_error(format!("invalid EndpointLease JSON: {e}")),
    };
    if lease.is_fresh(lease_duration_s) {
        1
    } else {
        0
    }
}

/// Report whether a health status string is routable (READY or DEGRADED).
/// `status_json` is a UTF-8 string (not JSON-quoted).
#[no_mangle]
pub unsafe extern "C" fn aster_registry_is_routable(
    status_ptr: *const u8,
    status_len: usize,
) -> i32 {
    if status_ptr.is_null() {
        return 0;
    }
    let bytes = slice::from_raw_parts(status_ptr, status_len);
    match std::str::from_utf8(bytes) {
        Ok(s) => {
            if aster_transport_core::registry::is_routable(s) {
                1
            } else {
                0
            }
        }
        Err(_) => 0,
    }
}

/// Apply mandatory filters + ranking to a list of EndpointLease JSON objects.
///
/// Input:
/// - `leases_json`: a JSON array of EndpointLease objects.
/// - `opts_json`: a ResolveOptions JSON object with fields:
///   service (string), version (int|null), channel (string|null),
///   contract_id (string|null), strategy (string), caller_alpn (string),
///   caller_serialization_modes (array), caller_policy_realm (string|null),
///   lease_duration_s (int).
///
/// Output (written to `out_buf`): a JSON array of EndpointLease objects in
/// ranked order. The top element is the resolved winner. If the output buffer
/// is too small, returns `BUFFER_TOO_SMALL` and sets `*out_len` to the
/// required size.
///
/// Note: the round-robin rotation state is reset on every call (stateless).
/// Stateful multi-call round-robin will be added when the async resolve FFI
/// lands; for now, bindings should maintain their own rotation counter or
/// accept per-call randomization.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_filter_and_rank(
    leases_json_ptr: *const u8,
    leases_json_len: usize,
    opts_json_ptr: *const u8,
    opts_json_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if leases_json_ptr.is_null() || opts_json_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let leases_bytes = slice::from_raw_parts(leases_json_ptr, leases_json_len);
    let opts_bytes = slice::from_raw_parts(opts_json_ptr, opts_json_len);

    let leases: Vec<aster_transport_core::registry::EndpointLease> =
        match serde_json::from_slice(leases_bytes) {
            Ok(v) => v,
            Err(e) => return set_last_error(format!("invalid leases JSON: {e}")),
        };

    let opts_val: serde_json::Value = match serde_json::from_slice(opts_bytes) {
        Ok(v) => v,
        Err(e) => return set_last_error(format!("invalid ResolveOptions JSON: {e}")),
    };
    let obj = match opts_val.as_object() {
        Some(o) => o,
        None => return set_last_error("ResolveOptions must be a JSON object"),
    };
    let get_str = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(String::from);
    let get_i32 = |k: &str| obj.get(k).and_then(|v| v.as_i64()).map(|v| v as i32);
    let get_str_list = |k: &str| {
        obj.get(k)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.as_str().map(String::from))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default()
    };
    let opts = aster_transport_core::registry::ResolveOptions {
        service: get_str("service").unwrap_or_default(),
        version: get_i32("version"),
        channel: get_str("channel"),
        contract_id: get_str("contract_id"),
        strategy: get_str("strategy").unwrap_or_else(|| "round_robin".to_string()),
        caller_alpn: get_str("caller_alpn").unwrap_or_else(|| "aster/1".to_string()),
        caller_serialization_modes: {
            let v = get_str_list("caller_serialization_modes");
            if v.is_empty() {
                vec!["fory-xlang".to_string()]
            } else {
                v
            }
        },
        caller_policy_realm: get_str("caller_policy_realm"),
        lease_duration_s: get_i32("lease_duration_s").unwrap_or(45),
    };

    let filtered = aster_transport_core::registry::apply_mandatory_filters(leases, &opts);
    let state = aster_transport_core::registry::ResolveState::new();
    let cid = opts.contract_id.clone().unwrap_or_else(|| "_".to_string());
    let ranked = state.rank(filtered, &opts.strategy, &cid);

    match serde_json::to_vec(&ranked) {
        Ok(bytes) => write_to_caller_buf(&bytes, out_buf, out_len) as i32,
        Err(e) => set_last_error(format!("failed to encode ranked leases: {e}")),
    }
}

/// Return one of the registry key-schema strings by kind, for all bindings to share.
///
/// `kind`: one of `aster_registry_key_kind_t`:
///   0 = contract_key(arg1)
///   1 = version_key(arg1, arg2 as int)
///   2 = channel_key(arg1, arg2)
///   3 = lease_key(arg1, arg2, arg3)
///   4 = lease_prefix(arg1, arg2)
///   5 = acl_key(arg1)
///
/// `arg1/arg2/arg3` are UTF-8 strings. For version_key, arg2 must parse as i32.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_key(
    kind: i32,
    arg1_ptr: *const u8,
    arg1_len: usize,
    arg2_ptr: *const u8,
    arg2_len: usize,
    arg3_ptr: *const u8,
    arg3_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let read = |ptr: *const u8, len: usize| -> Result<&str, i32> {
        if ptr.is_null() && len != 0 {
            return Err(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);
        }
        let s = if len == 0 {
            ""
        } else {
            match std::str::from_utf8(slice::from_raw_parts(ptr, len)) {
                Ok(s) => s,
                Err(e) => return Err(set_last_error(format!("invalid UTF-8: {e}"))),
            }
        };
        Ok(s)
    };
    let a1 = match read(arg1_ptr, arg1_len) {
        Ok(s) => s,
        Err(s) => return s,
    };
    let a2 = match read(arg2_ptr, arg2_len) {
        Ok(s) => s,
        Err(s) => return s,
    };
    let a3 = match read(arg3_ptr, arg3_len) {
        Ok(s) => s,
        Err(s) => return s,
    };

    let key_bytes: Vec<u8> = match kind {
        0 => aster_transport_core::registry::contract_key(a1),
        1 => {
            let v: i32 = match a2.parse() {
                Ok(n) => n,
                Err(e) => return set_last_error(format!("version must be int: {e}")),
            };
            aster_transport_core::registry::version_key(a1, v)
        }
        2 => aster_transport_core::registry::channel_key(a1, a2),
        3 => aster_transport_core::registry::lease_key(a1, a2, a3),
        4 => aster_transport_core::registry::lease_prefix(a1, a2),
        5 => aster_transport_core::registry::acl_key(a1),
        _ => return set_last_error(format!("unknown key kind: {kind}")),
    };

    write_to_caller_buf(&key_bytes, out_buf, out_len) as i32
}

// ============================================================================
// Registry FFI — async doc-backed operations (§11.8 / §11.9)
//
// These operations follow the standard event-based FFI model:
//   1. Caller invokes the function, gets back an operation handle.
//   2. The op runs on the bridge tokio runtime.
//   3. On completion the bridge emits an event which the caller drains via
//      `aster_event_recv`. The event payload (when present) is JSON.
//
// Round-robin rotation and stale-seq rejection are persistent across calls
// because all ops share `bridge.registry_state`. Per-doc ACLs are stored on
// the bridge keyed by doc handle and survive until the doc is freed.
// ============================================================================

fn parse_resolve_options_json(
    bytes: &[u8],
) -> Result<aster_transport_core::registry::ResolveOptions, String> {
    let val: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid ResolveOptions JSON: {e}"))?;
    let obj = val
        .as_object()
        .ok_or_else(|| "ResolveOptions must be a JSON object".to_string())?;
    let get_str = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(String::from);
    let get_i32 = |k: &str| obj.get(k).and_then(|v| v.as_i64()).map(|v| v as i32);
    let get_str_list = |k: &str| {
        obj.get(k)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.as_str().map(String::from))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default()
    };
    Ok(aster_transport_core::registry::ResolveOptions {
        service: get_str("service").unwrap_or_default(),
        version: get_i32("version"),
        channel: get_str("channel"),
        contract_id: get_str("contract_id"),
        strategy: get_str("strategy").unwrap_or_else(|| "round_robin".to_string()),
        caller_alpn: get_str("caller_alpn").unwrap_or_else(|| "aster/1".to_string()),
        caller_serialization_modes: {
            let v = get_str_list("caller_serialization_modes");
            if v.is_empty() {
                vec!["fory-xlang".to_string()]
            } else {
                v
            }
        },
        caller_policy_realm: get_str("caller_policy_realm"),
        lease_duration_s: get_i32("lease_duration_s").unwrap_or(45),
    })
}

/// Async resolve: pointer lookup → list_leases → seq filter → mandatory filters → rank.
///
/// Inputs:
/// - `doc`: a doc handle previously returned by `aster_docs_create` / `aster_docs_join`.
/// - `opts_json`: ResolveOptions JSON (same shape as `aster_registry_filter_and_rank`).
///
/// On success emits `ASTER_EVENT_REGISTRY_RESOLVED`. The event payload is the JSON
/// of the winning EndpointLease, or empty with `ASTER_STATUS_NOT_FOUND` if no
/// candidate survived the filters.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_resolve(
    runtime: aster_runtime_t,
    doc: u64,
    opts_json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let opts_bytes = unsafe { read_bytes(&opts_json) };
    let opts = match parse_resolve_options_json(&opts_bytes) {
        Ok(o) => o,
        Err(e) => return set_last_error(e),
    };
    let acl = bridge.registry_acl_lookup(doc);

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let result = aster_transport_core::registry::resolve(
            &doc_arc,
            &bridge2.registry_state,
            &opts,
            acl.as_deref(),
        )
        .await;

        match result {
            Ok(Some(lease)) => match serde_json::to_vec(&lease) {
                Ok(bytes) => {
                    let event = EventInternal::new(
                        aster_event_kind_t::ASTER_EVENT_REGISTRY_RESOLVED,
                        aster_status_t::ASTER_STATUS_OK,
                        op_id,
                        doc,
                        0,
                        user_data,
                        0,
                    );
                    bridge2.emit_with_data(event, bytes);
                }
                Err(e) => {
                    bridge2.emit_error(op_id, user_data, &format!("encode lease: {e}"));
                }
            },
            Ok(None) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_REGISTRY_RESOLVED,
                    aster_status_t::ASTER_STATUS_NOT_FOUND,
                    op_id,
                    doc,
                    0,
                    user_data,
                    aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Publish a lease and/or an artifact in a single op.
///
/// Either or both of `lease_json` / `artifact_json` may be provided (pass an
/// empty `aster_bytes_t` to skip). For artifact publication, `service` and
/// `version` must be set; `channel` is optional. `gossip_topic` is a topic
/// handle to broadcast the corresponding GossipEvent on, or 0 to skip.
///
/// Emits `ASTER_EVENT_REGISTRY_PUBLISHED` on success.
#[allow(clippy::too_many_arguments)]
#[no_mangle]
pub unsafe extern "C" fn aster_registry_publish(
    runtime: aster_runtime_t,
    doc: u64,
    author_id: aster_bytes_t,
    lease_json: aster_bytes_t,
    artifact_json: aster_bytes_t,
    service: aster_bytes_t,
    version: i32,
    channel: aster_bytes_t,
    gossip_topic: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let lease_bytes = unsafe { read_bytes(&lease_json) };
    let artifact_bytes = unsafe { read_bytes(&artifact_json) };

    let lease: Option<aster_transport_core::registry::EndpointLease> = if lease_bytes.is_empty() {
        None
    } else {
        match serde_json::from_slice(&lease_bytes) {
            Ok(l) => Some(l),
            Err(e) => return set_last_error(format!("invalid EndpointLease JSON: {e}")),
        }
    };

    let artifact: Option<aster_transport_core::registry::ArtifactRef> = if artifact_bytes.is_empty()
    {
        None
    } else {
        match serde_json::from_slice(&artifact_bytes) {
            Ok(a) => Some(a),
            Err(e) => return set_last_error(format!("invalid ArtifactRef JSON: {e}")),
        }
    };

    if lease.is_none() && artifact.is_none() {
        return set_last_error("aster_registry_publish: both lease and artifact are empty");
    }

    let service_str = match unsafe { read_string(&service) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let channel_str = match unsafe { read_bytes_opt(&channel) } {
        Some(b) => match String::from_utf8(b) {
            Ok(s) => Some(s),
            Err(e) => return set_last_error(format!("invalid channel UTF-8: {e}")),
        },
        None => None,
    };

    let topic = if gossip_topic == 0 {
        None
    } else {
        bridge.gossip_topics.get(gossip_topic)
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

        if let Some(ref lease) = lease {
            if let Err(e) = aster_transport_core::registry::publish_lease(
                &doc_arc,
                &author,
                lease,
                topic.as_deref(),
            )
            .await
            {
                bridge2.emit_error(op_id, user_data, &e.to_string());
                return;
            }
        }

        if let Some(ref artifact) = artifact {
            if let Err(e) = aster_transport_core::registry::publish_artifact(
                &doc_arc,
                &author,
                artifact,
                &service_str,
                version,
                channel_str.as_deref(),
                topic.as_deref(),
            )
            .await
            {
                bridge2.emit_error(op_id, user_data, &e.to_string());
                return;
            }
        }

        bridge2.emit_simple(
            aster_event_kind_t::ASTER_EVENT_REGISTRY_PUBLISHED,
            aster_status_t::ASTER_STATUS_OK,
            op_id,
            doc,
            gossip_topic,
            user_data,
            0,
        );
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Renew an existing lease in place. Reads the current row, bumps lease_seq +
/// timestamps, updates health/load, rewrites it.
///
/// `load` uses NaN as a sentinel for "no load reported".
/// Emits `ASTER_EVENT_REGISTRY_RENEWED` on success.
#[allow(clippy::too_many_arguments)]
#[no_mangle]
pub unsafe extern "C" fn aster_registry_renew_lease(
    runtime: aster_runtime_t,
    doc: u64,
    author_id: aster_bytes_t,
    service: aster_bytes_t,
    contract_id: aster_bytes_t,
    endpoint_id: aster_bytes_t,
    health: aster_bytes_t,
    load: f32,
    lease_duration_s: i32,
    gossip_topic: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let service_str = match unsafe { read_string(&service) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let cid = match unsafe { read_string(&contract_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let eid = match unsafe { read_string(&endpoint_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let health_str = match unsafe { read_string(&health) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let load_opt = if load.is_nan() { None } else { Some(load) };

    let topic = if gossip_topic == 0 {
        None
    } else {
        bridge.gossip_topics.get(gossip_topic)
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

        match aster_transport_core::registry::renew_lease(
            &doc_arc,
            &author,
            &service_str,
            &cid,
            &eid,
            &health_str,
            load_opt,
            lease_duration_s,
            topic.as_deref(),
        )
        .await
        {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_REGISTRY_RENEWED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    gossip_topic,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Add an author to the per-doc registry ACL writer set, persisting it to the
/// doc under `_aster/acl/writers`. Switches the ACL out of open mode if it was
/// in open mode. Emits `ASTER_EVENT_REGISTRY_ACL_UPDATED` on success.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_acl_add_writer(
    runtime: aster_runtime_t,
    doc: u64,
    author_id: aster_bytes_t,
    writer_id: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    aster_registry_acl_mutate_writer(
        runtime,
        doc,
        author_id,
        writer_id,
        user_data,
        out_operation,
        true,
    )
}

/// Remove an author from the per-doc registry ACL writer set and persist the
/// updated list. Emits `ASTER_EVENT_REGISTRY_ACL_UPDATED` on success.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_acl_remove_writer(
    runtime: aster_runtime_t,
    doc: u64,
    author_id: aster_bytes_t,
    writer_id: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    aster_registry_acl_mutate_writer(
        runtime,
        doc,
        author_id,
        writer_id,
        user_data,
        out_operation,
        false,
    )
}

unsafe fn aster_registry_acl_mutate_writer(
    runtime: aster_runtime_t,
    doc: u64,
    author_id: aster_bytes_t,
    writer_id: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
    add: bool,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let writer = match unsafe { read_string(&writer_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let acl = bridge.registry_acl_for_doc(doc);

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let result = if add {
            acl.add_writer(&doc_arc, &author, &writer).await
        } else {
            acl.remove_writer(&doc_arc, &author, &writer).await
        };

        match result {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_REGISTRY_ACL_UPDATED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
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

    aster_status_t::ASTER_STATUS_OK as i32
}

/// List the current writer set for the per-doc registry ACL.
///
/// Emits `ASTER_EVENT_REGISTRY_ACL_LISTED`. The event payload is a JSON array
/// of AuthorId strings. If the ACL is in open mode (no writers added yet),
/// the array is empty.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_acl_list_writers(
    runtime: aster_runtime_t,
    doc: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.docs.get(doc).is_none() {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    }

    let acl = bridge.registry_acl_for_doc(doc);

    let (op_id, cancelled) = bridge.new_operation();
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let writers = acl.writers();
        match serde_json::to_vec(&writers) {
            Ok(bytes) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_REGISTRY_ACL_LISTED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, bytes);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &format!("encode writers: {e}"));
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[cfg(test)]
mod string_release_tests {
    use super::*;

    unsafe fn view(b: &aster_bytes_t) -> &[u8] {
        std::slice::from_raw_parts(b.ptr, b.len)
    }

    #[test]
    fn alloc_string_round_trips_and_is_nul_terminated() {
        // Lengths chosen so that a naive `Vec::push(0)` would have grown the
        // capacity past `len + 1` (powers of two), which the old
        // `Vec::from_raw_parts(ptr, len, len)` release mis-sized.
        for n in [1usize, 7, 8, 15, 16, 31, 32, 64, 1000, 4096] {
            let s = "x".repeat(n);
            let b = alloc_string(s.clone());
            assert!(!b.ptr.is_null());
            assert_eq!(b.len, n);
            unsafe {
                assert_eq!(view(&b), s.as_bytes());
                assert_eq!(*b.ptr.add(n), 0, "missing NUL terminator");
                assert_eq!(
                    aster_string_release(b.ptr, b.len),
                    aster_status_t::ASTER_STATUS_OK as i32
                );
            }
        }
    }

    #[test]
    fn alloc_bytes_from_over_capacity_vec_round_trips() {
        let mut v = Vec::with_capacity(4096);
        v.extend_from_slice(b"h3");
        let b = alloc_bytes(v);
        assert_eq!(b.len, 2);
        unsafe {
            assert_eq!(view(&b), b"h3");
            assert_eq!(
                aster_string_release(b.ptr, b.len),
                aster_status_t::ASTER_STATUS_OK as i32
            );
        }
    }

    #[test]
    fn empty_buffers_do_not_allocate_and_release_is_noop() {
        let s = alloc_string(String::new());
        assert!(s.ptr.is_null());
        assert_eq!(s.len, 0);
        let b = alloc_bytes(Vec::new());
        assert!(b.ptr.is_null());
        assert_eq!(b.len, 0);
        unsafe {
            assert_eq!(
                aster_string_release(s.ptr, s.len),
                aster_status_t::ASTER_STATUS_OK as i32
            );
            assert_eq!(
                aster_string_release(std::ptr::null(), 0),
                aster_status_t::ASTER_STATUS_OK as i32
            );
        }
    }
}

#[cfg(test)]
mod runtime_drop_tests {
    use super::*;

    /// The last `Arc<BridgeRuntime>` released inside one of the runtime's own
    /// tasks must not panic ("Cannot drop a runtime in a context where
    /// blocking is not allowed") -- the .NET soak test hit exactly this when
    /// a connection-close task outlived `aster_runtime_close`.
    #[test]
    fn last_reference_dropped_on_own_worker_does_not_panic() {
        let mut handle: aster_runtime_t = 0;
        let status =
            unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, std::ptr::null(), &mut handle) };
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
        let bridge = load_runtime(handle).expect("runtime");

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let held = bridge.clone();
        bridge.runtime.spawn(async move {
            started_tx.send(()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            // By now close() has run and this is the last reference.
            drop(held);
            // Unreachable if the drop above panicked.
            let _ = done_tx.send(());
        });
        started_rx.recv().unwrap();
        drop(bridge);
        assert_eq!(
            unsafe { aster_runtime_close(handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );

        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("last runtime reference dropped on its own worker panicked");
    }
}
