# FFI Plan Patch — Datagram Completion & Endpoint Hooks

**Date:** 2026-04-01  
**Status:** PATCH — Supersedes/extends `FFI_PLAN.md`  
**Scope:** Complete datagram API surface + add endpoint hook system + add remote-info monitoring APIs

---

## Table of Contents

1. [Overview](#1-overview)
2. [Datagram API — Completion Checklist](#2-datagram-api--completion-checklist)
3. [Endpoint Hooks — Full Design](#3-endpoint-hooks--full-design)
4. [Remote-Info & Monitoring APIs](#4-remote-info--monitoring-apis)
5. [Core Layer Changes](#5-core-layer-changes)
6. [FFI Layer Changes](#6-ffi-layer-changes)
7. [Status Tracking](#7-status-tracking)

---

## 1. Overview

Two capability areas are missing from the current FFI surface:

### 1.1 Datagram API — Incomplete

The current FFI exposes `send_datagram` and `read_datagram` but omits the two synchronous capability-query functions:

| iroh API | Status in FFI | Missing |
|----------|--------------|---------|
| `Connection::send_datagram(data)` | ✅ `iroh_connection_send_datagram` | — |
| `Connection::read_datagram()` | ✅ `iroh_connection_read_datagram` | — |
| `Connection::max_datagram_size()` → `Option<usize>` | ❌ Not exposed | Must add |
| `Connection::datagram_send_buffer_space()` → `usize` | ❌ Not exposed | Must add |

### 1.2 Endpoint Hooks — Completely Absent

The current FFI has no hook/interception system. The iroh endpoint API supports hooking into:
- **before_connect**: fires before a connection attempt; allows the host to inspect the target, add auth context, and approve/deny
- **after_connect**: fires after a connection is established; allows the host to observe connection metadata for monitoring

The hook system requires a **two-phase reply protocol** because the host language must inspect state and respond asynchronously.

### 1.3 Remote-Info & Monitoring — Absent

Use cases like the `monitor-connections.rs` and `remote-info.rs` examples require querying per-peer connection state:
- Current connection state (active/idle/connecting)
- Last handshake time
- Bytes sent/received
- Path info (direct/relay)

These need explicit FFI query APIs beyond just hooks.

---

## 2. Datagram API — Completion Checklist

### 2.1 Core Layer

In `aster_transport_core/src/lib.rs`, on `CoreConnection`, add:

```rust
/// Returns the maximum size for datagrams on this connection.
/// Returns None if datagrams are disabled or unsupported by the peer.
pub fn max_datagram_size(&self) -> Option<usize> {
    self.inner.max_datagram_size()
}

/// Returns the amount of send buffer space available for datagrams.
/// Always returns 0 if datagrams are unsupported.
pub fn datagram_send_buffer_space(&self) -> usize {
    self.inner.datagram_send_buffer_space().into()
}
```

### 2.2 FFI Layer

Add two new C FFI functions:

```c
/// Query the maximum datagram size for this connection.
///
/// out_size receives the maximum size in bytes.
/// out_is_some receives 1 if datagrams are supported, 0 otherwise.
///
/// Returns IROH_STATUS_OK on success.
/// Returns IROH_STATUS_NOT_FOUND if the connection handle is invalid.
iroh_status_t iroh_connection_max_datagram_size(
    iroh_runtime_t runtime,
    iroh_connection_t connection,
    uint64_t* out_size,
    uint32_t* out_is_some
);

/// Query the available send buffer space for datagrams.
///
/// Returns the number of bytes that can be sent before blocking.
/// Always returns 0 if datagrams are unsupported.
///
/// Returns IROH_STATUS_OK on success.
/// Returns IROH_STATUS_NOT_FOUND if the connection handle is invalid.
iroh_status_t iroh_connection_datagram_send_buffer_space(
    iroh_runtime_t runtime,
    iroh_connection_t connection,
    uint64_t* out_bytes
);
```

### 2.3 Event Kind (Optional Improvement)

Currently `read_datagram` emits `IROH_EVENT_BYTES_RESULT`. Consider adding a dedicated event:

```c
typedef enum iroh_event_kind_e {
    // ... existing events ...
    
    // Datagrams
    IROH_EVENT_DATAGRAM_RECEIVED = 60,
    
    // ... rest of existing events ...
} iroh_event_kind_t;
```

Update `iroh_connection_read_datagram` to emit `IROH_EVENT_DATAGRAM_RECEIVED` instead of `IROH_EVENT_BYTES_RESULT` for clarity.

---

## 3. Endpoint Hooks — Full Design

### 3.1 Architecture Overview

```
┌──────────────────────────────────────────────────────────────┐
│                     Host Language Binding                      │
│  Python/Java/Go — registers hook callbacks, responds to events │
└──────────────────────────┬───────────────────────────────────┘
                           │ iroh_poll_events() poll
                           │ iroh_hook_before_connect_respond()
                           │ iroh_hook_after_connect_respond()
┌──────────────────────────▼───────────────────────────────────┐
│                 aster_transport_ffi (C ABI)                    │
│  - Hook registration                                           │
│  - Translates before_connect/after_connect to iroh events     │
│  - Stores hook invocation handles in registry                 │
│  - Calls into Rust hooks via Arc<HookCallbacks>               │
└──────────────────────────┬───────────────────────────────────┘
                           │
┌──────────────────────────▼───────────────────────────────────┐
│                 aster_transport_core                             │
│  - CoreNetClient / CoreNode wrap iroh hooks                   │
│  - Stores Arc<HookCallbacks> in EndpointBuilder               │
│  - Emits hook events back through FFI event system            │
└──────────────────────────────────────────────────────────────┘
```

### 3.2 Core Layer — Hook Types

```rust
// In aster_transport_core/src/lib.rs

/// Hook callback interface — stored as Arc<dyn HookCallbacks>
pub trait HookCallbacks: Send + Sync {
    /// Called before a connection attempt.
    /// Return true to allow, false to deny.
    fn before_connect(&self, info: &HookConnectInfo) -> bool;
    
    /// Called after a connection is established (success or failure).
    /// No return value — purely observational.
    fn after_connect(&self, info: &HookConnectInfo, success: bool);
}

/// Information about a connection attempt, passed to hooks
#[derive(Clone, Debug)]
pub struct HookConnectInfo {
    pub local_endpoint_id: String,
    pub target_node_id: String,
    pub target_addr: Option<CoreNodeAddr>,
    pub alpn: Vec<u8>,
    pub is_outbound: bool,
    /// Optional connection attempt start time (if available)
    pub attempt_start_ns: Option<u64>,
}

/// Configuration for hook registration
#[derive(Clone, Debug)]
pub struct CoreHookConfig {
    pub enable_before_connect: bool,
    pub enable_after_connect: bool,
    pub include_remote_info: bool,
    /// User data echoed back in hook events
    pub user_data: u64,
}
```

### 3.3 Core Layer — Hook Methods

On `CoreNetClient`, add:

```rust
/// Register connection hooks on this endpoint.
/// Returns a handle for unregistration.
pub fn set_hooks(&self, config: CoreHookConfig, callbacks: Arc<dyn HookCallbacks>) -> u64;

/// Unregister previously set hooks.
pub fn clear_hooks(&self, registration: u64);
```

On `CoreNode`, add equivalent methods that delegate to the underlying endpoint.

### 3.4 FFI Layer — New Types

#### New Handle Types

```rust
pub type iroh_hook_registration_t = u64;
pub type iroh_hook_invocation_t = u64;
```

#### New Event Kinds

```c
typedef enum iroh_event_kind_e {
    // ... existing events ...
    
    // Hooks
    IROH_EVENT_HOOK_BEFORE_CONNECT = 70,
    IROH_EVENT_HOOK_AFTER_CONNECT = 71,
    IROH_EVENT_HOOK_INVOCATION_RELEASED = 72,  // hook invocation data released
    
    // ... rest of existing events ...
} iroh_event_kind_t;
```

#### Before Connect Event Payload (`IROH_EVENT_HOOK_BEFORE_CONNECT`)

```c
typedef struct iroh_hook_connect_info_s {
    iroh_bytes_t local_endpoint_id;   // hex string
    iroh_bytes_t target_node_id;      // hex string
    iroh_bytes_t alpn;               // raw bytes
    uint32_t is_outbound;            // 1 = outbound, 0 = inbound
    iroh_node_addr_t target_addr;     // optional, may be empty
} iroh_hook_connect_info_t;
```

Fields point into scratch buffer via the same pattern as `iroh_node_addr_t`.

#### After Connect Event Payload (`IROH_EVENT_HOOK_AFTER_CONNECT`)

Same as `iroh_hook_connect_info_t` with additions:

```c
typedef struct iroh_hook_connect_info_s {
    // ... fields from before_connect ...
    
    uint32_t success;                // 1 = connected, 0 = failed
    int32_t error_code;              // 0 on success, negative on error
    uint64_t connection_handle;      // if success=1, the connection handle
    iroh_bytes_t error_reason;        // UTF-8 error message if failed
} iroh_hook_connect_info_t;
```

#### Before Connect Reply Decision

```c
typedef enum iroh_hook_decision_e {
    IROH_HOOK_DECISION_ALLOW = 0,
    IROH_HOOK_DECISION_DENY = 1,
} iroh_hook_decision_t;
```

### 3.5 FFI Layer — New Functions

#### Hook Registration

```c
/// Register connection hooks on an endpoint or node.
///
/// config: hook configuration (which hooks to enable, user data)
/// out_registration: receives the hook registration handle
///
/// Returns IROH_STATUS_OK on success.
/// Returns IROH_STATUS_NOT_FOUND if endpoint/node not found.
/// Returns IROH_STATUS_UNSUPPORTED if hook registration not available.
iroh_status_t iroh_endpoint_set_hooks(
    iroh_runtime_t runtime,
    uint64_t endpoint_or_node,
    const iroh_hook_config_t* config,
    iroh_hook_registration_t* out_registration
);

/// Unregister previously set hooks.
///
/// Returns IROH_STATUS_OK on success.
/// Returns IROH_STATUS_NOT_FOUND if registration not found.
iroh_status_t iroh_endpoint_clear_hooks(
    iroh_runtime_t runtime,
    uint64_t endpoint_or_node,
    iroh_hook_registration_t registration
);
```

#### Hook Configuration Struct

```c
typedef struct iroh_hook_config_s {
    uint32_t struct_size;
    uint32_t enable_before_connect;    // 1 = enabled, 0 = disabled
    uint32_t enable_after_connect;     // 1 = enabled, 0 = disabled
    uint32_t include_remote_info;      // 1 = include addr info in events
    uint64_t user_data;                // echoed in hook events
} iroh_hook_config_t;
```

#### Hook Invocation Reply

```c
/// Respond to a BEFORE_CONNECT hook invocation.
///
/// decision: ALLOW or DENY
/// reason: optional UTF-8 string (e.g., denial reason, auth token)
///         pass null ptr + 0 len to send no reason/token
///
/// Returns IROH_STATUS_OK on success.
/// Returns IROH_STATUS_NOT_FOUND if invocation handle not found.
/// Returns IROH_STATUS_INVALID_ARGUMENT if called for non-before-connect event.
iroh_status_t iroh_hook_before_connect_respond(
    iroh_runtime_t runtime,
    iroh_hook_invocation_t invocation,
    iroh_hook_decision_t decision,
    const iroh_bytes_t* reason
);

/// Acknowledge an AFTER_CONNECT hook invocation.
///
/// Call this to release any resources associated with the hook event.
/// After this call, the invocation handle is invalid.
///
/// Returns IROH_STATUS_OK on success.
/// Returns IROH_STATUS_NOT_FOUND if invocation handle not found.
iroh_status_t iroh_hook_after_connect_respond(
    iroh_runtime_t runtime,
    iroh_hook_invocation_t invocation
);
```

### 3.6 Event Delivery Contract for Hooks

**Before Connect:**
1. Native layer fires `before_connect` hook
2. FFI emits `IROH_EVENT_HOOK_BEFORE_CONNECT` with:
   - `handle = endpoint/node handle`
   - `related = hook_invocation_t`
   - `user_data = registration user_data`
   - payload = serialized `iroh_hook_connect_info_t`
3. Connection attempt is **suspended** until host calls `iroh_hook_before_connect_respond()`
4. On ALLOW: connection proceeds, host may receive AFTER_CONNECT
5. On DENY: connection aborts with denial reason

**After Connect:**
1. Native layer fires `after_connect` hook
2. FFI emits `IROH_EVENT_HOOK_AFTER_CONNECT` with:
   - `handle = endpoint/node handle`
   - `related = hook_invocation_t`
   - `user_data = registration user_data`
   - `status = IROH_STATUS_OK or error code`
   - payload = serialized `iroh_hook_connect_info_t` + error details
3. If ALLOW path was taken, `connection_handle` field is populated
4. Host calls `iroh_hook_after_connect_respond()` to ack

**Concurrency:** Multiple hook invocations can be pending simultaneously. Each invocation is identified by its unique `iroh_hook_invocation_t`. Hosts must respond to each invocation individually.

---

## 4. Remote-Info & Monitoring APIs

### 4.1 Use Cases

From the referenced examples:
- `monitor-connections.rs`: track endpoint connections, observe connect/disconnect events, read connection stats
- `remote-info.rs`: query information about known remote endpoints

### 4.2 Core Layer — Remote Info Types

```rust
/// Information about a known remote endpoint
#[derive(Clone, Debug)]
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

/// Type of connection to this peer
#[derive(Clone, Debug)]
pub enum ConnectionType {
    NotConnected,
    Connecting,
    Connected(ConnectionTypeDetail),
}

#[derive(Clone, Debug)]
pub enum ConnectionTypeDetail {
    /// Direct UDP connection
    UdpDirect,
    /// Relay-mediated connection
    UdpRelay,
    /// Some other mechanism
    Other(String),
}
```

### 4.3 Core Layer — Remote Info Methods

On `CoreNetClient`, add:

```rust
/// Query information about a specific known remote endpoint.
pub fn remote_info(&self, node_id: &str) -> Option<CoreRemoteInfo>;

/// Get information about all known remote endpoints.
pub fn remote_info_iter(&self) -> Vec<CoreRemoteInfo>;
```

On `CoreConnection`, add:

```rust
/// Get detailed information about this connection.
pub fn connection_info(&self) -> CoreConnectionInfo;
```

Where `CoreConnectionInfo` contains:
- `connection_type: ConnectionTypeDetail`
- `bytes_sent: u64`
- `bytes_received: u64`
- `rtt_ns: Option<u64>`
- `alpn: Vec<u8>`
- `is_connected: bool`

### 4.4 FFI Layer — Remote Info Structs

```c
/// Remote endpoint information
typedef struct iroh_remote_info_s {
    uint32_t struct_size;
    iroh_bytes_t node_id;              // hex string
    uint32_t is_connected;             // 1 = connected
    uint32_t connection_type;           // 0=none, 1=connecting, 2=udp_direct, 3=udp_relay
    iroh_bytes_t relay_url;             // UTF-8, empty if none
    uint64_t last_handshake_ns;        // 0 if never
    uint64_t bytes_sent;               // bytes sent to this peer
    uint64_t bytes_received;           // bytes received from this peer
} iroh_remote_info_t;

/// Connection information (per-connection)
typedef struct iroh_connection_info_s {
    uint32_t struct_size;
    uint32_t connection_type;           // 2=udp_direct, 3=udp_relay, etc.
    uint64_t bytes_sent;
    uint64_t bytes_received;
    uint64_t rtt_ns;                    // 0 if unknown
    iroh_bytes_t alpn;
    uint32_t is_connected;
} iroh_connection_info_t;
```

### 4.5 FFI Layer — Remote Info Functions

```c
/// Query information about a specific remote endpoint.
///
/// node_id: hex string of the target node
/// out_info: receives the remote info
///
/// Returns IROH_STATUS_OK on success.
/// Returns IROH_STATUS_NOT_FOUND if node_id not known to this endpoint.
iroh_status_t iroh_endpoint_remote_info(
    iroh_runtime_t runtime,
    uint64_t endpoint_or_node,
    iroh_bytes_t node_id,
    iroh_remote_info_t* out_info
);

/// Get information about all known remote endpoints.
///
/// out_infos: array of iroh_remote_info_t (caller-allocated)
/// max_infos: capacity of out_infos array
/// out_count: receives actual count of remote infos
///
/// Returns IROH_STATUS_OK on success.
/// Returns IROH_STATUS_BUFFER_TOO_SMALL if max_infos is insufficient.
iroh_status_t iroh_endpoint_remote_info_list(
    iroh_runtime_t runtime,
    uint64_t endpoint_or_node,
    iroh_remote_info_t* out_infos,
    size_t max_infos,
    size_t* out_count
);

/// Get detailed information about a specific connection.
///
/// out_info: receives connection info
///
/// Returns IROH_STATUS_OK on success.
/// Returns IROH_STATUS_NOT_FOUND if connection not found.
iroh_status_t iroh_connection_info(
    iroh_runtime_t runtime,
    iroh_connection_t connection,
    iroh_connection_info_t* out_info
);
```

---

## 5. Core Layer Changes

### 5.1 Summary of Core Additions

**File:** `aster_transport_core/src/lib.rs`

#### On `CoreConnection`:

```rust
/// Returns the maximum datagram size for this connection.
/// Returns None if datagrams are disabled or unsupported.
pub fn max_datagram_size(&self) -> Option<usize>;

/// Returns available datagram send buffer space.
pub fn datagram_send_buffer_space(&self) -> usize;

/// Get detailed connection information.
pub fn connection_info(&self) -> CoreConnectionInfo;
```

#### New types in core:

```rust
pub struct CoreConnectionInfo { ... }
pub trait HookCallbacks: Send + Sync { ... }
pub struct HookConnectInfo { ... }
pub struct CoreHookConfig { ... }
pub struct CoreRemoteInfo { ... }
pub enum ConnectionType { ... }
pub enum ConnectionTypeDetail { ... }
```

#### On `CoreNetClient`:

```rust
/// Register connection hooks.
pub fn set_hooks(&self, config: CoreHookConfig, callbacks: Arc<dyn HookCallbacks>) -> u64;

/// Unregister hooks.
pub fn clear_hooks(&self, registration: u64);

/// Query remote info for a specific peer.
pub fn remote_info(&self, node_id: &str) -> Option<CoreRemoteInfo>;

/// Get all known remote infos.
pub fn remote_info_iter(&self) -> Vec<CoreRemoteInfo>;
```

#### On `CoreNode`:

Same methods as `CoreNetClient`, delegating to the underlying endpoint.

### 5.2 Implementation Notes

- Hook registration requires storing `Arc<dyn HookCallbacks>` in the endpoint
- The callbacks are invoked synchronously from the endpoint's connection setup code
- For the FFI bridge, wrap the hook callbacks in `Arc<BridgeHookCallbacks>` that:
  - For `before_connect`: suspends the connection, emits event, waits for reply
  - For `after_connect`: emits event with metadata, waits for ack
- Remote info queries delegate to iroh's `Endpoint::remote_info()` / `remote_info_iter()`

---

## 6. FFI Layer Changes

### 6.1 Summary of FFI Additions

**File:** `aster_transport_ffi/src/lib.rs`

#### New handle types:

```rust
pub type iroh_hook_registration_t = u64;
pub type iroh_hook_invocation_t = u64;
```

#### New event kinds:

```rust
IROH_EVENT_DATAGRAM_RECEIVED = 60,
IROH_EVENT_HOOK_BEFORE_CONNECT = 70,
IROH_EVENT_HOOK_AFTER_CONNECT = 71,
IROH_EVENT_HOOK_INVOCATION_RELEASED = 72,
```

#### New event fields in `iroh_event_t`:

No new fields needed — `related` carries `iroh_hook_invocation_t`, `user_data` carries registration user data.

#### New config structs:

```rust
#[repr(C)]
pub struct iroh_hook_config_t {
    pub struct_size: u32,
    pub enable_before_connect: u32,
    pub enable_after_connect: u32,
    pub include_remote_info: u32,
    pub user_data: u64,
}

#[repr(C)]
pub struct iroh_hook_connect_info_t {
    pub struct_size: u32,
    pub local_endpoint_id: iroh_bytes_t,
    pub target_node_id: iroh_bytes_t,
    pub alpn: iroh_bytes_t,
    pub is_outbound: u32,
    pub target_addr: iroh_node_addr_t,
    pub success: u32,
    pub error_code: i32,
    pub connection_handle: u64,
    pub error_reason: iroh_bytes_t,
}

#[repr(C)]
pub struct iroh_remote_info_t {
    pub struct_size: u32,
    pub node_id: iroh_bytes_t,
    pub is_connected: u32,
    pub connection_type: u32,
    pub relay_url: iroh_bytes_t,
    pub last_handshake_ns: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

#[repr(C)]
pub struct iroh_connection_info_t {
    pub struct_size: u32,
    pub connection_type: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub rtt_ns: u64,
    pub alpn: iroh_bytes_t,
    pub is_connected: u32,
}
```

#### New enums:

```rust
#[repr(C)]
pub enum iroh_hook_decision_t {
    IROH_HOOK_DECISION_ALLOW = 0,
    IROH_HOOK_DECISION_DENY = 1,
}
```

#### New handle registries:

Add to `BridgeRuntime`:

```rust
struct BridgeRuntime {
    // ... existing fields ...
    
    // Hook registries
    hook_registrations: HandleRegistry<HookRegistrationState>,
    hook_invocations: HandleRegistry<HookInvocationState>,
    pending_hook_replies: Arc<Mutex<HashMap<u64, oneshot::Sender<bool>>>>,
}
```

Where:
- `HookRegistrationState` stores the Arc to the callbacks + config
- `HookInvocationState` stores pending reply channels for before_connect
- `pending_hook_replies` maps invocation → reply sender

#### New FFI functions to implement:

1. `iroh_connection_max_datagram_size`
2. `iroh_connection_datagram_send_buffer_space`
3. `iroh_endpoint_set_hooks`
4. `iroh_endpoint_clear_hooks`
5. `iroh_hook_before_connect_respond`
6. `iroh_hook_after_connect_respond`
7. `iroh_endpoint_remote_info`
8. `iroh_endpoint_remote_info_list`
9. `iroh_connection_info`

#### Hook Implementation Pattern

```rust
struct BridgeHookCallbacks {
    runtime: Arc<BridgeRuntime>,
    config: CoreHookConfig,
    endpoint_id: u64,
}

impl HookCallbacks for BridgeHookCallbacks {
    fn before_connect(&self, info: &HookConnectInfo) -> bool {
        // 1. Create hook invocation handle
        let (reply_tx, reply_rx) = oneshot::channel();
        let invocation_id = self.runtime.hook_invocations.insert(reply_tx);
        
        // 2. Emit BEFORE_CONNECT event with invocation handle in 'related' field
        let event = build_hook_before_connect_event(info, invocation_id, self.config.user_data);
        self.runtime.emit(event);
        
        // 3. Wait for reply from host (suspends connection attempt)
        match reply_rx.blocking_recv() {
            Ok(allow) => allow,
            Err(_) => true, // on channel drop, default to allow
        }
    }
    
    fn after_connect(&self, info: &HookConnectInfo, success: bool) {
        // 1. Create hook invocation handle
        let (ack_tx, _ack_rx) = oneshot::channel();
        let invocation_id = self.runtime.hook_invocations.insert(ack_tx);
        
        // 2. Emit AFTER_CONNECT event
        let event = build_hook_after_connect_event(info, invocation_id, success, self.config.user_data);
        self.runtime.emit(event);
        
        // Note: We don't wait for ack here — after_connect is observational
        // The ack just releases resources
    }
}
```

---

## 7. Status Tracking

### Tasks

| Task | Status | Notes |
|------|--------|-------|
| Core: Add `max_datagram_size` to `CoreConnection` | ⬜ TODO | |
| Core: Add `datagram_send_buffer_space` to `CoreConnection` | ⬜ TODO | |
| Core: Add `connection_info` to `CoreConnection` | ⬜ TODO | |
| Core: Define `HookCallbacks`, `HookConnectInfo`, `CoreHookConfig` | ⬜ TODO | |
| Core: Add `set_hooks` / `clear_hooks` to `CoreNetClient` | ⬜ TODO | |
| Core: Define `CoreRemoteInfo`, `ConnectionType` | ⬜ TODO | |
| Core: Add `remote_info` / `remote_info_iter` to `CoreNetClient` | ⬜ TODO | |
| Core: Add same methods to `CoreNode` | ⬜ TODO | |
| FFI: Add `iroh_connection_max_datagram_size` | ⬜ TODO | |
| FFI: Add `iroh_connection_datagram_send_buffer_space` | ⬜ TODO | |
| FFI: Add `IROH_EVENT_DATAGRAM_RECEIVED` | ⬜ TODO | Optional |
| FFI: Add hook types, enums, config structs | ⬜ TODO | |
| FFI: Add hook registries to `BridgeRuntime` | ⬜ TODO | |
| FFI: Implement `iroh_endpoint_set_hooks` | ⬜ TODO | |
| FFI: Implement `iroh_endpoint_clear_hooks` | ⬜ TODO | |
| FFI: Implement `iroh_hook_before_connect_respond` | ⬜ TODO | |
| FFI: Implement `iroh_hook_after_connect_respond` | ⬜ TODO | |
| FFI: Implement `iroh_endpoint_remote_info` | ⬜ TODO | |
| FFI: Implement `iroh_endpoint_remote_info_list` | ⬜ TODO | |
| FFI: Implement `iroh_connection_info` | ⬜ TODO | |
| Tests: Datagram size + buffer space tests | ⬜ TODO | |
| Tests: Hook before_connect allow/deny | ⬜ TODO | |
| Tests: Hook after_connect delivery + ack | ⬜ TODO | |
| Tests: Remote info query | ⬜ TODO | |
| Docs: Update FFI_PLAN.md with hook section | ⬜ TODO | |

---

## Appendix A: Full Function List for Hooks & Remote-Info

```c
// ─── Hook Registration ───
iroh_status_t iroh_endpoint_set_hooks(
    iroh_runtime_t runtime,
    uint64_t endpoint_or_node,
    const iroh_hook_config_t* config,
    iroh_hook_registration_t* out_registration
);

iroh_status_t iroh_endpoint_clear_hooks(
    iroh_runtime_t runtime,
    uint64_t endpoint_or_node,
    iroh_hook_registration_t registration
);

// ─── Hook Response ───
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

// ─── Remote Info ───
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

// ─── Datagram Completion ───
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

---

## Appendix B: Event Kind Reference (Complete)

```c
typedef enum iroh_event_kind_e {
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
    
    // Docs
    IROH_EVENT_DOC_CREATED = 40,
    IROH_EVENT_DOC_JOINED = 41,
    IROH_EVENT_DOC_SET = 42,
    IROH_EVENT_DOC_GET = 43,
    IROH_EVENT_DOC_SHARED = 44,
    IROH_EVENT_AUTHOR_CREATED = 45,
    
    // Gossip
    IROH_EVENT_GOSSIP_SUBSCRIBED = 50,
    IROH_EVENT_GOSSIP_BROADCAST_DONE = 51,
    IROH_EVENT_GOSSIP_RECEIVED = 52,
    IROH_EVENT_GOSSIP_NEIGHBOR_UP = 53,
    IROH_EVENT_GOSSIP_NEIGHBOR_DOWN = 54,
    IROH_EVENT_GOSSIP_LAGGED = 55,
    
    // Datagrams
    IROH_EVENT_DATAGRAM_RECEIVED = 60,
    
    // Hooks
    IROH_EVENT_HOOK_BEFORE_CONNECT = 70,
    IROH_EVENT_HOOK_AFTER_CONNECT = 71,
    IROH_EVENT_HOOK_INVOCATION_RELEASED = 72,
    
    // Generic
    IROH_EVENT_STRING_RESULT = 90,
    IROH_EVENT_BYTES_RESULT = 91,
    IROH_EVENT_UNIT_RESULT = 92,
    IROH_EVENT_OPERATION_CANCELLED = 98,
    IROH_EVENT_ERROR = 99,
} iroh_event_kind_t;
```
