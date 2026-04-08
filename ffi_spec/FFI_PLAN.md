# Iroh FFI — Complete Refactoring & Multi-Language Binding Plan

**Status:** Final Plan  
**Date:** 2026-04-04 (Phase 1c added; Phase 1b merged from FFI_PLAN_PATCH.md)  
**Scope:** Refactor `aster_transport_core` and `aster_transport_ffi` to provide a polished, language-neutral C ABI; update Python bindings; implement Java FFM bindings.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Architecture Overview](#2-architecture-overview)
3. [Phase 1: Core + FFI Refactoring](#3-phase-1-core--ffi-refactoring)
4. [Phase 1b: Datagram Completion & Hooks & Monitoring](#3b-phase-1b-datagram-completion--hooks--monitoring)
5. [Phase 1c: Registry & Publication Support](#3c-phase-1c-registry--publication-support)
5d. [Phase 1d: Endpoint Builder Gaps](#3d-phase-1d-endpoint-builder-gaps)
5f. [Phase 1f: Cross-Language Contract Identity, Framing & Signing](#3f-phase-1f-cross-language-contract-identity-framing--signing)
5g. [Phase 1g: Per-Connection Metrics & Prometheus Export](#phase-1g-per-connection-metrics--prometheus-export--done)
6. [Phase 2: Python Bindings Update](#4-phase-2-python-bindings-update)
7. [Phase 3: Java FFM Bindings](#5-phase-3-java-ffm-bindings)
8. [C ABI Reference](#6-c-abi-reference)
9. [Memory Ownership Rules](#7-memory-ownership-rules)
10. [Testing & Validation Strategy](#8-testing--validation-strategy)
11. [Migration Guide](#9-migration-guide)

---

## 1. Executive Summary

The current FFI layer (`aster_transport_ffi`) has several structural problems that prevent it from being a safe, efficient, multi-language ABI:

1. **Use-after-free risk**: Raw pointers are cast to `usize`, sent across async task boundaries, and reconstructed — if the caller frees the handle while an operation is in-flight, this causes UB.
   2. **Per-operation condvar model**: Each async operation gets its own `Mutex<Option<Result>>` + `Condvar`. This doesn't scale and is awkward for languages that want non-blocking completion.
   3. **No central completion queue**: Languages like Java (FFM) and Python (FFM/ctypes) work best with a poll-based event model rather than per-operation blocking waits.
   4. **No runtime handle**: The Tokio runtime is implicit/global, making multi-tenant use impossible and initialization order fragile.
   5. **No cancellation, no timeout, no readiness probe**.
   6. **Incomplete relay/discovery configuration**.

The plan replaces the current design with a **runtime + completion queue** architecture using `Arc`-backed handles, explicit lifetime rules, and a unified event model that serves Python, Java, and any future language binding equally.

---

## 2. Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Language Bindings                           │
│  ┌───────────┐      ┌──────────┐      ┌───────────┐                 │
│  │  Python   │      │   Java   │      │  C / Zig  │  ...            │
│  │  (PyO3)   │      │  (FFM)   │      │ (direct)  │                 │
│  └────┬──────┘      └────┬─────┘      └────┬──────┘                 │
│       │                  │                 │                        │
│       │ direct Rust API  │                 │                        │
│       │                  │                 │                        │
│       ▼                  ▼                 ▼                        │
│  ┌────────────────┐  ┌───────────────────────────────────────────┐  │
│  │ aster_transport│  │         aster_transport_ffi (C ABI)       │  │
│  │ _core          │  │  - #[no_mangle] extern "C" functions      │  │
│  │ (PyO3 backend) │  │  - #[repr(C)] structs                     │  │
│  └────────┬───────┘  │  - Opaque u64 handles                     │  │
│           │          │  - Central completion queue               │  │
│           │          ┤  - Arc-backed handle registry             │  │
│           │          └────────────────┬──────────────────────────┘  │
│           │                           │                             │
│           │          ┌────────────────▼────────────────────────┐    │
│           │          │         aster_transport_core            │    │
│           │          │  - Pure Rust async API                  │    │
│           └──────────│  - No FFI concerns                      │    │
│                      │  - Wraps iroh, iroh-blobs, iroh-docs,   │    │
│                      │    iroh-gossip                          │    │
│                      └─────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────┘
```

**Current repository reality:** Python is the special case here. `bindings/aster_rs`
uses **PyO3 directly over `aster_transport_core`**, while Java/C/Zig-style foreign bindings are
expected to consume the `aster_transport_ffi` C ABI.

### Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Handle representation | `u64` opaque IDs into a `HandleRegistry<Arc<T>>` | Safe, no raw pointers cross the boundary |
| Async model | Submit + poll completion queue | Works for Java FFM, Python ctypes, any language |
| Runtime | Explicit `iroh_runtime_t` handle | Allows multiple runtimes, clean init/shutdown |
| Error model | Status codes + per-event error payloads + `iroh_last_error_message()` | Numeric codes for programmatic handling, strings for diagnostics |
| Memory | All returned buffers owned by caller, freed via explicit `iroh_*_free()` | No ambiguity |
| Event payloads | `Arc<[u8]>` kept alive until `iroh_buffer_release()` | Zero-copy from Rust to caller for received frames |

---

## 3. Phase 1: Core + FFI Refactoring

### 3.1 `aster_transport_core` Changes

The core layer is already mostly clean. Changes needed:

#### 3.1.1 Make all public types `Arc`-wrapped internally

```rust
// Before:
pub struct CoreConnection { pub inner: Connection }

// After:
#[derive(Clone)]
pub struct CoreConnection { inner: Arc<Connection> }
```

Apply to: `CoreNode`, `CoreNetClient`, `CoreConnection`, `CoreSendStream`, `CoreRecvStream`, `CoreBlobsClient`, `CoreDocsClient`, `CoreDoc`, `CoreGossipClient`, `CoreGossipTopic`.

**Why:** The FFI layer can clone these into async tasks safely. No raw pointer resurrection needed.

#### 3.1.2 Remove `Arc<Mutex<>>` from stream wrappers

Currently `CoreSendStream` wraps `Arc<Mutex<iroh::endpoint::SendStream>>`. Since the core type itself will be `Arc`-wrapped at the FFI level, the inner mutex should stay but the outer `Arc` in the FFI handle registry provides the lifetime guarantee.

#### 3.1.3 Add relay URL configuration support

```rust
pub struct CoreEndpointConfig {
    pub relay_mode: Option<String>,
    pub relay_urls: Vec<String>,           // custom relay URLs
    pub alpns: Vec<Vec<u8>>,
    pub secret_key: Option<Vec<u8>>,
    pub enable_discovery: bool,            // default true
    pub enable_monitoring: bool,           // Phase 1b
    pub enable_hooks: bool,                // Phase 1b
    pub hook_timeout_ms: u64,              // Phase 1b; default 5000
    // Phase 1d: endpoint builder gaps
    pub bind_addr: Option<String>,         // e.g. "0.0.0.0:9000", "127.0.0.1:0"
    pub clear_ip_transports: bool,         // relay-only mode
    pub clear_relay_transports: bool,      // direct-IP-only mode
    pub portmapper_config: Option<String>, // "enabled" (default) | "disabled"
    pub proxy_url: Option<String>,         // HTTP/SOCKS proxy e.g. "http://proxy:8080"
    pub proxy_from_env: bool,              // read from HTTP_PROXY / HTTPS_PROXY
}
```

Update `build_endpoint_config()` to handle `relay_mode == "custom"` with the provided URLs and the new Phase 1d fields (see Phase 1d below).

#### 3.1.4 Add Collection/sendme-compatible blob API

The `sendme` CLI tool (and iroh's native blob sharing) wraps files in a `Collection` (HashSeq format) before creating tickets. Raw blob tickets (`BlobFormat::Raw`) are **not interoperable** with `sendme` — they cause "stream reset by peer: error 3" during transfer because the receiver expects a HashSeq structure.

To ensure cross-tool compatibility, the core layer provides Collection-aware methods:

```rust
impl CoreBlobsClient {
    /// Store bytes as a single-file Collection (HashSeq), compatible with sendme.
    /// Wraps the data in a Collection with the given filename.
    /// Returns the collection hash (hex).
    pub async fn add_bytes_as_collection(&self, name: String, data: Vec<u8>) -> Result<String>;

    /// Store a multi-file collection (HashSeq).
    /// Each (name, data) pair is stored as a raw blob, then wrapped in a Collection.
    /// A persistent HashSeq tag is set for GC protection.
    /// Returns the collection hash (hex).
    pub async fn add_collection(&self, entries: Vec<(String, Vec<u8>)>) -> Result<String>;

    /// List entries from a stored collection.
    /// Returns Vec<(name, hash_hex, size)>.
    pub async fn list_collection(&self, hash_hex: String) -> Result<Vec<(String, String, u64)>>;

    /// Create a ticket for a Collection (HashSeq format), compatible with sendme.
    pub fn create_collection_ticket(&self, hash_hex: String) -> Result<String>;

    /// Download a blob — auto-detects Raw vs HashSeq format from the ticket.
    /// If HashSeq (Collection), extracts and concatenates file contents.
    pub async fn download_blob(&self, ticket_str: String) -> Result<Vec<u8>>;

    /// Download a collection and return list of (name, data) pairs.
    pub async fn download_collection(&self, ticket_str: String) -> Result<Vec<(String, Vec<u8>)>>;
}
```

**FFI C ABI functions:**
- `iroh_blobs_add_collection(runtime, node, entries_json, user_data, out_operation)` — entries_json is `[["name","base64data"],...]`; emits `IROH_EVENT_BLOB_COLLECTION_ADDED`
- `iroh_blobs_list_collection(runtime, node, hash_hex, user_data, out_operation)` — emits `IROH_EVENT_BLOB_READ` with JSON `[["name","hash_hex",size],...]`

**Interoperability matrix:**

| Sender | Receiver | `create_ticket` (Raw) | `create_collection_ticket` (HashSeq) |
|--------|----------|-----------------------|--------------------------------------|
| Python | Python   | ✅ Works              | ✅ Works                             |
| Python | sendme   | ❌ stream reset       | ✅ Works                             |
| sendme | Python   | N/A (sendme always uses HashSeq) | ✅ `download_blob` auto-detects |

**Language binding requirements:**
- All language bindings MUST expose `add_bytes_as_collection`, `add_collection`, `list_collection`, `create_collection_ticket`, and the format-aware `download_blob`
- The `download_blob` method MUST auto-detect `BlobFormat::HashSeq` from the ticket and extract Collection contents
- The existing `add_bytes` + `create_ticket` (Raw format) remain available for Python-to-Python or internal use

#### 3.1.5 Add secret key export

```rust
impl CoreNetClient {
    pub fn export_secret_key(&self) -> Vec<u8> { ... }
}
impl CoreNode {
    pub fn export_secret_key(&self) -> Vec<u8> { ... }
}
```

#### 3.1.6 Add datagram support to core (already exists, verify completeness)

Verify `send_datagram` and `read_datagram` work correctly end-to-end.

### 3.2 `aster_transport_ffi` Complete Rewrite

The FFI layer gets a ground-up rewrite based on the completion queue architecture.

#### 3.2.1 Handle Registry

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

struct HandleRegistry<T> {
    next_id: AtomicU64,
    items: RwLock<HashMap<u64, Arc<T>>>,
}

impl<T> HandleRegistry<T> {
    fn insert(&self, value: T) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1; // 0 is invalid
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
}
```

**Critical safety property:** `get()` returns `Arc<T>`, so freeing the handle (via `remove()`) while an async task holds a clone is safe.

#### 3.2.2 Runtime and Event Queue

```rust
struct BridgeRuntime {
    tokio: tokio::runtime::Runtime,
    events_tx: mpsc::UnboundedSender<EventOwned>,
    events_rx: Mutex<mpsc::UnboundedReceiver<EventOwned>>,
    next_handle: AtomicU64,
    
    // Handle registries
    endpoints: HandleRegistry<CoreNetClient>,
    nodes: HandleRegistry<CoreNode>,
    connections: HandleRegistry<CoreConnection>,
    send_streams: HandleRegistry<CoreSendStream>,
    recv_streams: HandleRegistry<CoreRecvStream>,
    blobs: HandleRegistry<CoreBlobsClient>,
    docs_clients: HandleRegistry<CoreDocsClient>,
    docs: HandleRegistry<CoreDoc>,
    gossip_clients: HandleRegistry<CoreGossipClient>,
    gossip_topics: HandleRegistry<CoreGossipTopic>,
}
```

#### 3.2.3 Event Structure

```c
typedef struct iroh_event_s {
    uint32_t kind;           // iroh_event_kind_t
    uint32_t status;         // iroh_status_t (0 = success)
    uint64_t operation;      // operation handle that completed
    uint64_t handle;         // primary result handle (endpoint/connection/stream/etc.)
    uint64_t related;        // secondary handle (e.g., send stream + recv stream for open_bi)
    uint64_t user_data;      // echoed from submission
    const uint8_t* data_ptr; // payload data (for frame_received, string results, etc.)
    size_t data_len;         // payload length
    uint64_t buffer;         // buffer lease handle (call iroh_buffer_release when done)
    int32_t error_code;      // numeric error category
    uint32_t flags;          // reserved
    uint32_t subtype;        // event-kind-specific sub-result (see below)
    uint32_t _reserved0;     // padding for alignment
} iroh_event_t;
```

#### 3.2.4 Event Kinds

```c
typedef enum iroh_event_kind_e {
    IROH_EVENT_NONE = 0,
    
    // Lifecycle
    IROH_EVENT_NODE_CREATED = 1,
    IROH_EVENT_ENDPOINT_CREATED = 2,
    IROH_EVENT_CLOSED = 3,
    
    // Connections
    IROH_EVENT_CONNECTED = 10,
    IROH_EVENT_CONNECTION_ACCEPTED = 11,
    IROH_EVENT_CONNECTION_CLOSED = 12,
    
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
    
    // Docs
    IROH_EVENT_DOC_CREATED = 40,
    IROH_EVENT_DOC_JOINED = 41,
    IROH_EVENT_DOC_SET = 42,
    IROH_EVENT_DOC_GET = 43,
    IROH_EVENT_DOC_SHARED = 44,
    IROH_EVENT_DOC_QUERY = 46,
    IROH_EVENT_AUTHOR_CREATED = 45,
    
    // Gossip
    IROH_EVENT_GOSSIP_SUBSCRIBED = 50,
    IROH_EVENT_GOSSIP_BROADCAST_DONE = 51,
    IROH_EVENT_GOSSIP_RECEIVED = 52,
    IROH_EVENT_GOSSIP_NEIGHBOR_UP = 53,
    IROH_EVENT_GOSSIP_NEIGHBOR_DOWN = 54,
    IROH_EVENT_GOSSIP_LAGGED = 55,
    
    // Aster custom-ALPN (Phase 1e)
    IROH_EVENT_ASTER_ACCEPTED = 65,
    
    // Generic
    IROH_EVENT_STRING_RESULT = 90,
    IROH_EVENT_BYTES_RESULT = 91,
    IROH_EVENT_UNIT_RESULT = 92,
    IROH_EVENT_OPERATION_CANCELLED = 98,
    IROH_EVENT_ERROR = 99,
} iroh_event_kind_t;
```

#### 3.2.5 Core API Functions

```c
// ─── Versioning ───
uint32_t iroh_abi_version_major(void);
uint32_t iroh_abi_version_minor(void);
uint32_t iroh_abi_version_patch(void);

// ─── Runtime ───
iroh_status_t iroh_runtime_new(
    const iroh_runtime_config_t* config,
    iroh_runtime_t* out_runtime
);
iroh_status_t iroh_runtime_close(iroh_runtime_t runtime);

// ─── Event Polling ───
// Returns number of events written. timeout_ms=0 is non-blocking.
size_t iroh_poll_events(
    iroh_runtime_t runtime,
    iroh_event_t* out_events,
    size_t max_events,
    uint32_t timeout_ms
);

iroh_status_t iroh_buffer_release(
    iroh_runtime_t runtime,
    uint64_t buffer
);

// ─── Operations ───
iroh_status_t iroh_operation_cancel(
    iroh_runtime_t runtime,
    iroh_operation_t operation
);

// ─── Error ───
size_t iroh_last_error_message(
    uint8_t* buffer,
    size_t capacity
);
const char* iroh_status_name(iroh_status_t status);

// ─── Node ───
iroh_status_t iroh_node_memory(
    iroh_runtime_t runtime,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_node_persistent(
    iroh_runtime_t runtime,
    const uint8_t* path_ptr, size_t path_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_node_close(
    iroh_runtime_t runtime,
    uint64_t node,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_node_id(
    iroh_runtime_t runtime,
    uint64_t node,
    uint8_t* out_buf, size_t capacity, size_t* out_len
);
iroh_status_t iroh_node_addr_info(
    iroh_runtime_t runtime,
    uint64_t node,
    iroh_node_addr_t* out_addr
);
iroh_status_t iroh_node_add_peer(
    iroh_runtime_t runtime,
    uint64_t node,
    const iroh_node_addr_t* peer_addr
);
iroh_status_t iroh_node_export_secret_key(
    iroh_runtime_t runtime,
    uint64_t node,
    uint8_t* out_buf, size_t capacity, size_t* out_len
);

// ─── Endpoint (bare, no Router) ───
iroh_status_t iroh_endpoint_create(
    iroh_runtime_t runtime,
    const iroh_endpoint_config_t* config,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_endpoint_close(
    iroh_runtime_t runtime,
    uint64_t endpoint,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_endpoint_id(
    iroh_runtime_t runtime,
    uint64_t endpoint,
    uint8_t* out_buf, size_t capacity, size_t* out_len
);
iroh_status_t iroh_endpoint_addr_info(
    iroh_runtime_t runtime,
    uint64_t endpoint,
    iroh_node_addr_t* out_addr
);

// ─── Connections ───
iroh_status_t iroh_connect(
    iroh_runtime_t runtime,
    uint64_t endpoint_or_node,
    const iroh_connect_config_t* config,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_accept(
    iroh_runtime_t runtime,
    uint64_t endpoint,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_connection_remote_id(
    iroh_runtime_t runtime,
    uint64_t connection,
    uint8_t* out_buf, size_t capacity, size_t* out_len
);
iroh_status_t iroh_connection_close(
    iroh_runtime_t runtime,
    uint64_t connection,
    uint32_t error_code,
    const uint8_t* reason_ptr, size_t reason_len
);
iroh_status_t iroh_connection_closed(
    iroh_runtime_t runtime,
    uint64_t connection,
    uint64_t user_data,
    iroh_operation_t* out_operation
);  // Async: awaits until connection drops, emits CONNECTION_CLOSED with error_code and reason payload
iroh_status_t iroh_connection_send_datagram(
    iroh_runtime_t runtime,
    uint64_t connection,
    const uint8_t* data_ptr, size_t data_len
);
iroh_status_t iroh_connection_read_datagram(
    iroh_runtime_t runtime,
    uint64_t connection,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// ─── Streams ───
iroh_status_t iroh_open_bi(
    iroh_runtime_t runtime,
    uint64_t connection,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_accept_bi(
    iroh_runtime_t runtime,
    uint64_t connection,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_open_uni(
    iroh_runtime_t runtime,
    uint64_t connection,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_accept_uni(
    iroh_runtime_t runtime,
    uint64_t connection,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_stream_write(
    iroh_runtime_t runtime,
    uint64_t send_stream,
    const uint8_t* data_ptr, size_t data_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_stream_finish(
    iroh_runtime_t runtime,
    uint64_t send_stream,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_stream_read(
    iroh_runtime_t runtime,
    uint64_t recv_stream,
    size_t max_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_stream_read_to_end(
    iroh_runtime_t runtime,
    uint64_t recv_stream,
    size_t max_size,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_stream_read_exact(
    iroh_runtime_t runtime,
    uint64_t recv_stream,
    size_t exact_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_stream_stopped(
    iroh_runtime_t runtime,
    uint64_t send_stream,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_stream_stop(
    iroh_runtime_t runtime,
    uint64_t recv_stream,
    uint32_t error_code
);

// ─── Endpoint ───
iroh_status_t iroh_endpoint_export_secret_key(
    iroh_runtime_t runtime,
    uint64_t endpoint,
    uint8_t* out_buf, size_t capacity, size_t* out_len
);

// ─── Blobs ───
iroh_status_t iroh_blobs_add_bytes(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* data_ptr, size_t data_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_blobs_read(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* hash_hex_ptr, size_t hash_hex_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_blobs_create_ticket(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* hash_hex_ptr, size_t hash_hex_len,
    uint8_t* out_buf, size_t capacity, size_t* out_len
);
iroh_status_t iroh_blobs_download(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* ticket_ptr, size_t ticket_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// ─── Blobs: Collection / sendme-compatible ───
// Store bytes wrapped in a Collection (HashSeq format), compatible with sendme CLI.
// The name parameter is the filename within the collection.
// Emits IROH_EVENT_BLOB_ADDED with the collection hash hex in the payload.
iroh_status_t iroh_blobs_add_bytes_as_collection(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* name_ptr, size_t name_len,
    const uint8_t* data_ptr, size_t data_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
// Create a ticket for a Collection (HashSeq format), compatible with sendme CLI.
// The hash_hex must be a collection hash returned by iroh_blobs_add_bytes_as_collection.
iroh_status_t iroh_blobs_create_collection_ticket(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* hash_hex_ptr, size_t hash_hex_len,
    uint8_t* out_buf, size_t capacity, size_t* out_len
);

// ─── Docs ───
iroh_status_t iroh_docs_create(
    iroh_runtime_t runtime,
    uint64_t node,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_docs_create_author(
    iroh_runtime_t runtime,
    uint64_t node,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_docs_join(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* ticket_ptr, size_t ticket_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_doc_set_bytes(
    iroh_runtime_t runtime,
    uint64_t doc,
    const uint8_t* author_hex_ptr, size_t author_hex_len,
    const uint8_t* key_ptr, size_t key_len,
    const uint8_t* value_ptr, size_t value_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_doc_get_exact(
    iroh_runtime_t runtime,
    uint64_t doc,
    const uint8_t* author_hex_ptr, size_t author_hex_len,
    const uint8_t* key_ptr, size_t key_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_doc_query(
    iroh_runtime_t runtime,
    uint64_t doc,
    uint32_t mode,  // 0=key_exact, 1=key_prefix
    const uint8_t* key_ptr, size_t key_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);  // Returns DOC_QUERY event with packed entries in payload, entry count in flags
iroh_status_t iroh_doc_read_entry_content(
    iroh_runtime_t runtime,
    uint64_t doc,
    const uint8_t* content_hash_hex_ptr, size_t content_hash_hex_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);  // Returns BLOB_READ event with content bytes
iroh_status_t iroh_doc_share(
    iroh_runtime_t runtime,
    uint64_t doc,
    uint32_t mode,  // 0=read, 1=write
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// ─── Gossip ───
iroh_status_t iroh_gossip_subscribe(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* topic_ptr, size_t topic_len,
    const iroh_bytes_t* peers, size_t peers_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_gossip_broadcast(
    iroh_runtime_t runtime,
    uint64_t topic,
    const uint8_t* data_ptr, size_t data_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
iroh_status_t iroh_gossip_recv(
    iroh_runtime_t runtime,
    uint64_t topic,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// ─── Handle Free (typed, one per handle kind) ───
iroh_status_t iroh_node_free(iroh_runtime_t runtime, uint64_t node);
iroh_status_t iroh_endpoint_free(iroh_runtime_t runtime, uint64_t endpoint);
iroh_status_t iroh_connection_free(iroh_runtime_t runtime, uint64_t connection);
iroh_status_t iroh_send_stream_free(iroh_runtime_t runtime, uint64_t stream);
iroh_status_t iroh_recv_stream_free(iroh_runtime_t runtime, uint64_t stream);
iroh_status_t iroh_doc_free(iroh_runtime_t runtime, uint64_t doc);
iroh_status_t iroh_gossip_topic_free(iroh_runtime_t runtime, uint64_t topic);
```

#### 3.2.6 Config Structs

```c
typedef struct iroh_bytes_s {
    const uint8_t* ptr;
    size_t len;
} iroh_bytes_t;

typedef struct iroh_runtime_config_s {
    uint32_t struct_size;        // for forward compat
    uint32_t worker_threads;     // 0 = default
    uint32_t event_queue_capacity; // 0 = default (4096)
    uint32_t reserved;
} iroh_runtime_config_t;

typedef struct iroh_endpoint_config_s {
    uint32_t struct_size;
    uint32_t relay_mode;              // 0=default, 1=custom, 2=disabled, 3=staging
    iroh_bytes_t secret_key;          // 32 bytes or empty
    const iroh_bytes_t* alpns;
    size_t alpns_len;
    const iroh_bytes_t* relay_urls;   // UTF-8 URLs (used when relay_mode==1)
    size_t relay_urls_len;
    uint32_t enable_discovery;        // 1=yes (default), 0=no
    uint32_t enable_monitoring;       // Phase 1b; 0=no (default)
    uint32_t enable_hooks;            // Phase 1b; 0=no (default)
    uint64_t hook_timeout_ms;         // Phase 1b; 0 = use default (5000)
    // Phase 1d: endpoint builder gaps
    iroh_bytes_t bind_addr;           // socket addr string e.g. "0.0.0.0:9000"; empty=default
    uint32_t clear_ip_transports;     // 1=relay-only mode
    uint32_t clear_relay_transports;  // 1=direct-IP-only mode
    uint32_t portmapper_config;       // 0=enabled (default), 1=disabled
    iroh_bytes_t proxy_url;           // HTTP/SOCKS proxy URL string; empty=none
    uint32_t proxy_from_env;          // 1=read HTTP_PROXY/HTTPS_PROXY from env
    uint32_t reserved;
} iroh_endpoint_config_t;

typedef struct iroh_connect_config_s {
    uint32_t struct_size;
    uint32_t flags;
    iroh_bytes_t node_id;        // hex string or binary
    iroh_bytes_t alpn;
    const iroh_node_addr_t* addr; // optional full addr, NULL to use just node_id
} iroh_connect_config_t;

typedef struct iroh_node_addr_s {
    iroh_bytes_t endpoint_id;    // hex string
    iroh_bytes_t relay_url;      // UTF-8, empty if none
    const iroh_bytes_t* direct_addresses; // array of "ip:port" strings
    size_t direct_addresses_len;
} iroh_node_addr_t;
```

#### 3.2.7 Status Codes

```c
typedef enum iroh_status_e {
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
} iroh_status_t;
```

#### 3.2.8 Implementation Pattern for Async Operations

Every async FFI function follows this pattern:

```rust
#[unsafe(no_mangle)]
pub extern "C" fn iroh_connect(
    runtime: iroh_runtime_t,
    endpoint_or_node: u64,
    config: *const iroh_connect_config_t,
    user_data: u64,
    out_operation: *mut iroh_operation_t,
) -> iroh_status_t {
    // 1. Validate args
    if config.is_null() || out_operation.is_null() {
        return IROH_STATUS_INVALID_ARGUMENT;
    }
    
    // 2. Load runtime
    let bridge = match load_runtime(runtime) {
        Ok(rt) => rt,
        Err(s) => return s,
    };
    
    // 3. Allocate operation ID
    let op_id = bridge.next_operation();
    unsafe { *out_operation = op_id; }
    
    // 4. Clone Arc handle (safe even if caller frees later)
    let net = match bridge.endpoints.get(endpoint_or_node) {
        Some(n) => n,
        None => {
            emit_error(&bridge, op_id, user_data, "endpoint not found");
            return IROH_STATUS_OK; // error delivered via event
        }
    };
    
    // 5. Parse config (borrowed only during this call)
    let parsed = unsafe { parse_connect_config(&*config) };
    
    // 6. Spawn async task with cloned Arc
    let bridge2 = bridge.clone();
    bridge.tokio.spawn(async move {
        match net.connect(parsed.node_id, parsed.alpn).await {
            Ok(conn) => {
                let conn_handle = bridge2.connections.insert(conn);
                bridge2.emit(Event {
                    kind: IROH_EVENT_CONNECTED,
                    operation: op_id,
                    handle: conn_handle,
                    user_data,
                    ..Default::default()
                });
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });
    
    IROH_STATUS_OK
}
```

**Key safety properties:**
- `net` is `Arc<CoreNetClient>` — the async task owns a reference
  - Input pointers (`config`) are only read during the synchronous part
  - Data is copied into owned types before the async boundary
  - Freeing the handle from the caller's side only drops one `Arc` reference

---

## 3d. Phase 1d: Endpoint Builder Gaps

> **Added:** 2026-04-05. Addresses options present in the iroh `Endpoint::builder` API that were not previously exposed via any FFI layer.

### Overview

The following builder options are now exposed through `CoreEndpointConfig`, the Python `EndpointConfig` class, and the C `iroh_endpoint_config_t` struct. All are **opt-in** — existing callers that don't set these fields get identical behaviour to before.

### 3d.1 `bind_addr` — Socket Binding Control

**Field:** `bind_addr: Option<String>` / `iroh_bytes_t bind_addr`

Sets the UDP/QUIC socket bind address. By default iroh binds to `0.0.0.0:0` (random port, all IPv4 interfaces) and `[::]:0` (all IPv6 interfaces).

- `"0.0.0.0:9000"` — fixed port, all IPv4 interfaces
- `"127.0.0.1:0"` — loopback only, random port
- `"[::]:9000"` — fixed port, all IPv6 interfaces
- `":9000"` — all interfaces, fixed port

Calling this once **replaces** the default socket for the matching address family. Calling it twice (once IPv4, once IPv6) replaces both. Leave empty/`None` to use the iroh defaults.

**Wire-through:** `build_endpoint_config()` calls `builder.bind_addr(addr_str)?` when non-empty. Returns a descriptive error if the address string is invalid or the port is already in use.

### 3d.2 `clear_ip_transports` — Relay-only Mode

**Field:** `clear_ip_transports: bool` / `uint32_t clear_ip_transports`

When `true`, removes all direct IP (UDP/QUIC) transports. The endpoint communicates via relay only. Useful for firewall-restricted environments where UDP is blocked.

**Wire-through:** `build_endpoint_config()` calls `builder.clear_ip_transports()` when `true`.

> ⚠️ Do not set both `clear_ip_transports` and `clear_relay_transports` — the endpoint will be unable to connect to any peer.

### 3d.3 `clear_relay_transports` — Direct-IP-only Mode

**Field:** `clear_relay_transports: bool` / `uint32_t clear_relay_transports`

When `true`, removes all relay transports. The endpoint communicates via direct IP connections only. Suitable for LAN/VPN environments where relay infrastructure is unavailable or untrusted.

**Wire-through:** `build_endpoint_config()` calls `builder.clear_relay_transports()` when `true`.

### 3d.4 `portmapper_config` — NAT Port Mapping

**Field:** `portmapper_config: Option<String>` / `uint32_t portmapper_config`

Controls the UPnP/NAT-PMP port mapper. Accepted string values (Python/core): `"enabled"` (default) or `"disabled"`. C ABI: `0` = enabled (default), `1` = disabled.

Set to `"disabled"` in corporate or CI environments where UPnP probes fail or are unwanted.

**Wire-through:** `build_endpoint_config()` calls `builder.portmapper_config(PortmapperConfig::Disabled)` when set to `"disabled"`.

### 3d.5 `proxy_url` / `proxy_from_env` — HTTP/SOCKS Proxy

**Fields:** `proxy_url: Option<String>`, `proxy_from_env: bool`

Routes all HTTP/HTTPS traffic (relay connections, pkarr lookups) through a proxy.

- `proxy_url` — explicit URL e.g. `"http://proxy.corp:8080"`, `"socks5://localhost:1080"`
- `proxy_from_env` — reads `HTTP_PROXY` / `HTTPS_PROXY` / `http_proxy` / `https_proxy` from the environment

`proxy_url` takes precedence over `proxy_from_env`. Setting both is allowed but `proxy_url` wins.

**Wire-through:** `build_endpoint_config()` calls `builder.proxy_url(url)` when `proxy_url` is set, or `builder.proxy_from_env()` when `proxy_from_env` is `true`.

### 3d.6 Python API

```python
# Server with a fixed port
EndpointConfig(alpns=[b"myproto/1"], bind_addr="0.0.0.0:9000")

# Relay-only node behind strict firewall
EndpointConfig(alpns=[b"myproto/1"], clear_ip_transports=True)

# Direct IP-only for LAN cluster
EndpointConfig(alpns=[b"myproto/1"], clear_relay_transports=True)

# Corporate network — disable UPnP probes and use proxy
EndpointConfig(alpns=[b"myproto/1"], portmapper_config="disabled", proxy_url="http://proxy:8080")

# Read proxy from environment
EndpointConfig(alpns=[b"myproto/1"], proxy_from_env=True)
```

### 3d.7 What is NOT yet exposed

The following iroh builder options were evaluated but deferred (see `docs/endpointbuildergaps.md`):

| Option | Reason deferred |
|---|---|
| `bind_addr_with_opts` (BindOpts) | Niche multi-NIC routing; simple `bind_addr` covers 99% of use cases |
| `transport_config` (QUIC tuning) | Deep QUIC knobs; not needed for any current Python application |
| `dns_resolver` | Custom DNS is niche; system DNS is correct for almost all deployments |
| `ca_roots_config` | Enterprise PKI only; not needed with public iroh relay infrastructure |
| `address_lookup` | Custom discovery; relay-based discovery covers standard use cases |
| `addr_filter` | Advanced privacy control; not needed currently |
| `user_data_for_address_lookup` | Useful metadata feature; can be added on demand |
| `keylog` | Debugging tool; security risk in production — add only if explicitly requested |
| `max_tls_tickets` | Memory tuning; defaults are correct for Python use cases |

---

## 4. Phase 2: Python Bindings Update

### 4.1 Strategy

Phase 2 is intentionally **not** a lightweight cleanup pass. It is the first binding that must fully exercise the new architecture end-to-end under a high-level language runtime, and it should be treated as the **architectural proving ground** for all later language ports.

There are two possible implementation strategies:

**Option A: PyO3 directly wrapping `aster_transport_core`**
**Option B: PyO3 wrapping the C ABI via `aster_transport_ffi`**

**Decision: use Option A as the required implementation path, and require `aster_transport_core` to be the immediate backend for every Python wrapper.**

This is a deliberate architectural choice, not merely a convenience choice:

1. **Strict layering and reuse come first.** Python should consume the same Rust-level abstractions that the C ABI consumes. Any awkwardness discovered here is valuable signal that `aster_transport_core` itself still needs refinement before Java and other foreign-language bindings harden on top of it.
2. **Performance and safety remain primary goals.** PyO3 should expose the async/core model directly, preserve zero-copy behavior where practical, and avoid reintroducing ad-hoc blocking or duplicated marshalling logic.
3. **Python must expose the full intended Phase 1b surface immediately.** Datagram completion, hooks, monitoring, remote-info, and related endpoint/connection observability are not optional “later” additions for Python; they are part of Phase 2 readiness.
4. **Python depends on Phase 1b completeness.** We should not declare Phase 2 done while Phase 1b remains partially implemented, placeholder-backed, or undocumented in the core layer.
5. **The Python binding is a design validation harness for Java and future bindings.** If a surface is hard to expose ergonomically and safely from `aster_transport_core` into PyO3, it is a strong indicator that the same surface will be harder and riskier through Java FFM, Go, Zig, or other foreign runtimes.

The consequence is that `iroh_python_rs` should not be a special-case wrapper built directly against upstream crates, and it should not retain a legacy FFI-based implementation in `src/lib.rs`. Instead, it should become a thin, idiomatic Python-facing layer over `aster_transport_core`, with the core crate as the single source of truth for semantics, lifetime rules, async behavior, and feature completeness.

### 4.2 Changes

1. **Require `aster_transport_core` as the sole Python backend**
   - Remove the `aster_transport_ffi` dependency from `iroh_python_rs`
   - Remove any direct dependency on upstream `iroh*` crates from Python wrappers except where needed transitively through core-facing types
   - Treat `aster_transport_core` as the only authoritative implementation surface for Python behavior

2. **Delete the legacy FFI-based Python implementation path**
   - `iroh_python_rs/src/lib.rs` must become module registration only
   - All FFI-based wrapper logic currently embedded in `lib.rs` must be removed
   - Python should not maintain parallel implementations of the same API via both PyO3→core and PyO3→FFI paths

3. **Refactor every Python wrapper to map 1:1 onto core abstractions**
   - `IrohNode` → `CoreNode`
   - `NetClient` / endpoints → `CoreNetClient`
   - `IrohConnection` → `CoreConnection`
   - stream wrappers → `CoreSendStream` / `CoreRecvStream`
   - blobs/docs/gossip wrappers → `CoreBlobsClient`, `CoreDocsClient`, `CoreDoc`, `CoreGossipClient`, `CoreGossipTopic`
   - shared Python data carriers (`NodeAddr`, endpoint config, closed info, gossip events, remote-info types, hook event data) should be derived from core structs, not recreated independently with divergent semantics

4. **Expose the full intended Phase 1b surface in Python immediately**
   - datagram completion queries (`max_datagram_size`, `datagram_send_buffer_space`)
   - hook registration / callback plumbing once implemented in core
   - monitoring / remote-info / connection-info queries once implemented in core
   - protocol-level screening or any distinct admission-control surface if it is promoted into core

5. **Preserve the Python public API where possible, but allow additive API growth for newly-completed Phase 1b features**
   - Existing user-facing classes and method names should remain stable unless safety or correctness requires a change
   - Newly available observability and hook APIs should be added in Python as soon as they are available in core
   - Public Python docs and stubs must stay synchronized with the actual exported surface

6. **Adopt maintained async runtime integration**
   - Migrate from deprecated `pyo3_asyncio` if needed to the maintained `pyo3-async-runtimes` fork
   - Ensure async behavior remains native to Python (`asyncio`) while delegating transport semantics to core

7. **Do not hide architectural gaps with Python-only shims**
   - If Python needs behavior that core does not cleanly expose, fix core first
   - If Python needs a workaround for missing hook/monitoring/remote-info surfaces, that is evidence Phase 1b is incomplete, not a reason to patch around it in `iroh_python_rs`

### 4.3 Module Structure (after refactoring)

```
iroh_python_rs/src/
├── lib.rs          # Module registration only
├── node.rs         # IrohNode (PyO3 wrapper over CoreNode)
├── net.rs          # NetClient, Connection, Streams (PyO3 wrapper over core)
├── blobs.rs        # BlobsClient (PyO3 wrapper over CoreBlobsClient)
├── docs.rs         # DocsClient, DocHandle (PyO3 wrapper over core)
├── gossip.rs       # GossipClient, GossipTopicHandle (PyO3 wrapper over core)
├── monitor.rs      # Remote-info / connection-info Python wrappers
├── hooks.rs        # Hook registration, callback dispatch, screening APIs
└── error.rs        # Error types and core->Python error mapping
```

The additional `monitor.rs` and `hooks.rs` modules are intentional: they make the newly required Phase 1b surfaces first-class citizens in Python rather than burying them in miscellaneous helper types.

### 4.4 Python API (unchanged)

```python
# The public API should remain the same
node = await IrohNode.memory()
blobs = blobs_client(node)
hash_hex = await blobs.add_bytes(b"hello")
data = await blobs.read_to_bytes(hash_hex)
```

The existing API should remain source-compatible for already-supported functionality, but Phase 2 also adds new Python-visible surface as soon as core exposes it. Representative additions include:

```python
# Datagram completion / capacity
max_size = conn.max_datagram_size()
buffer_space = conn.datagram_send_buffer_space()

# Monitoring / connection info
info = conn.connection_info()
remote = endpoint.remote_info(peer_id)
all_remotes = endpoint.remote_info_list()

# Hooks / screening (illustrative)
registration = endpoint.set_hooks(
    before_connect=before_connect_cb,
    after_connect=after_connect_cb,
)
```

### 4.5 Design Constraints for the Python Layer

The Python binding must uphold the same architectural rules as the FFI layer wherever they make sense in a native Rust/PyO3 binding:

1. **Single semantic source of truth:** transport behavior lives in core, not in PyO3 wrappers.
2. **No duplicate protocol logic:** Python wrappers may adapt ergonomics, but must not reimplement connection, monitoring, hook, or datagram semantics independently.
3. **Preserve zero-copy where practical:**
   - Return Python `bytes` only when conversion is required by the Python API contract.
   - Prefer borrowing or shared-buffer strategies where PyO3 and ownership rules permit safe exposure.
   - Avoid unnecessary intermediate allocations when converting from core buffers into Python-visible values.
4. **Async-first design:** long-running operations must remain async; avoid blocking waits and thread-per-operation patterns.
5. **Thread/lifetime clarity:** PyO3 object ownership, Rust `Arc` lifetimes, and Python coroutine lifetimes must remain explicit and well-documented.
6. **Parity with foreign-language expectations:** if a capability is required for Java/FFM usability, Python should exercise that same capability through core as early as possible.

### 4.6 Phase 2 Exit Criteria

Phase 2 is complete only when all of the following are true:

1. `iroh_python_rs` depends on `aster_transport_core` as its immediate backend for all transport features.
2. The legacy FFI-based implementation path has been fully removed from Python.
3. All duplicate Python wrapper implementations have been consolidated.
4. The Python binding exposes the completed Phase 1 and Phase 1b surfaces that exist in core.
5. Existing Python tests pass without API regressions for previously supported features.
6. New Python tests cover datagram completion, hooks, monitoring, remote-info, and any screening/admission-control APIs exposed by core.
7. Any friction discovered while exposing these surfaces has either been fixed in `aster_transport_core` or explicitly recorded as a blocker for Phase 3.

---

## 5. Phase 3: Java FFM Bindings

### 5.1 Project Structure

```
iroh_java/
├── build.gradle.kts
├── src/
│   ├── main/java/computer/iroh/
│   │   ├── IrohRuntime.java          # Runtime lifecycle
│   │   ├── IrohEndpoint.java         # Endpoint handle
│   │   ├── IrohConnection.java       # Connection handle
│   │   ├── IrohStream.java           # Stream handles
│   │   ├── IrohNode.java             # Full node handle
│   │   ├── IrohBlobs.java            # Blobs API
│   │   ├── IrohDocs.java             # Docs API
│   │   ├── IrohGossip.java           # Gossip API
│   │   ├── NodeAddr.java             # Address record
│   │   ├── BridgeEvent.java          # Event record
│   │   ├── IrohException.java        # Exception hierarchy
│   │   └── internal/
│   │       ├── NativeBindings.java   # FFM downcall handles
│   │       ├── EventPoller.java      # Background event pump
│   │       └── HandleTracker.java    # Prevent leaks
│   └── test/java/computer/iroh/
│       ├── RuntimeTest.java
│       ├── EndpointTest.java
│       ├── ConnectionTest.java
│       ├── BlobsTest.java
│       ├── DocsTest.java
│       └── GossipTest.java
```

### 5.2 Java API Design

```java
// Async-first, CompletableFuture-based
public class IrohRuntime implements AutoCloseable {
    public static IrohRuntime create() { ... }
    public static IrohRuntime create(RuntimeConfig config) { ... }
    
    // Node API
    public CompletableFuture<IrohNode> createMemoryNode() { ... }
    public CompletableFuture<IrohNode> createPersistentNode(Path dataDir) { ... }
    
    // Endpoint API  
    public CompletableFuture<IrohEndpoint> createEndpoint(EndpointConfig config) { ... }
    
    @Override public void close() { ... }
}

public class IrohNode implements AutoCloseable {
    public String nodeId() { ... }
    public NodeAddr nodeAddr() { ... }
    public CompletableFuture<Void> close() { ... }
    
    // Blobs
    public CompletableFuture<String> addBytes(byte[] data) { ... }
    public CompletableFuture<byte[]> readBytes(String hashHex) { ... }
    public String createBlobTicket(String hashHex) { ... }
    public CompletableFuture<byte[]> downloadBlob(String ticket) { ... }
    
    // Docs
    public CompletableFuture<IrohDoc> createDoc() { ... }
    public CompletableFuture<String> createAuthor() { ... }
    public CompletableFuture<IrohDoc> joinDoc(String ticket) { ... }
    
    // Gossip
    public CompletableFuture<IrohGossipTopic> subscribe(
        byte[] topic, List<String> bootstrapPeers) { ... }
}

public class IrohEndpoint implements AutoCloseable {
    public String endpointId() { ... }
    public NodeAddr endpointAddr() { ... }
    public CompletableFuture<IrohConnection> connect(String nodeId, byte[] alpn) { ... }
    public CompletableFuture<IrohConnection> connect(NodeAddr addr, byte[] alpn) { ... }
    public CompletableFuture<IrohConnection> accept() { ... }
    public CompletableFuture<Void> close() { ... }
}

public class IrohConnection implements AutoCloseable {
    public String remoteId() { ... }
    public CompletableFuture<BiStream> openBi() { ... }
    public CompletableFuture<BiStream> acceptBi() { ... }
    public CompletableFuture<IrohSendStream> openUni() { ... }
    public CompletableFuture<IrohRecvStream> acceptUni() { ... }
    public void sendDatagram(byte[] data) { ... }
    public CompletableFuture<byte[]> readDatagram() { ... }
    public void close(int code, byte[] reason) { ... }
}

public record BiStream(IrohSendStream send, IrohRecvStream recv) {}

public class IrohSendStream implements AutoCloseable {
    public CompletableFuture<Void> write(byte[] data) { ... }
    public CompletableFuture<Void> finish() { ... }
}

public class IrohRecvStream implements AutoCloseable {
    public CompletableFuture<byte[]> read(int maxLen) { ... }
    public CompletableFuture<byte[]> readToEnd(int maxSize) { ... }
    public void stop(int errorCode) { ... }
}
```

### 5.3 Internal FFM Binding

```java
// NativeBindings.java - FFM downcall handles
final class NativeBindings {
    private static final Linker LINKER = Linker.nativeLinker();
    private static final SymbolLookup LIB;
    
    static {
        System.loadLibrary("aster_transport_ffi");
        LIB = SymbolLookup.loaderLookup();
    }
    
    // All method handles mirror the C ABI exactly
    static final MethodHandle iroh_runtime_new = downcall("iroh_runtime_new",
        FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS));
    
    static final MethodHandle iroh_poll_events = downcall("iroh_poll_events",
        FunctionDescriptor.of(JAVA_LONG, JAVA_LONG, ADDRESS, JAVA_LONG, JAVA_INT));
    
    // ... etc for all functions
}
```

### 5.4 Event Poller

```java
// EventPoller.java - Background event pump
final class EventPoller implements Runnable {
    private final long runtime;
    private final ConcurrentMap<Long, CompletableFuture<BridgeEvent>> pending;
    private volatile boolean running = true;
    
    @Override
    public void run() {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment events = arena.allocate(EVENT_LAYOUT, 64);
            while (running) {
                long count = (long) NativeBindings.iroh_poll_events
                    .invokeExact(runtime, events, 64L, 100); // 100ms timeout
                for (int i = 0; i < count; i++) {
                    BridgeEvent event = decode(events, i);
                    dispatch(event);
                }
            }
        }
    }
    
    private void dispatch(BridgeEvent event) {
        CompletableFuture<BridgeEvent> future = pending.remove(event.operation());
        if (future != null) {
            if (event.kind() == IROH_EVENT_ERROR) {
                future.completeExceptionally(new IrohException(event));
            } else {
                future.complete(event);
            }
        }
    }
}
```

---

## 6. C ABI Reference

### 6.1 ABI Version

- Major: `1` — breaking changes increment this
  - Minor: `0` — new functions increment this  
  - Patch: `0` — bug fixes increment this

### 6.2 Calling Convention

All functions use the platform C calling convention (`extern "C"`). All structs are `#[repr(C)]` with no padding surprises.

### 6.3 Thread Safety

- All `iroh_*` functions are safe to call from any thread
  - `iroh_poll_events` should be called from a single dedicated thread (or with external synchronization)
  - `iroh_last_error_message` is thread-local — must be called from the same thread that received the error status

### 6.4 Handle Types

| Type | Registry | Description |
|------|----------|-------------|
| `iroh_runtime_t` | Global | Tokio runtime + event queue |
| Node handle | Per-runtime | Full iroh node (endpoint + router + protocols) |
| Endpoint handle | Per-runtime | Bare QUIC endpoint |
| Connection handle | Per-runtime | QUIC connection |
| Send stream handle | Per-runtime | Write half of a QUIC stream |
| Recv stream handle | Per-runtime | Read half of a QUIC stream |
| Doc handle | Per-runtime | Document instance |
| Topic handle | Per-runtime | Gossip topic subscription |
| Operation handle | Per-runtime | Pending async operation |
| Buffer handle | Per-runtime | Payload lease for received data |

All handles are `uint64_t`. Value `0` is invalid/null.

---

## 7. Memory Ownership Rules

### 7.1 Input Memory

All pointer+length pairs passed TO the FFI are **borrowed for the duration of the call only**. The FFI layer copies any data it needs to keep. The caller may free/reuse the memory immediately after the function returns.

### 7.2 Output Strings/Bytes (synchronous)

For synchronous functions that return strings/bytes into caller-provided buffers (e.g., `iroh_node_id`), the caller provides `out_buf + capacity` and receives `out_len`. If `capacity < out_len`, the function returns `IROH_STATUS_BUFFER_TOO_SMALL` and sets `out_len` to the required size.

### 7.3 Event Payloads

Event `data_ptr`/`data_len` fields point to native-owned memory. The caller must:
1. Copy the data if needed beyond the event processing
   2. Call `iroh_buffer_release(runtime, event.buffer)` when done

If `buffer == 0`, the data is inline/ephemeral and need not be released.

### 7.4 Handle Lifetime

- Handles are reference-counted internally
  - Freeing a handle via its typed free function (e.g., `iroh_node_free()`, `iroh_connection_free()`) decrements the reference count
  - Each handle type has its own free function rather than a single generic `iroh_handle_free()`. This prevents misuse where a wrapper accidentally frees a connection handle as a node handle. Typed free functions are safer for generated code, hand-written wrappers, and language bindings that map each handle to a distinct class.
  - In-flight operations that hold a reference keep the underlying object alive
  - It is safe (though wasteful) to free a handle while operations are pending — the operations will complete or fail gracefully

### 7.5 Uniform Payload Lifetime Model

**All returned complex payloads follow exactly one of two models — no exceptions:**

1. **Caller-buffer-out**: The caller provides `out_buf + capacity + out_len`. The FFI writes into the caller's buffer. The caller owns the memory at all times. Used for synchronous scalar results (node ID, endpoint ID, secret key export, blob ticket creation).

   2. **Leased buffer with explicit release**: The FFI allocates and returns `data_ptr + data_len + buffer`. The caller must call `iroh_buffer_release(runtime, buffer)` when done. Used for all event payloads, including `FRAME_RECEIVED`, `BLOB_READ`, `DOC_GET`, `GOSSIP_RECEIVED`, and any async result that returns variable-length data.

**There is no "borrowed until next call" model.** In particular, `iroh_node_addr_info()` and `iroh_endpoint_addr_info()` write into a caller-provided `iroh_node_addr_t` whose internal `iroh_bytes_t` fields point into a caller-provided scratch buffer:

```c
iroh_status_t iroh_node_addr_info(
    iroh_runtime_t runtime,
    uint64_t node,
    uint8_t* scratch_buf,      // caller-owned scratch space
    size_t scratch_capacity,   // size of scratch space
    size_t* scratch_used,      // bytes consumed in scratch
    iroh_node_addr_t* out_addr // pointers inside will reference scratch_buf
);
```

If `scratch_capacity` is insufficient, the function returns `IROH_STATUS_BUFFER_TOO_SMALL` and sets `scratch_used` to the required size. The `iroh_node_addr_t` fields point into `scratch_buf`, so the data lives as long as the caller's buffer.

This eliminates the weak "valid until next call" rule that is error-prone in foreign runtimes with thread pools, virtual threads, and coroutine schedulers.

### 7.6 Operation Correlation Contract

Every async submission returns an `iroh_operation_t` (a `uint64_t`). Every completion event carries both `operation` and `user_data`. The correlation rules are:

1. **`operation` is the primary native correlation key.** It is unique within a runtime for the lifetime of the runtime. Wrappers MUST use `operation` to match completions to pending requests. The FFI guarantees exactly one terminal event per operation (success, error, or cancelled).

   2. **`user_data` is a host-language passthrough.** The FFI echoes it back unchanged. It has no semantic meaning to the native layer. Host wrappers may use it for language-specific bookkeeping (e.g., Python future ID, Java `CompletableFuture` index, Go channel tag). The FFI never inspects, deduplicates, or orders by `user_data`.

   3. **One operation → one terminal event.** After the terminal event, the operation ID is retired. Subsequent `iroh_operation_cancel` calls with the same ID return `IROH_STATUS_NOT_FOUND`.

   4. **Non-terminal events (e.g., `GOSSIP_RECEIVED`, `FRAME_RECEIVED` on a long-lived subscription)** use the **handle** field (topic handle, stream handle) for correlation, not an operation ID. The original subscription operation completes once (with `GOSSIP_SUBSCRIBED`) and subsequent receives are separate operations or pushed events keyed by handle.

This contract ensures that Python wrappers multiplexing many `asyncio.Future`s, Go wrappers using channels, and Java wrappers using `CompletableFuture` all have a deterministic, race-free dispatch model.

#### 7.6.1 Event Ordering Guarantees

The event queue provides the following ordering guarantees:

1. **Per-stream ordering**: Events for the same stream handle are delivered in the order they occurred. A `SEND_COMPLETED` for write A is always delivered before `SEND_COMPLETED` for write B if A was submitted first. A `FRAME_RECEIVED` sequence preserves wire order.

   2. **Per-topic ordering**: Gossip events for the same topic handle are delivered in order. `GOSSIP_RECEIVED` events preserve the order messages were received from the gossip layer.

   3. **Connection vs stream lifecycle**: A `CONNECTION_CLOSED` event is delivered only after all stream events on that connection's streams have been delivered. Wrappers can safely assume that after processing `CONNECTION_CLOSED`, no more stream events for that connection will appear.

   4. **Cross-handle ordering**: No ordering is guaranteed between events on different handles (different streams, different topics, different connections). The event queue is a single FIFO, but producers are concurrent async tasks, so interleaving across handles is non-deterministic.

#### 7.6.2 Unified Queue: Operations and Pushed Events

The single event queue intentionally carries **both** operation completions (one-shot results) and long-lived pushed events (gossip messages, stream frames, connection-accepted notifications). This is a stable design decision, not an implementation accident.

**Rationale:** A single queue means a single poller thread and a single dispatch loop per runtime. Wrappers dispatch by `kind` and `operation`/`handle` — the same code path handles both one-shot and streaming events. Splitting into separate queues would complicate every language binding without meaningful benefit at expected event rates.

**Wrapper guidance:** Dispatchers should:
- Match on `operation` first for one-shot completions (connect, open_bi, blob_add, etc.)
  - Fall through to match on `handle` for pushed events (frame_received, gossip_received, connection_accepted)
  - Handle `IROH_EVENT_ERROR` by checking `operation` first, then `handle`

### 7.7 Event Queue Scaling

A single `mpsc::unbounded_channel` per runtime is the default event queue. For most workloads (endpoints, connections, blobs, docs) this is sufficient — event rates are modest (hundreds to low thousands per second).

For high-volume mixed workloads (gossip + many concurrent stream reads + doc sync + blob transfers), the plan provides two scaling mechanisms:

1. **Bounded queue with backpressure (v1):** Replace `unbounded_channel` with a bounded channel. When the queue is full, `iroh_poll_events` drains it and async producers apply backpressure (slow down stream reads, pause gossip delivery). The `event_queue_capacity` field in `iroh_runtime_config_t` controls the bound. The default of 4096 events is generous for typical use.

   2. **Per-category queues (future v2 extension):** If profiling shows contention, a future ABI version can add `iroh_poll_events_filtered(runtime, category_mask, out_events, max, timeout)` that drains from category-specific sub-queues (e.g., stream events, gossip events, blob events). This is additive and backward-compatible — the unfiltered `iroh_poll_events` continues to drain all queues.

**Recommendation for v1:** Start with a single bounded queue (capacity from config). Add metrics counters for queue depth and producer wait time. If real workloads show contention, the per-category extension is straightforward to add without ABI breakage.

---

## 8. Testing & Validation Strategy

### 8.1 Rust Unit Tests (`aster_transport_core`)

```
tests/
├── test_node_lifecycle.rs      # Create/close memory and persistent nodes
├── test_endpoint_config.rs     # All relay modes, secret key, ALPNs
├── test_connection.rs          # Connect, accept, bi/uni streams
├── test_blobs.rs               # Add, read, ticket, download
├── test_docs.rs                # Create, set, get, share, join
├── test_gossip.rs              # Subscribe, broadcast, receive
└── test_datagram.rs            # Send/receive datagrams
```

### 8.2 FFI Integration Tests (`aster_transport_ffi`)

These test the C ABI from Rust, simulating what a foreign language would do:

```rust
#[test]
fn test_runtime_lifecycle() {
    let mut rt: u64 = 0;
    let config = iroh_runtime_config_t { struct_size: size_of::<iroh_runtime_config_t>() as u32, ..default() };
    assert_eq!(iroh_runtime_new(&config, &mut rt), IROH_STATUS_OK);
    assert_ne!(rt, 0);
    assert_eq!(iroh_runtime_close(rt), IROH_STATUS_OK);
}

#[test]
fn test_node_memory_via_events() {
    let rt = create_runtime();
    let mut op: u64 = 0;
    assert_eq!(iroh_node_memory(rt, 42 /* user_data */, &mut op), IROH_STATUS_OK);
    
    // Poll for completion
    let mut events = [iroh_event_t::default(); 8];
    let count = iroh_poll_events(rt, events.as_mut_ptr(), 8, 5000);
    assert!(count > 0);
    assert_eq!(events[0].kind, IROH_EVENT_NODE_CREATED as u32);
    assert_eq!(events[0].user_data, 42);
    assert_ne!(events[0].handle, 0); // node handle
    
    // Use the node handle
    let node = events[0].handle;
    let mut buf = [0u8; 128];
    let mut len = 0usize;
    assert_eq!(iroh_node_id(rt, node, buf.as_mut_ptr(), 128, &mut len), IROH_STATUS_OK);
    assert!(len > 0);
    
    cleanup(rt);
}

#[test]
fn test_handle_free_safety() {
    // Start an operation, free the handle, verify no crash
    let rt = create_runtime();
    let node = create_memory_node(rt);
    let mut op: u64 = 0;
    // Start a long-running accept
    iroh_accept(rt, node, 0, &mut op);
    // Immediately free the node
    iroh_handle_free(rt, HANDLE_TYPE_NODE, node);
    // Cancel the operation
    iroh_operation_cancel(rt, op);
    // Poll — should get CANCELLED event, not a crash
    let events = poll_events(rt, 1000);
    // Verify no segfault occurred
    cleanup(rt);
}

#[test]
fn test_echo_roundtrip() {
    let rt = create_runtime();
    let (ep1, ep2) = create_connected_endpoints(rt, b"echo/1");
    
    // Open bi-stream
    let stream_event = submit_and_poll(rt, |op| iroh_open_bi(rt, conn, 0, op));
    let send_stream = stream_event.handle;
    let recv_stream = stream_event.related;
    
    // Write
    let data = b"hello world";
    submit_and_poll(rt, |op| iroh_stream_write(rt, send_stream, data.as_ptr(), data.len(), 0, op));
    submit_and_poll(rt, |op| iroh_stream_finish(rt, send_stream, 0, op));
    
    // Read on other side
    let read_event = submit_and_poll(rt, |op| iroh_stream_read_to_end(rt, peer_recv, 1024, 0, op));
    assert_eq!(read_event.data(), b"hello world");
    
    cleanup(rt);
}
```

### 8.3 C Header Validation

Generate `iroh_transport.h` using `cbindgen` and verify:
- All structs have correct alignment
  - All function signatures are present
  - No Rust-specific types leak through

```bash
cbindgen --config cbindgen.toml --crate aster_transport_ffi --output iroh_transport.h
# Then compile a C test program against it
cc -c test_ffi.c -include iroh_transport.h
```

### 8.4 Python Integration Tests

The Python test strategy serves two purposes simultaneously:

1. validate user-facing Python behavior, and
2. pressure-test `aster_transport_core` as the reusable binding substrate for later languages.

Existing test suite (`tests/test_*.py`) should pass for the stable API surface:

```bash
maturin develop
pytest tests/python/ -v
```

In addition, Python must add coverage for every completed Phase 1b feature exposed through core:

```bash
pytest tests/python/test_net.py -v
pytest tests/python/test_hooks.py -v
pytest tests/python/test_monitoring.py -v
pytest tests/python/test_remote_info.py -v
```

Required Python-specific validation areas:

- core-backed object construction and teardown (`IrohNode`, `NetClient`, connections, streams)
- datagram completion queries and receive flows
- hook registration/callback behavior, including allow/deny and post-handshake observation
- remote-info and connection-info parity with core
- screening/admission APIs if they are promoted into core during Phase 1b
- regression coverage ensuring no Python wrapper still depends on the legacy FFI implementation path

Additional FFI-specific Python test using `ctypes` directly (validates the C ABI works from Python without PyO3):

```python
# tests/python/test_ffi_ctypes.py
import ctypes

lib = ctypes.CDLL("target/release/libaster_transport_ffi.dylib")

# Test runtime lifecycle
rt = ctypes.c_uint64(0)
assert lib.iroh_runtime_new(None, ctypes.byref(rt)) == 0
assert rt.value != 0
assert lib.iroh_runtime_close(rt) == 0
```

### 8.5 Java Integration Tests

```java
@Test
void testRuntimeLifecycle() {
    try (var runtime = IrohRuntime.create()) {
        assertNotNull(runtime);
    }
}

@Test
void testNodeCreateAndClose() throws Exception {
    try (var runtime = IrohRuntime.create()) {
        var node = runtime.createMemoryNode().get(5, TimeUnit.SECONDS);
        assertNotNull(node.nodeId());
        node.close().get(5, TimeUnit.SECONDS);
    }
}

@Test  
void testBlobRoundtrip() throws Exception {
    try (var runtime = IrohRuntime.create()) {
        var node = runtime.createMemoryNode().get(5, TimeUnit.SECONDS);
        String hash = node.addBytes("hello".getBytes()).get(5, TimeUnit.SECONDS);
        byte[] data = node.readBytes(hash).get(5, TimeUnit.SECONDS);
        assertArrayEquals("hello".getBytes(), data);
    }
}

@Test
void testEchoRoundtrip() throws Exception {
    try (var runtime = IrohRuntime.create()) {
        var ep1 = runtime.createEndpoint(config("echo/1")).get(5, TimeUnit.SECONDS);
        var ep2 = runtime.createEndpoint(config("echo/1")).get(5, TimeUnit.SECONDS);
        
        // Server accepts in background
        var acceptFuture = ep2.accept();
        
        // Client connects
        var conn = ep1.connect(ep2.endpointId(), "echo/1".getBytes()).get(5, TimeUnit.SECONDS);
        var serverConn = acceptFuture.get(5, TimeUnit.SECONDS);
        
        // Open stream and echo
        var biStream = conn.openBi().get(5, TimeUnit.SECONDS);
        biStream.send().write("hello".getBytes()).get(5, TimeUnit.SECONDS);
        biStream.send().finish().get(5, TimeUnit.SECONDS);
        
        var serverBi = serverConn.acceptBi().get(5, TimeUnit.SECONDS);
        byte[] received = serverBi.recv().readToEnd(1024).get(5, TimeUnit.SECONDS);
        assertArrayEquals("hello".getBytes(), received);
    }
}
```

### 8.6 Cross-Language Validation Matrix

| Test Case | Rust Unit | FFI Integration | Python pytest | Python ctypes | Java JUnit |
|-----------|-----------|-----------------|---------------|---------------|------------|
| Runtime init/close | ✓ | ✓ | N/A | ✓ | ✓ |
| Node memory | ✓ | ✓ | ✓ | ✓ | ✓ |
| Node persistent | ✓ | ✓ | ✓ | — | ✓ |
| Blob add/read | ✓ | ✓ | ✓ | — | ✓ |
| Blob ticket/download | ✓ | ✓ | ✓ | — | ✓ |
| Doc create/set/get | ✓ | ✓ | ✓ | — | ✓ |
| Doc share/join | ✓ | ✓ | ✓ | — | ✓ |
| Gossip sub/broadcast | ✓ | ✓ | ✓ | — | ✓ |
| Endpoint create | ✓ | ✓ | ✓ | ✓ | ✓ |
| Connect/accept | ✓ | ✓ | ✓ | — | ✓ |
| Bi-stream echo | ✓ | ✓ | ✓ | — | ✓ |
| Uni-stream | ✓ | ✓ | ✓ | — | ✓ |
| Datagrams | ✓ | ✓ | ✓ | — | ✓ |
| Handle free safety | — | ✓ | — | — | ✓ |
| Cancellation | — | ✓ | — | — | ✓ |
| Custom relay URLs | ✓ | ✓ | — | — | ✓ |
| Secret key export | ✓ | ✓ | — | — | ✓ |
| **Phase 1c** | | | | | |
| Tag set/get/delete | ✓ | ✓ | ✓ | — | ✓ |
| Tag list/prefix | ✓ | ✓ | ✓ | — | ✓ |
| Blob status/has | ✓ | ✓ | ✓ | — | ✓ |
| Doc subscribe | ✓ | ✓ | ✓ | — | ✓ |
| Doc start_sync/leave | ✓ | ✓ | ✓ | — | ✓ |
| Doc download policy | ✓ | ✓ | ✓ | — | ✓ |
| Doc share with addr | ✓ | ✓ | ✓ | — | ✓ |
| Doc join+subscribe | ✓ | ✓ | ✓ | — | ✓ |

### 8.7 Stress/Soak Tests

```bash
# Run 100 concurrent connections, 1000 messages each
cargo test --release -- stress_test_concurrent_connections --ignored

# Memory leak check (macOS)
leaks --atExit -- cargo test --release -- test_echo_roundtrip

# Java: run with -Xlog:foreign to trace FFM calls
java -Xlog:foreign --enable-native-access=ALL-UNNAMED -jar iroh-java-tests.jar
```

---

## 9. Migration Guide

### 9.1 For Existing Python Users

**No API changes.** The Python public API (`iroh_python/__init__.py`) remains identical. Internal implementation changes are transparent.

### 9.2 For New Java Users

```java
// Minimal example
try (var runtime = IrohRuntime.create()) {
    var node = runtime.createMemoryNode().join();
    
    System.out.println("Node ID: " + node.nodeId());
    
    // Store a blob
    String hash = node.addBytes("Hello from Java!".getBytes()).join();
    System.out.println("Blob hash: " + hash);
    
    // Read it back
    byte[] data = node.readBytes(hash).join();
    System.out.println("Read: " + new String(data));
    
    node.close().join();
}
```

### 9.3 For C/Other Language Users

```c
#include "iroh_transport.h"

int main() {
    iroh_runtime_t rt = 0;
    iroh_runtime_config_t cfg = { .struct_size = sizeof(cfg) };
    iroh_runtime_new(&cfg, &rt);
    
    // Create memory node
    iroh_operation_t op = 0;
    iroh_node_memory(rt, 0, &op);
    
    // Poll for completion
    iroh_event_t events[8];
    size_t count = iroh_poll_events(rt, events, 8, 5000);
    // events[0].kind == IROH_EVENT_NODE_CREATED
    uint64_t node = events[0].handle;
    
    // Get node ID
    char buf[128];
    size_t len = 0;
    iroh_node_id(rt, node, (uint8_t*)buf, 128, &len);
    printf("Node ID: %.*s\n", (int)len, buf);
    
    iroh_handle_free(rt, HANDLE_TYPE_NODE, node);
    iroh_runtime_close(rt);
}
```

---

## 3b. Phase 1b: Datagram Completion, Endpoint Hooks & Monitoring

> **History:** This section was originally developed as `ffi_spec/FFI_PLAN_PATCH.md` and has
> been merged into this document as of 2026-04-04. The patch document is retained for
> historical reference only; this section is authoritative.
>
> **Scope:** All features in this section target the pinned **`iroh = 0.97.0`** surface.
> The upstream references used when designing this phase are the `v0.97.0` examples:
> `monitor-connections.rs`, `screening-connection.rs`, `remote-info.rs`, `auth-hook.rs`.

This phase extends Phase 1 to add capabilities that are useful for production multi-language use,
but in the current repository they split into three categories:

1. **Complete the datagram API** — implementable now against `iroh 0.97.0`
2. **Add endpoint hooks** — planned, but must be constrained by the actual `v0.97.0` builder-time `EndpointHooks` API
3. **Add remote-info / monitoring APIs** — planned, but should be based on the actual `ConnectionInfo` / watcher / aggregation patterns shown by `v0.97.0`
4. **Distinguish protocol-level screening from endpoint hooks** — `screening-connection.rs` demonstrates a separate acceptance control point via `ProtocolHandler::on_accepting`

### 3b.0 Document Query (Multi-Author Read-Side Filtering)

iroh-docs stores entries per `(author, key)` — multiple authors can write to the same key, and each author's entry is an independent row. The existing `doc_get_exact` requires specifying both author and key, which prevents read-side filtering patterns where you query by key and then filter by trusted author.

#### Core API (`aster_transport_core`)

Two new query methods on `CoreDoc` and a helper to read entry content:

```rust
/// Returned from query methods — contains metadata about each entry.
pub struct CoreDocEntry {
    pub author_id: String,       // hex string
    pub key: Vec<u8>,
    pub content_hash: String,    // hex string
    pub content_len: u64,
    pub timestamp: u64,          // microseconds since epoch
}

impl CoreDoc {
    /// Query all entries for an exact key, across all authors.
    pub async fn query_key_exact(&self, key: Vec<u8>) -> Result<Vec<CoreDocEntry>>;

    /// Query all entries matching a key prefix, across all authors.
    pub async fn query_key_prefix(&self, prefix: Vec<u8>) -> Result<Vec<CoreDocEntry>>;

    /// Read the content bytes for a given content hash (from a CoreDocEntry).
    pub async fn read_entry_content(&self, content_hash_hex: String) -> Result<Vec<u8>>;
}
```

#### FFI API

```c
typedef enum iroh_doc_query_mode_e {
    IROH_DOC_QUERY_KEY_EXACT = 0,
    IROH_DOC_QUERY_KEY_PREFIX = 1,
} iroh_doc_query_mode_t;

// Query entries by key (exact or prefix), returning all authors' entries.
// Emits IROH_EVENT_DOC_QUERY with packed entries in payload, entry count in event.flags.
iroh_status_t iroh_doc_query(
    iroh_runtime_t runtime,
    uint64_t doc,
    uint32_t mode,  // iroh_doc_query_mode_t
    iroh_bytes_t key,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// Read content bytes for a doc entry by its content hash.
// Emits IROH_EVENT_BLOB_READ with content bytes.
iroh_status_t iroh_doc_read_entry_content(
    iroh_runtime_t runtime,
    uint64_t doc,
    iroh_bytes_t content_hash_hex,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
```

#### Payload Format for `IROH_EVENT_DOC_QUERY`

The event payload contains entries packed sequentially. The entry count is in `event.flags`. Each entry is:

| Field | Size | Description |
|-------|------|-------------|
| `author_id_len` | 4 bytes LE | Length of author ID string |
| `author_id` | variable | Author ID (UTF-8 hex string) |
| `key_len` | 4 bytes LE | Length of key |
| `key` | variable | Key bytes |
| `content_hash_len` | 4 bytes LE | Length of content hash string |
| `content_hash` | variable | Content hash (UTF-8 hex string) |
| `content_len` | 8 bytes LE | Content length in bytes |
| `timestamp` | 8 bytes LE | Timestamp (microseconds since epoch) |

#### Target Language Usage Pattern

This enables the multi-author read-side filtering pattern:

**Python:**
```python
async def read_type(self, type_hash: str) -> TypeDef | None:
    entries = await self.doc.query_key_exact(f"types/{type_hash}".encode())
    acl_writers = self.get_acl_writers()  # cached set of trusted author IDs

    for entry in entries:
        if entry.author_id in acl_writers:
            content = await self.doc.read_entry_content(entry.content_hash)
            return deserialize(content)

    return None  # no trusted author wrote this key
```

**Java:**
```java
TypeDef readType(String typeHash) {
    var entries = doc.queryKeyExact(("types/" + typeHash).getBytes());
    var aclWriters = getAclWriters();

    for (var entry : entries) {
        if (aclWriters.contains(entry.authorId())) {
            byte[] content = doc.readEntryContent(entry.contentHash());
            return deserialize(content);
        }
    }
    return null;
}
```

### 3b.1 Datagram Completion

#### Current State

| iroh API | Status in FFI | Missing |
|----------|--------------|---------|
| `Connection::send_datagram(data)` | ✅ `iroh_connection_send_datagram` | — |
| `Connection::read_datagram()` | ✅ `iroh_connection_read_datagram` | — |
| `Connection::max_datagram_size()` → `Option<usize>` | ❌ Not exposed | Must add |
| `Connection::datagram_send_buffer_space()` → `usize` | ❌ Not exposed | Must add |

#### Required Additions

**Core (`aster_transport_core/src/lib.rs`):**

```rust
// On CoreConnection:
pub fn max_datagram_size(&self) -> Option<usize>;
pub fn datagram_send_buffer_space(&self) -> usize;
```

**FFI (`aster_transport_ffi/src/lib.rs`):**

```c
iroh_status_t iroh_connection_max_datagram_size(
    iroh_runtime_t runtime,
    iroh_connection_t connection,
    uint64_t* out_size,
    uint32_t* out_is_some
);

iroh_status_t iroh_connection_datagram_send_buffer_space(
    iroh_runtime_t runtime,
    iroh_connection_t connection,
    uint64_t* out_bytes
);
```

**Event Kind (optional):** Add `IROH_EVENT_DATAGRAM_RECEIVED = 60`.

### 3b.2 Endpoint Hooks

**Implementation status:** DONE (core adapter); FFI event-queue wiring deferred to Phase 2.

**What `iroh 0.97.0` provides (confirmed from source and examples):**
- `EndpointHooks` trait with two async callbacks:
  - `before_connect(&self, remote_addr: &EndpointAddr, alpn: &[u8]) -> BeforeConnectOutcome`
  - `after_handshake(&self, conn: &ConnectionInfo) -> AfterHandshakeOutcome`
- Hooks are installed at **endpoint builder time**: `Endpoint::builder(...).hooks(hook).bind()`
- Multiple hooks can be chained; each is invoked in order until one rejects
- Examples: `auth-hook.rs` (outgoing auth via `before_connect`, incoming auth via `after_handshake`), `monitor-connections.rs` (observation via `after_handshake`)

#### Architecture (Implemented)

The core layer provides `CoreHooksAdapter`, a channel-based bridge that implements `EndpointHooks` and forwards events to the FFI/Python layer for asynchronous reply:

```
Endpoint (iroh 0.97.0)
  → calls CoreHooksAdapter::before_connect()
    → sends CoreHookConnectInfo + oneshot::Sender<bool> via mpsc channel
    → waits for reply (with configurable timeout, default 5s)
    → returns BeforeConnectOutcome::Accept or Reject

  → calls CoreHooksAdapter::after_handshake()
    → sends CoreHookHandshakeInfo + oneshot::Sender<CoreAfterHandshakeDecision> via mpsc channel
    → waits for reply (with configurable timeout, default 5s)
    → returns AfterHandshakeOutcome::Accept or Reject{error_code, reason}

FFI/Python layer:
  → takes CoreHookReceiver via CoreNetClient::take_hook_receiver()
  → drains before_connect_rx / after_handshake_rx channels
  → sends reply via oneshot sender
```

On timeout or channel error, the adapter defaults to **Accept** (fail-open).

#### Core Types (Implemented)

```rust
/// Information about a connect attempt, passed to hooks
pub struct CoreHookConnectInfo {
    pub remote_endpoint_id: String,
    pub alpn: Vec<u8>,
}

/// Information about a completed handshake, passed to hooks
pub struct CoreHookHandshakeInfo {
    pub remote_endpoint_id: String,
    pub alpn: Vec<u8>,
    pub is_alive: bool,
}

/// Decision for after_handshake hook
pub enum CoreAfterHandshakeDecision {
    Accept,
    Reject { error_code: u32, reason: Vec<u8> },
}
```

#### Configuration (Implemented)

Hooks are enabled via `CoreEndpointConfig`:

```rust
pub struct CoreEndpointConfig {
    // ... existing fields ...
    pub enable_hooks: bool,       // Install CoreHooksAdapter at builder time
    pub hook_timeout_ms: u64,     // Timeout for hook replies (default 5000)
}
```

#### FFI Types (Partial — event kinds / enums exist, full hook reply path not yet wired)

```c
IROH_EVENT_HOOK_BEFORE_CONNECT = 70,
IROH_EVENT_HOOK_AFTER_CONNECT = 71,
IROH_EVENT_HOOK_INVOCATION_RELEASED = 72,

typedef enum iroh_hook_decision_e {
    IROH_HOOK_DECISION_ALLOW = 0,
    IROH_HOOK_DECISION_DENY = 1,
} iroh_hook_decision_t;
```

#### FFI Hook Reply Functions (TODO — deferred to Phase 2)

The core `CoreHooksAdapter` and `CoreHookReceiver` are complete. The remaining work is wiring the FFI event queue to push `IROH_EVENT_HOOK_BEFORE_CONNECT` / `IROH_EVENT_HOOK_AFTER_CONNECT` events and implementing the reply functions:

```c
iroh_status_t iroh_hook_before_connect_respond(
    iroh_runtime_t runtime,
    iroh_hook_invocation_t invocation,
    iroh_hook_decision_t decision,
    const iroh_bytes_t* reason
);

iroh_status_t iroh_hook_after_connect_respond(
    iroh_runtime_t runtime,
    iroh_hook_invocation_t invocation
);
```

**Current repository status:** the Python hook *types* exist, but the end-to-end callback dispatch path is still partial; the fully-specified standalone FFI hook registration / invocation / reply ABI from `FFI_PLAN_PATCH.md` is not yet implemented.

**Note:** Python (via PyO3 directly over core) does not need the FFI hook reply path in principle, but this repository still needs additional Python-side wiring to consume and dispatch `CoreHookReceiver` events end-to-end.

### 3b.3 Remote-Info & Monitoring

**Implementation status:** DONE. Real tracking system modeled after upstream `remote-info.rs`.

**What `iroh 0.97.0` provides (confirmed from source and examples):**
- `monitor-connections.rs` demonstrates monitoring by cloning `ConnectionInfo` in `after_handshake`, then:
  - reading `conn.alpn()`, `conn.remote_id()`
  - inspecting `conn.paths()` (returns `PathWatcher` with `.get()` for current paths and `.stream()` for updates)
  - awaiting `conn.closed()` to obtain final `(ConnectionError, ConnectionStats)` including `udp_rx.bytes`, `udp_tx.bytes`
  - reading `path.stats()` for per-path RTT
- `remote-info.rs` demonstrates a **userland `RemoteMap`** that aggregates remote state by:
  - capturing `ConnectionInfo` objects in `after_handshake`
  - tracking path updates via `conn.paths().stream()`
  - tracking connection close via `conn.closed()`
  - maintaining per-remote aggregate stats (rtt_min, rtt_max, ip_path, relay_path)
  - retaining info after connections close (with configurable retention)

#### Implementation (Done)

The `CoreMonitor` struct in `aster_transport_core` faithfully implements the `remote-info.rs` pattern:

1. **`MonitorHook`** implements `EndpointHooks::after_handshake` — captures `ConnectionInfo` and sends it to the monitor task
2. **Monitor background task** receives `ConnectionInfo` and:
   - Stores it in a `HashMap<String, RemoteInfoEntry>` keyed by remote endpoint ID
   - Spawns a task to track `conn.paths().stream()` for path change updates → updates `CoreRemoteAggregate`
   - Spawns a task to track `conn.closed()` → removes connection from active set, accumulates final `ConnectionStats`
3. **`CoreRemoteAggregate`** tracks: `rtt_min`, `rtt_max`, `ip_path`, `relay_path`, `total_bytes_sent`, `total_bytes_received`, `last_update`
4. **Query methods** (`remote_info`, `remote_info_iter`) read from the `RwLock`-protected map and return `CoreRemoteInfo` structs populated from both live connection data and historical aggregates

#### Configuration

Monitoring is enabled via `CoreEndpointConfig.enable_monitoring = true`. Both monitoring and hooks are **opt-in** — bare endpoints created via `CoreNetClient::create()` or `create_endpoint()` have neither monitoring nor hooks enabled by default. To enable monitoring, use `create_endpoint_with_config(EndpointConfig(enable_monitoring=True))` or the equivalent FFI call with the config flag set.

#### Core Types (Implemented)

```rust
pub struct CoreRemoteInfo {
    pub node_id: String,
    pub addr: Option<CoreNodeAddr>,
    pub relay_url: Option<String>,
    pub connection_type: ConnectionType,
    pub last_handshake_ns: Option<u64>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub is_connected: bool,
}

pub enum ConnectionType {
    NotConnected,
    Connecting,
    Connected(ConnectionTypeDetail),
}

pub enum ConnectionTypeDetail {
    UdpDirect,
    UdpRelay,
    Other(String),
}

pub struct CoreRemoteAggregate {
    pub rtt_min: Duration,
    pub rtt_max: Duration,
    pub ip_path: bool,
    pub relay_path: bool,
    pub last_update: SystemTime,
    pub total_bytes_sent: u64,
    pub total_bytes_received: u64,
}
```

#### FFI Functions (Implemented)

```c
iroh_status_t iroh_endpoint_remote_info(
    iroh_runtime_t runtime,
    uint64_t endpoint_or_node,
    iroh_bytes_t node_id,
    iroh_remote_info_t* out_info
);

iroh_status_t iroh_endpoint_remote_info_list(
    iroh_runtime_t runtime,
    uint64_t endpoint_or_node,
    iroh_remote_info_t* out_infos,
    size_t max_infos,
    size_t* out_count
);

iroh_status_t iroh_connection_info(
    iroh_runtime_t runtime,
    iroh_connection_t connection,
    iroh_connection_info_t* out_info
);
```

### 3b.3a Screening / Connection Admission Control

`screening-connection.rs` shows an additional surface that should not be conflated with endpoint hooks:

- protocol-level screening via `ProtocolHandler::on_accepting(Accepting) -> Result<Connection, AcceptError>`

This is distinct from `EndpointHooks` and should be modeled separately in the plan. It is the right primitive for cases like:
- maintenance mode
- rate limiting / connection quotas
- protocol-specific admission control

This may lead to a future FFI feature separate from endpoint hooks, e.g. "accept screening" or "protocol admission callbacks". **Status: analyzed, deferred.**

### 3b.4 Implementation Order for Phase 1b

1. Core: Keep `max_datagram_size` / `datagram_send_buffer_space` as the first-class completed part
2. FFI: Keep datagram completion functions as completed work
3. Audit all hook and remote-info claims against `iroh v0.97.0` examples and actual compileable APIs
4. Downgrade any placeholders/stubs in docs/status from “DONE” to “PARTIAL”, “PLACEHOLDER”, or “TODO”
5. Only after that, implement the real `v0.97.0`-compatible subset of:
   - builder-time endpoint hooks (`before_connect`, `after_handshake`)
   - monitoring / connection info based on `ConnectionInfo` tracking
   - remote aggregation modeled after `remote-info.rs`
   - protocol-level admission control based on `ProtocolHandler::on_accepting` where needed
6. Add integration tests that prove the implemented subset end-to-end

---

## 3c. Phase 1c: Registry & Publication Support

> **Added:** 2026-04-04. Driven by §11.5 of `Aster-ContractIdentity.md`.
>
> **Scope:** iroh-blobs and iroh-docs extensions required for Aster contract
> publication (Phase 9) and service registry (Phase 10). These are capabilities
> that the upstream Rust APIs already provide but which are not yet exposed
> through `aster_transport_core`, the Python bindings, or the C FFI.

### 3c.0 Motivation

The contract publication workflow (§11.4 of `Aster-ContractIdentity.md`) requires:

1. **Blob Tags** — GC protection for published contract collections. Without tags, blobs are
   garbage-collected when `TempTag` values are dropped. The current `add_bytes_as_collection`
   implementation uses `std::mem::forget(tag)` as a workaround, which leaks memory and provides
   no way to unpublish.
2. **Blob Observe** — Partial transfer detection and completion tracking for resumable downloads.
3. **Blob Status** — Check whether a blob is complete, partial, or missing before attempting to serve it.
4. **Doc Subscribe** — Live event stream for `InsertRemote` / `ContentReady` events, needed for
   registry change notifications.
5. **Doc Sync Lifecycle** — Explicit `start_sync()` / `leave()` for controlling when a document
   actively syncs with peers.
6. **Doc Download Policy** — Selective sync by key prefix (`NothingExcept` policy) to avoid
   pulling all service data when only `_aster/` keys are needed.
7. **Doc Share with Full Addr** — Include relay URL and direct addresses in `DocTicket` for
   bootstrapping remote nodes that have no prior addressing information.

### 3c.1 Blob Tags API

**Why P0:** Without proper tags, published contract collections will be garbage-collected on
the next GC cycle. The `mem::forget` hack currently used in `add_bytes_as_collection` leaks
memory and makes unpublishing impossible.

#### Current State

The upstream `iroh-blobs` crate exposes a full `Tags` API via `store.tags()`:
- `set(name, hash_and_format)` — create/update a named tag
- `get(name)` → `Option<TagInfo>` — read a tag
- `delete(name)` → `u64` — delete a tag (returns count removed)
- `list()` / `list_prefix(prefix)` — enumerate tags
- `list_hash_seq()` — list only HashSeq-format tags

None of these are exposed in `aster_transport_core` or any binding.

#### Core API (`aster_transport_core`)

```rust
/// Information about a tag
pub struct CoreTagInfo {
    pub name: String,       // Tag name (UTF-8)
    pub hash: String,       // Hash hex string
    pub format: String,     // "raw" or "hash_seq"
}

impl CoreBlobsClient {
    /// Set a named tag for GC protection.
    /// If the tag already exists, it is overwritten.
    pub async fn tag_set(&self, name: String, hash_hex: String, format: String) -> Result<()>;

    /// Get a tag by name. Returns None if not found.
    pub async fn tag_get(&self, name: String) -> Result<Option<CoreTagInfo>>;

    /// Delete a tag by name. Returns the number of tags removed (0 or 1).
    pub async fn tag_delete(&self, name: String) -> Result<u64>;

    /// Delete all tags matching a prefix. Returns count removed.
    pub async fn tag_delete_prefix(&self, prefix: String) -> Result<u64>;

    /// List all tags. Returns a vector of tag info.
    pub async fn tag_list(&self) -> Result<Vec<CoreTagInfo>>;

    /// List tags matching a prefix.
    pub async fn tag_list_prefix(&self, prefix: String) -> Result<Vec<CoreTagInfo>>;

    /// List only HashSeq-format tags (collections).
    pub async fn tag_list_hash_seq(&self) -> Result<Vec<CoreTagInfo>>;
}
```

#### FFI API

```c
// ─── Blob Tags (Phase 1c) ───

// Set a named tag for GC protection.
// format: 0=raw, 1=hash_seq
// Emits IROH_EVENT_TAG_SET on success.
iroh_status_t iroh_tags_set(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* name_ptr, size_t name_len,
    const uint8_t* hash_hex_ptr, size_t hash_hex_len,
    uint32_t format,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// Get a tag by name.
// Emits IROH_EVENT_TAG_GET with tag info in payload, or IROH_EVENT_TAG_GET with
// status NOT_FOUND if the tag does not exist.
iroh_status_t iroh_tags_get(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* name_ptr, size_t name_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// Delete a tag by name.
// Emits IROH_EVENT_TAG_DELETED with count in event.flags.
iroh_status_t iroh_tags_delete(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* name_ptr, size_t name_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// List tags matching a prefix (empty prefix = all tags).
// Emits IROH_EVENT_TAG_LIST with packed tag entries in payload.
iroh_status_t iroh_tags_list_prefix(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* prefix_ptr, size_t prefix_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
```

#### Event Kinds (new)

```c
IROH_EVENT_TAG_SET = 36,
IROH_EVENT_TAG_GET = 37,
IROH_EVENT_TAG_DELETED = 38,
IROH_EVENT_TAG_LIST = 39,
```

#### Impact on Existing Code

The `add_bytes_as_collection` implementation must be updated to use proper tags instead of
`std::mem::forget`. The new flow:

1. `add_bytes` → gets `TempTag` with blob hash
2. Build `Collection`, store it → gets `TempTag` with collection hash
3. Call `tag_set("aster/contract/{name}@{hash}", collection_hash, "hash_seq")`
4. Drop both `TempTag` values — the named tag now protects the collection and its children

To unpublish: `tag_delete("aster/contract/{name}@{hash}")` → GC reclaims the blobs.

### 3c.2 Blob Status & Observe

**Why P1:** Needed for smart download decisions — check if a blob is already present before
fetching, and track partial download progress.

#### Core API

```rust
/// Status of a blob in the local store
pub enum CoreBlobStatus {
    /// Blob is not present at all
    NotFound,
    /// Blob is partially present (some chunks available)
    Partial { size: u64 },
    /// Blob is complete
    Complete { size: u64 },
}

impl CoreBlobsClient {
    /// Check the status of a blob in the local store.
    pub async fn blob_status(&self, hash_hex: String) -> Result<CoreBlobStatus>;

    /// Check if a blob is complete in the local store.
    pub async fn blob_has(&self, hash_hex: String) -> Result<bool>;
}
```

#### FFI API

```c
// Check blob status. Synchronous — writes directly to out params.
// out_status: 0=not_found, 1=partial, 2=complete
// out_size: blob size in bytes (0 if not_found)
iroh_status_t iroh_blobs_status(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* hash_hex_ptr, size_t hash_hex_len,
    uint32_t* out_status,
    uint64_t* out_size
);

// Check if blob is complete. Synchronous.
iroh_status_t iroh_blobs_has(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* hash_hex_ptr, size_t hash_hex_len,
    uint32_t* out_has  // 1 = complete, 0 = not complete
);
```

### 3c.3 Doc Subscribe (Live Events)

**Why P1:** The registry needs to react to remote document changes in real time. Without live
events, the only option is polling, which is both slow and wasteful.

#### Core API

```rust
/// A document event from the live subscription stream
pub enum CoreDocEvent {
    /// A local insert was made
    InsertLocal {
        author_id: String,
        key: Vec<u8>,
        content_hash: String,
        content_len: u64,
        timestamp: u64,
    },
    /// A remote insert was received via sync
    InsertRemote {
        author_id: String,
        key: Vec<u8>,
        content_hash: String,
        content_len: u64,
        timestamp: u64,
        from_endpoint_id: String,
    },
    /// Content for an entry is now available locally
    ContentReady {
        content_hash: String,
    },
    /// A neighbor (sync peer) came online
    NeighborUp {
        endpoint_id: String,
    },
    /// A neighbor went offline
    NeighborDown {
        endpoint_id: String,
    },
    /// Sync with a peer finished
    SyncFinished {
        endpoint_id: String,
    },
}

impl CoreDoc {
    /// Subscribe to live document events. Returns a receiver that yields events
    /// as they occur. The subscription is active until the receiver is dropped.
    pub async fn subscribe(&self) -> Result<CoreDocEventReceiver>;
}

pub struct CoreDocEventReceiver { /* wraps iroh_docs event stream */ }

impl CoreDocEventReceiver {
    /// Receive the next event. Returns None when the subscription ends.
    pub async fn recv(&self) -> Result<Option<CoreDocEvent>>;
}
```

#### FFI API

```c
// Subscribe to live document events.
// Emits IROH_EVENT_DOC_SUBSCRIBED once, then pushes DOC_EVENT_* events
// keyed by the doc handle.
iroh_status_t iroh_doc_subscribe(
    iroh_runtime_t runtime,
    uint64_t doc,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// Receive next document event (long-poll, like gossip_recv).
// Emits IROH_EVENT_DOC_EVENT with subtype indicating event kind.
iroh_status_t iroh_doc_event_recv(
    iroh_runtime_t runtime,
    uint64_t doc,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
```

#### Event Kinds (new)

```c
IROH_EVENT_DOC_SUBSCRIBED = 47,
IROH_EVENT_DOC_EVENT = 48,
```

The `subtype` field of `IROH_EVENT_DOC_EVENT` distinguishes event kinds:

| Subtype | Name | Payload |
|---------|------|---------|
| 0 | InsertLocal | Packed entry (same format as DOC_QUERY) |
| 1 | InsertRemote | Packed entry + from_endpoint_id |
| 2 | ContentReady | content_hash hex |
| 3 | NeighborUp | endpoint_id hex |
| 4 | NeighborDown | endpoint_id hex |
| 5 | SyncFinished | endpoint_id hex |

### 3c.4 Doc Sync Lifecycle

**Why P1:** Without explicit sync control, documents begin syncing immediately on join and
cannot be paused. The registry needs to control when sync happens to avoid downloading
contract data before ACL validation.

#### Core API

```rust
impl CoreDoc {
    /// Start syncing this document with the given peers.
    /// If already syncing, this is a no-op.
    pub async fn start_sync(&self, peers: Vec<String>) -> Result<()>;

    /// Stop syncing this document. Existing sync connections are dropped.
    pub async fn leave(&self) -> Result<()>;
}
```

#### FFI API

```c
// Start syncing a document with specified peers.
// peers is an array of endpoint_id hex strings.
// Emits IROH_EVENT_UNIT_RESULT on completion.
iroh_status_t iroh_doc_start_sync(
    iroh_runtime_t runtime,
    uint64_t doc,
    const iroh_bytes_t* peers, size_t peers_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// Leave (stop syncing) a document.
// Emits IROH_EVENT_UNIT_RESULT on completion.
iroh_status_t iroh_doc_leave(
    iroh_runtime_t runtime,
    uint64_t doc,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
```

### 3c.5 Doc Download Policy

**Why P2:** Allows selective content sync — e.g., only download `_aster/` prefix keys without
pulling all service data. This is an optimisation for large registries.

#### Core API

```rust
/// Download policy for a document
pub enum CoreDownloadPolicy {
    /// Download everything (default)
    Everything,
    /// Download nothing except entries matching these key prefixes
    NothingExcept { prefixes: Vec<Vec<u8>> },
    /// Download everything except entries matching these key prefixes
    EverythingExcept { prefixes: Vec<Vec<u8>> },
}

impl CoreDoc {
    /// Set the download policy for this document.
    pub async fn set_download_policy(&self, policy: CoreDownloadPolicy) -> Result<()>;

    /// Get the current download policy.
    pub async fn get_download_policy(&self) -> Result<CoreDownloadPolicy>;
}
```

#### FFI API

```c
typedef enum iroh_download_policy_mode_e {
    IROH_DOWNLOAD_POLICY_EVERYTHING = 0,
    IROH_DOWNLOAD_POLICY_NOTHING_EXCEPT = 1,
    IROH_DOWNLOAD_POLICY_EVERYTHING_EXCEPT = 2,
} iroh_download_policy_mode_t;

// Set download policy for a document.
// prefixes: array of key prefixes (only used when mode != EVERYTHING)
iroh_status_t iroh_doc_set_download_policy(
    iroh_runtime_t runtime,
    uint64_t doc,
    uint32_t mode,  // iroh_download_policy_mode_t
    const iroh_bytes_t* prefixes, size_t prefixes_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
```

### 3c.6 Doc Share with Full Address

**Why P2:** The existing `share()` returns a `DocTicket` containing only the node ID
(`AddrInfoOptions::Id`). Remote nodes need full addressing information (relay URL, direct
addresses) to bootstrap the first connection without prior discovery state.

#### Core API

```rust
impl CoreDoc {
    /// Share this document with full addressing information included in the ticket.
    /// This is needed when the recipient has no prior knowledge of the sharer's address.
    pub async fn share_with_addr(&self, mode: String) -> Result<String>;
}
```

This calls `doc.share(share_mode, AddrInfoOptions::RelayAndAddresses)` instead of
`AddrInfoOptions::Id`.

#### FFI API

```c
// Share a document with full address info in the ticket.
// mode: 0=read, 1=write
// Emits IROH_EVENT_DOC_SHARED with ticket string in payload.
iroh_status_t iroh_doc_share_with_addr(
    iroh_runtime_t runtime,
    uint64_t doc,
    uint32_t mode,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
```

### 3c.7 Doc Import and Subscribe (Race-Free Join)

**Why P2:** When joining a registry namespace, there's a race between subscribing for live
events and the initial sync delivering the first batch of entries. `import_and_subscribe`
atomically joins the document and starts the event subscription before the first sync
completes, ensuring no events are missed.

#### Core API

```rust
impl CoreDocsClient {
    /// Join a document and immediately subscribe to its events.
    /// This is atomic: the subscription is active before the first sync starts,
    /// so no InsertRemote events are missed.
    pub async fn join_and_subscribe(&self, ticket_str: String)
        -> Result<(CoreDoc, CoreDocEventReceiver)>;
}
```

#### FFI API

```c
// Join a document and immediately subscribe to events.
// Emits IROH_EVENT_DOC_JOINED with the doc handle,
// then automatically begins emitting DOC_EVENT_* events for the doc handle.
iroh_status_t iroh_docs_join_and_subscribe(
    iroh_runtime_t runtime,
    uint64_t node,
    const uint8_t* ticket_ptr, size_t ticket_len,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
```

### 3c.8 Priority Summary

| Feature | Priority | Aster Phase | Status |
|---------|----------|-------------|--------|
| Blob Tags (`set`, `get`, `delete`, `list`) | P0 | Phase 9 (contract publication) | ✅ Done (Phase 1c.1) |
| Blob Status / Has | P1 | Phase 10 (smart fetching) | ✅ Done (Phase 1c.3) |
| Doc Subscribe (live events) | P1 | Phase 10 (registry notifications) | ✅ Done (Phase 1c.4) |
| Doc Sync Lifecycle (`start_sync`, `leave`) | P1 | Phase 10 (sync control) | ✅ Done (Phase 1c.5) |
| Doc Download Policy | P2 | Phase 10 (efficient sync) | ✅ Done (Phase 1c.6) |
| Doc Share with Full Addr | P2 | Phase 10 (bootstrapping) | ✅ Done (Phase 1c.7) |
| Doc Import and Subscribe | P2 | Phase 10 (race-free join) | ✅ Done (Phase 1c.8) |

---

## 3d. Phase 1d: Blob Transfer Observability

These two capabilities were listed in `Aster-ContractIdentity.md §11.5` as pending. They were
inadvertently omitted from the Phase 1c checklist. Both are needed before §11.4 (contract
publication/consumption) can be fully implemented.

### 3d.1 `observe()` — Blob Bitfield Observation (P1)

**Why P1:** Callers fetching blobs from remote peers need to know when a blob has fully arrived
locally before reading it. `blob_observe_complete` is the canonical "wait until ready" primitive.
`blob_observe_snapshot` lets callers check the current state without blocking.

**Upstream model:** `store.blobs().observe(hash)` returns an `ObserveProgress` stream of
`Bitfield` values showing which byte ranges are locally available.

- Awaiting `ObserveProgress` directly (via `IntoFuture`) returns a snapshot of the **current**
  bitfield without blocking for further changes.
- `.await_completion()` drives the stream until a `Bitfield` with `is_complete() == true` is
  observed. For a blob that is already locally complete this resolves immediately. If the stream
  ends before completion (no active download, node shut down), it returns an error.

**`Bitfield` fields exposed by the Core layer:**
- `is_complete: bool` — all chunks are present and verified
- `size: u64` — total blob size in bytes; **0 if the header chunk has not yet been fetched**

**Behavior for an unknown or not-yet-downloaded hash:**
- `blob_observe_snapshot` returns `IROH_STATUS_OK` with `is_complete=0, size=0` — the blob is
  simply not present yet; this is not an error.
- `blob_observe_complete` will eventually error with `IROH_STATUS_INTERNAL` if no active download
  for that hash exists, because the stream will terminate without ever reaching completion.

#### Core API

```rust
/// Snapshot of the current download state for a blob.
pub struct CoreBlobObserveResult {
    /// True when all chunks are present and verified.
    pub is_complete: bool,
    /// Total size in bytes; 0 if the header chunk has not yet arrived.
    pub size: u64,
}

impl CoreBlobsClient {
    /// Single bitfield snapshot via `store.blobs().observe(hash).await`.
    /// Returns Ok even if the blob is unknown — is_complete will be false, size will be 0.
    pub async fn blob_observe_snapshot(&self, hash_hex: String) -> Result<CoreBlobObserveResult>;

    /// Wait until the blob is fully local via `store.blobs().observe(hash).await_completion()`.
    /// Resolves immediately if the blob is already complete.
    /// Errors if the observation stream ends without completion (no active download).
    pub async fn blob_observe_complete(&self, hash_hex: String) -> Result<()>;
}
```

#### FFI API

```c
// ─── Blob Observe (Phase 1d) ───

// Snapshot the current download state for a blob.
// Synchronous — writes directly to out params.
// out_is_complete: 1 = all chunks present and verified, 0 = incomplete or not present
// out_size:        total size in bytes; 0 if header chunk not yet fetched or blob unknown
//
// Returns IROH_STATUS_OK even if the blob is not locally present (is_complete=0, size=0).
// Returns IROH_STATUS_NOT_FOUND if the node handle is unknown.
// Returns IROH_STATUS_INVALID_ARGUMENT if any pointer is NULL.
// Returns IROH_STATUS_INTERNAL if the hash cannot be parsed.
int32_t iroh_blobs_observe_snapshot(
    iroh_runtime_t runtime,
    iroh_node_t    node,
    const uint8_t* hash_hex_ptr, uintptr_t hash_hex_len,
    uint32_t*      out_is_complete,
    uint64_t*      out_size
);

// Wait until a blob is fully downloaded locally.
// Async — returns immediately; completion is signalled via the event queue.
// Emits IROH_EVENT_BLOB_OBSERVE_COMPLETE when the blob becomes fully available.
//   event.operation = the operation handle returned in *out_operation
//   event.user_data = user_data passed here
//   event.handle    = node handle
//   event.status    = IROH_STATUS_OK
// Emits IROH_EVENT_ERROR if the observation stream ends before the blob completes
// (e.g., no active download, or node shut down).
//
// Resolves immediately (with IROH_EVENT_BLOB_OBSERVE_COMPLETE) if the blob is already complete.
// Supports cancellation: cancel the returned operation handle to stop waiting.
//
// Returns IROH_STATUS_NOT_FOUND if the node handle is unknown.
// Returns IROH_STATUS_INVALID_ARGUMENT if hash_hex_ptr or out_operation is NULL.
int32_t iroh_blobs_observe_complete(
    iroh_runtime_t    runtime,
    iroh_node_t       node,
    const uint8_t*    hash_hex_ptr, uintptr_t hash_hex_len,
    uint64_t          user_data,
    iroh_operation_t* out_operation
);
```

#### Event Kinds (new)

```c
IROH_EVENT_BLOB_OBSERVE_COMPLETE = 56,  // emitted by iroh_blobs_observe_complete
```

### 3d.2 Remote API — Local Availability Info (P1)

**Why P1:** `blob_observe_snapshot` reports whether the blob is complete; `blob_local_info`
reports *how many bytes* are already held locally. This is the right primitive for resumable
downloads: before re-fetching, check how much data you already have so you can request only the
missing ranges.

**Upstream model:** `store.remote().local(HashAndFormat::raw(hash))` returns a `LocalInfo` struct:
- `is_complete()` — all requested data is locally available
- `local_bytes()` — bytes already present on this node

**Behavior for an unknown or not-yet-downloaded hash:**
- Returns `IROH_STATUS_OK` with `is_complete=0, local_bytes=0` — not an error.

#### Core API

```rust
/// Local availability info for a blob from the Remote API.
pub struct CoreBlobLocalInfo {
    /// True when all bytes are present locally.
    pub is_complete: bool,
    /// Number of bytes already held locally (may be less than total size).
    pub local_bytes: u64,
}

impl CoreBlobsClient {
    /// Check local availability via `store.remote().local(HashAndFormat::raw(hash))`.
    /// Returns Ok even for unknown hashes — is_complete=false, local_bytes=0.
    pub async fn blob_local_info(&self, hash_hex: String) -> Result<CoreBlobLocalInfo>;
}
```

#### FFI API

```c
// ─── Blob Local Info (Phase 1d) ───

// Check how many bytes of a blob are held locally.
// Synchronous — writes directly to out params.
// out_is_complete: 1 = fully available locally, 0 = partial or not present
// out_local_bytes: number of bytes already held locally (0 if blob is unknown)
//
// Returns IROH_STATUS_OK even if the blob is not locally present (is_complete=0, local_bytes=0).
// Returns IROH_STATUS_NOT_FOUND if the node handle is unknown.
// Returns IROH_STATUS_INVALID_ARGUMENT if any pointer is NULL.
// Returns IROH_STATUS_INTERNAL if the hash cannot be parsed.
int32_t iroh_blobs_local_info(
    iroh_runtime_t runtime,
    iroh_node_t    node,
    const uint8_t* hash_hex_ptr, uintptr_t hash_hex_len,
    uint32_t*      out_is_complete,
    uint64_t*      out_local_bytes
);
```

### 3d.3 Implementation Notes

**`ObserveProgress` delegation pattern:** `store.blobs().observe(hash)` is called once per
invocation. For `blob_observe_snapshot`, awaiting the `ObserveProgress` directly (via its
`IntoFuture` impl) yields the current `Bitfield` snapshot without subscribing to further updates.
For `blob_observe_complete`, `.await_completion()` drives the stream to completion — internally
it polls the stream until `bitfield.is_complete()` is true or the stream ends with an error.

**`HashAndFormat` wrapper:** `blob_local_info` must wrap the hash as `HashAndFormat::raw(hash)`.
The Remote API is format-aware; using the wrong format would query a different hash slot.

**Comparison with `blob_status`:** Phase 1c added `blob_status` (not_found/partial/complete enum).
The Phase 1d additions are complementary:
- `blob_status` → fast enum check, no byte count for partial
- `blob_local_info` → byte-accurate local count, useful for resumable transfer decisions
- `blob_observe_snapshot` → same completeness/size info but from the bitfield stream (consistent
  with `observe_complete`'s internal model)

**`size` vs `local_bytes`:** `BlobObserveResult.size` is the *total* declared blob size (0 if
unknown); `BlobLocalInfo.local_bytes` is the bytes *we have*. For a partial download these differ.

### 3c.9 Phase 1c Implementation Notes

**Blob Tags:** The upstream `iroh-blobs` `Tags` API is accessed via `store.tags()` which
returns a `&Tags` reference. The `BlobStore` type already exposes this. Since `CoreBlobsClient`
holds `pub store: BlobStore`, adding tag methods is straightforward delegation.

**Doc Subscribe:** The upstream `iroh-docs` `Doc` type has a `subscribe()` method that returns
an event stream. The core wrapper needs to convert the upstream event types to `CoreDocEvent`.

**Doc Sync Lifecycle:** The upstream `Doc` has `start_sync(peers)` and `leave()` methods.
Direct delegation from `CoreDoc`.

**Download Policy:** The upstream `Doc` has `set_download_policy()` / `get_download_policy()`.
The policy types need to be mapped from upstream `DownloadPolicy` to `CoreDownloadPolicy`.

**Share with Full Addr:** The existing `CoreDoc::share()` calls
`doc.share(mode, AddrInfoOptions::Id)`. The new variant calls
`doc.share(mode, AddrInfoOptions::RelayAndAddresses)`.

**Import and Subscribe:** This requires calling `docs.api().import_namespace(capability)` and
`doc.subscribe()` in sequence before the first sync completes. The upstream API may provide
an atomic variant; if not, subscribe immediately after import before yielding to the event loop.

---

## 3e. Phase 1e: Unified Aster Node — Custom ALPNs on the Shared iroh Router

### Overview

Aster requires blobs (contract publication), docs (service registry), and gossip (producer-mesh coordination) alongside its own ALPNs (`aster/1`, `aster.consumer_admission`, `aster.producer_admission`). Running these on separate iroh endpoints wastes relay bandwidth and creates multiple node IDs. Phase 1e unifies everything on **one endpoint, one node ID** by extending `CoreNode` to register custom-ALPN `ProtocolHandler` instances on iroh's `Router`, forwarding accepted connections to the host language via a bounded tokio channel.

### 3e.1 Core API (`aster_transport_core`)

```rust
/// Queue-backed ProtocolHandler registered on the Router for each custom ALPN.
/// Forwards accepted connections to a shared bounded channel.
#[derive(Debug, Clone)]
struct AsterQueueHandler {
    alpn: Vec<u8>,
    tx: mpsc::Sender<(Vec<u8>, Connection)>,
}

impl ProtocolHandler for AsterQueueHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let _ = self.tx.send((self.alpn.clone(), conn)).await;
        Ok(())
    }
}

impl CoreNode {
    /// Create an in-memory node serving blobs + docs + gossip + custom ALPNs.
    pub async fn memory_with_alpns(
        aster_alpns: Vec<Vec<u8>>,
        endpoint_config: Option<CoreEndpointConfig>,
    ) -> Result<Self>;

    /// Persistent (FsStore-backed) counterpart.
    pub async fn persistent_with_alpns(
        path: String,
        aster_alpns: Vec<Vec<u8>>,
        endpoint_config: Option<CoreEndpointConfig>,
    ) -> Result<Self>;

    /// Wait for the next incoming custom-ALPN connection. Returns
    /// (alpn_bytes, connection). Returns Err once the node closes.
    pub async fn accept_aster(&self) -> Result<(Vec<u8>, CoreConnection)>;

    /// Take the hooks receiver (one-shot; None if not enabled / already taken).
    pub fn take_hook_receiver(&self) -> Option<CoreHookReceiver>;
}
```

The `endpoint_config` parameter enables the same `enable_hooks` / `enable_monitoring` / `secret_key` / `relay_mode` / `bind_addr` surface as `CoreNetClient::create_with_config`. The `alpns` field on the config is ignored — iroh's `Router::spawn()` calls `endpoint.set_alpns(...)` with the union of all registered protocol ALPNs automatically.

### 3e.2 Concurrency Contract

| Construct | Where it lives | Crosses FFI? |
|-----------|----------------|:------------:|
| iroh `Router` + its accept-loop task | tokio, inside CoreNode | No |
| `AsterQueueHandler` ProtocolHandlers | tokio, inside CoreNode | No |
| Bounded `mpsc::channel(256)` | tokio, inside CoreNode | No |
| `async fn accept_aster()` | core, exposed as tokio-async method | Via existing bridge |
| Returned `(Vec<u8>, CoreConnection)` | host language → used via existing IrohConnection | Yes (existing wrapper) |

- **Channel**: Bounded (`mpsc::channel(256)`). Back-pressure inside `AsterQueueHandler::accept()` blocks the per-ALPN Router task only; blobs/docs/gossip remain unaffected.
- **Receiver**: `tokio::sync::Mutex<mpsc::Receiver<...>>` — safe for multi-caller (calls serialize). Internal to Rust; never crosses FFI.
- **Cancellation**: Mutex released on drop; unreceived `(alpn, conn)` remains queued for the next caller. No lost connections.
- **Connection lifetime**: iroh `Connection` is `Arc`-backed `Clone`. The ProtocolHandler sends via the channel; Router drops its clone on return; the clone in the channel keeps the connection alive until `accept_aster()` hands it to the host.
- **Shutdown**: `router.shutdown()` drains protocol handlers, drops senders → `recv()` returns None → `accept_aster()` returns Err. Then closes the endpoint.

### 3e.3 FFI API

**New event kind:**

```c
IROH_EVENT_ASTER_ACCEPTED = 65,
```

**New functions:**

```c
// Create a full node with blobs/docs/gossip + custom aster ALPNs.
// Emits IROH_EVENT_NODE_CREATED with the node handle on success.
iroh_status_t iroh_node_memory_with_alpns(
    iroh_runtime_t runtime,
    const uint8_t* const* alpns,      // array of pointers to ALPN byte strings
    const size_t* alpn_lens,          // parallel array of lengths
    size_t alpn_count,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// Persistent variant (FsStore at `path`).
iroh_status_t iroh_node_persistent_with_alpns(
    iroh_runtime_t runtime,
    const char* path,
    const uint8_t* const* alpns,
    const size_t* alpn_lens,
    size_t alpn_count,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// Pull the next aster-ALPN connection from the node's queue.
// Long-poll: spawns a tokio task, emits IROH_EVENT_ASTER_ACCEPTED with:
//   handle      = connection handle
//   data_ptr/len = ALPN bytes
//   buffer      = lease to release via iroh_buffer_release
iroh_status_t iroh_node_accept_aster(
    iroh_runtime_t runtime,
    uint64_t node,
    uint64_t user_data,
    iroh_operation_t* out_operation
);

// Take the hook receiver from the node (one-shot, like existing
// iroh_endpoint_take_hook_receiver).
iroh_status_t iroh_node_take_hook_receiver(
    iroh_runtime_t runtime,
    uint64_t node,
    uint64_t user_data,
    iroh_operation_t* out_operation
);
```

**Implementation** mirrors `iroh_accept` (ffi/src/lib.rs:1824): load runtime, look up node handle, `new_operation()`, `runtime.spawn()` an async task calling `core_node.accept_aster().await`, emit the completion event. Connection handle via `bridge.connections.insert(conn)`. ALPN bytes via existing variable-length payload path.

### 3e.4 Memory Ownership

- ALPN buffer in `IROH_EVENT_ASTER_ACCEPTED`: follows existing event-payload rules (§7.3) — host calls `iroh_buffer_release(event.buffer)` when done.
- Connection handle: follows existing handle-lifetime rules (§7.4) — freed via `iroh_connection_close`.

### 3e.5 Gate 0 Integration

When any admission gate is active, the node is built with `enable_hooks=true` on the `CoreEndpointConfig`. iroh's endpoint hooks fire at the QUIC handshake layer — **before ALPN dispatch** — so Gate 0 (`MeshEndpointHook`, trust spec §3.3) gates *all* protocols (blobs, docs, gossip, aster/1, admission) uniformly.

The host language takes the `HookReceiver` from the node and runs a hook loop that:
1. Auto-accepts `before_connect` (the peer's EndpointId is not authenticated yet).
2. Applies the `MeshEndpointHook` allowlist at `after_handshake` (authenticated peer ID).
3. Admission ALPNs are always allowed (so unadmitted peers can present credentials).

### 3e.6 Target-Language Usage Pattern

**C (FFI):**
```c
// Create node with aster ALPNs
const uint8_t* alpns[] = { (uint8_t*)"aster/1", (uint8_t*)"aster.consumer_admission" };
size_t lens[] = { 7, 26 };
iroh_node_memory_with_alpns(rt, alpns, lens, 2, 0, &op);
// ... poll for IROH_EVENT_NODE_CREATED ...

// Accept loop
while (running) {
    iroh_node_accept_aster(rt, node_handle, 0, &op);
    // ... poll for IROH_EVENT_ASTER_ACCEPTED ...
    // event.handle = connection, event.data_ptr/data_len = ALPN bytes
    iroh_buffer_release(event.buffer);
    // dispatch by ALPN ...
}
```

**Python (PyO3):**
```python
node = await IrohNode.memory_with_alpns(
    [b"aster/1", b"aster.consumer_admission"],
    EndpointConfig(enable_hooks=True),
)
receiver = await node.take_hook_receiver()
asyncio.create_task(hook.run_hook_loop(receiver))  # Gate 0
while True:
    alpn, conn = await node.accept_aster()
    if alpn == b"aster/1":
        asyncio.create_task(server.handle_connection(conn))
    elif alpn == b"aster.consumer_admission":
        asyncio.create_task(handle_consumer_admission(conn, ...))
```

**Java (FFM):**
```java
var op = arena.allocate(LAYOUT_OP);
nativeBindings.iroh_node_memory_with_alpns(rt, alpnPtrs, alpnLens, 2, 0, op);
long nodeHandle = poller.awaitResult(op).handle();

// Accept loop (virtual thread)
Thread.startVirtualThread(() -> {
    while (running) {
        var acceptOp = arena.allocate(LAYOUT_OP);
        nativeBindings.iroh_node_accept_aster(rt, nodeHandle, 0, acceptOp);
        var event = poller.awaitResult(acceptOp);
        byte[] alpn = event.dataSlice();
        long connHandle = event.handle();
        nativeBindings.iroh_buffer_release(event.buffer());
        dispatch(alpn, connHandle);
    }
});
```

---

## 3f. Phase 1f: Cross-Language Contract Identity, Framing & Signing

### Overview

**Why:** Every language binding must produce byte-identical output for contract identity hashes, wire framing, and credential signing bytes. If Python, Java, and Go each implement their own canonical serialization independently, subtle divergences (float rounding, key ordering, NFC normalization) are inevitable. A single authoritative Rust implementation called through the C ABI eliminates this class of bugs entirely.

**What:** Six synchronous C FFI functions exposing:
- Contract ID computation (BLAKE3 hash of canonical bytes from a `ServiceContract` JSON)
- Canonical byte serialization for `ServiceContract`, `TypeDef`, and `MethodDef`
- Wire frame encoding/decoding (length-prefixed with flags byte)
- Credential signing bytes (producer and consumer enrollment)
- General-purpose canonical JSON normalization (sorted keys, compact)

**Pattern:** These are pure synchronous functions — no runtime handle, no event queue, no async. Data in, result out, status code returned. This is the simplest possible FFI pattern: the caller provides an output buffer, and the function writes to it.

### Core API (Rust)

The core logic lives in four modules under `core/src/`:

| Module | Purpose |
|--------|---------|
| `core/src/canonical.rs` | Fory XLANG canonical byte encoding primitives (varint, zigzag, string, bytes, list, optional) |
| `core/src/contract.rs` | Contract identity types (`ServiceContract`, `TypeDef`, `MethodDef`, `FieldDef`, etc.), canonical serialization, BLAKE3 hashing, Tarjan SCC for type dependency ordering |
| `core/src/framing.rs` | Wire framing: `encode_frame` / `decode_frame` with length prefix and flags byte |
| `core/src/signing.rs` | Credential signing bytes (`EnrollmentCredentialData`, `ConsumerEnrollmentCredentialData`), canonical JSON for attributes, ed25519 verification |

**Key types:**

- `ServiceContract` — name, version, methods, types, capabilities, scope config
- `TypeDef` — message/enum/union type definition with fields and enum values
- `MethodDef` — RPC method with pattern (unary/server-stream/client-stream/bidi), request/response types, capabilities
- `FieldDef` — typed field within a TypeDef (supports primitives, refs, self-refs, containers)
- `EnrollmentCredentialData` — producer credential with endpoint_id, root_pubkey, expires_at, attributes
- `ConsumerEnrollmentCredentialData` — consumer credential with type code (policy/ott), optional endpoint_id, optional nonce
- `CredentialData` — tagged enum (`"kind": "producer"` or `"kind": "consumer"`) for JSON dispatch

**Key functions:**

- `contract::compute_contract_id_from_json(json_str) -> Result<String>` — parse ServiceContract JSON, compute canonical bytes, return 64-char hex BLAKE3 hash
- `contract::canonical_bytes_from_json(type_name, json_str) -> Result<Vec<u8>>` — parse JSON for named type, return canonical bytes
- `framing::encode_frame(payload, flags) -> Result<Vec<u8>>` — encode `[4B LE length][flags][payload]`
- `framing::decode_frame(data) -> Result<(payload, flags, consumed)>` — decode one frame from buffer
- `signing::canonical_signing_bytes_from_json(json_str) -> Result<Vec<u8>>` — parse credential JSON (tagged by `"kind"`), produce signing bytes

### C FFI API

Six functions, all declared `extern "C"`, no runtime handle required:

```c
// Compute contract_id (64-char hex BLAKE3 hash) from ServiceContract JSON.
int32_t aster_contract_id(
    const uint8_t *json_ptr, size_t json_len,
    uint8_t *out_buf, size_t *out_len);

// Compute canonical bytes for "ServiceContract", "TypeDef", or "MethodDef".
int32_t aster_canonical_bytes(
    const uint8_t *type_name_ptr, size_t type_name_len,
    const uint8_t *json_ptr, size_t json_len,
    uint8_t *out_buf, size_t *out_len);

// Encode a wire frame: [4B LE length][flags][payload].
int32_t aster_frame_encode(
    const uint8_t *payload_ptr, size_t payload_len,
    uint8_t flags,
    uint8_t *out_buf, size_t *out_len);

// Decode a wire frame. Writes payload + flags.
int32_t aster_frame_decode(
    const uint8_t *data_ptr, size_t data_len,
    uint8_t *out_payload, size_t *out_payload_len,
    uint8_t *out_flags);

// Compute canonical signing bytes from credential JSON (tagged by "kind").
int32_t aster_signing_bytes(
    const uint8_t *json_ptr, size_t json_len,
    uint8_t *out_buf, size_t *out_len);

// Canonical JSON normalization: sort keys recursively, compact output.
int32_t aster_canonical_json(
    const uint8_t *json_ptr, size_t json_len,
    uint8_t *out_buf, size_t *out_len);
```

**Memory model:**
- Caller provides output buffer and capacity via `(out_buf, *out_len)`.
- On success: `*out_len` is set to actual bytes written, returns `IROH_STATUS_OK` (0).
- On buffer too small: `*out_len` is set to required size, returns `IROH_STATUS_BUFFER_TOO_SMALL` (5). Caller retries with a larger buffer.
- On error: stores diagnostic message in `LAST_ERROR` (retrievable via `iroh_last_error_message`), returns `IROH_STATUS_INTERNAL` (7) or `IROH_STATUS_INVALID_ARGUMENT` (1).
- No runtime handle needed. No event queue interaction.

### JSON Schema

**ServiceContract JSON format:**

```json
{
  "name": "MyService",
  "version": "1.0.0",
  "methods": [
    {
      "name": "GetItem",
      "pattern": "unary",
      "request_type": { "kind": "ref", "primitive": "", "ref": "<hex>", "self_ref_name": "" },
      "response_type": { "kind": "primitive", "primitive": "string", "ref": "", "self_ref_name": "" },
      "capabilities": [],
      "scope": "shared",
      "timeout_ms": null
    }
  ],
  "types": [
    {
      "name": "Item",
      "kind": "message",
      "fields": [ ... ],
      "enum_values": []
    }
  ],
  "capabilities": [],
  "scope_config": { "kind": "shared" }
}
```

**CredentialData JSON format (tagged enum with `"kind"` field):**

```json
{
  "kind": "producer",
  "endpoint_id": "abc123...",
  "root_pubkey": "aabb...<64 hex chars>",
  "expires_at": 1700000000,
  "attributes": {"role": "admin"}
}
```

```json
{
  "kind": "consumer",
  "credential_type": "ott",
  "root_pubkey": "aabb...<64 hex chars>",
  "expires_at": 1700000000,
  "attributes": {},
  "endpoint_id": "consumer-ep",
  "nonce": "eeff...<64 hex chars>"
}
```

**Hex encoding:** All byte-valued fields (`root_pubkey`, `nonce`, `type_ref`, `container_key_ref`) are hex-encoded strings in JSON. The core deserializes them to raw bytes internally.

### Target Language Integration

Each language builds its types natively, serializes to JSON, and passes the JSON string to the core via the C FFI. This means:

- **Python (PyO3):** Exposed via the `_aster.contract` submodule. Python `dataclass` types serialize to JSON, call core, get back bytes or hex strings. Already implemented directly over `aster_transport_core` (not via C FFI).

- **Java (Panama FFM):** Direct C function calls via `MethodHandle` downcalls. Java record types serialize to JSON via Jackson/Gson, pass byte arrays through the FFI, get back byte buffers. No JNI needed.

- **Go (cgo):** Call the C functions directly via `#cgo LDFLAGS`. Go structs marshal to JSON via `encoding/json`, pass `[]byte` through the FFI.

**Key design point:** The language binding never implements canonical serialization itself. It only needs to:
1. Build a native type (dataclass / record / struct)
2. Serialize to JSON
3. Call the FFI function
4. Read the output buffer

This ensures byte-identical output across all languages.

### Testing Strategy

- **Golden test vectors** in `tests/python/fixtures/canonical_test_vectors.json` define expected canonical bytes and contract IDs for reference `ServiceContract` instances.
- **Python consistency tests** serve as the reference implementation: they compute contract IDs and canonical bytes via the Python-accessible core API and compare against the golden vectors.
- **Each language must produce identical bytes and hashes.** The Java and Go test suites should load the same golden vectors and verify their FFI calls produce matching output.
- **Round-trip tests** for framing: `encode_frame` followed by `decode_frame` must recover the original payload and flags.
- **Signing bytes tests**: verify producer and consumer signing bytes match expected byte patterns for known credential inputs.
- **Canonical JSON tests**: verify that key reordering, nested object sorting, and compact serialization produce identical output regardless of input key order.

---

## Implementation Order

### Phase 1 (Core + FFI) — ~2-3 weeks

1. Refactor `aster_transport_core` — Arc-wrap types, add relay config, add secret key export
2. Build `HandleRegistry` and `BridgeRuntime` in `aster_transport_ffi`
3. Implement event queue (mpsc channel + poll)
4. Port all existing FFI functions to new architecture
5. Generate C header with cbindgen
6. Write Rust integration tests for the C ABI
7. Write documentation comments on all public functions

### Phase 1b (Datagram + Hooks + Monitoring) — ~1-2 weeks

1. Complete datagram API surface (`max_datagram_size`, `datagram_send_buffer_space`)
2. Reconcile hook, monitoring, remote-info, and screening design against the pinned `iroh 0.97.0` surface
3. Implement only the supported `v0.97.0` subset of:
   - builder-time endpoint hooks
   - monitoring via tracked `ConnectionInfo`
   - remote aggregation
   - protocol-level screening where appropriate
4. Write integration tests for all implemented features
5. Update documentation/status so placeholders are never reported as completed functionality

### Phase 1c (Registry & Publication Support) — ~1-2 weeks

1. **P0: Blob Tags** — Add `tag_set`, `tag_get`, `tag_delete`, `tag_list` to core + Python + FFI
2. **P0: Fix `add_bytes_as_collection`** — Replace `mem::forget` hack with proper tag-based GC protection
3. **P1: Blob Status / Has** — Add `blob_status`, `blob_has` to core + Python + FFI
4. **P1: Doc Subscribe** — Add live event subscription to core + Python + FFI
5. **P1: Doc Sync Lifecycle** — Add `start_sync`, `leave` to core + Python + FFI
6. **P2: Doc Download Policy** — Add `set_download_policy` to core + Python + FFI
7. **P2: Doc Share with Full Addr** — Add `share_with_addr` to core + Python + FFI
8. **P2: Doc Import and Subscribe** — Add `join_and_subscribe` to core + Python + FFI
9. Write integration tests for all Phase 1c features
10. Update `Aster-ContractIdentity.md` §11.5 to reflect completed items

### Phase 1e (Unified Aster Node) — ~1 week

1. **Core**: Add `AsterQueueHandler`, `CoreNode::{memory,persistent}_with_alpns`, `accept_aster`, `take_hook_receiver`, `build_node_endpoint` helper
2. **PyO3**: Expose `IrohNode.{memory,persistent}_with_alpns`, `accept_aster`, `take_hook_receiver`; add `NodeHookReceiver` + `NodeHookDecisionSender`
3. **Python**: Rewrite `AsterServer` to use `IrohNode` with unified accept loop and Gate 0 hook wiring; add `.blobs/.docs/.gossip/.node` lazy properties
4. **FFI**: Add `iroh_node_{memory,persistent}_with_alpns`, `iroh_node_accept_aster`, `iroh_node_take_hook_receiver`, `IROH_EVENT_ASTER_ACCEPTED`
5. **Server**: Add `owns_endpoint` flag to `Server.__init__` so `AsterServer` can own the node lifecycle
6. **Tests**: Core unit test (accept_aster round-trip), Python integration (unified node + blobs + RPC), Gate 0 enforcement test
7. **Docs**: Update `FFI_PLAN.md` §3e

### Phase 1g (Transport Metrics) — ~1 week

Expose iroh transport-layer metrics through core → FFI → bindings so operators get visibility into the networking layer alongside the existing application-level metrics.

**Upstream surface:** `endpoint.metrics()` returns `EndpointMetrics` implementing `MetricsGroupSet`. This contains sub-groups: `SocketMetrics`, `NetReportMetrics`, `PortmapMetrics`. Each group has named `Counter`, `Gauge`, and `Histogram` fields.

**Core additions (`core/src/lib.rs`):**

```rust
/// Snapshot of transport-layer metrics from the iroh endpoint.
pub struct CoreTransportMetrics {
    // Socket layer
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
    pub paths_direct: u64,      // gauge
    pub paths_relay: u64,       // gauge
    pub holepunch_attempts: u64,
    pub relay_home_change: u64,
    // Net report
    pub net_reports: u64,
    pub net_reports_full: u64,
}

impl CoreNetClient {
    /// Snapshot current transport metrics from the endpoint.
    pub fn transport_metrics(&self) -> Result<CoreTransportMetrics>;
}
```

**FFI additions (`ffi/src/lib.rs`):**

```c
typedef struct {
    uint64_t send_ipv4;
    uint64_t send_ipv6;
    uint64_t send_relay;
    uint64_t recv_data_ipv4;
    uint64_t recv_data_ipv6;
    uint64_t recv_data_relay;
    uint64_t recv_datagrams;
    uint64_t num_conns_direct;
    uint64_t num_conns_opened;
    uint64_t num_conns_closed;
    int64_t  paths_direct;
    int64_t  paths_relay;
    uint64_t holepunch_attempts;
    uint64_t relay_home_change;
    uint64_t net_reports;
    uint64_t net_reports_full;
} iroh_transport_metrics_t;

int32_t iroh_endpoint_transport_metrics(
    iroh_handle_t endpoint,
    iroh_transport_metrics_t* out_metrics
);
```

**Python additions (`bindings/python/rust/src/net.rs`):**

```python
class TransportMetrics:
    send_ipv4: int
    send_ipv6: int
    send_relay: int
    recv_data_ipv4: int
    recv_data_ipv6: int
    recv_data_relay: int
    recv_datagrams: int
    num_conns_direct: int
    num_conns_opened: int
    num_conns_closed: int
    paths_direct: int
    paths_relay: int
    holepunch_attempts: int
    relay_home_change: int
    net_reports: int
    net_reports_full: int

# On IrohNode or NetClient:
def transport_metrics() -> TransportMetrics
```

**TypeScript additions (`bindings/typescript/native/`):**

```typescript
interface TransportMetrics {
    sendIpv4: number;
    sendIpv6: number;
    sendRelay: number;
    recvDataIpv4: number;
    recvDataIpv6: number;
    recvDataRelay: number;
    recvDatagrams: number;
    numConnsDirect: number;
    numConnsOpened: number;
    numConnsClosed: number;
    pathsDirect: number;
    pathsRelay: number;
    holepunchAttempts: number;
    relayHomeChange: number;
    netReports: number;
    netReportsFull: number;
}

// On IrohNode:
transportMetrics(): TransportMetrics
```

**Health endpoint integration:**

Both Python and TypeScript health servers (`/metrics/prometheus`) should include transport metrics alongside existing application-level metrics:

```
# Transport layer (from iroh endpoint)
aster_transport_send_ipv4_total 12345
aster_transport_send_relay_total 678
aster_transport_paths_direct 3
aster_transport_paths_relay 1
aster_transport_holepunch_attempts_total 7
...

# Application layer (existing)
aster_rpc_started_total 500
aster_connections_active 4
...
```

**Implementation steps:**
1. **Core**: Add `CoreTransportMetrics` struct and `transport_metrics()` method on `CoreNetClient` that reads from `endpoint.metrics()`
2. **FFI**: Add `iroh_transport_metrics_t` struct and `iroh_endpoint_transport_metrics()` C function
3. **Python PyO3**: Add `TransportMetrics` class and `transport_metrics()` on `IrohNode`/`NetClient`
4. **Python health**: Update `_prometheus_text()` in `health.py` to include transport metrics
5. **TypeScript NAPI**: Add `transportMetrics()` on `IrohNode`
6. **TypeScript health**: Update Prometheus endpoint to include transport metrics
7. **Tests**: Verify non-zero counters after blob transfer or gossip exchange

**Metrics parity gaps to close (Python ↔ TypeScript):**

| Gap | Where | Fix |
|-----|-------|-----|
| RPC duration histogram | TS MetricsInterceptor | Add duration tracking (start time on request, record on response) |
| Streams active/total | TS ConnectionMetrics | Add stream counters matching Python |
| Uptime gauge | TS health.ts | Add `aster_uptime_seconds` to Prometheus output |
| Admission last duration (ms) | TS AdmissionMetrics | Add `lastAdmissionMs` field |
| OTel integration | TS MetricsInterceptor | Add optional `@opentelemetry/api` support (match Python pattern) |

### Phase 1f (Cross-Language Contract Identity, Framing & Signing) — ~3 days

1. **Core**: Implement `canonical.rs`, `contract.rs`, `framing.rs`, `signing.rs` in `aster_transport_core` (done)
2. **FFI**: Add 6 synchronous C functions (`aster_contract_id`, `aster_canonical_bytes`, `aster_frame_encode`, `aster_frame_decode`, `aster_signing_bytes`, `aster_canonical_json`) to `aster_transport_ffi` (done)
3. **Golden vectors**: Create `tests/python/fixtures/canonical_test_vectors.json` with reference contract IDs, canonical bytes, and signing bytes
4. **Python tests**: Validate Python-side contract identity and signing against golden vectors
5. **Java tests**: Load golden vectors, call FFI functions, verify byte-identical output
6. **Docs**: Update `FFI_PLAN.md` §3f (done)

### Phase 1g (Per-Connection Metrics & Prometheus Export) — DONE

Per-connection QUIC metrics and aggregate transport metrics in Prometheus format,
supporting the HA routing scorer and operational dashboards.

**Core (`core/src/lib.rs`):**

Added to `CoreConnection`:

| Method | Return | Source |
|--------|--------|--------|
| `rtt_ms()` | `f64` | `PathInfo::rtt()` (QUIC RTT) |
| `bytes_sent()` | `u64` | `PathStats::udp_tx.bytes` |
| `bytes_recv()` | `u64` | `PathStats::udp_rx.bytes` |
| `congestion_window()` | `u64` | `PathStats::cwnd` |
| `lost_packets()` | `u64` | `PathStats::lost_packets` |
| `congestion_events()` | `u64` | `PathStats::congestion_events` |
| `current_mtu()` | `u16` | `PathStats::current_mtu` |

All read from the selected path's `PathStats` (from quinn/noq-proto). Zero-cost when not called.

Added to `CoreNode`:

| Method | Return | Description |
|--------|--------|-------------|
| `transport_metrics_prometheus()` | `String` | All endpoint-level counters in Prometheus text exposition format |

Emits 16 metrics: `iroh_send_ipv4`, `iroh_send_ipv6`, `iroh_send_relay`, `iroh_recv_data_ipv4`, `iroh_recv_data_ipv6`, `iroh_recv_data_relay`, `iroh_recv_datagrams`, `iroh_conns_direct`, `iroh_conns_opened`, `iroh_conns_closed`, `iroh_paths_direct`, `iroh_paths_relay`, `iroh_holepunch_attempts`, `iroh_relay_home_change`, `iroh_net_reports`, `iroh_net_reports_full`.

**Python (`bindings/python/rust/src/net.rs`, `node.rs`):**

- `IrohConnection`: 7 new methods mirroring core (`rtt_ms`, `bytes_sent`, `bytes_recv`, `congestion_window`, `lost_packets`, `congestion_events`, `current_mtu`)
- `IrohNode.transport_metrics_prometheus()` → `str`

**TypeScript NAPI (`bindings/typescript/native/src/net.rs`, `node.rs`):**

- `IrohConnection`: 7 new methods (same as Python; u64 returned as f64 for JS number safety)
- `IrohNode.transportMetricsPrometheus()` → `string`

**Usage (routing scorer):**

```python
conn = await transport.connect(node_id, alpn)
rtt = conn.rtt_ms()          # feed to routing scorer
cwnd = conn.congestion_window()  # detect congestion
```

**Usage (Prometheus scrape):**

```python
# In HealthServer /metrics/prometheus endpoint
aster_metrics = format_aster_rpc_metrics()
transport_metrics = node.transport_metrics_prometheus()
return aster_metrics + "\n" + transport_metrics
```

### Phase 2 (Python) — ~1 week

1. Finish Phase 1b and 1c in `aster_transport_core` for the surfaces Python is required to expose
2. Make `aster_transport_core` the immediate backend for every Python wrapper
3. Remove the legacy FFI-based Python implementation from `iroh_python_rs`
4. Consolidate duplicate module implementations and make `lib.rs` registration-only
5. Add/complete Python modules for hooks, monitoring, remote-info, and related observability
6. Migrate to maintained async runtime support if needed
7. Verify existing Python tests still pass for stable APIs
8. Add Python tests for all newly exposed Phase 1b surfaces
9. Keep ctypes-based FFI validation as a separate ABI smoke test, not the implementation strategy for the main Python binding

### Phase 3 (Java) — ~2-3 weeks

1. Set up Gradle project with FFM dependencies
   2. Implement `NativeBindings.java` (all downcall handles)
   3. Implement `EventPoller.java`
   4. Implement public API classes (`IrohRuntime`, `IrohNode`, etc.)
   5. Write JUnit tests
   6. Write integration tests matching the Python test scenarios
   7. Document Java API with Javadoc

## 3h. Phase 1h: Compact Aster Ticket Format

**Status:** Implemented  
**Date:** 2026-04-08  
**Spec:** `docs/_internal/COMPACT_TICKET_FORMAT.md`

### Overview

Compact binary encoding for endpoint addresses + optional credentials, replacing verbose base64 NodeAddr strings. Wire format max 256 bytes, string format `aster1<base58>`.

### Core (`core/src/ticket.rs`)

- `AsterTicket` struct: `endpoint_id`, `relay`, `direct_addrs`, `credential`
- `TicketCredential` enum: `Open` (0x00), `ConsumerRcan` (0x01), `Enrollment` (0x02), `Registry` (0x03)
- `encode() -> Vec<u8>`: serialize to compact binary
- `decode(bytes) -> AsterTicket`: deserialize
- `to_base58_string() -> String`: `aster1<base58>` encoding
- `from_base58_str(s) -> AsterTicket`: parse `aster1<base58>` string

### FFI (`ffi/src/lib.rs`)

```c
int32_t aster_ticket_encode(
    const uint8_t *endpoint_id_hex, size_t endpoint_id_hex_len,
    const uint8_t *relay_addr, size_t relay_addr_len,
    const uint8_t *direct_addrs_json, size_t direct_addrs_json_len,
    const uint8_t *credential_type, size_t credential_type_len,
    const uint8_t *credential_data, size_t credential_data_len,
    uint8_t *out_buf, size_t *out_len
);

int32_t aster_ticket_decode(
    const uint8_t *ticket_str, size_t ticket_len,
    uint8_t *out_buf, size_t *out_len
);
```

Both are synchronous. `aster_ticket_encode` writes the `aster1...` string to `out_buf`. `aster_ticket_decode` writes a JSON object with `endpoint_id`, `relay_addr`, `direct_addrs`, `credential_type`, `credential_data_hex`.