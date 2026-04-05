# Aster Python Implementation Plan

**Status:** Plan  
**Date:** 2026-04-03  
**Scope:** Layer the Aster RPC framework (spec v0.7.1) onto `bindings/aster`, using the existing transport bindings as the foundation.

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
13. [Phase 11: Trust Foundations](#13-phase-11-trust-foundations)
14. [Phase 12: Producer Mesh & Clock Drift](#14-phase-12-producer-mesh--clock-drift)
15. [Phase 13: Testing & Conformance](#15-phase-13-testing--conformance)
16. [Dependency Map](#16-dependency-map)
17. [Open Pre-Requisites](#17-open-pre-requisites)

---

## 1. Current State

### 1.1 What Exists (Transport — Layer 1)

The `bindings/aster` package already provides a complete Layer 1 transport surface via PyO3 wrapping `aster_transport_core`:

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

The Rust-side crate is `bindings/aster_rs` with `lib.rs` as module registration only.

### 1.2 What Needs to Be Built (Layers 2–5)

All RPC-layer code is **pure Python** — it uses the transport bindings but does not need new Rust/PyO3 code (except potentially for performance-critical Fory canonical encoding, which can be deferred).

```
bindings/aster/
├── __init__.py              # Existing: transport re-exports
├── __init__.pyi             # Existing: type stubs
├── _aster.abi3.so    # Existing: compiled transport bindings
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
│   aster._aster (PyO3/Rust)                    │
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
        cls.__aster_tag__ = tag
        return cls
    return decorator
```

### 4.3 Steps

1. Verify pyfory availability and XLANG/NATIVE/ROW mode support. Write a spike test.
2. Implement `@fory_type(tag=...)` decorator that annotates classes with `__aster_tag__`.
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

**Trust model (§8.3.2):** LocalTransport is for trusted in-process composition — no remote peer, no admission gate, no credential presentation.
- `CallContext.peer` is **always `None`** on LocalTransport calls.
- `CallContext.attributes` is **always `{}`** unless a test harness populates it.
- Gates 0 and 1 (connection-level admission + credential verification) are bypassed — they apply only to remote iroh connections.
- Interceptors MUST handle `peer is None` gracefully (canonical behavior: allow, since in-process callers are trusted).
- Test harnesses MAY synthesize a `peer` (e.g. `peer="test://alice"`) for testing auth-interceptor logic, but this is a test-scoped feature.

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

**Spec references:** Aster-session-scoped-services.md §3 (service definition), §4 (framing), §5 (cancellation), §7 (lifecycle), §8 (CallContext), §9 (interactions with other features).

**Phase 1 prerequisite (now fixed):** `write_frame` previously rejected any empty payload except TRAILER. Spec §5.2 requires CANCEL to be a **flags-only frame** (Length=1, empty payload). Fixed in `aster/framing.py`: the zero-length guard now permits `flags & (TRAILER | CANCEL)`.

### 10.1 Service Definition

```python
@service(name="AgentControl", version=1, scoped="stream",
         serialization=[SerializationMode.XLANG])
class AgentControlSession:
    def __init__(self, peer: EndpointId):
        """Framework passes the *verified* remote EndpointId (§7.1)."""
        self.peer = peer
        self.state = {}

    async def on_session_close(self): ...

    @rpc
    async def assign_task(self, req: TaskAssignment) -> TaskAck: ...

    @server_stream
    async def step_updates(self, req: TaskId) -> AsyncIterator[StepUpdate]: ...
```

### 10.2 Wire Protocol

**Stream discriminator (§4.1):**
- `StreamHeader.method == ""` + service's `scoped == "stream"` → session mode.
- `StreamHeader.call_id` is the **session_id** for the life of the stream (§8.2).
- Server MUST reject streams where this combination is mismatched (e.g. method=="" on a non-session service, or method!="" on a `scoped="stream"` service) with `FAILED_PRECONDITION`.

**Per-call framing (§4.2–4.5):**
- Each call begins with a `CALL` frame (flag `0x10`) whose payload is a Fory-encoded `CallHeader` carrying `call_id`, `method`, optional `metadata`, optional `deadline_epoch_ms` (int64, 0 = no deadline).
- Client-stream and bidi-stream calls signal end-of-input with an **explicit `TRAILER` frame (status=OK)** — *not* `finish()`. `finish()` is reserved for `session.close()`.
- Bidi: client MUST wait for the server's response TRAILER before releasing the session lock and sending the next CALL frame.
- If a CALL frame arrives while a call is in flight, the server rejects with `FAILED_PRECONDITION` and resets the stream.

**Unary trailer semantics (§4.6) — diverges from stateless RPC:**
- **Success:** response payload only, **no trailer frame**.
- **Error:** trailer with non-OK status instead of response payload.

Phase 5 (stateless) always writes a trailer; session unary dispatch takes a different code path.

**Cancellation (§5):**
- CANCEL is `Length=1, Flags=0x20, Payload=<empty>` (flags-only frame).
- Client cancels a call by sending CANCEL; session remains open for subsequent calls.
- After sending CANCEL, client MUST drain and discard response frames until it reads a trailer with status `CANCELLED` (§5.5).
- Server receives CANCEL → cancels the handler task → sends trailer with `CANCELLED`.
- CANCEL received on a non-session (stateless) stream is ignored (may log) (§5.6).

### 10.3 `aster/session.py` — Session Support

```python
async def create_session(
    service_class: type,
    connection: IrohConnection | None = None,
    transport: Transport | None = None,
) -> Any:
    """Open a session stream and return a session stub.

    Client acquires an internal asyncio.Lock; each RPC method on the stub
    acquires the lock for the duration of the call (unary: send CALL + read
    response; streaming: send CALL + read/write until TRAILER).
    """
    ...


class SessionServer:
    """Per-stream server session loop (§7.2)."""
    async def run(self, instance: Any, stream_header: StreamHeader):
        # 1. Validate method=="" and service.scoped=="stream"
        # 2. Populate CallContext.session_id = stream_header.call_id
        # 3. Loop: read CALL frame → dispatch handler → on completion loop again
        # 4. On stream EOF / error / reset: call instance.on_session_close()
        ...
```

### 10.4 CallContext Changes (§8.2)

Sessions extend `CallContext` with session-specific and auth fields:

| Field | Source | Notes |
|-------|--------|-------|
| `session_id` | `StreamHeader.call_id` (constant for stream lifetime) | `None` for stateless streams |
| `call_id` | `CallHeader.call_id` (per-call) | |
| `peer` | Verified remote EndpointId from connection | Populated on every in-session call; `None` on LocalTransport unless synthesized |
| `metadata` | `CallHeader.metadata` | Per-call override |
| `deadline_epoch_ms` | `CallHeader.deadline_epoch_ms` | Per-call override (int64, 0 = none) |
| `is_streaming` | Derived from method's `MethodPattern` | True for server/client/bidi-stream |
| `attributes` | Verified rcan claims / admission-credential attributes | Populated by Phase 11 trust layer; `{}` if trust disabled |

Interceptors run **per-call** (not per-session) — the chain fires on every CALL frame with a fresh `CallContext`, but `session_id`, `peer`, and `attributes` stay constant for the stream's lifetime (they come from the stream-level handshake, not per-call state).

### 10.5 Feature Interactions (§9)

- **Serialization mode is fixed per-session (§9.1).** `StreamHeader.serialization_mode` is locked for the life of the stream. Per-call serialization overrides are **rejected** with `INVALID_ARGUMENT`.
- **Retry within a session (§9.4).** `RetryInterceptor` retries idempotent in-session calls on the **same stream**. A stream-level reset or transport error aborts the **entire session** — not retried.
- **Deadlines** are per-call (from `CallHeader.deadline_epoch_ms`); not session-wide.

**Session lock release semantics (per-pattern):**

| RPC pattern | Client releases session lock when… |
|-------------|-------------------------------------|
| unary (success) | Response payload frame is fully read (no trailer on success, §4.6) |
| unary (error) | Trailer with non-OK status is read |
| server-stream | Trailer is read (any status) |
| client-stream | Response payload frame is fully read after server's trailer, OR trailer-only if error |
| bidi-stream | Server's TRAILER frame is read (signals call complete) |

Do not wait for a trailer on successful unary — the response payload is the terminator.

### 10.6 Steps

1. Extend `@service` decorator to accept `scoped: Literal["shared", "stream"]` (default `"shared"`). Reflect into `ServiceInfo.scoped`.
2. Validate `service_class.__init__` signature accepts `peer` parameter when `scoped="stream"`.
3. Implement `CallHeader` Fory type + reader (Phase 1 dataclass exists; add `read_call_header(recv)` helper).
4. Server-side stream router: inspect `StreamHeader.method` + service's `scoped` → dispatch to `SessionServer.run()` or existing stateless dispatch. Reject mismatches with `FAILED_PRECONDITION`.
5. Implement `SessionServer.run()` loop: instantiate `service_class(peer=verified_endpoint_id)`, read CALL frames, dispatch to handlers with populated `CallContext` (session_id from StreamHeader, per-call context from CallHeader).
6. Implement server-side unary dispatch for in-session: success = response payload only (no trailer), error = trailer with non-OK status + no response (§4.6).
7. Implement server-side CANCEL handling: separate reader task monitors frames; on CANCEL flag, cancel the in-flight handler task and write a trailer with `CANCELLED`.
8. Implement server-side mid-call CALL rejection: if a new CALL arrives while handler is running, write trailer with `FAILED_PRECONDITION` and reset the stream.
9. Implement client-side session stub factory: walks `ServiceInfo.methods`, generates a stub class with an internal `asyncio.Lock`, per-method implementations that acquire the lock and drive one request/response cycle.
10. Implement client-side unary call: acquire lock → write CALL + request → read response payload → release lock. (No trailer on success.)
11. Implement client-side streaming calls: acquire lock → write CALL + request frames → read until TRAILER (for server-stream) or write request frames + TRAILER(status=OK) EoI → read response (for client-stream) → bidi combines both.
12. Implement client-side cancellation: `break` from async iterator OR `task.cancel()` → send CANCEL frame → drain response frames until trailer (status=CANCELLED expected) → release lock.
13. Implement `session.close()`: write `finish()` on send-stream (does NOT send CANCEL or TRAILER); server's reader sees EOF → runs `on_session_close()`.
14. Implement `on_session_close()` lifecycle hook — MUST fire on: (a) clean client close, (b) stream error, (c) stream reset, (d) server shutdown, (e) connection close.
15. Implement LocalTransport session support (asyncio.Queue pair per session; lock-free since in-process, but preserve interceptor-chain and lifecycle semantics).
16. Guard: reject per-call `serialization_mode` override if session's stream-level mode differs (§9.1).
17. Tests (see Exit Criteria for coverage).

### 10.7 Exit Criteria

- Session-scoped service maintains instance state across multiple sequential calls.
- `peer: EndpointId` passed to constructor on server side (verified, not self-reported).
- Sequential call semantics enforced (client-side `asyncio.Lock`).
- In-session unary success = response payload only, **no trailer**. Error = trailer only, **no response** (§4.6).
- Client-stream / bidi EoI signalled by explicit TRAILER(status=OK) frame (§4.5 rule 3).
- CANCEL cancels only the current call, not the session; client drains response until trailer (§5.5).
- Mid-call CALL rejected with FAILED_PRECONDITION + stream reset (§4.5 rule 5).
- `on_session_close()` fires on ALL termination paths (clean close, error, reset, server shutdown, connection loss).
- Interceptors run per-call; `CallContext.session_id` stable across calls.
- Retry within a session replays on the same stream; session-level errors abort the session without retry.
- Test coverage:
  - Multi-call session with state persistence
  - CANCEL mid-unary and mid-stream
  - Client close → `on_session_close` fires
  - Stream reset → `on_session_close` fires
  - Connection drop → `on_session_close` fires
  - Sequential lock enforcement (concurrent method calls serialized)
  - Mid-call CALL rejection
  - Unary no-trailer success path
  - Per-call serialization override rejection
  - LocalTransport parity

---

## 11. Phase 9: Contract Identity & Publication

**Goal:** Implement content-addressed contract identity: types → canonical XLANG bytes → BLAKE3 hash → Merkle DAG. Publish contract bundles to iroh-blobs and register them in iroh-docs.

**Spec references:** Aster-ContractIdentity.md §11.2 (architecture), §11.3 (canonicalization — normative), §11.4 (publication flow), §11.5 (iroh surface used), Appendix A (test vectors), Appendix B (cycle-breaking examples), session addendum Appendix A (scoped field).

### 11.1 Critical Design Points

**The canonical encoder is custom code — NOT a wrapper around `fory.serialize()`.** §11.3.2: *"Implementations must not hash the output of a generic `fory.serialize(...)` call."* The framework writes a **stripped** XLANG profile with no outer header, no ref/null metadata, no root type metadata, and specific integer encodings. Treat this as byte-level serialization with Fory-compatible primitives.

**Hash domains:**
- `contract_id = blake3(contract.xlang)` — identity of the **contract** (48 hex chars)
- `type_hash = blake3(type.xlang)` — identity of each **TypeDef** in the DAG
- `collection_hash` — identity of the **bundle** (Iroh HashSeq root); used for transfer only

Verification checks `blake3(contract.xlang bytes) == contract_id`. The `collection_hash` is incidental.

### 11.2 Discriminator Enums (§11.3.3 — normative IDs, spec-verbatim)

```python
class TypeKind(IntEnum):           # enum id=1
    PRIMITIVE = 0    # int32, string, bytes, etc. (type carried in type_primitive)
    REF       = 1    # reference to another TypeDef by hash (type_ref field)
    SELF_REF  = 2    # back-edge in a cycle (self_ref_name field)
    ANY       = 3    # Fory `Any` — type identity carried only by the enum discriminator

class ContainerKind(IntEnum):      # enum id=2
    NONE = 0         # field is not a container
    LIST = 1
    SET  = 2
    MAP  = 3

class TypeDefKind(IntEnum):        # enum id=3
    MESSAGE = 0      # struct-like (fields)
    ENUM    = 1      # enum_values
    UNION   = 2      # union_variants

class MethodPattern(IntEnum):      # enum id=4
    UNARY         = 0
    SERVER_STREAM = 1
    CLIENT_STREAM = 2
    BIDI_STREAM   = 3

class CapabilityKind(IntEnum):     # enum id=5
    ROLE   = 0       # caller must hold exactly this one role (roles has one entry)
    ANY_OF = 1       # caller must hold at least one of the listed roles
    ALL_OF = 2       # caller must hold every listed role

class ScopeKind(IntEnum):          # enum id=6
    SHARED = 0       # stateless service (default)
    STREAM = 1       # session-scoped service
```

**Fixed IDs + values drive the wire encoding — changing them breaks existing contract_ids in the wild.** These values are copied verbatim from Aster-ContractIdentity.md §11.3.3; do not reorder, renumber, or rename them. Containers are a **separate field on FieldDef**, not a TypeKind variant.

### 11.3 Framework-Internal Types (§11.3.3 — spec-verbatim field IDs)

```python
@aster_tag("_aster/FieldDef")
@dataclass
class FieldDef:
    id: int32                            # field=1: field number from IDL/code
    name: str                            # field=2: canonical field name (snake_case)
    type_kind: TypeKind                  # field=3: PRIMITIVE / REF / SELF_REF / ANY
    type_primitive: str                  # field=4: "string"/"int32"/… when type_kind=PRIMITIVE
    type_ref: bytes                      # field=5: 32-byte hash when type_kind=REF
    self_ref_name: str                   # field=6: fully-qualified name when type_kind=SELF_REF
    optional: bool                       # field=7
    ref_tracked: bool                    # field=8: Fory `ref` modifier
    container: ContainerKind             # field=9: NONE / LIST / SET / MAP
    container_key_kind: TypeKind         # field=10: PRIMITIVE or REF when container=MAP
    container_key_primitive: str         # field=11: when container=MAP + key_kind=PRIMITIVE
    container_key_ref: bytes             # field=12: when container=MAP + key_kind=REF

@aster_tag("_aster/EnumValueDef")
@dataclass
class EnumValueDef:
    name: str                            # field=1
    value: int32                         # field=2

@aster_tag("_aster/UnionVariantDef")
@dataclass
class UnionVariantDef:
    name: str                            # field=1
    id: int32                            # field=2
    type_ref: bytes                      # field=3: 32-byte BLAKE3 of variant TypeDef

@aster_tag("_aster/TypeDef")
@dataclass
class TypeDef:
    kind: TypeDefKind                    # field=1: MESSAGE / ENUM / UNION
    package: str                         # field=2: dotted package name
    name: str                            # field=3: unqualified type name
    fields: list[FieldDef]               # field=4: sorted by id (MESSAGE only; else [])
    enum_values: list[EnumValueDef]      # field=5: sorted by value (ENUM only; else [])
    union_variants: list[UnionVariantDef]# field=6: sorted by id (UNION only; else [])

@aster_tag("_aster/CapabilityRequirement")
@dataclass
class CapabilityRequirement:
    kind: CapabilityKind                 # field=1: ROLE / ANY_OF / ALL_OF
    roles: list[str]                     # field=2: sorted Unicode codepoint (NFC);
                                         #   ROLE → list of length 1
                                         #   ANY_OF → caller needs at least one
                                         #   ALL_OF → caller needs all

@aster_tag("_aster/MethodDef")
@dataclass
class MethodDef:
    name: str                            # field=1
    pattern: MethodPattern               # field=2: UNARY / SERVER_STREAM / CLIENT_STREAM / BIDI_STREAM
    request_type: bytes                  # field=3: 32-byte BLAKE3 of request TypeDef
    response_type: bytes                 # field=4: 32-byte BLAKE3 of response TypeDef
    idempotent: bool                     # field=5
    default_timeout: float               # field=6: seconds; 0.0 = none
    requires: CapabilityRequirement | None  # field=7: optional; absent = no cap check

@aster_tag("_aster/ServiceContract")
@dataclass
class ServiceContract:
    name: str                            # field=1
    version: int32                       # field=2
    methods: list[MethodDef]             # field=3: sorted by name (Unicode codepoint, NFC)
    serialization_modes: list[str]       # field=4
    scoped: ScopeKind                    # field=5: SHARED or STREAM
    requires: CapabilityRequirement | None  # field=6: optional
```

**Fully-qualified type name** = `package + "." + name`. Used for cycle-breaking SELF_REF encoding.

**Identifier rules (Aster-ContractIdentity.md §11.3.2.2):** method names, type names, package names, enum/union member names, and role names MUST conform to Unicode UAX #31 (`XID_Start` + `XID_Continue` — Python's `str.isidentifier()` implements this). Canonical form is **NFC-normalized**. Sort order is by Unicode codepoint on the NFC-normalized string. Implementations SHOULD warn on mixed-script identifiers.

**Distinctness invariant (session addendum Appendix A):** two services identical except for `scoped` produce different `contract_id`s. This is critical because a session-scoped service has different runtime semantics than a stateless one.

### 11.4 Canonical Encoding Rules (§11.3.2 — normative)

The canonical encoder produces **stripped XLANG bytes** with these rules:

1. **No outer header, no outer ref/null meta, no root type meta, no schema hash prefix.**
2. **UTF-8 strings** (`<len_varint><utf8_bytes>`).
3. **int32/int64** → **ZigZag VARINT32/VARINT64** (NOT fixed 4/8 bytes).
4. **Enum fields** (TypeKind, ContainerKind, etc.) → unsigned varint.
5. **bool** → single byte (0x00/0x01).
6. **bytes** → `<len_varint><raw_bytes>` (used for hash fields: exactly 32 bytes).
7. **Homogeneous list header byte = `0x0C`** (followed by `<len_varint>` then elements).
8. **Optional fields** (MethodDef.requires, ServiceContract.requires) use `NULL_FLAG = 0xFD` for absent; presence flag `0x00` + nested value when present (Appendix A.2).
9. **Zero-value conventions** for unused discriminator companion fields (Aster-ContractIdentity.md §11.3.3):
   - `type_kind=PRIMITIVE` → `type_ref = empty bytes, self_ref_name = ""`
   - `type_kind=REF` → `type_primitive = "", self_ref_name = ""`
   - `type_kind=SELF_REF` → `type_primitive = "", type_ref = empty bytes`
   - `type_kind=ANY` → `type_primitive = "", type_ref = empty bytes, self_ref_name = ""`
   - `container != MAP` → `container_key_kind = PRIMITIVE (0), container_key_primitive = "", container_key_ref = empty bytes`
10. **Sort order:** `TypeDef.fields` by `id`, `TypeDef.enum_values` by `value`, `TypeDef.union_variants` by `id`, `ServiceContract.methods` by `name` (Unicode codepoint, NFC-normalized), `CapabilityRequirement.roles` by Unicode codepoint (NFC-normalized).
11. **Identifier normalization:** before canonical encoding, all identifier strings (names) MUST be NFC-normalized and validated against UAX #31 (Python: `str.isidentifier()`).

Implementation: `aster/contract/canonical.py` exposes primitive writers (`write_varint`, `write_zigzag_i32`, `write_zigzag_i64`, `write_string`, `write_bytes`, `write_bool`, `write_list_header`, `write_null_flag`) and dataclass-specific writers for each contract type.

### 11.5 Cycle Breaking (§11.3.4 + Appendix B — normative)

Bottom-up hashing requires breaking cycles. Naive self-reference (`type_kind="self_ref"` on direct recursion) is **not sufficient** — mutual recursion and N-cycles are possible. The algorithm:

1. **Build the reference graph** over TypeDefs.
2. **Compute SCCs** (Strongly Connected Components — Tarjan's or Kosaraju's).
3. **For each SCC of size ≥ 2**, compute a **codepoint-ordered spanning tree** rooted at the NFC-codepoint-smallest fully-qualified type name; edges within the SCC not in the tree become **SELF_REF** back-edges (encoded with the target type's position in the SCC's DFS order).
4. **Hash bottom-up** over the condensation DAG (SCCs as super-nodes).
5. **Within an SCC**, all member types get the same "cluster hash" suffix, then their own position-specific prefix — this is deterministic across implementations.

Appendix B provides test vectors for: direct self-recursion, 2-type mutual recursion, 3-cycle, diamond with back-edge. Our implementation MUST produce the same hashes.

### 11.6 Collection Layout (§11.2, §11.4.2)

The published bundle is an Iroh **HashSeq collection** with fixed positions:

| Index | Entry | Notes |
|-------|-------|-------|
| 0 | `manifest.json` | `ContractManifest` (see §11.7) |
| 1 | `contract.xlang` | Canonical bytes, `blake3(...) == contract_id` |
| 2..N | `types/{hex(hash)}.xlang` | One entry per TypeDef in the graph |
| N+1.. | (optional additions) | e.g. `schemas/<id>.fory` for row schemas |

`BlobsClient.add_bytes_as_collection(name, bytes)` (already implemented, Phase 1c.2) handles single-file collections; multi-file collection building is new work here — iterate blobs, concatenate into HashSeq, tag the root.

### 11.7 ContractManifest (§11.4.4)

```python
@dataclass
class ContractManifest:
    # Identity
    service: str
    version: int
    contract_id: str              # hex
    canonical_encoding: str       # "fory-xlang/0.15"
    # DAG metadata
    type_count: int
    type_hashes: list[str]        # hex, in publish order
    method_count: int
    serialization_modes: list[str]
    scoped: str                   # "shared" | "stream"
    deprecated: bool
    # Provenance (written by `aster contract gen` — optional)
    semver: str | None            # e.g. "1.2.3"
    vcs_revision: str | None      # git sha
    vcs_tag: str | None
    vcs_url: str | None
    changelog: str | None
    # Runtime (written at startup)
    published_by: str             # AuthorId
    published_at_epoch_ms: int
```

`provenance` fields are populated by the `aster contract gen` CLI tool at commit/build time (offline, no network); `published_by` / `published_at_epoch_ms` are populated at runtime on first publish.

### 11.8 `aster contract gen` CLI (§11.4.2)

An **offline** tool (no network, no credentials) for git commit hooks and build pipelines:

```bash
aster contract gen --service my_module.MyService --out .aster/manifest.json
```

Steps: load service class → resolve type graph → compute all hashes → write `.aster/manifest.json`. The manifest is **committed to source control**. At runtime, Aster verifies `blake3(live ServiceContract bytes) == manifest.contract_id` before publishing (§11.9).

### 11.9 Publication Flow (§11.4.3 — normative ordering)

```python
async def publish_contract(
    node: IrohNode,
    service_class: type,
    registry_doc: DocHandle,
    blobs: BlobsClient,
    manifest_path: str = ".aster/manifest.json",
) -> str:
    # 1. Resolve type graph from service_class
    # 2. Canonically encode each TypeDef, compute type_hashes
    # 3. Canonically encode ServiceContract, compute contract_id
    # 4. STARTUP VERIFY (fatal): load committed manifest.json from disk;
    #    if blake3(live bytes) != manifest.contract_id:
    #      raise FatalContractMismatch(expected, actual, service, version,
    #          "rerun `aster contract gen` and commit the updated manifest")
    # 5. Build HashSeq collection: [manifest.json, contract.xlang, types/...]
    # 6. Set iroh-blobs tag: "aster/contract/{friendly}@{contract_id}" for GC
    #    (uses Tags API — Phase 1c.1 ✅ implemented)
    # 7. Write ArtifactRef at contracts/{contract_id} in registry_doc
    #    (NOT under _aster/ — see Aster-SPEC.md §11.2 key schema)
    # 8. Write version pointer at services/{name}/versions/v{version}
    # 9. Write optional tag/channel aliases at services/{name}/{channels|tags}/{…}
    # 10. Broadcast gossip CONTRACT_PUBLISHED
    # 11. (Endpoint leases published LAST — in Phase 10 register_endpoint())
    ...
```

The ordering matters: **consumers must see the contract before they see leases pointing at it.** Phase 10's `register_endpoint()` is the final step of the publication sequence.

### 11.10 Fetch & Verification (§11.4.4)

```python
async def fetch_contract(
    blobs: BlobsClient,
    ref: ArtifactRef,
) -> ServiceContract:
    # 1. Download collection via collection_hash (use provider hint if present)
    # 2. Wait for blob completion: await blobs.blob_observe_complete(ref.collection_hash)
    #    (or use blob_local_info() to short-circuit cache hit)
    # 3. Read index 1 (contract.xlang bytes)
    # 4. Verify blake3(bytes) == ref.contract_id
    # 5. Parse canonical bytes → ServiceContract instance
    # 6. (Optional) verify each TypeDef hash matches manifest's type_hashes
    ...
```

Uses Phase 1d FFI primitives (`blob_observe_complete`, `blob_local_info`) for cache-aware fetch.

### 11.11 Steps

1. Implement discriminator `IntEnum`s (`TypeKind`, `ContainerKind`, `TypeDefKind`, `MethodPattern`, `CapabilityKind`, `ScopeKind`) with spec-mandated IDs.
2. Implement `aster/contract/canonical.py` — low-level writers: `write_varint`, `write_zigzag_i32/i64`, `write_string`, `write_bytes`, `write_bool`, `write_list_header(count)`, `write_null_flag`, `write_present_flag`.
3. Implement dataclasses: `FieldDef`, `EnumValueDef`, `UnionVariantDef`, `TypeDef`, `CapabilityRequirement`, `MethodDef`, `ServiceContract`, with `@aster_tag` decorations (internal tags `_aster/*`).
4. Implement per-type canonical writers (all fields emitted in spec field-ID order, with zero-value conventions for unused discriminator companions):
   - `write_field_def(w, f)` — 12 fields (id, name, type_kind, type_primitive, type_ref, self_ref_name, optional, ref_tracked, container, container_key_kind, container_key_primitive, container_key_ref)
   - `write_enum_value_def(w, ev)` — 2 fields (name, value)
   - `write_union_variant_def(w, uv)` — 3 fields (name, id, type_ref)
   - `write_type_def(w, t)` — 6 fields (kind, package, name, fields[], enum_values[], union_variants[]); sorts fields by id, enum_values by value, union_variants by id
   - `write_capability_requirement(w, cr)` — 2 fields (kind, roles[]); roles NFC-normalized + sorted by Unicode codepoint
   - `write_method_def(w, m)` — 7 fields (name, pattern, request_type, response_type, idempotent, default_timeout, requires?); NULL_FLAG when requires absent
   - `write_service_contract(w, c)` — 6 fields (name, version, methods[], serialization_modes[], scoped, requires?); methods NFC-normalized + sorted by name (Unicode codepoint)
5. **Produce canonical golden vectors.** Python is the reference implementation; Phase 9 generates the first set of vectors. Write `tools/gen_canonical_vectors.py` that constructs Appendix A fixture inputs (A.2–A.6) + rule-level micro-fixtures (one per §11.3.2 rule — varint edges, ZigZag boundaries, NULL_FLAG placement, sort stability, zero-value conventions, `scoped` SHARED/STREAM distinctness), runs the canonical encoder, and emits `tests/fixtures/canonical_test_vectors.json` with hex bytes + BLAKE3 hex hashes. Copy the vectors into `Aster-ContractIdentity.md` Appendix A as "Python-reference v1, pending cross-verification". Tests assert byte-equality + hash-equality against the committed file.
6. Implement `resolve_type_graph(service_class) -> dict[type_name, TypeDef]`: walk method signatures via `typing.get_type_hints()` + `inspect`, construct TypeDefs.
7. Implement SCC computation (Tarjan's) on the type-reference graph.
8. Implement cycle-breaking: for each SCC of size ≥ 2, codepoint-ordered spanning tree (NFC-normalized names) → back-edges encoded as SELF_REF. Validate against Appendix B examples (direct recursion, 2-type mutual, 3-cycle, diamond+back-edge).
9. Implement `compute_type_hash(type_def) -> bytes` (32 bytes) and `compute_contract_id(contract) -> str` (hex).
10. Implement `ServiceContract` construction from `ServiceInfo` (Phase 4), including `scoped` field propagation from `@service(scoped=...)`.
11. Implement `ContractManifest` construction.
12. Implement `aster contract gen` CLI as a console script entry point in `pyproject.toml`:
    - `aster.contract.cli:main` — imports service class, computes contract, writes `manifest.json`, no network access.
13. Implement HashSeq collection building in `aster/contract/publication.py` — concatenate `manifest.json` + `contract.xlang` + `types/{hash}.xlang` entries.
14. Implement runtime manifest verification (`verify_manifest_or_fatal(live_contract, manifest_path)`): on mismatch, raise `FatalContractMismatch` with diagnostic including expected, actual, service_name, version, and the remediation string.
15. Implement `publish_contract()` following §11.4.3 ordering (collection import → tag-set → ArtifactRef → version pointer → optional aliases → gossip).
16. Implement GC-protection tag: `aster/contract/{friendly}@{contract_id}` via `BlobsClient.tag_set()` (Phase 1c.1).
17. Implement `fetch_contract()` using `blob_observe_complete` + `blob_local_info` (Phase 1d FFI).
18. Tests: hash stability, scope distinctness, Appendix A/B vectors, manifest-mismatch fatal, fetch round-trip.

### 11.12 Exit Criteria

- Canonical encoder passes all Appendix A fixture vectors (byte-equal + hash-equal).
- Cycle-breaking passes all Appendix B fixtures (direct, mutual, 3-cycle, diamond).
- `contract_id` is deterministic and stable across runs and Python versions.
- `scoped=SHARED` vs `scoped=STREAM` produces different `contract_id`s.
- `aster contract gen` CLI runs offline (no network, no creds) and produces committable `manifest.json`.
- Startup verification is **fatal** with spec-matching diagnostic on mismatch.
- Published collection has the fixed index layout (0=manifest, 1=contract, 2..N=types).
- GC-protection tag is set; tag deletion unpublishes.
- Publication order is normative: contract → version/channel keys → gossip → endpoint leases LAST.
- Fetched contract verifies `blake3(contract.xlang) == contract_id`.

### 11.13 Pre-Requisites

- **pyfory is NOT used for canonical encoding of `_aster/*` framework types.** This is custom code. pyfory is still used for user payload serialization (Phase 2).
- **`blake3` PyPI package** — available.
- **Reference test vectors:** Python IS the reference implementation. Phase 9 **produces** the first golden vectors for Appendix A (A.2–A.6) and commits them to `tests/fixtures/canonical_test_vectors.json` + `Aster-ContractIdentity.md` Appendix A. Java binding (future, post-Python) cross-verifies. See `ASTER_SPEC_ISSUES.md` §B3 for the bootstrap protocol and risk-mitigation micro-fixtures.

---

## 12. Phase 10: Service Registry & Discovery

**Goal:** Implement the decentralized service registry using iroh-docs (authoritative state), iroh-gossip (change notifications), and iroh-blobs (contract bundles).

**Spec references:** §11.2 (architecture), §11.2.1 (ArtifactRef), §11.2.3 (ACL filtering), §11.5 (iroh surface), §11.6 (EndpointLease), §11.7 (GossipEvent), §11.8 (resolution flows), §11.9 (endpoint selection), §11.10 (consistency model).

**Scope note:** Phase 10 implements the **docs-based registry** only. It assumes an already-established, trusted set of authors writing to `_aster/*` keys. **Authentication, enrollment credentials, producer-mesh admission (Gate 0), and clock-drift detection are deferred to Phase 11 (Trust Foundations) and Phase 12 (Producer Mesh).** Phase 10 may be run unauthenticated in local/trusted deployments.

### 12.1 Consistency Model (§11.10 — load-bearing invariants)

1. **Docs is authoritative.** Every fact is eventually reflected in `_aster/*` keys. Gossip is a notification accelerator; if a gossip event is missed, docs sync eventually delivers the same state.
2. **Consumers evict on lease expiry regardless of gossip.** A consumer MUST drop endpoints whose `updated_at_epoch_ms + lease_duration_s` has passed, even if no ENDPOINT_DOWN gossip was received.
3. **`lease_seq` is monotonic per endpoint per contract.** Consumers MUST reject a lease write whose `lease_seq` is ≤ the latest already observed for that `(service, contract_id, endpoint_id)` tuple — the dedup tuple matches the key path `services/{name}/contracts/{cid}/endpoints/{eid}`.
4. **ACL enforcement is sync-time on reads** (see §12.6).

### 12.2 Data Model (§11.2.1, §11.6, §11.7)

```python
@aster_tag("_aster/ArtifactRef")
@dataclass
class ArtifactRef:
    contract_id: str              # hex — the content address
    collection_hash: str          # hex — iroh-blobs HashSeq root
    # Provider hints (optional; speed up fetch)
    provider_endpoint_id: str | None
    relay_url: str | None
    ticket: str | None            # BlobTicket for direct fetch


@aster_tag("_aster/EndpointLease")
@dataclass
class EndpointLease:                     # field IDs follow Aster-SPEC.md §11.6
    endpoint_id: str                     # NodeId hex
    contract_id: str
    service: str                         # service_name
    version: int                         # int32
    lease_expires_epoch_ms: int          # int64 — absolute expiry
    lease_seq: int                       # int64, monotonic per (service, contract_id, endpoint_id)
    alpn: str                            # e.g. "aster/1"
    serialization_modes: list[str]       # modes this endpoint supports for this contract
    feature_flags: list[str]
    relay_url: str | None
    direct_addrs: list[str]              # ip:port strings
    load: float | None                   # 0.0–1.0, optional
    language_runtime: str | None         # "python/3.13", "rust/1.80", …
    aster_version: str                   # e.g. "0.8.0"
    policy_realm: str | None
    health_status: str                   # see HealthStatus enum / §12.3
    tags: list[str]
    updated_at_epoch_ms: int             # int64 — wall-clock at last write


class HealthStatus(str, Enum):    # §11.6
    STARTING = "starting"          # not yet ready to serve
    READY = "ready"                # accepting calls
    DEGRADED = "degraded"          # accepting but diminished
    DRAINING = "draining"          # graceful shutdown in progress


class GossipEventType(IntEnum):   # all 6 are normative (§11.7)
    CONTRACT_PUBLISHED       = 0
    CHANNEL_UPDATED          = 1
    ENDPOINT_LEASE_UPSERTED  = 2
    ENDPOINT_DOWN            = 3
    ACL_CHANGED              = 4
    COMPATIBILITY_PUBLISHED  = 5


@aster_tag("_aster/GossipEvent")
@dataclass
class GossipEvent:                       # flat structured shape per §11.7
    type: GossipEventType
    service: str | None                  # set for CONTRACT_PUBLISHED, CHANNEL_UPDATED,
                                         #   ENDPOINT_LEASE_UPSERTED, ENDPOINT_DOWN
    version: int | None                  # set for CONTRACT_PUBLISHED, ENDPOINT_LEASE_UPSERTED
    channel: str | None                  # set for CHANNEL_UPDATED
    contract_id: str | None              # set for CONTRACT_PUBLISHED, CHANNEL_UPDATED,
                                         #   ENDPOINT_LEASE_UPSERTED, COMPATIBILITY_PUBLISHED
    endpoint_id: str | None              # set for ENDPOINT_LEASE_UPSERTED, ENDPOINT_DOWN
    timestamp_ms: int                    # int64; sender wall-clock
```

**GossipEvent field population per type:**

| type | service | version | channel | contract_id | endpoint_id |
|---|---|---|---|---|---|
| CONTRACT_PUBLISHED | ✓ | ✓ | — | ✓ | — |
| CHANNEL_UPDATED | ✓ | — | ✓ | ✓ | — |
| ENDPOINT_LEASE_UPSERTED | ✓ | ✓ | — | ✓ | ✓ |
| ENDPOINT_DOWN | ✓ | — | — | — | ✓ |
| ACL_CHANGED | — | — | — | — | — |
| COMPATIBILITY_PUBLISHED | — | — | — | ✓ (source) | — |

(COMPATIBILITY_PUBLISHED packs source+target contract_ids: the `contract_id` field holds the source; target is implicit in the sender's doc entry.)

**Gossip event types (§11.7 — all six are normative):**

| event_type | Payload | Emitted by |
|------------|---------|------------|
| `CONTRACT_PUBLISHED` | `{contract_id, service, version}` | Publisher after ArtifactRef write |
| `CHANNEL_UPDATED` | `{service, channel, contract_id}` | Publisher on channel alias change |
| `ENDPOINT_LEASE_UPSERTED` | `{endpoint_id, service, lease_seq, contract_id}` | Publisher on lease write/refresh |
| `ENDPOINT_DOWN` | `{endpoint_id, service}` | Publisher on graceful withdraw |
| `ACL_CHANGED` | `{key_prefix}` | Admin on ACL write |
| `COMPATIBILITY_PUBLISHED` | `{source_contract_id, target_contract_id}` | Compatibility tool |

### 12.3 Health State Machine (§11.6)

States: `STARTING → READY → DEGRADED? → DRAINING → (absent)`.

**Consumer routing rules:**
- MUST skip `STARTING` and `DRAINING`.
- SHOULD prefer `READY` over `DEGRADED`.
- MAY use `DEGRADED` as a fallback when no `READY` endpoints exist.

### 12.4 Key Schema (Aster-SPEC.md §11.2 — normative)

**Critical:** registry keys (`contracts/`, `services/`, `endpoints/`, `compatibility/`) live at the **top level** of the registry namespace. Only `acl/` and `config/` are under the `_aster/` prefix. The `_aster/*` namespace is reserved exclusively for framework-internal state; application-visible registry data is NOT prefixed.

```
contracts/{contract_id}                              → ArtifactRef
services/{name}/versions/v{version}                  → contract_id (pointer)
services/{name}/channels/{channel}                   → contract_id (pointer)
services/{name}/meta                                 → service metadata JSON
services/{name}/contracts/{contract_id}/endpoints/{endpoint_id_hex}
                                                     → EndpointLease
endpoints/{endpoint_id_hex}/meta                     → optional static endpoint metadata
endpoints/{endpoint_id_hex}/tags                     → optional discovery tags
compatibility/{contract_id}/{other_contract_id}      → Compatibility report

_aster/acl/writers                                   → list[AuthorId]
_aster/acl/readers                                   → list[AuthorId]
_aster/acl/admins                                    → list[AuthorId]
_aster/acl/policy                                    → RegistryPolicy config
_aster/config/gossip_topic                           → TopicId (for change notifications)
_aster/config/lease_duration_s                       → int (default 45)
_aster/config/lease_refresh_interval_s               → int (default 15)
```

**Two-step resolution:** `(service, version|channel)` → `contract_id` (via `services/{name}/{versions|channels}/…`) → list leases under `services/{name}/contracts/{contract_id}/endpoints/*`.

Note: service *discovery* tags live under `endpoints/{eid}/tags`, distinct from contract channel aliases under `services/{name}/channels/`.

### 12.5 FFI Primitives Wired In (§11.5)

Phase 10 uses the new iroh-docs / iroh-blobs surfaces completed in Phase 1c/1d:

| FFI primitive | Used for |
|---------------|----------|
| `DocsClient.join_and_subscribe` | Race-free registry join (subscribe-before-first-sync) |
| `DocHandle.set_download_policy(NothingExcept, ["_aster/", "contracts/", "services/", "endpoints/", "compatibility/"])` | Selectively sync only registry keys; registry has multiple top-level prefixes (§12.4) |
| `DocHandle.subscribe()` live events (`InsertRemote` / `ContentReady`) | Drive `on_change` callbacks, invalidate caches |
| `DocHandle.share_with_addr(mode)` | Registry doc tickets with full relay+addr info |
| `BlobsClient.blob_observe_complete(hash)` | Wait for contract collection download completion before verification |
| `BlobsClient.blob_local_info(hash)` | Cache hit check before download |
| `BlobsClient.tag_*` | Managed by Phase 9 publication flow (contract GC protection) |
| `GossipClient.subscribe(topic, bootstrap)` | Listen for `GossipEvent`s |

### 12.6 ACL Enforcement (§11.2.3)

**Mechanism:** A sync-time callback rejects doc entries whose `AuthorId` is not in `_aster/acl/writers`. Rejection is silent (log, do not persist). Phase 10 reads the ACL once at registry open + on every `ACL_CHANGED` gossip event.

**Limitation:** Since iroh-docs does not (yet) expose a sync-time rejection hook at the FFI boundary, Phase 10 filters at **read time** — queries against `_aster/*` keys skip entries from untrusted authors. This is equivalent in steady state but allows untrusted writes to sit in the local store until garbage collected. **TODO for future FFI work:** add a sync-callback hook for true sync-time rejection.

### 12.7 `aster/registry/publisher.py`

```python
class RegistryPublisher:
    def __init__(
        self,
        node: IrohNode,
        registry_doc: DocHandle,
        blobs: BlobsClient,
        author_id: str,
        lease_duration_s: int = 45,
        lease_refresh_interval_s: int = 15,
    ): ...

    async def publish_contract(self, service_class: type) -> str:
        """Delegates to Phase 9 publish_contract(); returns contract_id."""

    async def register_endpoint(
        self,
        contract_id: str,
        service_name: str,
        version: int,
        *,
        alpn: str = "aster/1",
        direct_addrs: list[str] | None = None,
        relay_url: str | None = None,
        health_status: HealthStatus = HealthStatus.STARTING,
        feature_flags: list[str] = (),
        tags: list[str] = (),
        policy_realm: str | None = None,
    ) -> None:
        """
        Writes initial EndpointLease (health=STARTING) + starts refresh timer.
        Emits ENDPOINT_LEASE_UPSERTED gossip.
        """

    async def set_health(self, status: HealthStatus) -> None:
        """Transition health state; writes new lease row (lease_seq++)."""

    async def refresh_lease(self) -> None:
        """Internal: called every lease_refresh_interval_s by background task."""

    async def withdraw(self, grace_period_s: float = 5.0) -> None:
        """Graceful shutdown (§11.6, §11.8):
        1. set_health(DRAINING) — writes lease with new seq
        2. wait grace_period_s for in-flight calls
        3. delete lease row
        4. broadcast ENDPOINT_DOWN
        """
```

### 12.8 `aster/registry/client.py`

```python
class RegistryClient:
    def __init__(
        self,
        node: IrohNode,
        registry_doc: DocHandle,
        blobs: BlobsClient,
        acl: RegistryACL,
    ):
        """Applies DownloadPolicy.NothingExcept(['_aster/','contracts/','services/',
        'endpoints/','compatibility/']) on registry_doc — see §12.4 for key schema."""

    async def resolve(
        self,
        service_name: str,
        *,
        version: int | None = None,
        channel: str | None = None,
        tag: str | None = None,
        strategy: str = "round_robin",
    ) -> EndpointLease:
        """Two-step resolve + filter + rank (§12.9).

        Step 1: read pointer key (versions/channels/tags) → contract_id.
        Step 2: list _aster/services/{name}/contracts/{contract_id}/endpoints/*
                → candidate leases.
        Filter: (authoritative invariants, §12.9)
        Rank: by strategy.
        """

    async def resolve_all(self, ...) -> list[EndpointLease]:
        """Same as resolve() but returns all surviving candidates (unranked)."""

    async def fetch_contract(self, contract_id: str) -> ServiceContract:
        """Delegates to Phase 9 fetch_contract() using blob_observe_complete +
        blob_local_info."""

    def on_change(self, callback: Callable[[GossipEvent], Awaitable]) -> None:
        """Subscribe to gossip + doc.subscribe() change notifications."""
```

### 12.9 Endpoint Selection — Filters Before Ranking (§11.9 — normative)

Any selection strategy MUST apply these mandatory filters first, then rank survivors:

**Mandatory filters (in order):**
1. `lease.contract_id` matches the resolved `contract_id` (no drift).
2. `lease.alpn` is supported by the caller.
3. At least one of the caller's `serialization_modes` is in `lease.serialization_modes` (via contract).
4. `lease.health_status` is `READY` or `DEGRADED` (skip STARTING/DRAINING).
5. Lease is fresh: `now - updated_at_epoch_ms <= lease_duration_s * 1000`.
6. `lease.policy_realm` is compatible with caller's policy (if configured).

**Rank (strategy):**
- `round_robin` — stateful round-robin over survivors.
- `least_load` — lowest `lease.load` wins.
- `random` — uniform random.

If both `READY` and `DEGRADED` survive, rank `READY` first within strategy.

### 12.10 `aster/registry/acl.py`

```python
class RegistryACL:
    async def reload(self) -> None: ...                 # called on ACL_CHANGED
    def is_trusted_writer(self, author_id: str) -> bool: ...
    async def get_writers(self) -> list[str]: ...
    async def get_readers(self) -> list[str]: ...
    async def get_admins(self) -> list[str]: ...
    async def add_writer(self, author_id: str) -> None: ...   # admin-only
    async def remove_writer(self, author_id: str) -> None: ...
```

### 12.11 `aster/registry/gossip.py`

```python
class RegistryGossip:
    async def broadcast_contract_published(
        self, contract_id: str, service: str, version: int) -> None: ...
    async def broadcast_channel_updated(
        self, service: str, channel: str, contract_id: str) -> None: ...
    async def broadcast_endpoint_lease_upserted(
        self, endpoint_id: str, service: str, lease_seq: int, contract_id: str) -> None: ...
    async def broadcast_endpoint_down(
        self, endpoint_id: str, service: str) -> None: ...
    async def broadcast_acl_changed(self, key_prefix: str) -> None: ...
    async def broadcast_compatibility_published(
        self, src: str, dst: str) -> None: ...
    async def listen(self) -> AsyncIterator[GossipEvent]: ...
```

### 12.12 Steps

1. Define `ArtifactRef`, `EndpointLease`, `GossipEvent`, `HealthStatus` enum.
2. Implement key-schema constants in `aster/registry/keys.py` (helpers: `lease_key(service, contract_id, endpoint_id)`, `contract_key(contract_id)`, etc.).
3. Implement `RegistryACL` with in-memory cache + reload on `ACL_CHANGED`.
4. Implement `RegistryClient.__init__`:
   - Apply `DownloadPolicy.NothingExcept(["_aster/", "contracts/", "services/", "endpoints/", "compatibility/"])` to registry_doc.
   - Prefer `DocsClient.join_and_subscribe` for race-free initial sync.
   - Load ACL snapshot.
5. Implement `RegistryClient.resolve()` with two-step lookup + mandatory filters + strategy ranking.
6. Implement trusted-author filtering on all registry reads (post-read filter on `contracts/*`, `services/*`, `endpoints/*`, `compatibility/*`, `_aster/acl/*`, `_aster/config/*`).
7. Implement `lease_seq` monotonicity tracking: reject stale writes keyed by `(service, contract_id, endpoint_id)` — the same endpoint can serve multiple contracts, each with its own lease row.
8. Implement `RegistryClient.on_change()`: subscribe to gossip, route to callbacks; also subscribe to `DocHandle.subscribe()` InsertRemote events as authoritative backup.
9. Implement `RegistryClient.fetch_contract()` → delegates to Phase 9 `fetch_contract()` (uses `blob_local_info` for cache hit + `blob_observe_complete` for download wait).
10. Implement `RegistryPublisher.register_endpoint()`: lease_seq initialized, writes lease with `health=STARTING`, starts refresh timer.
11. Implement `RegistryPublisher.set_health()`: bump lease_seq, write new lease row, emit `ENDPOINT_LEASE_UPSERTED` gossip.
12. Implement `RegistryPublisher.refresh_lease()` as `asyncio.create_task()` loop, cadence = `lease_refresh_interval_s` (default 15s).
13. Implement `RegistryPublisher.withdraw()` with full state machine (DRAINING → grace → delete → gossip DOWN).
14. Implement `RegistryGossip` with all 6 broadcast types + `listen()` async iterator.
15. Implement `_aster/config/*` reader: `lease_duration_s`, `lease_refresh_interval_s` (defaults 45 / 15).
16. Tests (see Exit Criteria).

### 12.13 Exit Criteria

- Publisher on node A can register a contract + endpoint; client on node B can resolve and connect.
- Registry doc is synced with `DownloadPolicy.NothingExcept(["_aster/", "contracts/", "services/", "endpoints/", "compatibility/"])`.
- `lease_seq` monotonicity enforced: client discards stale lease writes.
- Expired leases evicted by consumer even without `ENDPOINT_DOWN` gossip.
- `withdraw()` transitions through DRAINING, waits grace period, deletes lease, broadcasts ENDPOINT_DOWN.
- All 6 `GossipEvent` types round-trip.
- Endpoint selection applies mandatory filters before strategy ranking.
- Consumers skip `STARTING` and `DRAINING`, prefer `READY` over `DEGRADED`.
- Untrusted-author entries rejected on read (ACL post-read filter).
- Contract fetch uses `blob_observe_complete` + `blob_local_info` for cache-aware download.

---

## 13. Phase 11: Trust Foundations

**Goal:** Implement offline root-key authorization, enrollment credentials, and Gate 0 connection-level admission. This is the minimum authentication layer; Phase 12 builds the live producer mesh on top.

**Spec references:** Aster-trust-spec.md §2.2 (enrollment credentials), §2.4 (admission: offline + runtime checks), §2.9 (gate composition), §3.1 (consumer credentials), §3.2 (consumer admission), §3.3 (EndpointHooks wiring).

### 13.1 Trust Model in Brief

- **Offline root key** (ed25519) signs **enrollment credentials**. The private root key never touches the mesh.
- **Producer credentials** bind an `EndpointId` to a set of attributes (roles, IID claims) with a signed expiry.
- **Consumer credentials** are either `Policy` (reusable, attribute-gated) or `OTT` (one-time token, nonce-consumed).
- **Gate 0** = connection-level admission: an iroh `EndpointHooks` implementation that inspects the `ALPN` and the remote EndpointId, admitting only peers whose credential has been verified.
- **Gate 1** = app-level capability checks (enforced by interceptors in Phase 7; Phase 11 just provides the credentials).

**Scope — Gates 0 and 1 apply only to remote iroh connections.** LocalTransport (Phase 3) runs in-process with no connection to gate and no credential to verify. `CallContext.peer` is `None` on LocalTransport, `CallContext.attributes` is empty, and both gates are bypassed by construction. See Aster-trust-spec.md §1.3 and Aster-SPEC.md §8.3.2.

### 13.2 Data Model

```python
@aster_tag("_aster/EnrollmentCredential")
@dataclass
class EnrollmentCredential:
    endpoint_id: str              # hex — the producer's NodeId
    root_pubkey: bytes            # 32 — ed25519 public key of the root
    expires_at: int               # epoch seconds
    attributes: dict[str, str]    # reserved: aster.role, aster.name, aster.iid_*
    signature: bytes              # 64 — ed25519(root_privkey, canonical(fields))


@aster_tag("_aster/ConsumerEnrollmentCredential")
@dataclass
class ConsumerEnrollmentCredential:
    credential_type: str          # "policy" or "ott"
    endpoint_id: str | None       # None for Policy credentials
    root_pubkey: bytes
    expires_at: int
    attributes: dict[str, str]
    nonce: bytes | None           # 32 bytes for OTT; None for Policy
    signature: bytes


@dataclass
class AdmissionResult:
    admitted: bool
    attributes: dict[str, str] | None
    reason: str | None            # rejection reason for structured logging
```

**Canonical signing message:**
```
endpoint_id.encode('utf-8')
    || root_pubkey                                 (32 bytes)
    || u64_be(expires_at)                          (8 bytes)
    || canonical_json(attributes)                  (UTF-8, sorted keys)
    || (nonce if OTT else b"")                     (32 or 0 bytes)
```

### 13.3 Attribute Conventions (§2.2)

| Key | Meaning | Example |
|-----|---------|---------|
| `aster.role` | Semantic role (producer, consumer, admin) | `"producer"` |
| `aster.name` | Human-readable identifier | `"payments-svc"` |
| `aster.iid_provider` | Cloud provider for IID check | `"aws"` \| `"gcp"` \| `"azure"` |
| `aster.iid_account` | Cloud account/project id | `"123456789012"` |
| `aster.iid_region` | Cloud region | `"us-east-1"` |
| `aster.iid_role_arn` | AWS role ARN (AWS-only) | `"arn:aws:iam::..."` |

Additional namespaced keys (`aster.*`) are reserved for future framework use. Non-`aster.*` keys are passed through to `CallContext.attributes`. **Network-level controls (CIDR, source-IP allowlists) are deliberately NOT part of the reserved attribute set** — see Aster-trust-spec.md §1 "Network-level controls are out of scope". Operators who need them MUST implement at the network boundary (VPN, firewall) or via application-namespaced custom attributes with their own admission callback.

### 13.4 Admission Checks (§2.4)

**Offline checks (no network):**
1. Verify `signature` against `root_pubkey`.
2. `expires_at > now`.
3. `peer_endpoint_id == cred.endpoint_id` (producer creds only; Policy consumer creds skip this).
4. For OTT: nonce has not been consumed (check `NonceStore`).

**Runtime checks (may hit network):**
5. If `aster.iid_*` is set: fetch IID from cloud provider metadata endpoint (`169.254.169.254` for AWS), verify provider signature, compare claims.

**Refusal logging (§2.4):** log refusal reason without leaking oracle info to the peer. Peer sees only `CONNECTION_REFUSED` at the QUIC layer.

### 13.5 Gate 0: `MeshEndpointHook` (§3.3)

```python
class MeshEndpointHook:
    """Connection-level admission via iroh EndpointHooks.

    Maintains an allowlist of admitted EndpointIds. The admission ALPNs
    (`aster.producer_admission`, `aster.consumer_admission`) are always
    accepted — they carry credential presentation; post-admission, the
    server calls add_peer() on success.
    """

    def __init__(self, allow_unenrolled: bool = False):
        self.admitted: set[str] = set()
        self.allow_unenrolled = allow_unenrolled  # True for local/dev

    # Iroh hook callbacks (Phase 1b FFI)
    async def before_connect(self, info: HookConnectInfo) -> HookDecision:
        if info.alpn in (b"aster.producer_admission", b"aster.consumer_admission"):
            return HookDecision.create_allow()
        if info.remote_endpoint_id in self.admitted or self.allow_unenrolled:
            return HookDecision.create_allow()
        return HookDecision.create_deny(403, b"not admitted")

    def add_peer(self, endpoint_id: str) -> None: ...
    def remove_peer(self, endpoint_id: str) -> None: ...
```

### 13.6 Nonce Store (§3.1)

OTT credentials carry a 32-byte nonce that MUST be consumed exactly once. Persistence is needed across restarts.

```python
class NonceStore:
    """Persistent one-shot nonce consumption."""
    async def consume(self, nonce: bytes) -> bool:
        """Atomically mark nonce as used. Returns True on first call,
        False if already consumed."""
    async def is_consumed(self, nonce: bytes) -> bool: ...
```

**Storage options:**
- Dev/small: JSON file under `~/.aster/nonces.json`.
- Production: iroh-docs `_aster/trust/nonces` (replicated, but adds sync latency to admission).

Phase 11 ships the JSON-file implementation; iroh-docs backend is a drop-in later.

### 13.7 Module Layout

```
aster/trust/
├── __init__.py
├── credentials.py      # EnrollmentCredential, ConsumerEnrollmentCredential
├── signing.py          # Canonical signing, ed25519 verify
├── admission.py        # Offline + runtime checks, AdmissionResult
├── iid.py              # Cloud metadata fetchers + signature verification
├── hooks.py            # MeshEndpointHook, Gate 0 wiring
├── nonces.py           # NonceStore (file + docs backends)
└── cli.py              # `aster trust keygen`, `aster trust sign`
```

### 13.8 Steps

1. Add `cryptography>=42` to `pyproject.toml` dependencies (ed25519).
2. Implement `aster/trust/signing.py`:
   - `canonical_signing_bytes(cred) -> bytes` per §13.2.
   - `sign_credential(cred, root_privkey) -> bytes` (used offline, in CLI).
   - `verify_signature(cred, root_pubkey) -> bool`.
3. Implement `EnrollmentCredential` + `ConsumerEnrollmentCredential` dataclasses.
4. Implement `aster/trust/admission.py`:
   - `check_offline(cred, peer_endpoint_id, nonce_store) -> AdmissionResult`.
   - `check_runtime(cred) -> AdmissionResult` (IID only — no network-level checks).
   - `admit(cred, ctx) -> AdmissionResult` (orchestrates both).
5. Implement `aster/trust/iid.py` for AWS, GCP, Azure (async HTTP GET to metadata endpoints + JWKS signature verification). Mock-pluggable for tests.
6. Implement `NonceStore` (file backend): atomic write with `os.replace`, fsync.
7. Implement `MeshEndpointHook` wired to Iroh's `HookManager` (Phase 1b FFI).
8. Implement `aster trust keygen` CLI: emits ed25519 root key pair to files, refuses if target exists.
9. Implement `aster trust sign` CLI: offline credential signing from CLI-supplied fields.
10. Implement ALPN constants: `ALPN_PRODUCER_ADMISSION = b"aster.producer_admission"`, `ALPN_CONSUMER_ADMISSION = b"aster.consumer_admission"`.
11. Tests — see Exit Criteria.

### 13.9 Exit Criteria

- Valid credential passes offline checks; invalid signature / expired / wrong endpoint_id fails.
- IID verification (mocked) passes with matching claims; fails otherwise.
- OTT nonce consumable exactly once; second call returns False.
- Policy credential reusable indefinitely (within expiry).
- `MeshEndpointHook` rejects unenrolled peer on non-admission ALPN, allows on admission ALPN, allows admitted peer.
- `aster trust keygen` produces a valid ed25519 pair; `aster trust sign` produces credentials that verify.
- Refusal logs contain reason; refused peer receives only QUIC-level reject.

### 13.10 Pre-Requisites

- `cryptography>=42` (ed25519) — add to `pyproject.toml`.
- Iroh `HookManager` / `EndpointHooks` FFI (Phase 1b) — already done.
- Optional: `PyJWT>=2.8` for cloud-provider JWT verification (only if IID checks are used).

---

## 14. Phase 12: Producer Mesh & Clock Drift

**Goal:** Build the producer mesh on top of Phase 11: bootstrap, signed gossip envelope, introduction, drift detection, self-departure, lease heartbeats.

**Spec references:** Aster-trust-spec.md §2.1 (bootstrap), §2.3 (gossip topic derivation), §2.5 (introduction), §2.6 (ProducerMessage envelope), §2.7 (departure), §2.10 (clock drift).

### 14.1 Mesh Model

- **Founding node:** first producer. Generates random 32-byte `salt`, derives gossip `topic_id = blake3(root_pubkey || b"aster-producer-mesh" || salt)`, prints bootstrap ticket (containing own EndpointAddr).
- **Subsequent node:** dials bootstrap peer over `aster.producer_admission` ALPN, presents credential, receives `{salt, accepted_producers}` in admission response.
- **Accepted producers** share a signed gossip channel. Messages outside the accepted set are dropped.
- **Clock drift detection:** every message carries `epoch_ms`; nodes compute mesh-median offset, self-depart on >5s drift, isolate peers on >5s deviation.

### 14.2 Data Model

```python
class ProducerMessageType(IntEnum):
    INTRODUCE          = 1
    DEPART             = 2
    CONTRACT_PUBLISHED = 3
    LEASE_UPDATE       = 4


@aster_tag("_aster/ProducerMessage")
@dataclass
class ProducerMessage:
    type: ProducerMessageType    # 1..4
    payload: bytes               # Fory-encoded per-type payload (empty for DEPART)
    sender: str                  # endpoint_id (hex)
    epoch_ms: int                # wall-clock at send time (int64)
    signature: bytes             # 64 — ed25519 over canonical signing bytes


@aster_tag("_aster/IntroducePayload")
@dataclass
class IntroducePayload:
    rcan: bytes                  # serialized rcan grant conveying "Producer" cap
                                 # (opaque bytes; rcan format TBD — see §14.12)


@aster_tag("_aster/DepartPayload")
@dataclass
class DepartPayload:
    reason: str                  # human-readable, optional (empty string if none)
    # NOTE: spec §2.6 defines Depart payload as Empty; we carry an optional
    # reason string for operator visibility. Empty string = no reason.


@aster_tag("_aster/ContractPublishedPayload")
@dataclass
class ContractPublishedPayload:
    service_name: str
    version: int
    contract_collection_hash: str    # hex — HashSeq root of the published bundle


@aster_tag("_aster/LeaseUpdatePayload")
@dataclass
class LeaseUpdatePayload:
    service_name: str
    version: int
    contract_id: str
    health_status: str
    addressing_info: dict[str, str]  # optional (relay_url, direct_addrs, etc.)


@dataclass
class MeshState:
    accepted_producers: set[str]     # admitted endpoint_ids (incl. self)
    salt: bytes                      # 32
    topic_id: bytes                  # 32
    peer_offsets: dict[str, int]     # endpoint_id → offset_ms
    drift_isolated: set[str]
    last_heartbeat_epoch_ms: int
    mesh_joined_at_epoch_ms: int     # for grace period
```

**ProducerMessage canonical signing bytes** (spec §2.6 — normative order):

```
u8(type) || payload || sender.encode('utf-8') || u64_be(epoch_ms)
```

The sign-bytes order is `type || payload || sender || epoch_ms`. Do NOT
reorder `epoch_ms` before `sender+payload` — the spec fixes this byte order
and any deviation breaks signature verification across implementations.

### 14.3 Topic Derivation (§2.3)

```python
def derive_gossip_topic(root_pubkey: bytes, salt: bytes) -> bytes:
    return blake3(root_pubkey + b"aster-producer-mesh" + salt).digest()
```

The salt keeps the topic private to admitted producers. Anyone with the root_pubkey + salt can subscribe; but they cannot **send** valid messages without their own accepted credential's signing key.

### 14.4 Replay Window & Drift Config (§2.6, §2.10)

```python
@dataclass
class ClockDriftConfig:
    replay_window_ms: int = 30_000       # ±30s (§2.6)
    drift_tolerance_ms: int = 5_000      # ±5s (§2.10)
    lease_heartbeat_ms: int = 900_000    # 15 min (SHOULD)
    grace_period_ms: int = 60_000        # 60s post-join
    min_peers_for_median: int = 3        # need 3+ peers for mesh median
```

- Messages outside `now ± replay_window_ms` are **dropped silently**.
- Drift detection requires ≥3 peers and waits `grace_period_ms` after join.
- `ASTER_CLOCK_DRIFT_TOLERANCE_MS` env var overrides `drift_tolerance_ms`.

### 14.5 Bootstrap Flow (§2.1, §2.5)

**Founding node startup:**
1. Load root pubkey + own credential from env (`ASTER_ENROLLMENT`).
2. Offline-verify own credential.
3. Generate or load producer key (iroh secret key, persisted `~/.aster/producer.key`).
4. Generate random 32-byte salt → `~/.aster/mesh_salt`.
5. Compute `topic_id`.
6. Initialize `MeshState(accepted_producers={self}, salt, topic_id)`.
7. Start iroh endpoint + `MeshEndpointHook` (from Phase 11).
8. Subscribe to gossip topic.
9. Print bootstrap ticket (`EndpointAddr`-based) to stdout.

**Subsequent node join:**
1. Load own credential + `ASTER_BOOTSTRAP_TICKET` env var.
2. Parse ticket → dial bootstrap peer.
3. Open `aster.producer_admission` ALPN bidi stream.
4. Send Fory-encoded `AdmissionRequest{credential, optional_iid}`.
5. Receive `AdmissionResponse{accepted: bool, salt, accepted_producers, reason?}`.
6. If accepted: derive topic_id → subscribe to gossip → send `Introduce` → persist `MeshState`.
7. Bootstrap peer adds new node to `accepted_producers` and rebroadcasts `Introduce` from its side for mesh convergence.

### 14.6 Gossip Handler

```python
async def handle_producer_message(msg: ProducerMessage, state: MeshState) -> None:
    # Order is normative (trust-spec §2.10 "Interaction with replay resistance"):
    # 1. Replay window check: abs(now - msg.epoch_ms) <= replay_window_ms
    #    → fail = silent drop
    # 2. Sender membership: msg.sender in state.accepted_producers
    #    → fail = SECURITY ALERT drop
    # 3. Signature verify against sender's signing pubkey (cached from
    #    credential verified at admission time)
    #    → fail = SECURITY ALERT drop
    # 4. Track offset: state.peer_offsets[msg.sender] = now_ms - msg.epoch_ms
    #    (feeds the drift detector — §14.7)
    # 5. Dispatch by type:
    #    - INTRODUCE (1): validate rcan, add sender to accepted set (always)
    #    - DEPART (2): remove sender from accepted set (always)
    #    - CONTRACT_PUBLISHED (3): if sender in drift_isolated → skip;
    #      else forward to Phase 10 registry handler
    #    - LEASE_UPDATE (4): if sender in drift_isolated → skip;
    #      else forward to Phase 10 registry handler
    # Silent drops (log at debug): malformed, unknown type, outside replay
    #   window, peer drift-isolated (for types 3+4 only).
    # SECURITY ALERT drops (log at warn, increment counter): sender not in
    #   accepted set, signature verification failed from accepted sender
    #   (spec §2.6).
```

### 14.7 Clock Drift Detector (§2.10)

```python
class ClockDriftDetector:
    def __init__(self, config: ClockDriftConfig): ...

    def track_offset(self, peer: str, epoch_ms: int) -> None: ...

    def mesh_median_offset(self) -> int | None:
        """Median of peer_offsets.values(); None if < min_peers_for_median."""

    def peer_in_drift(self, peer: str) -> bool:
        """True if |peer_offset - median| > drift_tolerance_ms."""

    def self_in_drift(self, self_offset_estimate: int) -> bool: ...
```

**Self-departure** (fail-fast): if `self_in_drift()` returns True past grace period, log error, broadcast `Depart` message, set `mesh_dead=True`, skip all further gossip sends.

**Peer isolation:** on drift detection, add peer to `drift_isolated`. Skip applying `ContractPublished`/`LeaseUpdate` from isolated peers. Process `Introduce`/`Depart` normally (membership operations are not time-sensitive).

**Recovery:** any fresh message from an isolated peer with acceptable offset removes it from isolation.

### 14.8 Module Layout

```
aster/trust/
├── mesh.py             # MeshState, ProducerMessage, payload types
├── bootstrap.py        # Founding + subsequent node startup
├── gossip.py           # Signing, verification, dispatch loop
├── drift.py            # ClockDriftDetector, ClockDriftConfig, self-departure
└── rcan.py             # rcan grant serialization (stub; spec TBD)
```

### 14.9 Steps

1. Define `ProducerMessage`, `IntroducePayload`, `LeaseUpdatePayload`, `MeshState`, `ClockDriftConfig`.
2. Implement `aster/trust/gossip.py`:
   - `sign_producer_message(type, payload, sender, epoch_ms, signing_key) -> ProducerMessage`.
   - `verify_producer_message(msg, peer_pubkey) -> bool`.
   - Canonical signing bytes per §14.2.
3. Implement `derive_gossip_topic(root_pubkey, salt) -> bytes`.
4. Implement founding-node startup in `aster/trust/bootstrap.py::start_founding_node()`.
5. Implement subsequent-node join in `aster/trust/bootstrap.py::join_mesh()` (admission RPC over `aster.producer_admission` ALPN).
6. Implement persistent `MeshState` (JSON under `~/.aster/mesh_state.json`).
7. Implement `ClockDriftDetector` with median computation (use `statistics.median_high`).
8. Implement gossip handler loop (async task subscribing to topic via `GossipClient.subscribe`).
9. Implement replay-window + sender-membership + signature verification in handler.
10. Implement message dispatch (Introduce/Depart/ContractPublished/LeaseUpdate) with drift-isolated filter.
11. Implement self-departure path: broadcast Depart, set `mesh_dead`, stop sending.
12. Implement peer-isolation path + recovery.
13. Implement lease heartbeat timer: every `lease_heartbeat_ms`, broadcast `LeaseUpdate`.
14. Read `ASTER_CLOCK_DRIFT_TOLERANCE_MS` + related env vars for config override.
15. Wire `ContractPublished`/`LeaseUpdate` forwarding to Phase 10 `RegistryClient.on_change` callbacks.
16. Tests — see Exit Criteria.

### 14.10 Exit Criteria

- Founding node starts, prints bootstrap ticket, salt persisted, topic derived correctly.
- Subsequent node joins via ticket, receives salt + accepted set, subscribes to same gossip topic.
- `ProducerMessage` sign/verify round-trip with canonical bytes.
- Messages outside ±30s replay window dropped silently.
- Messages from non-accepted senders dropped.
- Drift detector: correct median over ≥3 peers; <3 peers → None (no decisions).
- Self-departure triggers on >5s drift past 60s grace: Depart broadcast, subsequent gossip sends suppressed.
- Peer isolation: >5s drift → added to isolated; ContractPublished/LeaseUpdate from isolated peers skipped; Introduce/Depart still processed.
- Peer isolation recovery on fresh acceptable message.
- Lease heartbeat broadcast every 15 min.
- Test coverage:
  - Sign/verify round-trip with tampered payload (must fail)
  - Replay attack rejection
  - Unknown-sender rejection
  - 3-peer median + drift detection
  - Self-departure on synthetic clock skew
  - Peer isolation + recovery
  - Bootstrap ticket round-trip (founding → subsequent)
  - Admission RPC round-trip (accepted + rejected cases)

### 14.11 Pre-Requisites

- Phase 11 (enrollment credentials, MeshEndpointHook, admission checks).
- `blake3` PyPI package (already required by Phase 9).
- `GossipClient` / `GossipTopicHandle` FFI (already done).
- `NetClient.remote_info` for direct_addresses (already done).

### 14.12 Open Design Questions

1. **rcan grant format:** §2.5 introduces rcan grants as the introduction payload, but the format is not yet specified. Phase 12 ships with an opaque `bytes` field and a TODO to pin down the rcan serialization once upstream defines it.
2. **Admission RPC schema:** `AdmissionRequest`/`AdmissionResponse` are Fory-encoded but the exact field set for `reason` on denial needs confirmation — should it mirror `AdmissionResult.reason` from Phase 11?

---

## 15. Phase 13: Testing & Conformance

**Goal:** Comprehensive test coverage + wire/canonical conformance vectors + cross-language interop readiness.

### 15.1 Test Categories

| Category | Location | Description |
|----------|----------|-------------|
| Unit | `tests/python/test_aster_framing.py` | Frame encoding/decoding |
| Unit | `tests/python/test_aster_codec.py` | Fory serialization modes |
| Unit | `tests/python/test_aster_decorators.py` | Service introspection |
| Unit | `tests/python/test_aster_canonical.py` | Canonical XLANG byte vectors (Appendix A) |
| Unit | `tests/python/test_aster_cycles.py` | Cycle-breaking (Appendix B) |
| Unit | `tests/python/test_aster_trust.py` | Credentials, admission, nonces |
| Unit | `tests/python/test_aster_drift.py` | Clock drift median + self-departure |
| Integration | `tests/python/test_aster_unary.py` | Unary RPC end-to-end |
| Integration | `tests/python/test_aster_streaming.py` | All streaming patterns |
| Integration | `tests/python/test_aster_session.py` | Session services |
| Integration | `tests/python/test_aster_interceptors.py` | Interceptor chain |
| Integration | `tests/python/test_aster_registry.py` | Registry publish/resolve |
| Integration | `tests/python/test_aster_mesh.py` | Producer mesh bootstrap + join |
| Integration | `tests/python/test_aster_local.py` | LocalTransport parity |
| Conformance | `tests/conformance/wire/` | Byte-level wire frames |
| Conformance | `tests/conformance/canonical/` | Contract identity bytes + hashes |
| Conformance | `tests/conformance/interop/` | Echo service + scenarios for future Java binding |

### 15.2 Conformance Vectors

**Wire vectors (`tests/conformance/wire/*.bin`):**
- Stateless: HEADER, CALL, unary request/response, TRAILER-only, COMPRESSED.
- Session: HEADER with `method=""`, CALL frame, **CANCEL flags-only (1 byte)**, in-session unary success (no trailer), in-session client-stream EoI TRAILER(status=OK).
- Size cases: minimum (1-byte flags), max-1, frame-boundary cases.

**Canonical vectors (`tests/conformance/canonical/*.bin` + `.hashes.json`):**
- Appendix A.2: empty ServiceContract.
- Appendix A.3: enum TypeDef.
- Appendix A.4: TypeDef with TYPE_REF field.
- Appendix A.5: MethodDef without `requires`.
- Appendix A.6: MethodDef with `requires`.
- Scope distinctness: two ServiceContracts identical except `scoped` → different `contract_id`s.

**Interop (`tests/conformance/interop/`):**
- `echo_service.fdl` + `scenarios.yaml` — runnable against the future Java binding.
- Round-trip: Python client ↔ Java server, Java client ↔ Python server (lands when Java binding exists).

### 15.3 `aster/testing/harness.py` — Test Harness

```python
class AsterTestHarness:
    async def create_local_pair(
        self,
        service_class: type,
        implementation: object,
        wire_compatible: bool = True,
    ) -> tuple[Any, Server]:
        """In-process LocalTransport client+server; wire_compatible=True
        exercises full frame + Fory path."""

    async def create_remote_pair(
        self,
        service_class: type,
        implementation: object,
    ) -> tuple[Any, Server, IrohConnection, IrohNode, IrohNode]:
        """Two Iroh nodes + connection; real network path."""

    async def create_session_pair(
        self,
        service_class: type,  # scoped="stream"
        implementation: object,
        wire_compatible: bool = True,
    ) -> tuple[Any, Server]: ...
```

### 15.4 Steps

1. Write unit tests for framing (Phase 1) — ensure CANCEL flags-only frame round-trips.
2. Write unit tests for codec (Phase 2).
3. Write unit tests for decorators (Phase 4).
4. Write canonical byte-vector tests (Phase 9 — load Appendix A fixtures).
5. Write cycle-breaking tests (Phase 9 — Appendix B fixtures).
6. Write scope-distinctness test: SHARED vs STREAM contracts hash differently.
7. Write manifest-mismatch fatal test (Phase 9).
8. Write session wire vectors + in-session unary no-trailer test.
9. Write interceptor chain + deadline + retry + circuit-breaker tests (Phase 7).
10. Write registry publish/resolve tests (Phase 10).
11. Write `lease_seq` monotonicity test.
12. Write trust credential verify/expire/wrong-endpoint tests (Phase 11).
13. Write nonce-consumption test (Phase 11).
14. Write clock drift + self-departure + peer-isolation tests (Phase 12).
15. Write admission RPC round-trip test (Phase 12).
16. Implement `AsterTestHarness` with all three factories.
17. Stand up cross-language interop fixtures (structure ready; scenarios lie dormant until Java binding exists).

### 15.5 Exit Criteria

- All RPC patterns work end-to-end over real Iroh connections.
- LocalTransport produces identical wire bytes to IrohTransport when `wire_compatible=True`.
- Canonical vectors produced by Phase 9 are committed as "Python-reference v1"; rule-level micro-fixtures isolate each §11.3.2 rule.
- All Appendix A + B fixture hashes match.
- Session wire vectors include CANCEL flags-only (1-byte) frame.
- Conformance vectors exist for: wire (stateless + session), canonical contract bytes, scope distinctness.
- Cross-language interop harness structure in place (scenarios activate when Java binding lands).

---

## 16. Dependency Map

```
Phase 1: Wire Protocol & Framing
    ↓
Phase 2: Serialization (Fory)      ← REQUIRES: pyfory verification
    ↓
Phase 3: Transport Abstraction
    ↓
Phase 4: Service Definitions ──────────────────────────┐
    ↓                                                   │
Phase 5: Server ───────┐                               │
    ↓                   │                               │
Phase 6: Client ────────┤                               │
    ↓                   │                               │
Phase 7: Interceptors ──┘                               │
    ↓                                                   │
Phase 8: Sessions ──────────────────┐                  │
    ↓                                │                  │
Phase 9: Contract Identity ←──── adds scoped field ────┘
    ↓
Phase 10: Registry  ←── uses FFI: import_and_subscribe,
    ↓                   DownloadPolicy, doc.subscribe,
    ↓                   blob_observe_complete,
    ↓                   blob_local_info, Tags API
Phase 11: Trust Foundations ────┐
    ↓                            │
Phase 12: Producer Mesh ←────────┘ (requires Phase 11)
    ↓
Phase 13: Testing & Conformance (ongoing throughout)
```

**Critical path for minimum viable RPC:** Phases 1 → 2 → 3 → 4 → 5 → 6.

**Can proceed in parallel:**
- Phase 7 (Interceptors) can start after Phase 4.
- Phase 9 (Contract Identity) depends on Phase 8 for `scoped` field in `ServiceContract`; otherwise can start after Phase 4.
- Phase 11 (Trust) is independent of Phase 10; can be built in parallel.
- Phase 13 (Testing) runs continuously.

**Cross-phase couplings:**
- **Phase 8 ⇄ Phase 9:** `ServiceContract.scoped` field is Phase 9 but is driven by Phase 8's `scoped="stream"` decorator. Two services identical except `scoped` MUST hash to different `contract_id`s.
- **Phase 9 → Phase 10:** Phase 10's `publish_contract()` delegates to Phase 9; registry publishes ArtifactRefs that resolve via Phase 9 hashes.
- **Phase 10 → Phase 12:** Gossip events `ContractPublished` and `LeaseUpdate` forward into the registry handler. Phase 12 without Phase 10 still works (producer mesh alone), but the registry benefits from mesh signing.

---

## 17. Open Pre-Requisites

### 17.1 Must Verify Before Starting

| Item | Risk | Mitigation |
|------|------|------------|
| pyfory XLANG mode works in Python | Medium | Spike test in Phase 2 step 1 ✅ verified |
| pyfory supports tag-based type registration | Medium | ✅ verified |
| pyfory ROW mode available in Python | Low | ✅ verified |
| Canonical encoder produces stable bytes | High | Phase 9 uses **custom byte-level encoder**, not pyfory |
| blake3 Python package | None | ✅ available |
| zstandard Python package | None | ✅ available |
| cryptography Python package (ed25519) | None | Phase 11 requirement |
| Canonical test vectors | Low | Python IS the reference; Phase 9 produces the first set, Java binding cross-verifies |
| Iroh source-IP availability | N/A | CIDR filtering removed from normative spec (IPs are unreliable in Iroh's relay/hole-punch model — see Aster-trust-spec.md §1) |

### 17.2 Dependencies to Add to `pyproject.toml`

```toml
[project]
dependencies = [
    "pyfory>=0.15",       # Apache Fory serialization (user payloads)
    "blake3>=1.0",        # BLAKE3 hashing for contract identity
    "zstandard>=0.22",    # zstd compression for frame payloads
    "cryptography>=42",   # ed25519 signing/verify (Phase 11+)
]

[project.optional-dependencies]
otel = ["opentelemetry-api>=1.20"]      # Optional: MetricsInterceptor
iid  = ["PyJWT>=2.8"]                   # Optional: cloud IID verification (Phase 11)

[project.scripts]
aster = "aster.cli:main"                # `aster contract gen`, `aster trust ...`
```

### 17.3 Decisions to Lock Before Implementation

| Decision | Recommendation | Status |
|----------|---------------|--------|
| All RPC layer code is pure Python (no new Rust) | Yes — transport FFI is sufficient | ✅ Locked |
| Package location: `bindings/aster/aster/` | Yes — sub-package of existing binding | ✅ Locked |
| pyfory version pin | Pin to 0.15.x until Fory 1.0 | ✅ Locked |
| ALPN for Aster services | `aster/1` (core), `aster.producer_admission`, `aster.consumer_admission` | ✅ Spec-defined |
| Session support included | Yes — Phase 8 | ✅ Locked |
| Registry is optional | Yes — services work with direct connect | ✅ Locked |
| Trust is in scope as Phase 11+12 | Yes — decided 2026-04 | ✅ Locked |
| Canonical encoder is custom code (not pyfory wrapper) | Yes — per §11.3.2 normative | ✅ Locked |
| Network-level controls (CIDR, source-IP) | Out of scope — use network boundary (VPN/firewall) | ✅ Locked |
| rcan grant format | **OPEN** — track upstream | ⚠️ Phase 12 TODO |
| AdmissionRequest/Response schema details | **OPEN** — confirm with spec team | ⚠️ Phase 12 TODO |

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
| 9 | Contract Identity | 5–8 days | 25–40 days |
| 10 | Registry | 5–7 days | 30–47 days |
| 11 | Trust Foundations | 4–6 days | 34–53 days |
| 12 | Producer Mesh | 5–8 days | 39–61 days |
| 13 | Testing & Conformance | Ongoing | — |

**Minimum viable RPC (Phases 1–6):** ~2–3 weeks for unary + streaming RPCs working end-to-end.

Phase 9 effort increased (5–8 days) reflecting the custom canonical encoder + SCC cycle-breaking, not a wrapper around pyfory.

---

*End of plan.*