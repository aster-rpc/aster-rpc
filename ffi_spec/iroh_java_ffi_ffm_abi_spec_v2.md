# Iroh Java FFI/FFM Bridge — C ABI Design Specification

**Status:** Draft v1.1  
**Audience:** Rust bridge implementers, Java/Kotlin library authors, bindings for other languages  
**Scope:** Defines a portable C ABI for an iroh-based transport bridge implemented in Rust and consumed from Java via the Foreign Function & Memory (FFM) API. The Java API is asynchronous; the native ABI is queue/completion oriented.

---

## 1. Goals

This specification defines a native bridge that lets Java applications use iroh transport capabilities without mirroring Rust async traits or Tokio internals across the language boundary.

The design follows six principles:

1. **Rust owns transport and async I/O.** Rust manages iroh endpoints, connections, QUIC streams, ALPN routing, backpressure, buffering, and runtime scheduling.
2. **Java owns schema and object serialization.** Java chooses the serialization library and maps bytes to its own type system.
3. **The C ABI stays portable and language-neutral.** It uses opaque handles, POD structs, explicit lengths, and explicit memory ownership.
4. **The Java API is async-first.** The native layer uses submission and completion queues so Java can expose `CompletableFuture`, `Flow`, virtual-thread friendly wrappers, and Kotlin coroutines without making long blocking foreign calls.
5. **Endpoint identity is explicit.** Java can supply a concrete secret key so an endpoint has a stable, strong identity across restarts.
6. **Relay configuration is explicit.** Java can select relay behavior and provide a custom relay set instead of relying on defaults.

---

## 2. Non-goals

This specification does not define:

- a Java object serialization format
- Java code generation for service stubs
- RPC semantics above framed messages
- a wire protocol for application payloads
- a stable ABI for exposing raw Rust futures, Tokio types, or Rust stream traits
- a GraalVM LLVM embedding model

---

## 3. Background and rationale

Iroh provides encrypted QUIC connections, endpoints, and protocols layered on top of those connections. Protocol behavior is application-defined after the connection is established, and ALPN is used to route incoming connections to the right protocol handler.

QUIC streams in iroh are cheap to open and intended for efficient protocol construction, but that does **not** imply that Java should drive each stream through tiny per-read and per-write foreign calls. This bridge is therefore message-oriented at the FFI boundary.

On the JVM side, virtual threads are pinned while executing a foreign function. That does not make them unsafe, but it can reduce scalability if foreign calls block for long periods. The ABI therefore avoids long blocking calls and instead prefers short submission calls plus bounded event polling.

---

## 4. High-level architecture

### 4.1 Layer split

**Rust/native side**
- Owns Tokio runtime
- Owns iroh endpoint lifecycle
- Owns connection establishment and accept loops
- Owns QUIC stream lifecycle
- Performs framed send/receive I/O
- Maintains submission/completion queues
- Performs batching and backpressure
- Emits events to consumers

**Java/JVM side**
- Owns request and response classes
- Owns serializer/deserializer choice
- Owns async abstractions (`CompletableFuture`, `Flow`, coroutines)
- Owns high-level RPC semantics
- Owns business logic and service dispatch

### 4.2 Boundary contract

The native ABI transports **opaque application frames**. Native code must not deserialize application payloads.

The core contract is:

> Rust transports framed bytes. Java defines what those bytes mean.

---

## 5. Design principles

### 5.1 Queue/completion model

The native ABI uses:
- **submission operations** for connect, open stream, send frame, finish stream, cancel, etc.
- **completion events** for connected, accepted, frame received, send completed, stream finished, error, etc.

### 5.2 Opaque handles

All long-lived native objects are referenced through opaque 64-bit handles. Java must never interpret their contents.

### 5.3 Explicit ownership

Any native-allocated memory returned to the host must be released through an explicit ABI function.

### 5.4 No host callbacks in v1

Version 1 does not require native-to-host callbacks. The host polls for events. This keeps the ABI simpler, more portable, and easier to adapt to many languages.

### 5.5 Async at the Java layer

The C ABI is not expressed as Java futures. Instead, Java wraps short native calls and event polling in an async facade.

### 5.6 Explicit identity and relay control

The endpoint configuration allows the host to provide:
- a **32-byte Ed25519 secret key seed** for stable identity
- zero or more **custom relay URLs**
- a **relay mode** controlling whether defaults, custom relays, or no relays are used

