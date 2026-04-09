# Aster Python API — Public Surface Audit

**Date:** 2026-04-09  
**Goal:** Define what's public, what's internal, and what needs docs.

## Proposed tiers

### Tier 1: Essential (every user needs these)

These are the "5-minute getting started" symbols. A developer who reads
the Mission Control guide will use all of these.

| Symbol | Module | What it does |
|--------|--------|-------------|
| `AsterServer` | high_level | Start a server with services |
| `AsterClient` | high_level | Connect to a server |
| `@service` | decorators | Declare an RPC service class |
| `@rpc` | decorators | Mark a unary RPC method |
| `@server_stream` | decorators | Mark a server-streaming method |
| `@client_stream` | decorators | Mark a client-streaming method |
| `@bidi_stream` | decorators | Mark a bidirectional streaming method |
| `@wire_type` | codec | Register a dataclass for cross-language serialization |
| `RpcError` | status | Exception raised on RPC failure |
| `StatusCode` | status | Enum of RPC status codes |
| `AsterConfig` | config | Server/client configuration |

**Count: 11 symbols.** This is the actual public API for Day 0.

### Tier 2: Power user (interceptors, advanced patterns)

Used by developers who need auth, retry, deadlines, or custom
middleware. Documented but not in the quickstart.

| Symbol | Module | What it does |
|--------|--------|-------------|
| `Interceptor` | interceptors | Base class for custom interceptors |
| `CallContext` | interceptors | Per-call context passed to interceptors |
| `DeadlineInterceptor` | interceptors | Enforce RPC deadlines |
| `RetryInterceptor` | interceptors | Automatic retry with backoff |
| `CircuitBreakerInterceptor` | interceptors | Circuit breaker pattern |
| `AuthInterceptor` | interceptors | Capability-based auth checking |
| `AuditLogInterceptor` | interceptors | Audit trail for RPC calls |
| `MetricsInterceptor` | interceptors | Observability metrics |
| `SerializationMode` | types | XLANG / NATIVE / ROW mode enum |
| `ServiceClient` | client | Base class for generated clients |
| `create_client` | client | Create a typed client from a @service class |
| `create_local_client` | client | In-process client for testing |
| `ServiceInfo` | decorators | Service metadata (read-only) |
| `MethodInfo` | service | Method metadata (read-only) |
| `RetryPolicy` | types | Retry configuration |
| `ExponentialBackoff` | types | Backoff configuration |

**Count: 16 symbols.**

### Tier 3: Framework internals (NOT public API)

These are exported in `__init__.py` but should NOT be in public docs.
They're implementation details that advanced users might import but
we don't document or guarantee stability for.

| Symbol | Why it's internal |
|--------|------------------|
| `COMPRESSED`, `TRAILER`, `HEADER`, `ROW_SCHEMA`, `CALL`, `CANCEL` | Wire framing constants |
| `MAX_FRAME_SIZE`, `FramingError`, `write_frame`, `read_frame` | Wire framing functions |
| `StreamHeader`, `CallHeader`, `RpcStatus` | Wire protocol types |
| `ForyCodec`, `ForyConfig`, `DEFAULT_COMPRESSION_THRESHOLD` | Serialization internals |
| `Transport`, `BidiChannel`, `TransportError`, `ConnectionLostError` | Transport abstractions |
| `IrohTransport`, `LocalTransport` | Transport implementations |
| `Metadata` | Call metadata carrier |
| `Server`, `ServerError`, `ServiceNotFoundError`, `MethodNotFoundError` | Low-level server (use AsterServer) |
| `SerializationModeError` | Internal error type |
| `ServiceRegistry`, `get_default_registry`, `set_default_registry` | Registry internals |
| `RpcPattern` | Pattern enum (exposed via decorators) |
| `HealthServer`, `check_health`, `check_ready`, `metrics_snapshot` | Health system (not Day 0) |
| `RPC_ALPN` | Protocol constant |
| `ClientError`, `ClientTimeoutError` | Should be caught as RpcError |
| `load_endpoint_config` | Config internals |

### Tier 4: Native bindings (iroh transport layer)

Exported for direct iroh access. Not part of the Aster RPC API per se,
but available for advanced P2P use cases.

| Symbol | What it does |
|--------|-------------|
| `IrohNode` | Low-level iroh node |
| `BlobsClient`, `DocsClient`, `GossipClient` | iroh protocol clients |
| `NodeAddr`, `EndpointConfig` | Networking types |
| `IrohConnection`, `IrohSendStream`, `IrohRecvStream` | QUIC stream types |
| `AsterTicket` | Compact ticket format |
| Various hook types | Connection lifecycle hooks |

These should be documented separately as "Iroh Transport API" —
they're a different audience (P2P networking, not RPC).

## Recommendation

**Document Tier 1 thoroughly.** Every symbol gets:
- One-line description
- Constructor/decorator signature with all parameters explained
- Usage example
- Common patterns / gotchas

**Document Tier 2 as reference.** Each symbol gets:
- One-line description
- Signature
- Brief example

**Do not document Tier 3.** Remove from `__init__.py` or move to
a `_internals` re-export. If someone needs `StreamHeader`, they can
import from `aster.protocol` directly.

**Document Tier 4 separately** under "Iroh Transport API" for
advanced users who want raw P2P access.

## Doc format

Use standard Python docstrings (already in the code) + generate with
a tool. Options:
- **pdoc** — zero config, generates from docstrings, outputs HTML
- **mkdocs + mkdocstrings** — markdown-based, more customisable
- **sphinx** — most powerful, most setup

For Day 0: **pdoc** on Tier 1 + Tier 2 only. Fast, no config, looks
good enough. Generate to `docs/api/` and host on GitHub Pages.

## Action items

1. Audit docstrings on all Tier 1 symbols — are they complete?
2. Audit docstrings on all Tier 2 symbols — are they present?
3. Add missing docstrings
4. Generate docs with pdoc
5. Review the output
6. Ship
