# Iroh FFI — Complete Refactoring & Multi-Language Binding Plan

**Status:** Final Plan  
**Date:** 2026-04-01  
**Scope:** Refactor `aster_transport_core` and `aster_transport_ffi` to provide a polished, language-neutral C ABI; update Python bindings; implement Java FFM bindings.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Architecture Overview](#2-architecture-overview)
3. [Phase 1: Core + FFI Refactoring](#3-phase-1-core--ffi-refactoring)
4. [Phase 1b: Datagram Completion & Hooks & Monitoring](#3b-phase-1b-datagram-completion--hooks--monitoring)
5. [Phase 2: Python Bindings Update](#4-phase-2-python-bindings-update)
6. [Phase 3: Java FFM Bindings](#5-phase-3-java-ffm-bindings)
7. [C ABI Reference](#6-c-abi-reference)
8. [Memory Ownership Rules](#7-memory-ownership-rules)
9. [Testing & Validation Strategy](#8-testing--validation-strategy)
10. [Migration Guide](#9-migration-guide)

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
┌─────────────────────────────────────────────────────┐
│                   Language Bindings                   │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐           │
│  │  Python   │  │   Java   │  │  C / Zig  │  ...     │
│  │  (PyO3)   │  │  (FFM)   │  │ (direct)  │          │
│  └────┬──────┘  └────┬─────┘  └────┬──────┘          │
│       │              │              │                 │
│  ┌────▼──────────────▼──────────────▼──────┐         │
│  │         aster_transport_ffi (C ABI)       │         │
│  │  - #[no_mangle] extern "C" functions     │         │
│  │  - #[repr(C)] structs                    │         │
│  │  - Opaque u64 handles                    │         │
│  │  - Central completion queue              │         │
│  │  - Arc-backed handle registry            │         │
│  └────────────────┬────────────────────────┘         │
│                   │                                   │
│  ┌────────────────▼────────────────────────┐         │
│  │         aster_transport_core              │         │
│  │  - Pure Rust async API                   │         │
│  │  - No FFI concerns                       │         │
│  │  - Wraps iroh, iroh-blobs, iroh-docs,   │         │
│  │    iroh-gossip                           │         │
│  └─────────────────────────────────────────┘         │
└─────────────────────────────────────────────────────┘
```

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
    pub relay_urls: Vec<String>,        // NEW: custom relay URLs
    pub alpns: Vec<Vec<u8>>,
    pub secret_key: Option<Vec<u8>>,
    pub enable_discovery: bool,          // NEW: default true
}
```

Update `build_endpoint_config()` to handle `relay_mode == "custom"` with the provided URLs.

#### 3.1.4 Add secret key export

```rust
impl CoreNetClient {
    pub fn export_secret_key(&self) -> Vec<u8> { ... }
}
impl CoreNode {
    pub fn export_secret_key(&self) -> Vec<u8> { ... }
}
```

#### 3.1.5 Add datagram support to core (already exists, verify completeness)

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
    uint32_t relay_mode;         // 0=default, 1=custom, 2=disabled
    iroh_bytes_t secret_key;     // 32 bytes or empty
    const iroh_bytes_t* alpns;
    size_t alpns_len;
    const iroh_bytes_t* relay_urls; // UTF-8 URLs
    size_t relay_urls_len;
    uint32_t enable_discovery;   // 1=yes, 0=no
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

> **See also:** `ffi_spec/FFI_PLAN_PATCH.md` for the original detailed design.
>
> **Scope correction (2026-04-01):** `FFI_PLAN_PATCH.md` was written as a forward-looking design,
> but this repository is currently pinned to **`iroh = 0.97.0`**. Some of the planned Phase 1b
> surfaces line up more naturally with newer/stabilizing upstream APIs. Therefore, for the
> current branch, this section should be interpreted as:
>
> - **Datagram completion:** in scope now
> - **Hooks:** design target only unless/until mapped cleanly to the `v0.97.0` surface
> - **Remote-info / monitoring:** implement only what can be faithfully represented from `v0.97.0`
>
> The upstream references to use when refining this plan are the `v0.97.0` examples:
> - `monitor-connections.rs`
> - `screening-connection.rs`
> - `remote-info.rs`
> - `auth-hook.rs`

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

#### FFI Types (Existing, ready for wiring)

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

**Note:** Python (via PyO3 directly over core) does not need the FFI hook reply path — it can consume `CoreHookReceiver` directly.

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

### Phase 2 (Python) — ~1 week

1. Finish Phase 1b in `aster_transport_core` for the surfaces Python is required to expose
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