---

## 6. Transport model

### 6.1 Entities

The ABI exposes these native concepts:

- **Runtime** — global async engine and queue owner
- **Endpoint** — local iroh endpoint identity and connection pool
- **Connection** — an iroh peer connection negotiated for a given ALPN
- **Stream** — a bidirectional or unidirectional QUIC stream
- **Operation** — an async native action tracked until completion
- **Event** — a completion or inbound transport signal
- **Buffer lease** — a native-owned payload buffer published to the host

### 6.2 Protocol assumptions

- One application protocol is identified by one ALPN string.
- A connection is associated with one ALPN at connect/accept time.
- Streams are used for framed application messages.
- Message framing is native-managed at the stream boundary.

---

## 7. Native framing

### 7.1 Purpose

The ABI defines a **transport framing layer**, not an application serialization format.

### 7.2 Framing format

Each application frame carried over a QUIC stream is encoded as:

```text
[varint payload_length][payload_bytes]
```

### 7.3 Optional envelope

Applications that need native-side correlation may prepend their own envelope inside `payload_bytes`, for example:

```text
version | service_id | method_id | request_id | flags | body...
```

This envelope is application-defined and out of scope for the C ABI.

### 7.4 Stream semantics

A stream can carry zero or more frames. Stream completion is signaled separately from frame boundaries.

---

## 8. ABI data types

### 8.1 Primitive types

The ABI uses C99 fixed-width integer types:

- `uint8_t`
- `uint16_t`
- `uint32_t`
- `uint64_t`
- `int32_t`
- `int64_t`
- `size_t`

### 8.2 Opaque handles

```c
typedef uint64_t iroh_runtime_t;
typedef uint64_t iroh_endpoint_t;
typedef uint64_t iroh_connection_t;
typedef uint64_t iroh_stream_t;
typedef uint64_t iroh_operation_t;
typedef uint64_t iroh_buffer_t;
```

Handle value `0` is invalid and reserved.

### 8.3 ABI versioning

```c
#define IROH_ABI_VERSION_MAJOR 1
#define IROH_ABI_VERSION_MINOR 1
#define IROH_ABI_VERSION_PATCH 0
```

The library must export:

```c
uint32_t iroh_abi_version_major(void);
uint32_t iroh_abi_version_minor(void);
uint32_t iroh_abi_version_patch(void);
```

---

## 9. Error model

### 9.1 Immediate return status

Synchronous ABI calls return an `iroh_status_t`.

```c
typedef enum iroh_status_e {
    IROH_STATUS_OK = 0,
    IROH_STATUS_INVALID_ARGUMENT = 1,
    IROH_STATUS_NOT_FOUND = 2,
    IROH_STATUS_ALREADY_CLOSED = 3,
    IROH_STATUS_QUEUE_FULL = 4,
    IROH_STATUS_BUFFER_TOO_SMALL = 5,
    IROH_STATUS_UNSUPPORTED = 6,
    IROH_STATUS_INTERNAL = 7
} iroh_status_t;
```

### 9.2 Asynchronous failures

Async operation failures are reported via `IROH_EVENT_ERROR`.

### 9.3 Error strings

The bridge may retain a thread-local last-error string retrievable through:

```c
size_t iroh_last_error_message(uint8_t* buffer, size_t capacity);
```

---

## 10. Structs

### 10.1 Byte slice views

```c
typedef struct iroh_bytes_s {
    const uint8_t* ptr;
    size_t len;
} iroh_bytes_t;
```

`iroh_bytes_t` is a borrowed view only.

### 10.2 Mutable buffer view

```c
typedef struct iroh_mut_bytes_s {
    uint8_t* ptr;
    size_t len;
} iroh_mut_bytes_t;
```

### 10.3 Borrowed list of byte slices

```c
typedef struct iroh_bytes_list_s {
    const iroh_bytes_t* items;
    size_t len;
} iroh_bytes_list_t;
```

For relay URLs, each `iroh_bytes_t` contains one UTF-8 string.

### 10.4 Runtime config

```c
typedef struct iroh_runtime_config_s {
    uint32_t struct_size;
    uint32_t flags;
    uint32_t worker_threads;   // 0 = runtime default
    uint32_t event_queue_capacity;
} iroh_runtime_config_t;
```

