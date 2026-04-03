# Aster Python Implementation Plan

**Status:** Plan  
**Date:** 2026-04-03  
**Scope:** Layer the Aster RPC framework (spec v0.7.1) onto `bindings/aster_python`, using the existing transport bindings as the foundation.

---

## Table of Contents

1. [Current State](#1-current-state)
2. [Architecture](#2-architecture)
3. [Phase 1: Wire Protocol & Framing](#3-phase-1-wire-protocol--framing)
4. [Phase 2: Serialization Integration (Fory)](#4-phase-2-serialization-integration-fory)
5. [Phase 3: Transport Abstraction](#5-phase-3-transport-abstraction)
6. [Phase 4: Service Definition Layer](#6-phase-4-service-definition-layer)
7. [Phase 5: Server Implementation](#7-phase-5-server-implementation)
8. [Phase 6: Client Stub Generation](#8-phase-6-client-stub-generation)
9. [Phase 7: Interceptors & Middleware](#9-phase-7-interceptors--middleware)
10. [Phase 8: Session-Scoped Services](#10-phase-8-session-scoped-services)
11. [Phase 9: Contract Identity & Publication](#11-phase-9-contract-identity--publication)
12. [Phase 10: Service Registry & Discovery](#12-phase-10-service-registry--discovery)
13. [Phase 11: Testing & Conformance](#13-phase-11-testing--conformance)
14. [Dependency Map](#14-dependency-map)
15. [Open Pre-Requisites](#15-open-pre-requisites)

---

## 1. Current State

### 1.1 What Exists (Transport — Layer 1)

The `bindings/aster_python` package already provides a complete Layer 1 transport surface via PyO3 wrapping `aster_transport_core`:

| Component | Module | Status |
|-----------|--------|--------|
| `IrohNode` (memory/persistent, node_id, addr, close, secret key) | `node.rs` | ✅ Done |
| `NetClient` (connect, accept, endpoint_id/addr, close, monitoring, hooks) | `net.rs` | ✅ Done |
| `IrohConnection` (open_bi/uni, accept_bi/uni, datagrams, close, connection_info) | `net.rs` | ✅ Done |
| `IrohSendStream` (write_all, finish, stopped) | `net.rs` | ✅ Done |
| `IrohRecvStream` (read, read_exact, read_to_end, stop) | `net.rs` | ✅ Done |
| `BlobsClient` (add_bytes, read, tickets, collections, download) | `blobs.rs` | ✅ Done |
| `DocsClient` / `DocHandle` (create, set, get, share, join) | `docs.rs` | ✅ Done |
| `GossipClient` / `GossipTopicHandle` (subscribe, broadcast, recv) | `gossip.rs` | ✅ Done |
| `NodeAddr`, `EndpointConfig`, `ConnectionInfo`, `RemoteInfo` | `net.rs` | ✅ Done |
| Hooks (`HookConnectInfo`, `HookHandshakeInfo`, `HookDecision`, etc.) | `hooks.rs` | ✅ Done |
| Error types (`IrohError`, `BlobNotFound`, etc.) | `error.rs` | ✅ Done |

The Rust-side crate is `bindings/aster_python_rs` with `lib.rs` as module registration only.

### 1.2 What Needs to Be Built (Layers 2–5)

All RPC-layer code is **pure Python** — it uses the transport bindings but does not need new Rust/PyO3 code (except potentially for performance-critical Fory canonical encoding, which can be deferred).

```
bindings/aster_python/
├── __init__.py              # Existing: transport re-exports
├── __init__.pyi             # Existing: type stubs
├── _aster_python.abi3.so    # Existing: compiled transport bindings
│
│  ── NEW: Aster RPC Layer ──
├── aster/                   # NEW: Pure-Python RPC package
│   ├── __init__.py          # Public API: @service, @rpc, Server, create_client, etc.
│   ├── decorators.py        # @service, @rpc, @server_stream, @client_stream, @bidi_stream
│   ├── service.py           # ServiceRegistry, HandlerInfo, method introspection
│   ├── server.py            # Server accept loop, stream dispatch
│   ├── client.py            # Client stub generation (remote + local + session)
│   ├── codec.py             # ForyCodec (XLANG, NATIVE, ROW), compression
│   ├── framing.py           # Frame read/write, length-prefix, flags
│   ├── protocol.py          # StreamHeader, CallHeader, RpcStatus, constants
│   ├── status.py            # StatusCode enum, RpcError hierarchy
│   ├── types.py             # SerializationMode enum, RetryPolicy, shared types
│   ├── interceptors/
│   │   ├── __init__.py
│   │   ├── base.py          # Interceptor ABC, CallContext
│   │   ├── auth.py          # AuthInterceptor
│   │   ├── deadline.py      # DeadlineInterceptor
│   │   ├── audit.py         # AuditLogInterceptor
│   │   ├── metrics.py       # MetricsInterceptor (OTel)
│   │   ├── retry.py         # RetryInterceptor
│   │   └── circuit_breaker.py
│   ├── transport/
│   │   ├── __init__.py
│   │   ├── base.py          # Transport Protocol, BidiChannel
│   │   ├── iroh.py          # IrohTransport (remote, over Iroh connection)
│   │   └── local.py         # LocalTransport (in-process, asyncio.Queue)
│   ├── session.py           # Session-scoped service support (client + server)
│   ├── contract/
│   │   ├── __init__.py
│   │   ├── identity.py      # TypeDef, ServiceContract, canonical hashing
│   │   ├── manifest.py      # ContractManifest
│   │   └── publication.py   # Contract publication to registry
│   ├── registry/
│   │   ├── __init__.py
│   │   ├── client.py        # Registry consumer (discover, sync, resolve)
│   │   ├── publisher.py     # Registry publisher (register, heartbeat, leases)
│   │   ├── acl.py           # ACL management, sync-time validation
│   │   └── gossip.py        # Gossip event handling
│   └── testing/
│       ├── __init__.py
│       └── harness.py       # Mock services, local client factory, wire-compat mode
```

---

## 2. Architecture

### 2.1 Layer Mapping

```
┌─────────────────────────────────────────────────────────────┐
│ Layer 5: Registry (optional)                                │
│   contract/, registry/                                       │
│   Uses: DocsClient, GossipClient, BlobsClient from Layer 1 │
├─────────────────────────────────────────────────────────────┤
│ Layer 4: Service Definition                                 │
│   decorators.py, service.py                                 │
│   @service, @rpc, @server_stream, etc.                      │
├─────────────────────────────────────────────────────────────┤
│ Layer 3: RPC Protocol                                       │
│   framing.py, protocol.py, status.py, server.py, client.py │
│   interceptors/                                             │
├─────────────────────────────────────────────────────────────┤
│ Layer 2: Serialization                                      │
│   codec.py (wraps pyfory)                                   │
├─────────────────────────────────────────────────────────────┤
│ Layer 1: Transport (EXISTING)                               │
│   aster_python._aster_python (PyO3/Rust)                    │
│   IrohNode, NetClient, IrohConnection, streams, etc.        │
└─────────────────────────────────────────────────────────────┘
```

### 2.2 Design Principles

1. **Pure Python for Layers 2–5.** No new Rust/PyO3 code needed for the RPC layer. The transport bindings are the only FFI boundary.
2. **async/await throughout.** All RPC operations are asyncio coroutines.
3. **Spec-driven wire format.** Byte-level conformance with Aster-SPEC.md §6.
4. **pyfory for serialization.** Apache Fory's Python package handles XLANG, NATIVE, ROW.
5. **Incremental delivery.** Each phase produces testable, usable functionality.

---

## 3. Phase 1: Wire Protocol & Framing

**Goal:** Implement the byte-level wire protocol that all RPC communication rides on.

**Spec references:** §6.1 (framing), §6.2 (StreamHeader), §6.4 (RpcStatus/trailer), §6.5 (status codes)

### 3.1 `aster/status.py` — Status Codes & Errors

```python
class StatusCode(IntEnum):
    OK = 0
    CANCELLED = 1
    UNKNOWN = 2
    INVALID_ARGUMENT = 3
    DEADLINE_EXCEEDED = 4
    NOT_FOUND = 5
    ALREADY_EXISTS = 6
    PERMISSION_DENIED = 7
    RESOURCE_EXHAUSTED = 8
    FAILED_PRECONDITION = 9
    ABORTED = 10
    OUT_OF_RANGE = 11
    UNIMPLEMENTED = 12
    INTERNAL = 13
    UNAVAILABLE = 14
    DATA_LOSS = 15
    UNAUTHENTICATED = 16

class RpcError(Exception):
    code: StatusCode
    message: str
    details: dict[str, str]
```

### 3.2 `aster/types.py` — Shared Types

```python
class SerializationMode(IntEnum):
    XLANG = 0
    NATIVE = 1
    ROW = 2

@dataclass
class RetryPolicy:
    max_attempts: int = 3
    backoff: ExponentialBackoff = ...
```

### 3.3 `aster/framing.py` — Frame Read/Write

Implement the length-prefix framing from §6.1:

- `write_frame(send_stream, payload: bytes, flags: int) -> None`
- `read_frame(recv_stream) -> tuple[bytes, int] | None`
- Flag constants: `COMPRESSED = 0x01`, `TRAILER = 0x02`, `HEADER = 0x04`, `ROW_SCHEMA = 0x08`, `CALL = 0x10`, `CANCEL = 0x20`
- Max frame size enforcement (16 MiB)
- Zero-length frame rejection

### 3.4 `aster/protocol.py` — StreamHeader, CallHeader, RpcStatus

These types are always Fory XLANG-serialized on the wire:

```python
@dataclass
@fory_type(tag="_aster/StreamHeader")
class StreamHeader:
    service: str
    method: str
    version: int
    contract_id: str
    call_id: str
    deadline_epoch_ms: int
    serialization_mode: int
    metadata_keys: list[str]
    metadata_values: list[str]

@dataclass
@fory_type(tag="_aster/CallHeader")
class CallHeader:
    method: str
    call_id: str
    deadline_epoch_ms: int
    metadata_keys: list[str]
    metadata_values: list[str]

@dataclass
@fory_type(tag="_aster/RpcStatus")
class RpcStatus:
    code: int
    message: str
    detail_keys: list[str]
    detail_values: list[str]
```

### 3.5 Steps

1. Create `aster/status.py` with `StatusCode` enum and `RpcError` hierarchy.
2. Create `aster/types.py` with `SerializationMode`, `RetryPolicy`, decorator option types.
3. Create `aster/framing.py` with async frame read/write over `IrohSendStream`/`IrohRecvStream`.
4. Create `aster/protocol.py` with `StreamHeader`, `CallHeader`, `RpcStatus` dataclasses.
5. Unit tests: round-trip frame encoding/decoding, flag parsing, max-size rejection.

### 3.6 Exit Criteria

- Can write and read frames with all flag combinations over real Iroh streams.
- StreamHeader/RpcStatus serialize to deterministic bytes via Fory XLANG.
- Status codes and error types are complete per §6.5.

---

## 4. Phase 2: Serialization Integration (Fory)

**Goal:** Integrate Apache Fory (pyfory) as the codec layer for all three serialization modes.

**Spec references:** §5.1–5.6 (serialization protocols), §5.3 (XLANG tags), §5.5 (ROW mode)

### 4.1 `aster/codec.py` — ForyCodec

```python
class ForyCodec:
    def __init__(self, mode: SerializationMode, types: list[type]):
        """Initialize codec, register all types with Fory."""
        ...

    def encode(self, obj: Any) -> bytes:
        """Serialize an object according to the configured mode."""
        ...

    def decode(self, data: bytes, expected_type: type) -> Any:
        """Deserialize bytes into the expected type."""
        ...

    def encode_row_schema(self) -> bytes:
        """For ROW mode: serialize the schema for hoisting."""
        ...
```

### 4.2 `@fory_type` Decorator

```python
def fory_type(tag: str):
    """Declare a canonical tag for XLANG type registration."""
    def decorator(cls):
        cls.__fory_tag__ = tag
        return cls
    return decorator
```

### 4.3 Steps

1. Verify pyfory availability and XLANG/NATIVE/ROW mode support. Write a spike test.
2. Implement `@fory_type(tag=...)` decorator that annotates classes with `__fory_tag__`.
3. Implement `ForyCodec` wrapping pyfory for all three modes.
4. Implement tag-based type registration: walk type graph, register with Fory, validate all types have tags (for XLANG).
5. Implement compression: zstd compress/decompress for payloads exceeding threshold (default 4096 bytes).
6. Implement ROW schema hoisting helpers (encode schema frame, decode schema from first frame).
7. Register framework-internal types (`StreamHeader`, `CallHeader`, `RpcStatus`) with reserved `_aster/*` tags.
8. Tests: XLANG round-trip for dataclasses, NATIVE round-trip, ROW random-access field read, compression round-trip.

### 4.4 Exit Criteria

- All three serialization modes work end-to-end with Python dataclasses.
- `@fory_type` tag validation catches untagged types at registration time.
- Compression integrates transparently with framing.

### 4.5 Pre-Requisites

- **pyfory must support XLANG mode in Python.** Needs verification.
- **pyfory must support tag-based registration.** Needs verification.
- If pyfory gaps are found, document them and decide: contribute upstream, use a shim, or defer that mode.

---

## 5. Phase 3: Transport Abstraction

**Goal:** Define the `Transport` protocol that decouples client stubs from the underlying transport (Iroh vs. in-process).

**Spec references:** §8.3.1 (Transport protocol), §8.3.2 (LocalTransport interceptors), §8.3.3 (wire-compatible mode)

### 5.1 `aster/transport/base.py` — Transport Protocol

```python
from typing import Protocol, AsyncIterator, Any

class Transport(Protocol):
    async def unary(
        self, service: str, method: str, request: Any,
        metadata: dict[str, str] = {}, deadline_epoch_ms: int = 0,
    ) -> Any: ...

    def server_stream(
        self, service: str, method: str, request: Any,
        metadata: dict[str, str] = {}, deadline_epoch_ms: int = 0,
    ) -> AsyncIterator[Any]: ...

    async def client_stream(
        self, service: str, method: str, requests: AsyncIterator[Any],
        metadata: dict[str, str] = {}, deadline_epoch_ms: int = 0,
    ) -> Any: ...

    def bidi_stream(
        self, service: str, method: str,
        metadata: dict[str, str] = {}, deadline_epoch_ms: int = 0,
    ) -> "BidiChannel": ...

class BidiChannel:
    async def send(self, msg: Any) -> None: ...
    async def recv(self) -> Any: ...
    async def close(self) -> None: ...
    # Async context manager support
```

### 5.2 `aster/transport/iroh.py` — IrohTransport

The remote transport that opens a QUIC stream per RPC call:

1. `open_bi()` on the `IrohConnection`.
2. Write `StreamHeader` (HEADER flag) as first frame.
3. Write request payload frame(s).
4. Read response payload frame(s) and/or trailer.
5. Handle streaming patterns per §6.3.

### 5.3 `aster/transport/local.py` — LocalTransport

In-process transport using `asyncio.Queue`:

1. Dispatches directly to the service handler.
2. Runs the full interceptor chain (not optional — §8.3.2).
3. Supports `wire_compatible=True` mode for Fory round-trip testing.

### 5.4 Steps

1. Define `Transport` protocol and `BidiChannel` in `transport/base.py`.
2. Implement `IrohTransport` using framing + codec + existing transport bindings.
3. Implement `LocalTransport` with asyncio.Queue-based streaming.
4. Implement `wire_compatible` flag on LocalTransport.
5. Tests: IrohTransport unary round-trip over real Iroh connection, LocalTransport unary round-trip.

### 5.5 Exit Criteria

- Both transports pass identical unary call tests.
- LocalTransport with `wire_compatible=True` catches missing type tags.
- BidiChannel works for both transports.

---

## 6. Phase 4: Service Definition Layer

**Goal:** Implement the decorator-based service definition that developers use to define RPC services.

**Spec references:** §7.1–7.4 (Python decorators), §7.6 (language ownership)

### 6.1 `aster/decorators.py` — Decorators

```python
@service(name="AgentControl", version=1, serialization=[SerializationMode.XLANG])
class AgentControlService:

    @rpc(timeout=30.0, idempotent=True)
    async def assign_task(self, req: TaskAssignment) -> TaskAck: ...

    @server_stream
    async def step_updates(self, req: TaskId) -> AsyncIterator[StepUpdate]: ...

    @client_stream
    async def upload_artifacts(self, stream: AsyncIterator[ArtifactChunk]) -> UploadResult: ...

    @bidi_stream
    async def approval_loop(
        self, requests: AsyncIterator[ApprovalRequest]
    ) -> AsyncIterator[ApprovalResponse]: ...
```

### 6.2 `aster/service.py` — Service Registry & Introspection

```python
@dataclass
class MethodInfo:
    name: str
    pattern: str  # "unary", "server_stream", "client_stream", "bidi_stream"
    request_type: type
    response_type: type  # For streaming: the item type
    timeout: float | None
    idempotent: bool
    serialization: SerializationMode | None  # Override

@dataclass
class ServiceInfo:
    name: str
    version: int
    scoped: str  # "shared" or "stream"
    methods: dict[str, MethodInfo]
    serialization_modes: list[SerializationMode]
    interceptors: list[type]
    max_concurrent_streams: int

class ServiceRegistry:
    """Holds all registered services, supports dispatch lookup."""
    def register(self, service_class: type) -> ServiceInfo: ...
    def lookup(self, service: str, method: str) -> tuple[ServiceInfo, MethodInfo] | None: ...
```

### 6.3 Steps

1. Implement `@service` class decorator: attaches `ServiceInfo` to the class, inspects all methods.
2. Implement `@rpc`, `@server_stream`, `@client_stream`, `@bidi_stream` method decorators.
3. Type introspection: extract request/response types from method signatures via `typing.get_type_hints()` and `inspect`.
4. Eager Fory type validation: at decoration time, verify all types in the graph have `@fory_type` tags (for XLANG mode).
5. `ServiceRegistry` for looking up services by name and dispatching to methods.
6. Tests: decorate a test service, verify introspection produces correct `ServiceInfo` and `MethodInfo`.

### 6.4 Exit Criteria

- Decorating a class produces complete `ServiceInfo` with all methods, types, and options.
- Missing `@fory_type` tags on XLANG types raise `TypeError` at class definition time.
- `ServiceRegistry` can look up services and methods by name.

---

## 7. Phase 5: Server Implementation

**Goal:** Implement the server that accepts connections, dispatches RPC calls to handlers, and sends responses.

**Spec references:** §8.1 (Server API), §6.3 (stream lifecycle per pattern), §6.4 (trailer)

### 7.1 `aster/server.py` — Server

```python
class Server:
    def __init__(
        self,
        endpoint: NetClient | IrohNode,
        services: list[object],  # Service implementation instances
        interceptors: list[Interceptor] = [],
    ): ...

    async def serve(self) -> None:
        """Accept connections and dispatch RPCs. Runs until shutdown."""
        ...

    async def drain(self, grace_period: float = 10.0) -> None:
        """Graceful shutdown."""
        ...
```

### 7.2 Server Accept Loop

```
1. endpoint.accept() → Connection
2. Per connection: spawn task for connection.accept_bi() loop
3. Per stream:
   a. Read first frame (HEADER flag) → StreamHeader
   b. Validate: service exists, method exists, contract_id matches, serialization_mode supported
   c. If method == "" and scoped == "stream" → session dispatch (Phase 8)
   d. Else → stateless dispatch:
      - Run interceptor chain on_request
      - Call handler
      - Run interceptor chain on_response
      - Write response frame(s) + trailer
      - finish()
```

### 7.3 Pattern-Specific Dispatch

| Pattern | Server reads | Server writes |
|---------|-------------|---------------|
| Unary | 1 request frame | 1 response frame |
| Server stream | 1 request frame | N response frames + trailer |
| Client stream | N request frames (until client finish) | 1 response frame |
| Bidi stream | Concurrent read loop | Concurrent write loop + trailer |

### 7.4 Steps

1. Implement `Server.__init__`: register services, build dispatch table.
2. Implement connection accept loop with per-connection task spawning.
3. Implement stream dispatch: read StreamHeader, validate, route to handler.
4. Implement unary dispatch: read request, call handler, write response.
5. Implement server-stream dispatch: read request, iterate handler, write frames + trailer.
6. Implement client-stream dispatch: read frames until finish, call handler, write response.
7. Implement bidi-stream dispatch: concurrent read/write tasks.
8. Implement `drain()` for graceful shutdown.
9. Error handling: catch handler exceptions, map to RpcStatus, send trailer.
10. Tests: echo service (unary), counter service (server-stream), aggregation service (client-stream).

### 7.5 Exit Criteria

- Server dispatches all four RPC patterns correctly.
- Handler exceptions produce proper trailer status codes.
- Graceful shutdown drains in-flight calls.
- Unknown service/method returns `UNIMPLEMENTED`.

---

## 8. Phase 6: Client Stub Generation

**Goal:** Generate typed client stubs that make RPC calls through a Transport.

**Spec references:** §8.2 (Client API), §8.3 (Local Client)

### 8.1 `aster/client.py` — Client Factories

```python
async def create_client(
    service_class: type,
    connection: IrohConnection | None = None,
    transport: Transport | None = None,
    interceptors: list[Interceptor] = [],
) -> Any:
    """Create a remote client stub for the given service."""
    ...

def create_local_client(
    service_class: type,
    implementation: object,
    wire_compatible: bool = False,
    interceptors: list[Interceptor] = [],
) -> Any:
    """Create an in-process client stub."""
    ...
```

### 8.2 Stub Generation

The client stub is a dynamically-generated class that mirrors the service's method signatures:

```python
client = await create_client(AgentControlService, connection=conn)

# Unary
ack = await client.assign_task(task, timeout=10.0, metadata={"trace_id": "abc"})

# Server stream
async for update in client.step_updates(TaskId(task_id="t1")):
    print(update)

# Client stream
result = await client.upload_artifacts(artifact_iterator)

# Bidi stream
async with client.approval_loop() as (send, recv):
    await send(ApprovalRequest(...))
    response = await recv()
```

### 8.3 Steps

1. Implement stub class generation from `ServiceInfo` metadata.
2. For each method, generate a stub that:
   a. Acquires interceptor chain.
   b. Calls `transport.unary()` / `transport.server_stream()` / etc.
   c. Returns typed result.
3. Implement `create_client()` factory (IrohTransport backend).
4. Implement `create_local_client()` factory (LocalTransport backend).
5. Implement per-call metadata and timeout override.
6. Tests: client ↔ server unary round-trip, streaming round-trip, local client round-trip.

### 8.4 Exit Criteria

- Client stubs are type-safe and match the service interface.
- Remote and local clients produce identical results for the same service.
- Metadata and timeout overrides propagate to StreamHeader.
- LocalTransport with `wire_compatible=True` catches serialization issues.

---

## 9. Phase 7: Interceptors & Middleware

**Goal:** Implement the interceptor chain that wraps every RPC call.

**Spec references:** §9.1 (Interceptor interface), §9.2 (standard interceptors), §6.8 (deadlines)

### 9.1 `aster/interceptors/base.py` — Interceptor ABC

```python
@dataclass
class CallContext:
    service: str
    method: str
    call_id: str
    session_id: str | None
    peer: str | None  # EndpointId
    metadata: dict[str, str]
    deadline: float | None
    is_streaming: bool

class Interceptor(ABC):
    async def on_request(self, ctx: CallContext, request: object) -> object:
        return request
    async def on_response(self, ctx: CallContext, response: object) -> object:
        return response
    async def on_error(self, ctx: CallContext, error: RpcError) -> RpcError | None:
        return error
```

### 9.2 Standard Interceptors

| Interceptor | Priority | Description |
|-------------|----------|-------------|
| `DeadlineInterceptor` | High | Enforce `deadline_epoch_ms`, cancel handler on expiry |
| `AuthInterceptor` | High | Inject/validate tokens in metadata |
| `RetryInterceptor` | Medium | Auto-retry idempotent methods on transient failure |
| `AuditLogInterceptor` | Medium | Log calls for replay/audit |
| `MetricsInterceptor` | Low | OTel spans and metrics |
| `CircuitBreakerInterceptor` | Low | Trip on sustained failure |

### 9.3 Steps

1. Implement `CallContext` and `Interceptor` base class.
2. Implement interceptor chain runner (ordered execution, short-circuit on error).
3. Implement `DeadlineInterceptor`: check `deadline_epoch_ms`, set `asyncio.timeout`, cancel on expiry.
4. Implement `AuthInterceptor`: token injection/validation via metadata.
5. Implement `RetryInterceptor`: retry idempotent methods on `UNAVAILABLE` with exponential backoff.
6. Implement `CircuitBreakerInterceptor`: CLOSED → OPEN → HALF-OPEN state machine.
7. Implement `AuditLogInterceptor`: structured logging.
8. Implement `MetricsInterceptor`: OTel span creation (optional dependency).
9. Wire interceptors into both server dispatch and client stubs.
10. Tests: deadline enforcement, retry behavior, circuit breaker transitions.

### 9.4 Exit Criteria

- Interceptor chain runs on both client and server side.
- Interceptors run on LocalTransport calls (not skipped).
- DeadlineInterceptor cancels handlers correctly.
- CircuitBreakerInterceptor transitions through all states.

---

## 10. Phase 8: Session-Scoped Services

**Goal:** Implement session-scoped services where a single QUIC stream carries multiple sequential typed RPC calls against a persistent instance.

**Spec references:** Aster-session-scoped-services.md (full addendum)

### 10.1 Service Definition

```python
@service(name="AgentControl", version=1, scoped="stream",
         serialization=[SerializationMode.XLANG])
class AgentControlSession:
    def __init__(self, peer: str):
        self.peer = peer
        self.state = {}

    async def on_session_close(self): ...

    @rpc
    async def assign_task(self, req: TaskAssignment) -> TaskAck: ...

    @server_stream
    async def step_updates(self, req: TaskId) -> AsyncIterator[StepUpdate]: ...
```

### 10.2 Wire Protocol Changes

- `StreamHeader.method = ""` signals session mode.
- `CALL` flag (0x10) introduces each call within the session.
- `CANCEL` flag (0x20) cancels the current in-flight call without killing the session.
- Client-stream and bidi-stream calls use explicit `TRAILER` instead of `finish()` to signal end-of-input.
- Calls are strictly sequential (one at a time, async-locked on client).

### 10.3 `aster/session.py` — Session Support

```python
async def create_session(
    service_class: type,
    connection: IrohConnection | None = None,
    transport: Transport | None = None,
) -> Any:
    """Open a session stream and return a session stub."""
    ...
```

### 10.4 Steps

1. Extend `@service` decorator to accept `scoped="stream"` parameter.
2. Implement session client stub with internal `asyncio.Lock` for sequential call dispatch.
3. Implement `CALL` frame writing/reading on session streams.
4. Implement `CANCEL` frame sending (client) and handling (server).
5. Implement server-side session loop: instantiate class, loop on CALL frames, dispatch.
6. Implement `on_session_close()` lifecycle hook.
7. Implement client-side: `break` from async iteration sends CANCEL, `session.close()` sends finish.
8. Implement LocalTransport session support.
9. Tests: session with multiple sequential calls, cancellation mid-stream, session close lifecycle.

### 10.5 Exit Criteria

- Session-scoped services maintain instance state across calls.
- Sequential call semantics enforced (async lock on client).
- CANCEL frame cancels only the current call, not the session.
- `on_session_close()` fires on all termination paths.

---

## 11. Phase 9: Contract Identity & Publication

**Goal:** Implement content-addressed contract identity using Fory XLANG canonical encoding and BLAKE3 hashing.

**Spec references:** Aster-ContractIdentity.md (full document), §11.3 (canonicalization)

### 11.1 `aster/contract/identity.py` — Type & Contract Hashing

```python
def compute_type_hash(type_def: TypeDef) -> str:
    """Serialize TypeDef to canonical XLANG bytes, hash with BLAKE3, return hex."""
    ...

def compute_contract_id(contract: ServiceContract) -> str:
    """Serialize ServiceContract to canonical XLANG bytes, hash with BLAKE3, return hex."""
    ...

def resolve_type_graph(service_class: type) -> dict[str, TypeDef]:
    """Walk method signatures, build TypeDef for each type, hash bottom-up."""
    ...
```

### 11.2 Framework-Internal Types

Implement `TypeDef`, `FieldDef`, `EnumValueDef`, `UnionVariantDef`, `MethodDef`, `ServiceContract` as Python dataclasses with `@fory_type(tag="_aster/...")` tags. These are serialized with the canonical XLANG profile (§11.3.2):

- Fields in ascending field ID order
- No reference tracking
- Schema-consistent mode
- Standalone serialization
- No compression

### 11.3 `aster/contract/manifest.py` — ContractManifest

```python
@dataclass
class ContractManifest:
    service: str
    version: int
    contract_id: str
    canonical_encoding: str  # "fory-xlang/0.15"
    type_count: int
    type_hashes: list[str]
    method_count: int
    serialization_modes: list[str]
    alpn: str
    deprecated: bool
    published_by: str  # AuthorId
    published_at_epoch_ms: int
```

### 11.4 `aster/contract/publication.py` — Publish to Registry

```python
async def publish_contract(
    node: IrohNode,
    service_class: type,
    registry_doc: DocHandle,
) -> str:
    """
    1. Resolve type graph
    2. Serialize each TypeDef to canonical XLANG bytes
    3. Serialize ServiceContract, compute contract_id
    4. Build Iroh collection
    5. Import into blobs store
    6. Write ArtifactRef to docs
    7. Return contract_id
    """
    ...
```

### 11.5 Steps

1. Implement framework-internal type dataclasses (`TypeDef`, `FieldDef`, etc.).
2. Implement canonical XLANG profile serialization (field-order enforcement, no ref tracking).
3. Implement bottom-up type hashing with self-reference handling.
4. Implement `ServiceContract` construction from `ServiceInfo`.
5. Implement `compute_contract_id()`.
6. Implement `ContractManifest` construction.
7. Implement contract collection building (requires `BlobsClient.add_bytes_as_collection` or equivalent).
8. Implement `publish_contract()` to write ArtifactRef to docs.
9. Implement contract fetching and verification (`blake3(contract.xlang) == contract_id`).
10. Tests: hash stability (same input → same hash), self-referencing types, contract publication round-trip.

### 11.6 Exit Criteria

- `contract_id` is deterministic and stable for the same service definition.
- Changing any type in the graph changes the `contract_id`.
- Published contracts can be fetched and verified from another node.

### 11.7 Pre-Requisites

- **pyfory canonical XLANG profile must produce deterministic bytes.** This is the critical verification point. If pyfory cannot guarantee field-order and standalone serialization, a custom canonical encoder is needed.
- **blake3 Python package** must be available (it is — `blake3` on PyPI).

---

## 12. Phase 10: Service Registry & Discovery

**Goal:** Implement the decentralized service registry using iroh-docs, iroh-gossip, and iroh-blobs.

**Spec references:** §11 (full registry section), §11.6 (endpoint leases), §11.7 (gossip events), §11.8 (resolution flows)

### 12.1 `aster/registry/publisher.py` — Service Advertisement

```python
class RegistryPublisher:
    def __init__(self, node: IrohNode, registry_doc: DocHandle, author_id: str): ...

    async def publish_contract(self, service_class: type) -> str: ...
    async def advertise_endpoint(
        self, contract_id: str, service_name: str, version: int,
        health_status: str = "ready",
    ) -> None: ...
    async def refresh_lease(self) -> None: ...
    async def withdraw(self) -> None: ...
```

### 12.2 `aster/registry/client.py` — Service Resolution

```python
class RegistryClient:
    def __init__(self, node: IrohNode, registry_doc: DocHandle): ...

    async def resolve(
        self, service_name: str, version: int | None = None,
        channel: str | None = None,
    ) -> list[EndpointLease]: ...

    async def fetch_contract(self, contract_id: str) -> ServiceContract: ...

    def on_change(self, callback: Callable) -> None:
        """Subscribe to gossip-driven change notifications."""
        ...
```

### 12.3 `aster/registry/acl.py` — Access Control

```python
class RegistryACL:
    async def get_writers(self) -> list[str]: ...
    async def get_readers(self) -> list[str]: ...
    async def get_admins(self) -> list[str]: ...
    async def add_writer(self, author_id: str) -> None: ...
    async def remove_writer(self, author_id: str) -> None: ...
```

### 12.4 `aster/registry/gossip.py` — Change Notifications

```python
class RegistryGossip:
    async def broadcast_contract_published(self, contract_id: str, service: str) -> None: ...
    async def broadcast_endpoint_lease(self, endpoint_id: str, service: str) -> None: ...
    async def broadcast_endpoint_down(self, endpoint_id: str) -> None: ...
    async def listen(self) -> AsyncIterator[GossipEvent]: ...
```

### 12.5 Steps

1. Define `EndpointLease` dataclass matching §11.6.
2. Define `ArtifactRef` dataclass matching §11.2.1.
3. Define `GossipEvent` dataclass matching §11.7.
4. Implement `RegistryPublisher`: write lease to docs, refresh on timer, withdraw on shutdown.
5. Implement `RegistryClient`: resolve service → contract_id → endpoint leases, fetch contract collection.
6. Implement trusted-author filtering on docs reads (§11.2.3): query key, filter by ACL writers.
7. Implement `RegistryACL`: read/write `_aster/acl/*` keys.
8. Implement `RegistryGossip`: broadcast and listen for registry events.
9. Implement endpoint selection strategies: round_robin, least_load, random (§11.9).
10. Tests: publish contract, resolve from second node, lease expiry, gossip notification round-trip.

### 12.6 Exit Criteria

- A service can publish its contract and advertise its endpoint.
- A client can resolve a service by name and connect to the best endpoint.
- Lease refresh keeps endpoints alive; lease expiry removes stale entries.
- Gossip notifications accelerate discovery (but docs remains authoritative).
- ACL enforcement: untrusted authors' entries are rejected.

---

## 13. Phase 11: Testing & Conformance

**Goal:** Comprehensive test coverage and wire-format conformance vectors.

### 13.1 Test Categories

| Category | Location | Description |
|----------|----------|-------------|
| Unit tests | `tests/python/test_aster_framing.py` | Frame encoding/decoding |
| Unit tests | `tests/python/test_aster_codec.py` | Fory serialization modes |
| Unit tests | `tests/python/test_aster_decorators.py` | Service introspection |
| Integration | `tests/python/test_aster_unary.py` | Unary RPC end-to-end |
| Integration | `tests/python/test_aster_streaming.py` | All streaming patterns |
| Integration | `tests/python/test_aster_session.py` | Session-scoped services |
| Integration | `tests/python/test_aster_interceptors.py` | Interceptor chain |
| Integration | `tests/python/test_aster_registry.py` | Registry publish/resolve |
| Integration | `tests/python/test_aster_local.py` | LocalTransport parity |
| Conformance | `tests/conformance/wire/` | Byte-level wire format vectors |
| Conformance | `tests/conformance/fory/` | Serialization golden vectors |

### 13.2 `aster/testing/harness.py` — Test Harness

```python
class AsterTestHarness:
    """Convenience harness for testing Aster services."""

    async def create_local_pair(
        self, service_class: type, implementation: object,
        wire_compatible: bool = True,
    ) -> tuple[Any, Server]:
        """Create a local client + server for testing."""
        ...

    async def create_remote_pair(
        self, service_class: type, implementation: object,
    ) -> tuple[Any, Server, IrohNode, IrohNode]:
        """Create two Iroh nodes, server + client, connected."""
        ...
```

### 13.3 Steps

1. Write unit tests for framing (Phase 1 deliverable).
2. Write unit tests for codec (Phase 2 deliverable).
3. Write unit tests for decorators and service introspection (Phase 4 deliverable).
4. Write integration tests for each RPC pattern with a real echo/counter service.
5. Write integration tests for session-scoped services.
6. Write integration tests for interceptor chain.
7. Write integration tests for registry.
8. Generate conformance byte vectors for wire format.
9. Implement test harness for convenient local and remote testing.

### 13.4 Exit Criteria

- All RPC patterns work end-to-end over real Iroh connections.
- LocalTransport produces identical results to IrohTransport.
- Wire-compatible mode catches serialization issues.
- Conformance vectors exist for all frame types and StreamHeader/RpcStatus.

---

## 14. Dependency Map

```
Phase 1: Wire Protocol & Framing
    ↓
Phase 2: Serialization (Fory)      ← REQUIRES: pyfory verification
    ↓
Phase 3: Transport Abstraction
    ↓
Phase 4: Service Definitions ──────────────────┐
    ↓                                           │
Phase 5: Server ───────┐                       │
    ↓                   │                       │
Phase 6: Client ────────┤                       │
    ↓                   │                       │
Phase 7: Interceptors ──┘                       │
    ↓                                           │
Phase 8: Sessions                               │
    ↓                                           │
Phase 9: Contract Identity ─────────────────────┘
    ↓
Phase 10: Registry
    ↓
Phase 11: Testing & Conformance (ongoing throughout)
```

**Critical path:** Phases 1 → 2 → 3 → 4 → 5 → 6 (minimal viable RPC)

**Can proceed in parallel:**
- Phase 7 (Interceptors) can start after Phase 4.
- Phase 9 (Contract Identity) can start after Phase 2 + Phase 4.
- Phase 11 (Testing) runs continuously.

---

## 15. Open Pre-Requisites

### 15.1 Must Verify Before Starting

| Item | Risk | Mitigation |
|------|------|------------|
| pyfory XLANG mode works in Python | Medium | Spike test in Phase 2 step 1 |
| pyfory supports tag-based type registration | Medium | Spike test; fallback to numeric IDs |
| pyfory ROW mode available in Python | Low | ROW is optional for Phase 1; defer if unavailable |
| pyfory canonical profile produces deterministic bytes | High | Golden vector test; custom encoder if needed |
| blake3 Python package | None | Available on PyPI |
| zstandard Python package | None | Available on PyPI |

### 15.2 Dependencies to Add to `pyproject.toml`

```toml
[project]
dependencies = [
    "pyfory>=0.15",       # Apache Fory serialization
    "blake3>=1.0",        # BLAKE3 hashing for contract identity
    "zstandard>=0.22",    # zstd compression for frame payloads
]

[project.optional-dependencies]
otel = ["opentelemetry-api>=1.20"]  # Optional: MetricsInterceptor
```

### 15.3 Decisions to Lock Before Implementation

| Decision | Recommendation | Status |
|----------|---------------|--------|
| All RPC layer code is pure Python (no new Rust) | Yes — transport FFI is sufficient | Proposed |
| Package location: `bindings/aster_python/aster/` | Yes — sub-package of existing binding | Proposed |
| pyfory version pin | Pin to 0.15.x until Fory 1.0 | Proposed |
| ALPN for all Aster services | `aster/1` (single ALPN per §6.6) | Spec-defined |
| Session support included in Phase 1 | Yes — it's in the spec | Proposed |
| Registry is optional (can run without it) | Yes — services work with direct connect | Proposed |

---

## Implementation Timeline (Estimated)

| Phase | Description | Estimated Effort | Cumulative |
|-------|-------------|-----------------|------------|
| 1 | Wire Protocol & Framing | 2–3 days | 2–3 days |
| 2 | Serialization (Fory) | 3–5 days | 5–8 days |
| 3 | Transport Abstraction | 2–3 days | 7–11 days |
| 4 | Service Definitions | 2–3 days | 9–14 days |
| 5 | Server | 3–5 days | 12–19 days |
| 6 | Client | 2–3 days | 14–22 days |
| 7 | Interceptors | 3–5 days | 17–27 days |
| 8 | Sessions | 3–5 days | 20–32 days |
| 9 | Contract Identity | 3–5 days | 23–37 days |
| 10 | Registry | 5–7 days | 28–44 days |
| 11 | Testing & Conformance | Ongoing | — |

**Minimum viable RPC (Phases 1–6):** ~2–3 weeks for unary + streaming RPCs working end-to-end.

---

*End of plan.*