### 10.5 Endpoint config

```c
typedef struct iroh_endpoint_config_s {
    uint32_t struct_size;
    uint32_t flags;
    uint32_t relay_mode;                 // iroh_relay_mode_t
    iroh_bytes_t secret_key_seed;        // optional; exactly 32 bytes when present
    iroh_bytes_list_t custom_relay_urls; // optional UTF-8 relay URLs
    iroh_bytes_t data_dir_utf8;          // optional
    uint32_t max_concurrent_connections;
    uint32_t max_concurrent_streams;
} iroh_endpoint_config_t;
```

### 10.6 Connect config

```c
typedef struct iroh_connect_config_s {
    uint32_t struct_size;
    uint32_t flags;
    iroh_bytes_t ticket;
    iroh_bytes_t alpn_utf8;
} iroh_connect_config_t;
```

### 10.7 Send config

```c
typedef struct iroh_send_config_s {
    uint32_t struct_size;
    uint32_t flags;
    uint64_t application_message_id;
} iroh_send_config_t;
```

### 10.8 Relay modes

```c
typedef enum iroh_relay_mode_e {
    IROH_RELAY_MODE_DEFAULT = 0,
    IROH_RELAY_MODE_CUSTOM = 1,
    IROH_RELAY_MODE_DISABLED = 2
} iroh_relay_mode_t;
```

### 10.9 Event kinds

```c
typedef enum iroh_event_kind_e {
    IROH_EVENT_NONE = 0,
    IROH_EVENT_ENDPOINT_READY = 1,
    IROH_EVENT_CONNECTION_ACCEPTED = 2,
    IROH_EVENT_CONNECTION_CONNECTED = 3,
    IROH_EVENT_CONNECTION_CLOSED = 4,
    IROH_EVENT_STREAM_OPENED = 5,
    IROH_EVENT_STREAM_ACCEPTED = 6,
    IROH_EVENT_FRAME_RECEIVED = 7,
    IROH_EVENT_SEND_COMPLETED = 8,
    IROH_EVENT_STREAM_FINISHED = 9,
    IROH_EVENT_STREAM_RESET = 10,
    IROH_EVENT_OPERATION_CANCELLED = 11,
    IROH_EVENT_ERROR = 12
} iroh_event_kind_t;
```

### 10.10 Event struct

```c
typedef struct iroh_event_s {
    uint32_t struct_size;
    uint32_t kind;
    uint64_t operation;
    uint64_t endpoint;
    uint64_t connection;
    uint64_t stream;
    uint64_t application_message_id;
    uint32_t flags;
    uint32_t error_code;
    uint64_t buffer;
    const uint8_t* data_ptr;
    size_t data_len;
} iroh_event_t;
```

Rules:
- `data_ptr` is valid only if `buffer != 0` or event kind explicitly documents borrowed inline data.
- `buffer` identifies a native-owned payload lease.
- `application_message_id` echoes the message id from send operations if available.

---

## 11. Endpoint identity

### 11.1 Secret key seed

When `secret_key_seed.len != 0`, the host supplies the endpoint’s secret key material. In v1.1 this is defined as a **32-byte Ed25519 secret key seed**.

Validation rules:
- length must be either `0` or `32`
- invalid lengths cause `IROH_STATUS_INVALID_ARGUMENT`

### 11.2 Persistence expectations

If the host wants a stable endpoint identity across process restarts, it must persist this seed and pass it again when creating the endpoint.

### 11.3 Generated identity

If the seed is omitted, the bridge may generate an ephemeral identity or use library defaults.

---

## 12. Relay configuration

### 12.1 Relay mode

`relay_mode` controls how the endpoint is configured:

- `IROH_RELAY_MODE_DEFAULT`: use iroh defaults
- `IROH_RELAY_MODE_CUSTOM`: use only the provided `custom_relay_urls`
- `IROH_RELAY_MODE_DISABLED`: disable relay usage in the bridge’s endpoint builder path

### 12.2 Custom relay URLs

When `relay_mode == IROH_RELAY_MODE_CUSTOM`:
- at least one relay URL should be provided
- each relay URL must be valid UTF-8
- URL parsing errors surface as `IROH_STATUS_INVALID_ARGUMENT`

### 12.3 Discovery interaction

Discovery behavior is intentionally left separate from this ABI. The bridge focuses on endpoint identity, relays, connections, streams, and framed payload transport.

---

## 13. Functions

### 13.1 Versioning and diagnostics

```c
uint32_t iroh_abi_version_major(void);
uint32_t iroh_abi_version_minor(void);
uint32_t iroh_abi_version_patch(void);
size_t iroh_last_error_message(uint8_t* buffer, size_t capacity);
```

### 13.2 Runtime lifecycle

```c
iroh_status_t iroh_runtime_new(
    const iroh_runtime_config_t* config,
    iroh_runtime_t* out_runtime
);

iroh_status_t iroh_runtime_close(iroh_runtime_t runtime);
```

### 13.3 Endpoint lifecycle

```c
iroh_status_t iroh_endpoint_new(
    iroh_runtime_t runtime,
    const iroh_endpoint_config_t* config,
    iroh_endpoint_t* out_endpoint,
    iroh_operation_t* out_operation
);

iroh_status_t iroh_endpoint_close(iroh_endpoint_t endpoint);
```

`iroh_endpoint_new` is async in effect. `out_operation` completes with `IROH_EVENT_ENDPOINT_READY` or `IROH_EVENT_ERROR`.

### 13.4 Connection management

```c
iroh_status_t iroh_connect(
    iroh_endpoint_t endpoint,
    const iroh_connect_config_t* config,
    iroh_operation_t* out_operation
);

iroh_status_t iroh_accept(
    iroh_endpoint_t endpoint,
    iroh_operation_t* out_operation
);

iroh_status_t iroh_connection_close(
    iroh_connection_t connection,
    uint32_t error_code
);
```

### 13.5 Stream management

```c
iroh_status_t iroh_open_bi(
    iroh_connection_t connection,
    iroh_operation_t* out_operation
);

iroh_status_t iroh_accept_bi(
    iroh_connection_t connection,
    iroh_operation_t* out_operation
);

iroh_status_t iroh_stream_finish(
    iroh_stream_t stream,
    iroh_operation_t* out_operation
);

iroh_status_t iroh_stream_reset(
    iroh_stream_t stream,
    uint32_t error_code,
    iroh_operation_t* out_operation
);
```

### 13.6 Framed send

```c
iroh_status_t iroh_stream_send(
    iroh_stream_t stream,
    iroh_bytes_t payload,
    const iroh_send_config_t* config,
    iroh_operation_t* out_operation
);
```

The bridge frames the payload as `[varint length][payload bytes]` and schedules the write asynchronously.

### 13.7 Event polling

```c
size_t iroh_poll_events(
    iroh_runtime_t runtime,
    iroh_event_t* out_events,
    size_t max_events,
    uint32_t timeout_ms
);
```

Returns the number of populated events. `timeout_ms == 0` is nonblocking.

### 13.8 Buffer release

```c
iroh_status_t iroh_buffer_release(
    iroh_runtime_t runtime,
    iroh_buffer_t buffer
);
```

### 13.9 Operation cancellation

```c
iroh_status_t iroh_operation_cancel(
    iroh_runtime_t runtime,
    iroh_operation_t operation
);
```

---

## 14. Event semantics

### 14.1 `IROH_EVENT_ENDPOINT_READY`

Signals successful endpoint creation. `endpoint` is set.

### 14.2 `IROH_EVENT_CONNECTION_CONNECTED`

Signals a successful outbound connection. `connection` is set.

### 14.3 `IROH_EVENT_CONNECTION_ACCEPTED`

Signals an accepted inbound connection. `connection` is set.

### 14.4 `IROH_EVENT_STREAM_OPENED`

Signals a successful outbound bidirectional stream open. `stream` is set.

### 14.5 `IROH_EVENT_STREAM_ACCEPTED`

Signals an accepted inbound bidirectional stream. `stream` is set.

### 14.6 `IROH_EVENT_FRAME_RECEIVED`

Signals that one complete framed payload has been read from the stream. `buffer`, `data_ptr`, and `data_len` are set.

### 14.7 `IROH_EVENT_SEND_COMPLETED`

Signals that the specified send operation completed.

### 14.8 `IROH_EVENT_STREAM_FINISHED`

Signals that the peer cleanly finished its sending side, or the local finish operation completed.

### 14.9 `IROH_EVENT_STREAM_RESET`

Signals abrupt stream termination.

### 14.10 `IROH_EVENT_ERROR`

Signals an async failure. `error_code` is bridge-defined and `operation` identifies the failed operation when available.

---

## 15. Memory ownership rules

### 15.1 Borrowed input memory

All `iroh_bytes_t` and `iroh_bytes_list_t` input memory is borrowed for the duration of the FFI call only.

### 15.2 Event payload memory

For `IROH_EVENT_FRAME_RECEIVED`, the payload memory remains valid until the host calls `iroh_buffer_release` with the supplied buffer handle.

### 15.3 No retained host pointers

The bridge must not retain raw host pointers after the FFI call returns.

---

## 16. Java FFM binding model

### 16.1 Java shape

The Java layer should present an async-first API:

- `CompletableFuture<EndpointHandle> createEndpoint(...)`
- `CompletableFuture<ConnectionHandle> connect(...)`
- `CompletableFuture<StreamHandle> openStream(...)`
- `CompletableFuture<Void> send(...)`
- `Flow.Publisher<ByteBuffer>` or equivalent for inbound frames

### 16.2 Poller thread

The Java implementation should run an event pump that repeatedly calls `iroh_poll_events` and dispatches completions to futures and subscribers.

### 16.3 Virtual threads and coroutines

The Java/Kotlin layer may offer virtual-thread wrappers or coroutine wrappers, but foreign calls should remain short and bounded.

---

## 17. Portability

Because the ABI is a plain C ABI with opaque handles and POD structs, it can be reused by Java FFM, JNI, Python `ctypes`/CFFI, C#, Swift, Zig, Go, and other host languages.

Per-platform native builds are still required.

---

## 18. Security and operational guidance

- Treat the 32-byte secret key seed as highly sensitive secret material.
- Zero sensitive temporary buffers on the host and native side where practical.
- Prefer explicit custom relay configuration for production systems that require stronger control over routing and infrastructure.
- Use application-level authentication and authorization in addition to endpoint identity where needed.

---

## 19. Future extensions

Possible future ABI extensions include:

- unidirectional streams
- discovery configuration
- endpoint address watchers
- endpoint ticket helpers
- metrics and tracing export
- zero-copy shared-buffer registration
- optional callback-based wakeups

---

## 20. Summary

This specification defines a bridge where:

- Rust owns iroh, Tokio, connections, streams, framing, and event queues.
- Java owns serialization, types, futures, publishers, and coroutine-friendly wrappers.
- Endpoint identity can be made stable by supplying a 32-byte secret key seed.
- Relay behavior can be explicitly controlled with default, custom, or disabled modes.
- The ABI remains portable because it is plain C with opaque handles and explicit ownership rules.


## 24. Endpoint identity and relay configuration

The bridge must allow the Java host to provide a persisted secret key so the endpoint retains a stable cryptographic identity across restarts. Iroh documents that an endpoint has an `EndpointID` which is the public half of an Ed25519 keypair, and that the private key is used to sign and decrypt messages. This is why the ABI treats the secret key as a first-class part of endpoint creation. citeturn956274search1

The bridge must also allow the Java host to configure custom relay URLs. Iroh documents custom relay configuration via relay mode and relay URLs, including a `RelayMode::Custom` example and a recommendation to use at least two relays in different regions for production redundancy. citeturn666237search3turn666237search7

### 24.1 Secret key handling

Version 1 of the bridge uses raw secret-key bytes across the ABI. The bridge header and FFM layer therefore include:

- a helper to generate a fresh secret key
- an endpoint creation field for caller-supplied secret key bytes
- a helper to export the current endpoint secret key bytes

The application is responsible for secure persistence of those bytes.

### 24.2 Relay configuration

Version 1 of the bridge includes:

- default relay mode
- custom relay mode with an explicit list of relay URLs
- disabled relay mode placeholder in the ABI, even if the exact Rust builder path is version-sensitive and may need adjustment in the concrete implementation

### 24.3 Discovery

The bridge includes an `enable_default_discovery` flag so Java can opt into the default iroh discovery path or prepare for stricter custom discovery setups later. Iroh documents that discovery is enabled by default on the standard endpoint builder path, and that dialing by Endpoint ID depends on configured discovery. citeturn956274search1turn666237search10
