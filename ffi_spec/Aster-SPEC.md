# Aster Specification

**Version:** 0.7.2-internal-draft
**Status:** Design Phase
**Last Updated:** 2026-04-04

-----

## Table of Contents

1. [Overview](#1-overview)
2. [Design Rationale: Why Not gRPC?](#2-design-rationale-why-not-grpc)
3. [Architecture](#3-architecture)
4. [Transport Layer — Iroh FFI](#4-transport-layer--iroh-ffi)
5. [Serialization Layer — Apache Fory](#5-serialization-layer--apache-fory)
6. [Wire Protocol](#6-wire-protocol)
7. [Service Definition Layer](#7-service-definition-layer)
8. [Client and Server APIs](#8-client-and-server-apis)
9. [Interceptors and Middleware](#9-interceptors-and-middleware)
10. [Connection Lifecycle](#10-connection-lifecycle)
11. [Service Registry and Discovery](#11-service-registry-and-discovery)
12. [Security and Access Control](#12-security-and-access-control)
13. [Conformance and Interoperability](#13-conformance-and-interoperability)
14. [Implementation Roadmap](#14-implementation-roadmap)
15. [Package Structure](#15-package-structure)
16. [Open Design Questions](#16-open-design-questions)

-----

## 1. Overview

Aster is a cross-language RPC framework composed of three independent layers:

- **Transport:** Iroh QUIC (dial by public key, NAT traversal, E2E encryption)
- **Serialization:** Apache Fory (native objects, cross-language, zero-copy, JIT-optimised)
- **Contract:** gRPC-style service definitions expressed through language-native idioms

The framework includes a decentralised service registry built on Iroh’s data
primitives (iroh-docs, iroh-gossip, iroh-blobs, with Automerge optional at
higher layers), providing content-addressed contract publication, schema
discovery, endpoint capability leasing, and access-controlled publishing — with
no infrastructure servers.

### 1.1 Design Principles

1. **Composed, not monolithic.** Each layer (transport, serialization, contract)
   is an independent choice. Swap any layer without affecting the others.
1. **Language-native surfaces.** Each language uses idiomatic patterns. No
   language is forced into another’s idioms.
1. **Single wire protocol.** The byte-level wire format is the interoperability
   contract. Language implementations must conform to the wire spec.
1. **Identity is the connection.** Cryptographic identity (Iroh EndpointId) is
   the authentication primitive. No bolt-on certificate infrastructure.
1. **Stream-per-RPC.** Each RPC invocation opens a dedicated QUIC stream.
   No multiplexing, no request-ID correlation, no head-of-line blocking
   between concurrent calls.
1. **Python first.** Python is the exemplar implementation. Other languages
   follow, validated by conformance tests.

### 1.2 Target Languages

|Language             |Priority          |Status|
|---------------------|------------------|------|
|Python               |Phase 1 (exemplar)|Implemented through Phase 7 |
|Rust                 |Phase 2           |TODO  |
|JVM (Java/Kotlin)    |Phase 3           |TODO  |
|.NET (C#/F#)         |Phase 3           |TODO  |
|Go                   |Phase 4           |TODO  |
|JavaScript/TypeScript|Phase 4           |TODO  |

-----

## 2. Design Rationale: Why Not gRPC?

### 2.1 The Transport Problem

gRPC is inseparable from HTTP/2 over TCP. This creates three problems
Aster solves:

**NAT traversal.** gRPC assumes routable addresses. Aster dials by public
key — Iroh handles hole-punching, relay fallback, and path selection. No load
balancer, no VPN, no port forwarding.

**Head-of-line blocking.** HTTP/2 multiplexes streams over one TCP connection.
A lost packet stalls every stream. QUIC provides independent per-stream flow
control — a lost packet on one stream does not affect any other.

**Connection migration.** TCP connections are bound to a 4-tuple (src IP, src
port, dst IP, dst port). QUIC connections are bound to a connection ID and
survive network changes transparently.

### 2.2 The Identity Problem

gRPC identifies services by hostname:port. Authentication is bolt-on (TLS
certs, call credentials). In Aster, every endpoint has a keypair. The QUIC
handshake authenticates both sides cryptographically. Identity is a property of
the transport, not a separate concern.

### 2.3 The Serialization Problem

gRPC is tightly coupled to Protocol Buffers, which forces: IDL compilation as a
build step, non-idiomatic generated code in every language, and no support for
object-graph semantics (shared references, cycles, polymorphism).

Apache Fory serializes native objects directly with 10–170x better performance,
handles object graphs natively, and offers optional IDL for cross-language
contracts that generates idiomatic code indistinguishable from hand-written
domain objects.

### 2.4 Summary

|Concern                  |gRPC                              |Aster                                     |
|-------------------------|----------------------------------|------------------------------------------|
|Transport                |HTTP/2 over TCP                   |QUIC over Iroh                            |
|Connectivity             |Requires routable addresses       |Dial by public key, NAT traversal built-in|
|Identity                 |Bolt-on (TLS certs, tokens)       |Cryptographic identity *is* the connection|
|Serialization            |Protobuf (IDL + codegen required) |Fory (native objects, optional IDL)       |
|Head-of-line blocking    |Yes (TCP)                         |No (QUIC stream independence)             |
|Connection migration     |No                                |Yes (QUIC connection IDs)                 |
|Object graphs            |Not supported                     |Native (shared refs, cycles, polymorphism)|
|Serialization performance|Baseline                          |10–170x faster (workload-dependent)       |
|Service discovery        |Server reflection (point-to-point)|Decentralised registry (CRDT-synced)      |
|Ecosystem maturity       |Excellent                         |Greenfield                                |

### 2.5 The Honest Tradeoff

gRPC has ecosystem maturity that Aster does not: tooling, observability
integration, service mesh support, load balancer awareness. Aster is the
right choice when peers are behind NATs/firewalls, identity-first connectivity
matters, object-graph serialization is needed, or HTTP/2 is dead weight.

-----

## 3. Architecture

### 3.1 Layer Model

```
┌───────────────────────────────────────────────────────┐
│  Layer 4: Service Definition                          │
│  Decorators / annotations / macros (per language)     │
│  Service registry, dispatch, client stub generation   │
├───────────────────────────────────────────────────────┤
│  Layer 3: RPC Protocol                                │
│  Stream header, framing, status codes, deadlines      │
│  Interceptor chain, retry logic                       │
├───────────────────────────────────────────────────────┤
│  Layer 2: Serialization                               │
│  Apache Fory (negotiated ordered formats per service/method) │
│  Codec abstraction, compression, type registry        │
├───────────────────────────────────────────────────────┤
│  Layer 1: Transport                                   │
│  Iroh FFI (per-language wrapper around Rust core)     │
│  Endpoint, Connection, SendStream, RecvStream         │
└───────────────────────────────────────────────────────┘

┌───────────────────────────────────────────────────────┐
│  Layer 5: Service Registry (optional)                 │
│  Content-addressed contract publication (iroh-docs)   │
│  Change notification and hinting (iroh-gossip)        │
│  Large artifact transfer (iroh-blobs)                 │
│  Endpoint leasing, channels, access control           │
└───────────────────────────────────────────────────────┘
```

**Interoperability boundary:** Layers 1–3 must produce identical bytes across
all language implementations. Layer 4 must produce identical wire behaviour but
is implemented idiomatically per language. Layer 5 uses Iroh’s own wire
protocols and is cross-language by construction.

### 3.1.1 Ownership by Layer

Aster deliberately separates **transport ownership** from **RPC API ownership**.

**Rust owns the transport substrate (Layer 1):**

- Iroh endpoint, connection, stream, datagram, docs, blobs, and gossip primitives
- Runtime lifecycle, handle registry, completion queue, buffer ownership, and cancellation
- Cryptographic identity, secret key import/export, relay configuration, and discovery
- The thin C ABI / FFI surface shared by all foreign language wrappers

**The target language owns the RPC surface (primarily Layers 2–4):**

- Service definitions and developer-facing API shape
- Request/response/update types and type registration
- Futures/promises/coroutines/channels/publishers and streaming abstractions
- Serialization integration for application objects
- Client stubs, server adapters, dispatch ergonomics, middleware/interceptors, and error mapping

**The specification owns the wire contract:**

- Session handshake and capability negotiation
- Stream header format
- Message framing
- Status and trailer semantics
- Serialization mode signalling
- Service and method naming rules
- Conformance vectors and interoperability tests

Rust is the shared **transport core**, not the owner of language-specific RPC
idioms. Each language implementation must feel native in that language while
still producing bytes that conform to the common Aster wire protocol.

### 3.1.2 Layer Responsibilities

|Layer                      |Owned by                                      |Purpose                                                                     |
|---------------------------|----------------------------------------------|----------------------------------------------------------------------------|
|Layer 1: Transport         |Rust core + thin FFI                          |Iroh transport primitives and runtime                                       |
|Layer 2: Serialization     |Target language runtime, constrained by spec  |Encode/decode application objects according to negotiated serialization mode|
|Layer 3: RPC Protocol      |Implemented in every language, defined by spec|Headers, framing, status, trailers, deadlines, cancellation semantics       |
|Layer 4: Service Definition|Target language                               |Idiomatic decorators, annotations, interfaces, macros, codegen, stubs       |
|Layer 5: Service Registry  |Shared protocol + per-language wrappers       |Registry publication, sync, lookup, health, policy                          |

### 3.2 Stream-per-RPC Model

Each RPC invocation opens a new bidirectional QUIC stream via `open_bi()`:

- **No multiplexing complexity.** No request IDs, no correlation logic, no
  interleaved frame parsing. The stream *is* the correlation.
- **Clean lifecycle.** Each stream maps exactly to one RPC lifecycle: open →
  exchange → finish. Stream termination signals RPC completion.

This follows the design pattern established by irpc (n0-computer), adapted for
cross-language operation with Fory replacing postcard as the serialization
framework.

### 3.2.1 Sibling Channels for Non-RPC Data Paths

The stream that carries an Aster call must remain an Aster stream for its entire
lifetime. Implementations must **not** “upgrade” an RPC stream into a raw byte
tunnel or any other non-Aster protocol after the header is sent.

If an RPC needs to establish a bulk-data or transport side channel, it must
*negotiate a sibling channel* on the same underlying Iroh connection:

- **Reliable byte tunnel:** negotiate a sibling bidirectional QUIC stream.
- **Best-effort datagram tunnel:** negotiate connection datagrams.
- **Large immutable payload transfer:** negotiate an `iroh-blobs` capability.

This rule prevents a class of bugs where one side continues speaking Aster
frames while the other side has switched the stream into a raw transport mode.
The control-plane RPC remains Aster-framed throughout; any data-plane path is a
separate sibling channel identified by application metadata such as a
`tunnel_id`.

-----

## 4. Transport Layer — Iroh FFI

### 4.1 FFI Contract

Each language wraps the same Iroh Rust core library, exposing identical
transport primitives. The Rust FFI crate is intentionally thin (~200–300 lines)
and exposes only transport concerns — no service definitions, no request or
response object models, no language-specific futures/promises/publishers, and
no RPC API concepts beyond the raw transport substrate.

```
Endpoint:
    bind(config?) → Endpoint
    endpoint_id → EndpointId               # This endpoint's public key
    connect(addr: NodeAddr, alpn: bytes) → Connection
    accept() → Incoming
    close()

Connection:
    remote_endpoint_id → EndpointId
    open_bi() → (SendStream, RecvStream)
    accept_bi() → (SendStream, RecvStream)
    open_uni() → SendStream
    accept_uni() → RecvStream
    send_datagram(data: bytes)
    read_datagram() → bytes
    max_datagram_size() → u32?              # null/None = datagrams unsupported on this connection
    datagram_send_buffer_space() → u32
    closed()                                # Blocks/awaits until connection drops
    close(code: u32, reason: bytes)

SendStream:
    write(data: bytes)
    finish()                                # Signals clean end of stream
    stopped() → u32                         # Remote cancelled; returns error code

RecvStream:
    read(max_len: u32) → bytes?             # null/None = stream finished
    read_exact(n: u32) → bytes
    stop(code: u32)                         # Cancel from receiver side

NodeAddr:
    endpoint_id: EndpointId
    relay_url: string?
    direct_addresses: list[SocketAddr]

EndpointConfig:
    relay_mode: enum { Disabled, Default, Custom(url) }
    discovery: list[enum { Dns, Dht, Mdns }]
    alpns: list[bytes]
    secret_key: bytes?                      # null = generate new keypair
```

### 4.1.1 Datagram Semantics

Datagrams are a first-class transport primitive in the common FFI surface. They
are intended for **unreliable, unordered, message-oriented** delivery on an
existing Iroh connection.

Rules:

1. Datagrams are connection-scoped, not stream-scoped.
1. Datagrams may be lost, reordered, or dropped under pressure.
1. Applications must not assume QUIC datagram boundaries correspond to any
   stream write boundaries.
1. Callers must check `max_datagram_size()` before using datagrams and must
   fragment or reject oversize application datagrams themselves.
1. When datagrams are unsupported for a connection, applications must fall back
   to sibling streams or reject the requested mode with `UNIMPLEMENTED` or
   `FAILED_PRECONDITION`.

Datagrams are therefore appropriate for UDP-like tunnels and other best-effort
message paths. Reliable, ordered byte transport remains the responsibility of
QUIC streams.

### 4.2 Per-Language FFI Strategy

**Phase 1 (Python exemplar):**

|Component    |Technology                           |Package      |
|-------------|-------------------------------------|-------------|
|FFI mechanism|PyO3 / maturin → compiled `.so` wheel|`iroh-python`|
|Async model  |`asyncio` (async/await)              |—            |
|Distribution |PyPI wheel                           |—            |

The Python FFI wrapper uses the archived `iroh-ffi` repository and its Python
type stubs (`.pyi`) as a reference blueprint.

**Future phases (TODO):**

|Language|FFI Mechanism                     |Async Model                            |Status|
|--------|----------------------------------|---------------------------------------|------|
|Rust    |Direct `iroh` crate dependency    |`tokio`                                |TODO  |
|JVM     |JNI via `jni` crate → `.so`/`.dll`|`CompletableFuture` / Kotlin coroutines|TODO  |
|.NET    |P/Invoke via C ABI → native lib   |`Task<T>` / async-await                |TODO  |
|Go      |CGo via C ABI → `.so`             |Goroutines / channels                  |TODO  |
|JS/TS   |NAPI via `napi-rs` → native addon |`Promise` / async-await                |TODO  |


> **TODO:** Evaluate UniFFI as a single-source generator for Python, Kotlin, and
> Swift bindings from the same Rust crate. This could reduce maintenance burden
> for Phases 3–4.

-----

## 5. Serialization Layer — Apache Fory

### 5.1 Serialization Protocols

Fory provides three distinct binary protocol families. Aster maps these directly
— no invented abstraction on top. The default is `XLANG` for maximum
interoperability.

|Protocol         |Fory Family        |Cross-Language                                      |Schema Evolution            |Use Case                                                                                                |
|-----------------|-------------------|----------------------------------------------------|----------------------------|--------------------------------------------------------------------------------------------------------|
|`XLANG` (default)|Xlang Serialization|Yes — Java, Python, C++, Go, JS, Rust, Scala, Kotlin|Via content-addressed registry (§11.3)|Cross-language services. Object graphs, shared refs, polymorphism.                                      |
|`NATIVE`         |Language-specific  |No — single language only                           |Via content-addressed registry (§11.3)|Maximum performance within one language. Python: pickle replacement. JVM: JDK serialization replacement.|
|`ROW`            |Row Format         |Yes — Java, Python, C++, Rust                       |N/A                         |Zero-copy random access, partial deserialization, Arrow integration.                                    |

`NATIVE_COMPATIBLE` and `SerializationMode.JAVA` do not exist in Aster. Schema
evolution is handled by the content-addressed contract registry (§11.3): each
type version produces a distinct hash, so different schema versions coexist as
separate immutable artifacts. Fory's `compatible` mode (which embeds per-field
metadata in every payload) is not used — the registry provides the same
forward/backward evolution guarantees without per-payload overhead.

### 5.2 Protocol Selection

Services declare a default protocol. Individual methods may override:

```python
@service(
    name="AgentControl",
    version=1,
    serialization=[SerializationMode.XLANG],
)
class AgentControlService:

    @rpc  # Inherits XLANG from service
    async def assign_task(self, req: TaskAssignment) -> TaskAck: ...

    @server_stream  # Also inherits
    async def step_updates(self, req: TaskId) -> AsyncIterator[StepUpdate]: ...

    @rpc(serialization=[SerializationMode.ROW])  # Override to ROW for this method
    async def query_metrics(self, req: MetricsQuery) -> MetricsResult: ...
```

The wire protocol carries the protocol mode in the `StreamHeader` (see §6.2)
so the receiver knows how to deserialize.

**Cross-language enforcement:** If client and server are different languages,
only `XLANG` and `ROW` protocols are permitted. `NATIVE` is always
single-language. The framework detects language mismatch during connection
handshake (via metadata exchange) and rejects incompatible protocol selections
with `INVALID_ARGUMENT`.

### 5.3 XLANG Mode (Default)

Cross-language object graph serialization. Types are identified on the wire by
a **canonical tag string**, not a numeric ID. Numeric IDs are a local
implementation optimisation invisible to the wire format.

#### 5.3.1 Canonical Tag Strings

Every type used in an XLANG service must carry a canonical tag. The tag format
is:

```
"{dotted.package}/{TypeName}"
```

Examples: `"aster.agent/TaskAssignment"`, `"aster.transfer/FileRef"`.

Rules:

- Packages use dot notation with no language-specific separators.
- `TypeName` is the unqualified struct, class, or dataclass name exactly as
  defined in the IDL or source.
- Tags are case-sensitive and must be identical across all language
  implementations of the same type.
- The namespace `_aster/*` is reserved for framework-internal types
  (`_aster/StreamHeader`, `_aster/RpcStatus`, etc.). Application packages must
  not use the `_aster` prefix.

**Tag collisions are handled by the combination of Fory registration and
content-addressed contract identity, not by first-wins/last-wins policy:**

- **Within a process:** Fory registration fails fast on duplicate tags. If
  two classes attempt to register the same tag in the same `ForyCodec`,
  registration raises an error at startup. Developers see the conflict
  immediately and rename or re-namespace.
- **Across processes:** `contract_id` is a BLAKE3 hash over the canonical
  bytes of the full type graph (§11.3). Two services that use the same tag
  string but define the type with different fields produce different
  `type_hash`es → different `contract_id`s → the registry routes them as
  distinct services. Consumers that resolve on `contract_id` will only
  connect to endpoints whose structural definition matches.
- **Identical tag + identical structure:** by design, these are the same
  type and are safely interchangeable. Content-addressed identity is what
  makes this correct.

The framework therefore does not define a collision-resolution policy —
none is needed. Implementations MUST fail at registration time on
intra-process duplicates; MUST NOT silently discard one of two registrations;
and MUST NOT attempt cross-process deduplication by tag alone (tags are
hints, `contract_id` is identity).

#### 5.3.2 Tag Declaration

**IDL-defined types** derive their tag automatically from the IDL package and
struct name. No manual declaration is required:

```
// agent_control.fdl
package aster.agent;

struct TaskAssignment { ... }   // tag = "aster.agent/TaskAssignment"
struct TaskAck { ... }          // tag = "aster.agent/TaskAck"
```

**Code-first types** must declare their tag explicitly using the `@fory_type`
decorator (Python) or equivalent annotation in other languages:

```python
from aster import fory_type
from dataclasses import dataclass

@dataclass
@fory_type(tag="aster.agent/TaskAssignment")
class TaskAssignment:
    task_id: str
    workflow_yaml: str
    credential_refs: list[str]
    step_budget: int
```

```java
// Java
@ForyType(tag = "aster.agent/TaskAssignment")
public record TaskAssignment(
    String taskId,
    String workflowYaml,
    List<String> credentialRefs,
    int stepBudget
) {}
```

Explicit tag declaration prevents a class of silent deserialization bugs that
arise when a type is refactored into a different module or package — the tag
stays stable even if the language-native fully-qualified name changes.

#### 5.3.3 Auto-Registration and Eager Validation

The `@service` decorator (and equivalent in other languages) inspects method
signatures and recursively registers all referenced types with Fory using their
declared tags. If any type reachable from an XLANG service method lacks a tag
declaration, the framework raises an error **at class definition time**, not at
call time:

```python
@service(name="AgentControl", version=1, serialization=[SerializationMode.XLANG])
class AgentControlService:
    @rpc
    async def assign_task(self, req: TaskAssignment) -> TaskAck: ...
    # Raises immediately if TaskAssignment or TaskAck lacks @fory_type(tag=...)
    # TypeError: TaskAssignment used in XLANG service but has no fory_type tag.
    # Add @fory_type(tag="your.package/TaskAssignment") to the class.
```

Tag validation is skipped for `NATIVE` mode, which is single-language and does
not require cross-language type identity.

#### 5.3.4 Numeric Type IDs (Local Optimisation Only)

Implementations may use numeric type IDs internally for fast in-process Fory
registration lookups. When needed, derive numeric IDs deterministically:

```
numeric_id = int.from_bytes(blake3(tag.encode("utf-8"))[:4], "little") & 0x7FFF_FFFF
```

Numeric IDs derived this way are invisible to the wire — the tag string is the
wire identity. Framework-internal types occupy the reserved range 0–999 and use
spec-assigned IDs, not hash-derived ones.

### 5.4 NATIVE Protocol

Language-specific protocol for maximum performance within a single language.
Each language maps to its own Fory native implementation:

|Language|Fory Native Protocol|Notes                                                                                       |
|--------|--------------------|--------------------------------------------------------------------------------------------|
|Python  |Python Native       |Drop-in replacement for pickle/cloudpickle. Supports local functions, lambdas, `__reduce__`.|
|JVM     |Java Serialization  |Drop-in JDK serialization replacement, ~100x faster.                                        |
|Rust    |Rust native         |Compile-time codegen via `#[derive(ForyObject)]`.                                           |
|Go      |Go native           |AOT codegen for struct serialization.                                                       |
|C++     |C++ native          |Compile-time via `FORY_STRUCT` macro.                                                       |
|JS/TS   |JS native           |Dynamic serialization.                                                                      |

No tag registration required. Not wire-compatible across languages — `NATIVE`
is only valid when both client and server are the same language.

### 5.5 ROW Mode

Fory’s row format provides cache-friendly binary layout with zero-copy random
access. Individual fields can be read without deserializing the entire object.
This is particularly valuable for:

- **Large analytical payloads** where the consumer only needs a subset of fields
- **Partial deserialization** in routing/filtering middleware that inspects
  specific fields without full deserialization cost — for example, a
  `RiskInterceptor` reading `ApprovalRequest.risk_level` without deserializing
  `action_description` or `screenshot_region`
- **Arrow integration** — row format converts directly to/from Apache Arrow
  RecordBatch for analytics pipelines
- **Streaming aggregation** where intermediate nodes read metrics fields
  without deserializing payload bodies

```python
# Python ROW mode example
@rpc(serialization=SerializationMode.ROW)
async def query_metrics(self, req: MetricsQuery) -> MetricsResult: ...

# Consumer can read individual fields without full deserialization:
row = fory_row_data(schema, raw_bytes)
timestamp = row.get_field("timestamp")  # Zero-copy access
value = row.get_field("value")          # No full deserialization
```

Row format is cross-language compatible (Java, Python, C++ today; other
languages per Fory’s roadmap).

#### 5.5.1 ROW Mode Framing

ROW mode payloads use **identical length-prefix framing** to all other
serialization modes (§6.1). The 4-byte `Length` field serves the stream reader,
not the Fory row parser — even though Fory’s row binary has rich internal
structure (null bitmaps, field offsets, variable-length sections), that
structure does not help a stream reader locate the next message boundary. The
frame boundary and the row boundary are separate concerns. ROW payloads slot
into the existing frame format as the `Payload` field, with the `COMPRESSED`
flag applying as normal.

#### 5.5.2 ROW Mode in Streaming Patterns

ROW mode is fully supported in all streaming RPC patterns (`@server_stream`,
`@client_stream`, `@bidi_stream`). Each item in the stream is independently
framed per §6.1.

For streaming patterns where all items share a single type — which is always the
case for the typed sides of `@server_stream`, `@client_stream`, and
`@bidi_stream` — repeating the Fory row schema in every frame is wasteful.
Aster resolves this with **schema hoisting**:

- The first data frame of a ROW-mode stream sets the `ROW_SCHEMA` flag (bit 3,
  `0x08`) and carries the Fory row schema as its entire payload.
- All subsequent data frames on the same stream carry pure row binary with no
  schema prefix and no `ROW_SCHEMA` flag.
- A stream carrying ROW items must contain **exactly one** `ROW_SCHEMA` frame,
  and it must appear before any non-schema data frames.
- Receivers must buffer the schema from the `ROW_SCHEMA` frame and apply it to
  all subsequent row frames for the lifetime of that stream.
- Unary ROW-mode RPCs do not use schema hoisting; the schema is embedded
  directly in the single request and response payloads per Fory’s standard row
  encoding.

This preserves the zero-copy field-access advantage of ROW mode across streams
without per-frame schema overhead.

### 5.6 Compression

Payloads exceeding a configurable threshold (default: 4096 bytes) are
compressed with zstd (level 3 default) and the `COMPRESSED` flag is set in the
frame header. Compression applies after serialization, regardless of mode.

Implementations must support decompression. Compression is optional for
senders — a receiver must handle both compressed and uncompressed frames.

### 5.7 Fory IDL for Cross-Language Contracts

When cross-language interop is required, message types are defined in Fory IDL
(`.fdl` files) and compiled with `foryc` to generate idiomatic code per language.

```
// agent_control.fdl
package aster.kar.agent;

struct TaskAssignment {
    task_id: string;
    workflow_yaml: string;
    credential_refs: list<string>;
    step_budget: int32;
}

struct TaskAck {
    accepted: bool;
    reason: optional<string>;
}

struct StepUpdate {
    step_number: int32;
    tool_name: string;
    status: string;
    output: optional<string>;
    screenshot_region: optional<binary>;
}

enum RiskLevel {
    LOW = 0;
    MEDIUM = 1;
    HIGH = 2;
    CRITICAL = 3;
}

struct ApprovalRequest {
    step_number: int32;
    action_description: string;
    risk_level: RiskLevel;
}

struct ApprovalResponse {
    approved: bool;
    reason: optional<string>;
}
```

### 5.8 Service Contract IDL Extension

Aster extends Fory IDL with service definition blocks:

```
// agent_control.fdl (continued)

service AgentControl {
    version = 1;
    alpn = "aster/1";
    serialization = [xlang, native];    // ordered list of supported formats, producer preference order
                                        // (client picks first producer-listed mode it also supports — see §6.2.1)

    rpc assign_task(TaskAssignment) returns (TaskAck) {
        timeout = 30.0;
        idempotent = true;
    }

    rpc cancel_task(TaskId) returns (CancelAck);

    server_stream step_updates(TaskId) returns (stream StepUpdate);

    client_stream upload_artifacts(stream ArtifactChunk) returns (UploadResult);

    bidi_stream approval_loop(stream ApprovalRequest) returns (stream ApprovalResponse);
}
```

Code generation from service blocks is optional per language:

- **Full codegen:** Generate client stubs, server interfaces, and dispatch
  (useful for Go, Rust, Java)
- **Type-only codegen:** Generate message types, define services idiomatically
  in code (useful for Python, TypeScript)
- **No codegen:** Define everything in code, register types manually
  (useful for prototyping)

> **TODO:** Implement the `foryc` service block extension. Currently Fory IDL
> only defines data types. The service block syntax, parser, and per-language
> code generators are new work.

### 5.9 Large Payloads and Blob Capability Responses

Aster keeps RPC as the **control plane**. For small payloads, normal frame
payloads are sufficient. For larger payloads such as files, directories, model
artifacts, bundles, or other immutable content, a service may return a
**blob capability** instead of inlining bytes in the RPC response.

This is the normative semantic rule:

> A file response is not necessarily bytes; it may be a blob capability.

The service may choose one of two patterns:

1. **Bearer ticket:** a `sendme`-style ticket or equivalent opaque capability.
   Possession of the ticket is sufficient to fetch the content.
1. **Authenticated locator:** a non-public blob locator plus a short-lived auth
   token. The fetch still uses `iroh-blobs`, but the provider checks the token
   before serving content.

Recommended server behaviour for large immutable content:

1. Materialize or locate the file or directory.
1. Import it into an `iroh-blobs` store.
1. Ensure the endpoint or router is serving the blobs ALPN.
1. Mint either a bearer ticket or an authenticated `FileRef`.
1. Return that capability object as the RPC result.

This keeps Aster focused on negotiation, metadata, errors, and policy, while
letting Iroh’s native blob machinery handle verified transfer, resumability,
and large-object distribution.

#### 5.9.1 Example IDL for Blob Capability Responses

```
// transfer.fdl

enum FileTransferMode {
    INLINE_BYTES = 0;
    BLOB_TICKET = 1;
    AUTHENTICATED_BLOB_REF = 2;
}

struct FileRef {
    name: string;
    media_type: optional<string>;
    size_bytes: uint64;
    root_hash: binary;
    format: string;                    // e.g. raw, hashseq, collection
    provider_endpoint_id: optional<string>;
    relay_url: optional<string>;
    direct_addresses: list<string>;
    auth_token: optional<string>;
    expires_at_epoch_ms: optional<int64>;
}

struct FileResponse {
    mode: FileTransferMode;
    inline_bytes: optional<binary>;
    ticket: optional<string>;
    file_ref: optional<FileRef>;
}

struct FileRequest {
    path: string;
}

struct DirectoryRequest {
    path: string;
}

service ArtifactStore {
    version = 1;
    alpn = "aster/1";
    serialization = [xlang];

    rpc get_file(FileRequest) returns (FileResponse);
    rpc get_directory(DirectoryRequest) returns (FileResponse);
}
```

`FileResponse.mode` determines which field the receiver must inspect:

- `INLINE_BYTES` → use `inline_bytes`
- `BLOB_TICKET` → use `ticket`
- `AUTHENTICATED_BLOB_REF` → use `file_ref`

Applications may choose to support only one large-payload mode, but all
implementations should treat the capability-return pattern as first-class.

### 5.10 Sibling Tunnel Negotiation

Some RPCs do not return domain objects but instead negotiate a new data-plane
path on the same underlying Iroh connection. Aster standardizes this as
**sibling tunnel negotiation**. The control RPC remains an ordinary Aster
call; the transport path it establishes is separate.

#### 5.10.1 Tunnel Negotiation Rules

1. The initiating RPC runs on a normal Aster bidirectional stream.
1. The RPC response identifies a `tunnel_id` and the transport mode.
1. The existing RPC stream is never reused as the tunnel itself.
1. If `mode = stream`, peers bind the tunnel to a sibling bidirectional QUIC
   stream on the same connection.
1. If `mode = datagram`, peers bind the tunnel to connection datagrams and use
   `tunnel_id` in their application datagram envelope to demultiplex traffic.
1. If the requested mode is unavailable, the RPC fails or returns
   `accepted = false`. Silent fallback is discouraged.

#### 5.10.2 Example IDL for Tunnels

```
// tunneling.fdl

enum TunnelMode {
    STREAM = 0;
    DATAGRAM = 1;
}

struct OpenTunnelRequest {
    protocol: string;   // "tcp" | "udp"
    target_host: string;
    target_port: int32;
}

struct OpenTunnelResponse {
    accepted: bool;
    tunnel_id: string;
    mode: TunnelMode;
}

struct TunnelStreamPreamble {
    tunnel_id: string;
}

struct TunnelDatagram {
    tunnel_id: string;
    payload: binary;
}

service EdgeTransport {
    version = 1;
    alpn = "aster/1";
    serialization = [xlang];

    rpc open_tunnel(OpenTunnelRequest) returns (OpenTunnelResponse);
}
```

#### 5.10.3 Implementation Guidance

- **TCP-like forwarding:** request `protocol = "tcp"`, receive `mode = STREAM`,
  then bind raw byte forwarding to a sibling bidirectional stream. The side
  opening that sibling stream must send a `TunnelStreamPreamble` first so the
  receiver can bind the stream to the correct `tunnel_id` before treating the
  rest of the stream as raw bytes.
- **UDP-like forwarding:** request `protocol = "udp"`, receive `mode = DATAGRAM`,
  then exchange `TunnelDatagram` envelopes over QUIC datagrams.
- **Backpressure:** stream tunnels inherit QUIC stream backpressure; datagram
  tunnels are lossy and must define their own overflow policy.
- **Security:** authorization remains an application concern and should be
  enforced by the control RPC before the sibling path is activated.

-----

## 6. Wire Protocol

**This section is the interoperability contract.** All language implementations
must produce and consume bytes conforming to this specification.

### 6.1 Stream Framing

Every message on a QUIC stream is framed as:

```
┌─────────────┬──────────┬─────────────────────┐
│ Length       │ Flags    │ Payload             │
│ (4B LE u32) │ (1B)     │ (Length - 1 bytes)   │
└─────────────┴──────────┴─────────────────────┘
```

- **Length** (4 bytes, little-endian unsigned 32-bit): Total size of Flags +
  Payload. Maximum 16 MiB per frame. A Length of 0 is invalid.
- **Flags** (1 byte, bitfield):
  - Bit 0 (`0x01`): `COMPRESSED` — payload is zstd-compressed.
  - Bit 1 (`0x02`): `TRAILER` — trailing status frame (see §6.4).
  - Bit 2 (`0x04`): `HEADER` — stream header frame (first frame on stream).
  - Bit 3 (`0x08`): `ROW_SCHEMA` — payload is a Fory row schema (see §5.5.2).
    Valid only on the first data frame of a ROW-mode stream; must not be set on
    any other frame. Must not be combined with `HEADER` or `TRAILER`.
  - Bit 4 (`0x10`): `CALL` — per-call header within a session stream (see
    session-scoped services addendum). Must not appear on non-session streams.
  - Bit 5 (`0x20`): `CANCEL` — cancel the current in-flight call on a session
    stream (see session-scoped services addendum). Must not appear on
    non-session streams.
  - Bits 6–7: Reserved, must be zero.
- **Payload**: Serialized bytes (Fory or raw, depending on flags).

### 6.2 Stream Header

The first frame on every QUIC stream must have the `HEADER` flag set. Its
payload is always Fory XLANG-serialized (regardless of the service’s
serialization mode) so any language can route it:

```
StreamHeader {
    service: string             // e.g. "AgentControl"
    method: string              // e.g. "assign_task"
    version: int32              // Human-facing service version label
    contract_id: string         // BLAKE3 of canonical contract bytes; authoritative compatibility key
    call_id: string             // UUID, unique per call
    deadline_epoch_ms: int64    // Absolute deadline (ms since Unix epoch), 0 = none
    serialization_mode: uint8   // Selected mode for this call (see §6.2.1 for selection algorithm);
                                // MUST be supported by the advertised contract + endpoint lease
    metadata_keys: list<string>
    metadata_values: list<string>
}
```

The `version` field is a human-facing routing hint. `contract_id` is the
authoritative identity of the service contract and must match the published
canonical contract selected during registry resolution. Servers must reject the
call with `FAILED_PRECONDITION` if the referenced contract is unknown, disabled,
or incompatible with the selected method or serialization mode.

**Session-scoped streams:** When `method` is an empty string (`""`), the stream
is a session-scoped stream (see session-scoped services addendum).

**Server-side validation (normative):** the server MUST verify that the
`method` field on `StreamHeader` matches the discovered service's scope:

- If `method == ""` but the resolved service has `scoped=SHARED`, the server
  MUST reject the stream with `FAILED_PRECONDITION` (the client is attempting
  to open a session against a non-session service).
- If `method != ""` but the resolved service has `scoped=STREAM`, the server
  MUST reject with `FAILED_PRECONDITION` (the client is calling a session
  service as if it were stateless).

This validation closes the footgun where a client that forgets to set
`method` would otherwise silently attempt to open a session against a shared
service.

**session_id mapping (normative, see also addendum §8.2):** on a session
stream, `StreamHeader.call_id` is the **session identifier** for the life of
the stream. Each in-session CALL frame carries its own `call_id` in the
`CallHeader`. The framework populates `CallContext.session_id` from
`StreamHeader.call_id` (stable for the stream) and `CallContext.call_id`
from `CallHeader.call_id` (per-call). On stateless streams, `session_id` is
`None`.

The server
instantiates a per-stream service instance and enters a session loop. The
`call_id` field serves as the session identifier. Per-call method dispatch uses
`CALL` frames (bit 4) instead of the `StreamHeader.method` field.

`contract_id` is a BLAKE3 hash of canonical contract bytes. The canonical
encoding is normatively defined in **Aster-ContractIdentity.md §11.3**
("Canonical XLANG profile") — a stripped Fory XLANG byte stream with
specific rules for integer encoding, optional fields, list headers, and
discriminator zero-values. Both ends of a connection compute the same
`contract_id` by following that specification byte-for-byte.

#### 6.2.1 Serialization Mode Selection (normative)

The `serialization` list on a `ServiceContract` is **ordered by producer
preference**. The client selects a concrete `serialization_mode` for each
stream using this algorithm:

1. Let `producer_modes = contract.serialization` (producer's preference order,
   e.g. `[XLANG, NATIVE]`).
2. Let `client_modes` be the set of serialization modes the client supports.
3. Walk `producer_modes` in order. Select the first mode that is in
   `client_modes`.
4. If no mode is shared, fail the call with `FAILED_PRECONDITION` — the
   client and producer cannot agree on a serialization format.

Producer preference wins on ties. Clients MUST NOT reorder by their own
preference; this keeps routing deterministic and makes producer capacity
planning predictable (a producer that lists `[XLANG, NATIVE]` knows it will
be called with XLANG whenever the client supports it).

### 6.3 Stream Lifecycle per RPC Pattern

**Unary (`@rpc`):**

```
Client                          Server
  ├─ [HEADER] StreamHeader ────►
  ├─ Request payload ──────────►
  ├─ finish() ─────────────────►  (client done)
  │                               ├─ Response payload
  ◄───────────────────────────────┤
  │                               ├─ finish()
  ◄───────────────────────────────┤  (server done)
```

**Server stream (`@server_stream`):**

```
Client                          Server
  ├─ [HEADER] StreamHeader ────►
  ├─ Request payload ──────────►
  ├─ finish() ─────────────────►
  │                               ├─ Response frame 1
  ◄───────────────────────────────┤
  │                               ├─ Response frame N
  ◄───────────────────────────────┤
  │                               ├─ [TRAILER] RpcStatus
  ◄───────────────────────────────┤
  │                               ├─ finish()
  ◄───────────────────────────────┤
```

**Client stream (`@client_stream`):**

```
Client                          Server
  ├─ [HEADER] StreamHeader ────►
  ├─ Request frame 1 ──────────►
  ├─ Request frame N ──────────►
  ├─ finish() ─────────────────►
  │                               ├─ Response payload
  ◄───────────────────────────────┤
  │                               ├─ finish()
  ◄───────────────────────────────┤
```

**Bidi stream (`@bidi_stream`):**

```
Client                          Server
  ├─ [HEADER] StreamHeader ────►
  ├─ Request frame 1 ──────────►
  │                               ├─ Response frame 1
  ◄───────────────────────────────┤
  ├─ Request frame N ──────────►
  │                               ├─ Response frame N
  ◄───────────────────────────────┤
  ├─ finish() ─────────────────►
  │                               ├─ [TRAILER] RpcStatus
  ◄───────────────────────────────┤
  │                               ├─ finish()
  ◄───────────────────────────────┤
```

**Termination semantics:** A trailer frame (§6.4) before `finish()` indicates
clean completion with an explicit status. A `finish()` without a preceding
trailer on a streaming RPC implies `OK`. A stream reset (QUIC `RESET_STREAM`)
without a trailer indicates abnormal termination.

**Session-scoped streams (see Aster-session-scoped-services.md §4.5–§4.6):**
the rules above describe stateless (one-call-per-stream) RPC. Session
streams diverge in two ways:

1. **Client-origin TRAILER frames are permitted on session streams.** A
   session client sends a TRAILER(status=OK) frame to signal end-of-input on
   client-stream and bidi-stream calls inside the session (addendum §4.5
   rule 3). Implementations MUST accept client-written TRAILER frames on
   streams whose HEADER has `method == ""`. Stateless streams continue to
   reject client-origin TRAILER frames.
2. **In-session unary calls do NOT emit a trailer on success.** The response
   payload frame alone signals success; errors use a trailer with non-OK
   status instead of a response payload (addendum §4.6). This diverges from
   stateless unary, which always emits a trailer.

Stateless parsers that cannot reach a session dispatch path may reject
client-origin TRAILER frames; session-aware parsers MUST accept them once
`StreamHeader.method == ""` has been observed.

### 6.4 Status / Trailer Frame

The trailer payload is always Fory XLANG-serialized:

```
RpcStatus {
    code: int32                 // StatusCode enum value
    message: string
    detail_keys: list<string>
    detail_values: list<string>
}
```

### 6.4A Sibling Channel Negotiation

Sibling channels are negotiated by RPC semantics, not by any separate transport
handshake in the Aster wire layer. The wire-level requirements are:

1. The initiating control call must still conform to the normal Aster framing
   rules in this section.
1. The control call’s response payload may instruct the peers to use a sibling
   stream or connection datagrams identified by application metadata such as a
   `tunnel_id`.
1. The initiating Aster stream must then terminate normally; it must not be
   repurposed into a raw data stream after the response is sent.
1. If the sibling channel cannot be opened after a successful control response,
   the application must surface that failure explicitly rather than silently
   continuing on the original RPC stream.

This keeps the Aster wire contract simple while still allowing richer transport
patterns to be built above it.

### 6.5 Status Codes

```
OK                    = 0
CANCELLED             = 1
UNKNOWN               = 2
INVALID_ARGUMENT      = 3
DEADLINE_EXCEEDED     = 4
NOT_FOUND             = 5
ALREADY_EXISTS        = 6
PERMISSION_DENIED     = 7
RESOURCE_EXHAUSTED    = 8
FAILED_PRECONDITION   = 9
ABORTED               = 10
OUT_OF_RANGE          = 11
UNIMPLEMENTED         = 12
INTERNAL              = 13
UNAVAILABLE           = 14
DATA_LOSS             = 15
UNAUTHENTICATED       = 16
```

Codes 0–16 are semantically identical to gRPC status codes. Application-
specific codes use 100+ and are opaque to the framework.

Application-tier codes let services express domain failures that the framework
should not interpret. The framework forwards these codes faithfully in trailers
and surfaces them to the caller, but never acts on them — no retry, no circuit
breaker trip, no special logging. Examples:

```
APPROVAL_DENIED       = 100   // Human reviewer rejected an agent action
BUDGET_EXCEEDED       = 101   // Step or token budget exhausted for this task
CREDENTIAL_EXPIRED    = 102   // Upstream credential no longer valid
POLICY_VIOLATION      = 103   // Action blocked by a safety or compliance rule
RATE_LIMITED_UPSTREAM  = 104   // Downstream API returned a rate limit
```

Services should document their application codes in the service contract. Codes
below 100 are reserved for the framework and must not be used by application
logic.

### 6.6 ALPN

Aster uses a **single ALPN identifier per wire protocol version**:

```
aster/1
```

The version component (`1`) identifies the wire protocol version. All services
share this ALPN; service identity and version are carried entirely in the
`StreamHeader` (§6.2). Per-service ALPN filtering at the connection level is
intentionally not supported.

**Rationale.** Per-service ALPNs introduce an ambiguity between wire protocol
version and service version, and make wire protocol evolution painful — a
wire-level breaking change would require every service’s ALPN to be bumped
simultaneously. A single shared ALPN avoids this: a wire protocol change
increments `aster/N` once, and a v1 server rejects a v2 connection cleanly at
the QUIC handshake before any application-layer parsing occurs. This is a
cleaner rejection boundary than a version byte in the header. Additionally, in
NAT-traversal and topology-agnostic dial-by-key scenarios the connecting peer
often does not know which specific service it needs at connection time — making
connection-level service filtering impractical regardless.

When a future wire protocol version is introduced, it is deployed as `aster/2`.
A server endpoint may accept both `aster/1` and `aster/2` during a transition
period by advertising both ALPNs in `EndpointConfig.alpns`.

### 6.7 Streaming Error Recovery

Aster does not define application-layer streaming recovery. Mid-stream
failures are handled entirely at the QUIC transport layer.

**Observable behaviour on failure:**

When a server crashes, closes unexpectedly, or resets a stream during a
streaming RPC (`@server_stream`, `@client_stream`, `@bidi_stream`), the QUIC
transport delivers a `RESET_STREAM` frame to the remote peer. The framework
maps this to `UNAVAILABLE`.

When a connection is lost entirely, the QUIC connection close propagates to all
open streams on that connection. Each stream surfaces as `UNAVAILABLE`.

**Framework behaviour on receipt of `RESET_STREAM`:**

The receiving side terminates the stream immediately and surfaces the error to
the application. No partial result is delivered. No silent retry is attempted
by the framework.

**Retry responsibility:**

Recovery is the caller’s responsibility, governed by the call’s `retry_policy`:

- **Idempotent methods** (`idempotent=True`) may be retried automatically by
  the `RetryInterceptor` according to their configured policy.
- **Non-idempotent methods** must never be auto-retried by the framework.
  Surfacing `UNAVAILABLE` to the caller is the correct and complete framework
  response.

**Rationale.** QUIC stream reset is a well-defined, reliable signal. Attempting
application-layer recovery above this boundary would require the framework to
reason about partial delivery, stream state reconstruction, and idempotency
semantics that only the application can know.

### 6.8 Deadline Semantics

#### 6.8.1 Per-Call Deadlines

Each RPC call carries an optional absolute deadline in the `StreamHeader`:

```
deadline_epoch_ms: int64    // Absolute deadline in milliseconds since Unix epoch.
                            // 0 = no deadline imposed by this caller.
```

Using an absolute epoch rather than a relative duration means the value is
unambiguous regardless of when it is read — no clock-at-send context is needed
to interpret it.

The `DeadlineInterceptor` enforces the deadline locally on receipt:

1. If `deadline_epoch_ms = 0`, no deadline is enforced unless the service has
   a local default configured.
1. If `now_ms >= deadline_epoch_ms` on receipt, the call is rejected
   immediately with `DEADLINE_EXCEEDED` before the handler is invoked.
1. If the deadline expires while the handler is executing, the handler is
   cancelled and the stream is reset with `DEADLINE_EXCEEDED`.
1. A configurable clock skew tolerance (default: 1000ms) is applied on
   receipt. A deadline is treated as still valid if
   `now_ms < deadline_epoch_ms + skew_tolerance_ms`.

#### 6.8.2 No Framework-Level Deadline Propagation

Aster does **not** propagate deadlines automatically across service chains.
Each call’s `deadline_epoch_ms` is set independently by whoever opens that
stream. This works because cancellation propagates naturally through the stream
lifecycle: when service A’s deadline expires, A’s stream is reset and A’s
handler is cancelled; any downstream stream A held to service B is orphaned,
and B observes a `RESET_STREAM` that surfaces as `CANCELLED`. Handlers must
cancel their own downstream calls when their context is cancelled — this is
standard structured concurrency (`asyncio.TaskGroup` in Python,
`tokio::select!` in Rust, `context.Context` in Go). If a tighter downstream
deadline is desired, the caller sets a shorter `deadline_epoch_ms` on the
downstream call explicitly.

-----

## 7. Service Definition Layer

### 7.1 Python (Phase 1 — Exemplar)

```python
from aster import service, rpc, server_stream, client_stream, bidi_stream
from aster import SerializationMode
from dataclasses import dataclass
from typing import AsyncIterator

@dataclass
class TaskAssignment:
    task_id: str
    workflow_yaml: str
    credential_refs: list[str]
    step_budget: int

@dataclass
class TaskAck:
    accepted: bool
    reason: str | None = None

@service(name="AgentControl", version=1, serialization=[SerializationMode.XLANG, SerializationMode.NATIVE])
class AgentControlService:

    @rpc(timeout=30.0, idempotent=True)
    async def assign_task(self, req: TaskAssignment) -> TaskAck: ...

    @rpc
    async def cancel_task(self, req: TaskId) -> CancelAck: ...

    @server_stream
    async def step_updates(self, req: TaskId) -> AsyncIterator[StepUpdate]: ...

    @client_stream
    async def upload_artifacts(self, stream: AsyncIterator[ArtifactChunk]) -> UploadResult: ...

    @bidi_stream
    async def approval_loop(
        self, requests: AsyncIterator[ApprovalRequest]
    ) -> AsyncIterator[ApprovalResponse]: ...
```

### 7.2 Decorator Semantics

|Decorator       |Client sends      |Server returns    |QUIC mapping                        |
|----------------|------------------|------------------|------------------------------------|
|`@rpc`          |Single message    |Single message    |`open_bi()`: send → recv → close    |
|`@server_stream`|Single message    |`AsyncIterator[T]`|`open_bi()`: send → recv N → trailer|
|`@client_stream`|`AsyncIterator[T]`|Single message    |`open_bi()`: send N → recv          |
|`@bidi_stream`  |`AsyncIterator[T]`|`AsyncIterator[T]`|`open_bi()`: concurrent read/write  |

### 7.3 Decorator Options

```python
@rpc(
    timeout=30.0,               # Default deadline (seconds), overridable per-call
    idempotent=True,            # Safe to retry on transport failure
    serialization=None,         # Override service-level serialization mode
    retry_policy=RetryPolicy(
        max_attempts=3,
        backoff=ExponentialBackoff(initial=0.1, maximum=2.0),
    ),
)
```

### 7.4 Service Options

```python
@service(
    name="AgentControl",                    # Wire name (stable across refactors)
    version=1,                              # Service version
    serialization=[SerializationMode.XLANG, SerializationMode.NATIVE],  # Ordered supported formats
    alpn=b"aster/1",                         # ALPN protocol identifier (always aster/{wire_version})
    max_concurrent_streams=64,              # Per-connection stream limit
    interceptors=[AuthInterceptor, AuditLogInterceptor],
)
```

### 7.5 Future Language Definitions (TODO)

Each language will use idiomatic patterns:

- **Java/Kotlin:** `@AsterService`, `@Rpc`, `@ServerStream` annotations
- **C#/.NET:** `[AsterService]`, `[Rpc]`, `[ServerStream]` attributes
- **Go:** Interface conventions with comment-based tags
- **Rust:** `#[aster::service]`, `#[rpc]` proc macros
- **TypeScript:** `@AsterService`, `@Rpc` decorators

> **TODO:** Write full language-specific service definition examples for each
> target language once Python exemplar is stable.

### 7.6 Language Ownership of Service Definitions

Service definitions are intentionally **language-owned**. This specification
defines the wire behaviour that service definitions must produce, but it does
not require a single shared API surface across languages.

Examples:

- Python may use decorators and type hints
- Java/Kotlin may use annotations, interfaces, and generated stubs
- Go may use interfaces, helper structs, and code generation
- Rust may use traits, procedural macros, and strongly typed builders

The responsibility of this layer is to project the common Aster wire protocol
into idiomatic language constructs. Different languages may choose different
surface syntax so long as they preserve identical wire behaviour.

-----

## 8. Client and Server APIs

### 8.1 Server (Python)

```python
from aster import Server

class AgentControlImpl(AgentControlService):
    async def assign_task(self, req: TaskAssignment) -> TaskAck:
        if req.step_budget > 500:
            return TaskAck(accepted=False, reason="Step budget too high")
        return TaskAck(accepted=True)

    async def step_updates(self, req: TaskId) -> AsyncIterator[StepUpdate]:
        async for update in self.agent_loop.run(req):
            yield update

server = Server(
    endpoint=endpoint,
    services=[AgentControlImpl()],
)
await server.serve()
```

**Server accept loop:**

1. `endpoint.accept()` → new `Connection`
1. Per connection: loop on `connection.accept_bi()`
1. Per stream: read `StreamHeader` (first frame, `HEADER` flag)
1. Dispatch to handler by `(service, method, version)`
1. Execute handler, write response frames
1. Send trailer (if streaming), call `finish()`

### 8.2 Client (Python)

```python
from aster import create_client

# Remote client (over Iroh)
client = await create_client(AgentControlService, connection=conn)

# Unary
ack = await client.assign_task(task, timeout=10.0, metadata={"trace_id": "abc"})

# Server stream
async for update in client.step_updates(TaskId(task_id="t1")):
    print(f"Step {update.step_number}: {update.status}")

# Bidi stream
async with client.approval_loop() as (send, recv):
    await send(ApprovalRequest(...))
    response = await recv()
```

### 8.3 Local Client (In-Process)

Same API as the remote client, no serialization, no network. Uses `asyncio.Queue`
internally for streaming patterns.

```python
local_client = create_local_client(AgentControlService, implementation=impl)
ack = await local_client.assign_task(task)  # Zero-copy, in-memory
```

#### 8.3.1 Transport Protocol Abstraction

Both remote and local clients are backed by a `Transport` implementation. The
client stub is transport-unaware — the same generated or decorator-derived stub
class works identically regardless of backing. `create_client` and
`create_local_client` are factories that produce different `Transport`
implementations:

```python
# transport/base.py
from typing import Protocol, AsyncIterator, Any

class Transport(Protocol):
    """
    Structural protocol implemented by IrohTransport and LocalTransport.
    Client stubs hold a Transport reference and call these methods directly.
    """

    async def unary(
        self,
        service: str,
        method: str,
        request: Any,
        metadata: dict[str, str] = {},
        deadline_epoch_ms: int = 0,
    ) -> Any: ...

    def server_stream(
        self,
        service: str,
        method: str,
        request: Any,
        metadata: dict[str, str] = {},
        deadline_epoch_ms: int = 0,
    ) -> AsyncIterator[Any]: ...

    async def client_stream(
        self,
        service: str,
        method: str,
        requests: AsyncIterator[Any],
        metadata: dict[str, str] = {},
        deadline_epoch_ms: int = 0,
    ) -> Any: ...

    def bidi_stream(
        self,
        service: str,
        method: str,
        metadata: dict[str, str] = {},
        deadline_epoch_ms: int = 0,
    ) -> "BidiChannel": ...
```

The practical consequence: whether a tool-calling agent loop runs its tool
services in-process or out-of-process is a deployment-time decision, not a
code-time decision. The agent code does not change.

#### 8.3.2 LocalTransport Interceptor Behaviour

Interceptors run on `LocalTransport` just as they do on `IrohTransport`. This
is not optional. The interceptor chain is part of the service contract, not the
transport — an `AuthInterceptor` validating metadata tokens must fire whether
the call crosses a network boundary or a module boundary. Skipping interceptors
on local transport creates security gaps that only manifest in production.

##### Trust model and `CallContext.peer`

LocalTransport runs in a single process, with caller and callee in the same
trust domain. Consequently:

- **There is no remote peer.** `CallContext.peer` is `None` on every in-process
  call. It is not a placeholder or synthesized identity — the absence of a
  peer is part of the data model.
- **No admission gate applies.** Gate 0 (connection-level admission via iroh
  `EndpointHooks`, see Aster-trust-spec.md §3.3) is a hook on QUIC/iroh
  connections, not on in-process function calls. LocalTransport bypasses
  Gate 0 entirely because there is no connection to gate.
- **`CallContext.attributes` is empty (`{}`)** unless explicitly populated
  by a test harness. No credential presentation occurs on LocalTransport,
  so no verified attributes exist.
- **Interceptors MUST handle `peer is None` gracefully.** The canonical
  behavior is "trust the in-process caller" — e.g. an `AuthInterceptor`
  that normally validates a bearer token against `peer`'s expected identity
  should return "allow" when `peer is None` rather than raising
  `UNAUTHENTICATED`. Interceptors that genuinely require an authenticated
  remote identity (rare) must document this and either fail fast with a
  clear error message or be excluded from local chains by the test harness.
- **Test harnesses MAY synthesize a `peer`** (e.g. `peer="test://alice"`)
  to drive auth-interceptor test scenarios. This is a test-scoped feature;
  production code paths must not rely on synthesized peers.

In short: LocalTransport is appropriate for trusted in-process composition
(embedded agents, unit tests, integration tests). It is not an
authentication boundary.

#### 8.3.3 Wire-Compatible Mode

`LocalTransport` accepts a `wire_compatible: bool` flag (default: `False`).
When `True`, the local transport runs a full Fory serialize → deserialize
roundtrip before dispatching the request and again before returning the
response. This catches type registration errors, missing `@fory_type` tag
declarations, and schema mismatch bugs that would otherwise only surface in
cross-language calls.

In production, `wire_compatible=False` provides zero-copy in-memory object
passing. In the conformance test harness (`testing/harness.py`),
`wire_compatible=True` is the default so that local tests exercise the same
serialization paths as remote ones.

```python
# Production: zero-copy, no serialize/deserialize overhead
local_client = create_local_client(AgentControlService, implementation=impl)

# Test harness: full wire roundtrip, catches schema errors early
test_client = create_local_client(
    AgentControlService,
    implementation=impl,
    wire_compatible=True,
)
```

-----

## 9. Interceptors and Middleware

### 9.1 Interceptor Interface (Python)

```python
class CallContext:
    service: str
    method: str
    call_id: str
    session_id: str | None      # Non-None for session-scoped calls
    peer: EndpointId | None     # Remote endpoint identity (always set for remote calls)
    metadata: dict[str, str]
    deadline: float | None
    is_streaming: bool

class Interceptor:
    async def on_request(self, ctx: CallContext, request: object) -> object:
        return request

    async def on_response(self, ctx: CallContext, response: object) -> object:
        return response

    async def on_error(self, ctx: CallContext, error: RpcError) -> RpcError | None:
        return error
```

### 9.2 Standard Interceptors

|Interceptor                |Purpose                                           |Status|
|---------------------------|--------------------------------------------------|------|
|`AuthInterceptor`          |Inject/validate auth tokens in metadata           |Implemented |
|`DeadlineInterceptor`      |Enforce per-call deadlines (see §6.8)             |Implemented |
|`AuditLogInterceptor`      |Log calls for replay/audit                        |Implemented |
|`MetricsInterceptor`       |Call count, latency, error rate (OTel)            |Implemented |
|`RetryInterceptor`         |Auto-retry idempotent RPCs on transient failures  |Implemented |
|`CompressionInterceptor`   |Override per-call compression settings            |TODO  |
|`CircuitBreakerInterceptor`|Open circuit on sustained failure; prevent cascade|Implemented |

**`CircuitBreakerInterceptor` state machine:**

```
CLOSED ──(failure threshold reached)──► OPEN
  ▲                                       │
  │                                       │ (timeout elapsed)
  │                                       ▼
  └──(probe succeeds)──────────── HALF-OPEN
                                          │
                                          └──(probe fails)──► OPEN
```

- **CLOSED:** calls pass through normally; failures are counted.
- **OPEN:** calls are rejected immediately with `UNAVAILABLE`; no downstream
  attempt is made.
- **HALF-OPEN:** a single probe call is permitted; success closes the circuit,
  failure re-opens it.

Threshold, timeout, and probe policy are configurable. This interceptor is
complementary to `RetryInterceptor`: retry handles transient single-call
failures; circuit breaker handles sustained degradation of a downstream service.

-----

## 10. Connection Lifecycle

### 10.1 Bootstrap Flow

```
1. Service starts → Iroh endpoint binds → EndpointId derived from secret key
2. Service joins registry namespace → publishes EndpointLease
3. Client resolves EndpointId via registry (see §11.8)
4. Client dials by EndpointId (Iroh handles hole-punch / relay fallback)
5. QUIC connection established, ALPN aster/1 negotiated
6. Client creates typed stub → RPCs begin
```

**EndpointId stability.** Iroh’s `EndpointId` is the public key derived from
the endpoint’s keypair. It is identical to Iroh’s `NodeId`. An endpoint started
with the same secret key always produces the same `EndpointId` regardless of
host, IP address, network location, or restart. Stable identity is achieved by
supplying a stable secret key via `EndpointConfig.secret_key`.

Key material may be injected by any mechanism appropriate to the deployment:
environment variable, secret store (Vault, AWS Secrets Manager, Kubernetes
secret), or configuration file. The framework does not prescribe the injection
mechanism — it is an operational concern. What the framework requires is that
production deployments treat the secret key with the same care as a TLS private
key: generate once, store securely, rotate deliberately.

This means that in container-based deployments, a pod restart or migration does
not change the service’s identity. Clients that dialled by `EndpointId` before
the restart can reconnect to the same identity without reconfiguration. This is
a stronger operational guarantee than hostname-based identity, which changes
whenever a pod is rescheduled to a different node or IP.

### 10.2 Connection Health and Idle Timeout

QUIC-level health monitoring via `connection.closed()`. QUIC’s built-in
keep-alive mechanism (periodic PING frames) is sufficient to hold connections
open during idle periods and across NAT rebinding events. Iroh enables QUIC
keep-alive by default; no application-level heartbeat RPC is required for
connection liveness. Long-lived streaming RPCs (`@server_stream`,
`@bidi_stream`) remain alive as long as the underlying QUIC connection is
alive — an open stream with no data in flight is not considered idle by QUIC,
so idle timeout does not apply to streams that are merely waiting for the next
application message.

### 10.3 Graceful Shutdown

```python
await server.drain(grace_period=10.0)
# 1. Stop accepting new connections
# 2. Stop accepting new streams on existing connections
# 3. Wait for in-flight RPCs to complete (up to grace_period)
# 4. Cancel remaining handlers
# 5. Close all connections
# 6. Close endpoint
```

-----

## 11. Service Registry and Discovery

The registry is a decentralised control plane built entirely on Iroh’s existing
primitives. The RPC invocation path remains direct stream-per-RPC over QUIC;
registry state is used to publish contracts, advertise live endpoints, and
resolve compatibility without requiring infrastructure servers.

**Design rule:** `iroh-docs` is the authoritative, eventually-consistent source
of mutable registry state (aliases, leases, ACLs, channel pointers). `iroh-gossip`
carries low-latency notifications and hints. Consumers always reconcile against
docs before acting on gossip. `iroh-blobs` stores and transfers immutable
contract artifacts as **Iroh collections** (HashSeq format with built-in
`CollectionMeta` naming). Automerge is optional for higher-layer collaborative
state, but is not the canonical storage format for the registry itself.

**Separation of concerns:** Docs owns mutability. Blobs collections own
immutability. A contract is published as an immutable Iroh collection bundle;
docs stores lightweight pointers (collection root hash, optional provider
metadata) that resolve to those bundles. This avoids simulating a filesystem
hierarchy in docs keys for artifact storage, reduces read amplification, and
aligns with Iroh's native content-addressed transfer primitives.

### 11.1 Iroh Primitives Used

|Primitive               |Role                                                                                                                                 |
|------------------------|-------------------------------------------------------------------------------------------------------------------------------------|
|**iroh-docs**           |Authoritative, eventually-consistent CRDT KV store. Stores immutable contract manifests and mutable service/channel/endpoint records.|
|**iroh-gossip**         |Real-time pub-sub hints. Announces contract publication, channel updates, endpoint lease changes, and health alerts.                 |
|**iroh-blobs**          |Content-addressed blob transfer (BLAKE3). Transfers large schema artifacts, documentation bundles, and compatibility reports.        |
|**Automerge (optional)**|Higher-layer shared application state. Not required for contract publication, service discovery, or endpoint leasing.                |

### 11.2 Registry Data Model and Namespace Structure

The registry separates **immutable contract artifacts** (stored as Iroh Blobs
collections) from **mutable service aliases, pointers, and endpoint leases**
(stored as iroh-docs entries).

Immutable contract bundles live in `iroh-blobs` as **Iroh collections**
(HashSeq format). Each collection uses Iroh's native `CollectionMeta` to name
its members — no custom packaging format is needed. Docs stores lightweight
`ArtifactRef` pointers that resolve to collection root hashes.

```text
{namespace}/
├── _aster/
│   ├── acl/
│   │   ├── writers                              → list[AuthorId]
│   │   ├── readers                              → list[AuthorId]
│   │   ├── admins                               → list[AuthorId]
│   │   └── policy                               → RegistryPolicy config
│   └── config/
│       ├── gossip_topic                         → TopicId for change notifications
│       ├── lease_duration_s                     → int (default: 45)
│       └── lease_refresh_interval_s             → int (default: 15)
│
├── contracts/
│   └── {contract_id}                            → ArtifactRef JSON (see §11.2.1)
│
├── services/
│   ├── {service_name}/
│   │   ├── versions/
│   │   │   └── v{version}                       → contract_id
│   │   ├── channels/
│   │   │   ├── stable                           → contract_id
│   │   │   ├── canary                           → contract_id
│   │   │   └── dev                              → contract_id
│   │   ├── meta                                 → service metadata JSON
│   │   └── contracts/
│   │       └── {contract_id}/
│   │           └── endpoints/
│   │               ├── {endpoint_id_hex}        → EndpointLease JSON
│   │               └── ...
│   └── {another_service}/
│       └── ...
│
├── endpoints/
│   └── {endpoint_id_hex}/
│       ├── meta                                 → optional static endpoint metadata
│       └── tags                                 → optional discovery tags
│
└── compatibility/
    └── {contract_id}/
        └── {other_contract_id}                  → Compatibility report / diff
```

All entries are signed by their author's keypair. The `AuthorId` on each entry
is the cryptographic proof of who wrote it.

#### 11.2.1 ArtifactRef — Docs Pointer to a Collection Bundle

Each `contracts/{contract_id}` docs entry stores a small JSON `ArtifactRef`
that points to the immutable Iroh collection containing the contract artifacts:

```text
ArtifactRef {
    contract_id: string              // hex-encoded BLAKE3 of ServiceContract
    collection_hash: string          // hex-encoded BLAKE3 root hash of the Iroh collection
    provider_endpoint_id: string?    // optional: endpoint serving the blobs ALPN
    relay_url: string?               // optional: relay for the provider
    ticket: string?                  // optional: bearer blob ticket for direct fetch
    published_by: AuthorId
    published_at_epoch_ms: int64
}
```

The `collection_hash` is the root hash of an Iroh collection (HashSeq format).
The collection's `CollectionMeta` names its members. Consumers fetch the
collection via `iroh-blobs` using the root hash or ticket.

#### 11.2.2 Contract Collection Layout

A contract is published as an Iroh collection with the following named members:

| Collection member name     | Content                                       | Required |
|---------------------------|-----------------------------------------------|----------|
| `contract.xlang`          | Canonical XLANG bytes of `ServiceContract`    | Yes      |
| `manifest.json`           | `ContractManifest` JSON (see §11.4)           | Yes      |
| `types/{type_hash}.xlang` | Canonical XLANG bytes of each `TypeDef`       | Yes      |
| `schema.fdl`              | Human-readable Fory IDL source text           | No       |
| `docs/`                   | Documentation bundle                          | No       |
| `compatibility/{other_id}`| Compatibility report vs another contract      | No       |

The collection member names are carried by Iroh's native `CollectionMeta`
(the `names: Vec<String>` field in the collection metadata blob). No custom
metadata format is needed — the names are the layout.

**Identity rule:** The `contract_id` is derived from the canonical
`ServiceContract` bytes (the `contract.xlang` member), not from the collection
root hash. The collection root hash identifies the *bundle*; the `contract_id`
identifies the *contract*. Two bundles with different optional members (e.g.
one includes `schema.fdl`, the other does not) may share the same
`contract_id` if their `contract.xlang` bytes are identical.

**Verification:** After fetching a contract collection, consumers must verify
that `blake3(contract.xlang bytes) == contract_id` before trusting the bundle.

#### 11.2.3 Trusted-Author Filtering on Docs Reads

Because iroh-docs is multi-author (multiple authors can write to the same key),
consumers must filter docs reads by trusted `AuthorId` before accepting values.

When reading any mutable registry entry (alias, lease, channel pointer, ACL),
consumers must:

1. Query the key across all authors (e.g. via `query_key_exact`).
2. Filter results to entries written by `AuthorId`s in the appropriate ACL
   tier (`_aster/acl/writers` for service entries, `_aster/acl/admins` for
   ACL entries).
3. Among trusted entries, select the one with the highest `lease_seq`. If
   two trusted entries share the same `lease_seq` (possible when multiple
   writers are authorized for the same endpoint key), break the tie by
   comparing `(lease_seq, updated_at_epoch_ms, AuthorId_hex)` lexicographically
   and taking the largest. For non-lease keys (e.g. `services/{name}/meta`),
   use `updated_at_epoch_ms` then `AuthorId_hex` as the tiebreak.

This prevents untrusted authors from poisoning the registry by writing to
well-known keys. The ACL tiers are cached locally and refreshed on gossip
`ACL_CHANGED` events.

### 11.3 Contract Canonicalization and Identity

Every published service contract must be reduced to a deterministic canonical
form before publication. The canonical bytes are hashed with BLAKE3:

```text
contract_id = blake3(canonical_contract_bytes)
```

`contract_id` is the authoritative identity of the contract. Service versions,
channel names, and human-friendly labels are aliases that resolve to a
`contract_id`.

Canonicalization rules:

1. Parse the source contract into a language-neutral contract model.
1. Remove comments and non-semantic formatting.
1. Normalize identifiers (method names, type names, package names, enum/union
   member names, role names) to Unicode NFC. Identifiers MUST conform to
   UAX #31 (`XID_Start` + `XID_Continue` — same rule as Python/Java/Rust
   identifiers).
1. Sort services, methods, fields, enums, and option keys by Unicode
   codepoint on the NFC-normalized name.
1. Emit canonical contract bytes in a deterministic form.
1. Hash the canonical bytes with BLAKE3 and encode as lowercase hex.

The **canonical encoding is normatively defined in Aster-ContractIdentity.md
§11.3** ("Canonical XLANG profile"), including the discriminator enum IDs,
the byte-level encoding rules (ZigZag VARINT for int32/int64, NULL_FLAG for
absent optional fields, list header `0x0C`, UTF-8 strings with length-prefix
varints), zero-value conventions for unused discriminator companion fields,
and the sort order for fields, methods, enum values, and union variants.
Golden byte vectors are published in Appendix A of that document.

### 11.4 Contract Publication

A published contract is immutable. Publication creates an Iroh collection
bundle containing the contract artifacts and writes an `ArtifactRef` pointer
into docs. Re-publishing the same canonical bytes is idempotent — the
`contract_id` (BLAKE3 of canonical `ServiceContract` bytes) guarantees
identity.

**Publication procedure:**

1. Resolve the type graph from the service definition (decorators, IDL, or
   code-first annotations).
2. For each type in the closure, serialize a `TypeDef` to canonical XLANG
   bytes.
3. Serialize the `ServiceContract` to canonical XLANG bytes. Compute
   `contract_id = hex(blake3(bytes))`.
4. Build an Iroh collection with the layout defined in §11.2.2:
   - `contract.xlang` → canonical `ServiceContract` bytes
   - `manifest.json` → `ContractManifest` JSON (see below)
   - `types/{type_hash}.xlang` → canonical `TypeDef` bytes for each type
   - Optionally: `schema.fdl`, documentation bundle, compatibility reports
5. Import the collection into the local `iroh-blobs` store. The collection
   root hash is the BLAKE3 of the HashSeq (computed automatically by Iroh).
6. Write an `ArtifactRef` to `contracts/{contract_id}` in the registry
   namespace docs (see §11.2.1). If the key already exists with matching
   `contract_id`, the write is idempotent.
7. Write or confirm the version pointer at
   `services/{name}/versions/v{version}` → `contract_id`.
8. Optionally update channel aliases
   (`services/{name}/channels/{channel}` → `contract_id`).
9. Broadcast `CONTRACT_PUBLISHED` on gossip.

```text
ContractManifest {
    service: string
    version: int32
    contract_id: string              // hex-encoded BLAKE3 of ServiceContract
    canonical_encoding: string       // e.g. "fory-xlang/0.15" (pinned Fory wire version)
    type_count: int32                // number of distinct types in closure
    type_hashes: list<string>        // all TypeDef hashes (transitive closure)
    method_count: int32
    serialization_modes: list<string>   // ordered by producer preference
    alpn: string
    deprecated: bool
    published_by: AuthorId
    published_at_epoch_ms: int64
}
```

The `type_hashes` field allows a consumer to verify the type closure without
walking the Merkle DAG. The authoritative type graph is encoded in the
`TypeDef` references themselves; `type_hashes` is an optimisation for
prefetching and integrity checking.

**Fetching a contract:** A consumer that knows a `contract_id` reads the
`ArtifactRef` from `contracts/{contract_id}` in docs, fetches the Iroh
collection via `iroh-blobs` using the `collection_hash` (or `ticket`),
verifies `blake3(contract.xlang) == contract_id`, and loads the type closure
from the `types/` members.

### 11.5 Service Aliases, Versions, and Channels

Service names remain the human-facing discovery key, but resolution is always a
two-step process:

1. Resolve `{service_name}` plus either `v{version}` or a channel (`stable`,
   `canary`, `dev`, etc.) to a `contract_id`.
1. Resolve live endpoint leases advertising that `contract_id`.

This allows semantic version labels and rollout channels to move without
changing the identity of the underlying contract artifact.

Rules:

- `services/{service}/versions/v{version}` is append-only once published.
- `services/{service}/channels/{channel}` is mutable and may be repointed.
- Consumers must prefer an exact `contract_id` match when one is already known.
- Channel aliases are advisory routing choices, not compatibility proofs.

### 11.6 Endpoint Leases and Capability Manifests

Live endpoint advertisement is modelled as a renewable lease, not a bare
heartbeat timestamp.

```text
EndpointLease {
    endpoint_id: string
    contract_id: string
    service: string
    version: int32
    lease_expires_epoch_ms: int64
    lease_seq: int64                    // monotonically increasing per endpoint/service/contract
    alpn: string
    serialization_modes: list<string>   // supported by this endpoint for this contract
    feature_flags: list<string>
    relay_url: string?
    direct_addrs: list<string>
    load: float?                        // 0.0 - 1.0 if exposed
    language_runtime: string?
    aster_version: string
    policy_realm: string?
    health_status: string?              // see health status values below
    tags: list<string>
    updated_at_epoch_ms: int64
}
```

**Health status values and routing behaviour:**

|Value     |Meaning                      |Consumer routing behaviour                                 |
|----------|-----------------------------|-----------------------------------------------------------|
|`ready`   |Fully operational            |Eligible for new calls                                     |
|`degraded`|Operational but impaired     |Eligible for new calls; prefer other endpoints if available|
|`draining`|Graceful shutdown in progress|Do not send new calls; finish in-flight calls              |
|`starting`|Initialising, not yet ready  |Do not send calls                                          |

A consumer must not send new streams to an endpoint whose `health_status` is
`draining` or `starting`. A consumer may send new streams to a `degraded`
endpoint but should prefer `ready` endpoints when available. If no `ready`
endpoints exist for a required contract, `degraded` endpoints are acceptable
fallbacks.

Lease semantics:

- A lease is considered live until `lease_expires_epoch_ms` plus skew tolerance.
- Publishers refresh the lease every `lease_refresh_interval_s`.
- `lease_seq` must increase monotonically so consumers can ignore stale writes.
- Explicit shutdown sets `health_status = draining`, waits for in-flight calls
  to complete (up to `grace_period`), then deletes the lease entry and emits
  an `ENDPOINT_DOWN` hint.
- Consumers must treat docs state as authoritative if gossip is missed.

### 11.7 Gossip Events

Events are broadcast on the namespace’s gossip topic. Payloads are small
notifications and hints; consumers sync full data from `iroh-docs` before
changing routing decisions.

```text
GossipEvent {
    type: enum {
        CONTRACT_PUBLISHED,
        CHANNEL_UPDATED,
        ENDPOINT_LEASE_UPSERTED,
        ENDPOINT_DOWN,
        ACL_CHANGED,
        COMPATIBILITY_PUBLISHED,
    }
    service: string?
    version: int32?
    channel: string?
    contract_id: string?
    endpoint_id: EndpointId?
    timestamp_ms: int64
}
```

### 11.8 Publication and Resolution Flows

**Contract publication:** The framework canonicalizes the contract, computes
`contract_id = blake3(canonical_contract_bytes)`, and publishes the manifest,
schema, methods, and metadata under `contracts/{contract_id}/` if it does not
already exist. It then writes or confirms the version pointer at
`services/{name}/versions/v{version}`, optionally moves channel aliases, and
broadcasts `CONTRACT_PUBLISHED` on gossip.

**Endpoint advertisement:** A service binds its Iroh endpoint, resolves its own
`contract_id`, joins the registry namespace, and writes an `EndpointLease` with
`health_status = "starting"`. After initialisation completes it updates to
`"ready"` and broadcasts `ENDPOINT_LEASE_UPSERTED`. It refreshes the lease
every `lease_refresh_interval_s`. On graceful shutdown it transitions to
`"draining"`, waits for in-flight RPCs, deletes the lease, and broadcasts
`ENDPOINT_DOWN`.

**Client resolution:** A consumer joins the registry namespace, subscribes to
gossip, resolves `service_name + version/channel → contract_id`, fetches the
`ContractManifest`, lists `EndpointLease` entries, filters by expiry, health,
serialization support, and policy, ranks the survivors (see §11.9), dials the
chosen `EndpointId`, and sends a `StreamHeader` including `contract_id`. The
server verifies `contract_id + method + serialization_mode` before dispatch.

### 11.9 Endpoint Selection

Consumers select from eligible endpoints using a configurable strategy. The
framework defines three that all implementations must provide: `round_robin`
(default, simple rotation), `least_load` (prefer lowest reported `load` value,
fall back to round-robin when unavailable), and `random` (uniform selection,
no affinity). All strategies first apply mandatory filters: exact `contract_id`
match, supported `serialization_mode`, `health_status` eligibility (§11.6),
lease freshness, and local policy constraints. Gossip `ENDPOINT_DOWN` allows
fast removal; consumers must also evict endpoints whose lease has expired in
docs even without a gossip notification.

### 11.10 Compatibility, Staleness, and Failure

Compatibility between contract versions is explicit and artifact-based.
Reports may be published under `compatibility/{contract_id}/{other_contract_id}`
expressing wire or source compatibility, breaking changes, serialization mode
differences, and field-level diffs. Consumers may use these to warn operators
or gate channel promotions.

> **TODO:** Define the compatibility report schema and whether reports are
> normative inputs to resolution or advisory tooling artifacts.

`iroh-docs` is the source of truth; gossip loss must never create incorrect
registry state. Consumers must ignore a lease update with a lower `lease_seq`
than one already observed. If `schema.fdl` exceeds `max_inline_contract_bytes`,
the docs entry stores a blob reference fetched via `iroh-blobs`. Consumers
should cache immutable contract artifacts indefinitely. If the registry is
unreachable, consumers may operate from cache until leases expire; mutable
state (channel pointers, lease freshness) must not be assumed valid beyond
`lease_expires_epoch_ms`.

-----

## 12. Security and Access Control

### 12.1 Transport-Level Security

All Iroh connections are E2E encrypted via TLS 1.3 (QUIC). Both endpoints
authenticate cryptographically — the EndpointId (public key) is verified during
the QUIC handshake. No additional TLS configuration required.

### 12.2 Connection-Level Access Control

Iroh’s `EndpointHooks` allow intercepting connections before acceptance:

```python
class RegistryEndpointHook:
    """Reject connections from unknown peers."""

    def __init__(self, allowed_endpoints: set[EndpointId]):
        self.allowed = allowed_endpoints

    async def on_accepting(self, incoming: Incoming) -> Outcome:
        if incoming.remote_endpoint_id in self.allowed:
            return Outcome.Accept
        return Outcome.Reject
```

### 12.3 Registry Write Access Control

The registry namespace uses a three-tier ACL model stored within the namespace
itself:

```
_aster/acl/admins    → list[AuthorId]   # Can modify ACL entries
_aster/acl/writers   → list[AuthorId]   # Can publish/update services
_aster/acl/readers   → list[AuthorId]   # Can sync namespace (read-only)
_aster/acl/policy    → RegistryPolicy   # Admission policy configuration
```

**How it works:**

1. iroh-docs uses a `NamespaceSecret` as a write capability token. Possessing
   the secret allows writing entries signed by any `AuthorId` you hold.
1. The ACL entries are themselves stored in the namespace and signed by an admin
   author. They are the application-level authorization layer.
1. **Sync-time validation:** When a node receives entries during iroh-docs sync,
   it checks: is this entry’s `AuthorId` in the `_aster/acl/writers` list? If
   not, the entry is rejected and not persisted locally. This is enforced by a
   sync callback, not by iroh-docs itself.
1. **The implementation decides the admission policy.** The framework provides
   the mechanism; deployments provide the policy:

```python
class RegistryPolicy:
    """Each deployment implements this to decide writer admission."""

    async def should_admit_writer(
        self,
        author_id: AuthorId,
        endpoint_id: EndpointId,
        metadata: dict[str, str],
    ) -> bool:
        """
        Called when a new node requests write access.
        Implementation decides:
        - Open: always return True
        - Invite-only: validate an invite token in metadata
        - Org-managed: check against corporate directory / LDAP
        - KAR-specific: verify control plane provisioned this endpoint
        """
        ...
```

### 12.4 Trust Model

|Layer         |Trust Primitive                                         |
|--------------|--------------------------------------------------------|
|Transport     |EndpointId (public key, verified in QUIC handshake)     |
|Connection    |EndpointHooks allowlist (accept/reject by peer key)     |
|Registry write|AuthorId in ACL (signed entries, sync-time validation)  |
|RPC auth      |Metadata tokens (interceptor-level, application-defined)|


> **TODO:** Design the writer admission handshake. When a new service wants to
> publish to the registry, how does it request write access? Proposed: a
> dedicated `_aster/admission_requests/{author_id}` key where the requester
> writes a signed request, and an admin processes it (approve → add to writers,
> deny → delete entry).

> **TODO:** Define key rotation. If an admin’s AuthorId is compromised, how is
> it revoked and replaced? The ACL is a CRDT — removing an entry requires a
> new write from another admin, but the compromised key could also write.
> Consider a “revocation list” pattern or a quorum requirement for ACL changes.

-----

## 13. Conformance and Interoperability

### 13.1 Conformance Test Suite

A language-neutral test suite validates wire format compliance:

```
tests/
├── wire/
│   ├── unary_request.bin           # Valid unary (header + payload)
│   ├── unary_response.bin          # Valid unary response
│   ├── server_stream_3_items.bin   # Server stream + trailer
│   ├── error_deadline.bin          # Trailer-only (DEADLINE_EXCEEDED)
│   ├── compressed_payload.bin      # Compressed frame
│   ├── all_serialization_modes.bin # One frame per mode
│   └── ...
├── interop/
│   ├── echo_service.fdl            # Minimal echo service definition
│   └── test_scenarios.yaml         # Client→server test matrix
└── fory/
    ├── task_assignment_xlang.bin    # Fory XLANG bytes with expected fields
    ├── task_assignment_row.bin      # Fory ROW bytes with expected fields
    └── ...
```

Each language implementation runs this suite. Pass = interoperates.

### 13.2 Cross-Language CI Matrix

```
         Server
         Py   Rust  JVM  .NET  Go   JS
Client
Py       ✓    ✓     ✓    ✓     ✓    ✓
Rust     ✓    ✓     ✓    ✓     ✓    ✓
JVM      ✓    ✓     ✓    ✓     ✓    ✓
.NET     ✓    ✓     ✓    ✓     ✓    ✓
Go       ✓    ✓     ✓    ✓     ✓    ✓
JS       ✓    ✓     ✓    ✓     ✓    ✓
```

Every cell is a CI job using the echo service as the minimal interop contract.

> **TODO:** Build the conformance test suite. Start with byte-level wire format
> tests (Phase 1), add echo-service interop tests (Phase 2+).

-----

## 14. Implementation Roadmap

### Phase 1: Python Exemplar + Wire Protocol

|Deliverable              |Description                                                                |Status|
|-------------------------|---------------------------------------------------------------------------|------|
|`iroh-python`            |PyO3 FFI wrapper for Iroh transport primitives                             |✅ Done |
|Wire protocol            |Framing, StreamHeader, RpcStatus implementation                            |✅ Done |
|`Aster` (Python)         |Decorators, server, client, dispatch                                       |✅ Done |
|Fory integration         |XLANG + NATIVE modes                                                       |✅ Done |
|Fory ROW integration     |ROW mode with random access                                                |✅ Done |
|Blob capability responses|File/directory responses via `iroh-blobs` tickets or authenticated locators|TODO  |
|Sibling tunnel support   |Stream/datagram tunnel negotiation and helper APIs                         |TODO  |
|Core interceptors        |Auth, deadline, audit, circuit breaker                                     |✅ Done |
|Conformance tests        |Byte-level wire format test vectors                                        |TODO  |
|Local client             |In-process transport (asyncio.Queue)                                       |✅ Done |

### Phase 2: Rust + Cross-Language Foundation

|Deliverable              |Description                                              |Status|
|-------------------------|---------------------------------------------------------|------|
|`Aster` (Rust)           |Native implementation (no FFI needed)                    |TODO  |
|Fory XLANG interop       |Python ↔ Rust cross-language tests                       |TODO  |
|Fory IDL codegen         |`foryc` service block extension (Python + Rust)          |TODO  |
|Service registry         |iroh-docs schema publishing, gossip events               |TODO  |
|ACL system               |Writer admission, sync-time validation                   |TODO  |
|OpenTelemetry integration|Canonical span structure and metric names (Python + Rust)|TODO  |
|Conformance CI           |Python ↔ Rust interop matrix                             |TODO  |

### Phase 3: JVM + .NET

|Deliverable         |Description                                   |Status|
|--------------------|----------------------------------------------|------|
|`iroh-jvm`          |JNI FFI wrapper                               |TODO  |
|`Aster` (JVM)       |Annotations, CompletableFuture/Flow/coroutines|TODO  |
|`Iroh.Native` (.NET)|P/Invoke FFI wrapper                          |TODO  |
|`Aster` (.NET)      |Attributes, Task/IAsyncEnumerable             |TODO  |
|Fory IDL codegen    |Java, Kotlin, C# targets                      |TODO  |
|Conformance CI      |4×4 interop matrix                            |TODO  |

### Phase 4: Go + JavaScript

|Deliverable       |Description                        |Status|
|------------------|-----------------------------------|------|
|`iroh-go`         |CGo FFI wrapper                    |TODO  |
|`Aster` (Go)      |Interfaces, channels, context      |TODO  |
|`@Aster/core` (JS)|NAPI-RS FFI + TypeScript decorators|TODO  |
|Fory IDL codegen  |Go, TypeScript targets             |TODO  |
|Conformance CI    |Full 6×6 interop matrix            |TODO  |

### Phase 5: Ecosystem

|Deliverable       |Description                                  |Status|
|------------------|---------------------------------------------|------|
|CLI tooling       |`aster generate`, `aster test`, `aster bench`|TODO  |
|Documentation site|Per-language guides, tutorials               |TODO  |
|Registry UI       |Web UI for browsing published services       |TODO  |

-----

## 15. Package Structure

```
Aster/
├── spec/
│   ├── SPEC.md                         # This document
│   ├── wire-protocol.md                # Byte-level wire format (extracted from §6)
│   ├── status-codes.md                 # Status code registry
│   └── conformance/                    # Language-neutral test vectors
│       ├── wire/
│       ├── interop/
│       └── fory/
│
├── idl/
│   └── *.fdl                           # Shared Fory IDL definitions
│
├── iroh-ffi/                           # Iroh FFI wrappers (one per language)
│   ├── python/                         # iroh-python (PyPI wheel)
│   ├── jvm/                            # iroh-jvm (Maven/Gradle + native lib)
│   ├── dotnet/                         # Iroh.Native (NuGet)
│   ├── go/                             # iroh-go (Go module)
│   └── js/                             # @Aster/iroh (npm native addon)
│
├── python/                             # Phase 1 exemplar
│   └── aster/
│       ├── __init__.py                 # Public API
│       ├── decorators.py               # @service, @rpc, @server_stream, etc.
│       ├── service.py                  # ServiceRegistry, HandlerInfo, dispatch
│       ├── server.py                   # Server accept loop, stream dispatch
│       ├── client.py                   # Client stub generation (remote + local)
│       ├── codec.py                    # ForyCodec (all modes), compression
│       ├── framing.py                  # Frame read/write, length prefix
│       ├── protocol.py                 # StreamHeader, RpcStatus, constants
│       ├── errors.py                   # StatusCode, RpcError hierarchy
│       ├── interceptors/
│       │   ├── base.py                 # Interceptor ABC, CallContext
│       │   ├── auth.py
│       │   ├── deadline.py
│       │   ├── audit.py
│       │   ├── metrics.py
│       │   └── circuit_breaker.py
│       ├── transport/
│       │   ├── base.py                 # Transport Protocol, BidiChannel
│       │   ├── iroh.py                 # IrohTransport (remote, over Iroh connection)
│       │   └── local.py                # LocalTransport (in-process, asyncio.Queue)
│       ├── registry/
│       │   ├── client.py               # Registry consumer (discover, sync)
│       │   ├── publisher.py            # Registry publisher (register, heartbeat)
│       │   ├── acl.py                  # ACL management, sync-time validation
│       │   └── gossip.py               # Gossip event handling
│       └── testing/
│           └── harness.py              # Mock services, local client factory
│
├── rust/                               # Phase 2
│   └── Aster/                      # cargo add Aster
│
├── jvm/                                # Phase 3
│   └── Aster/                      # Maven/Gradle
│
├── dotnet/                             # Phase 3
│   └── Aster/                          # NuGet
│
├── go/                                 # Phase 4
│   └── Aster/                      # go get
│
└── js/                                 # Phase 4
    └── @Aster/core/                # npm
```

-----

## 16. Open Design Questions

Items requiring resolution. Ordered by priority.

### 16.1 Blocking (must resolve before Phase 1)

All blocking questions are resolved. See §5.3 (type ID assignment), §5.5 (ROW
mode framing and streaming), §6.1 (ROW_SCHEMA flag), and §8.3 (local client
transport abstraction).

### 16.2 Important (resolve before Phase 2)

|# |Question                         |Notes                                                                                                                                                                                              |
|--|---------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|4 |**Writer admission handshake**   |How does a new service request write access to the registry? Proposed: `_aster/admission_requests/{author_id}` signed request pattern.                                                             |
|5 |**Key rotation / revocation**    |How to handle compromised admin AuthorIds in the ACL CRDT? Consider revocation list or quorum requirement for ACL changes.                                                                         |
|6 |**Schema compatibility checking**|Auto-generate compatibility reports from Fory schema evolution rules? Or manual?                                                                                                                   |
|7 |**Load metric format**           |Simple float vs. structured vs. opaque.                                                                                                                                                            |
|8 |**Large schema blob threshold**  |Proposed 64KB. Validate against real-world IDL sizes.                                                                                                                                              |
|9 |**Heartbeat clock source**       |Wall clock + skew tolerance, or logical clock?                                                                                                                                                     |
|10|~~**Canonical contract encoding**~~ ✅ Resolved  |Resolved: canonical XLANG profile normatively defined in Aster-ContractIdentity.md §11.3 (stripped Fory XLANG byte stream with spec-pinned integer encoding, NULL_FLAG, sort orders, zero-value conventions). Golden vectors in Appendix A of that document.|
|11|**Multi-registry federation**    |How do consumers reference services across registry namespace boundaries? Options: namespace federation, explicit cross-namespace lookup, or a root namespace acting as a directory of directories.|
|12|**OTel span and metric schema**  |Define canonical span attribute names and metric names shared across all language implementations so traces and metrics are consistent regardless of which language emits them.                    |
|13|**Channel promotion rules**      |Who can promote a `contract_id` from `canary` to `stable`? Should a compatibility report be a precondition for `stable` promotion? Define the approval workflow.                                   |

### 16.3 Deferred (Phase 3+)

|# |Question                          |Notes                                                                                                                                                                                                                     |
|--|----------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|14|**UniFFI evaluation**             |Can UniFFI generate FFI wrappers for Python + Kotlin + Swift from a single Rust crate? Would reduce Phase 3-4 effort.                                                                                                     |
|15|**Backpressure semantics**        |QUIC stream-level flow control → language-level async. Document per-language mapping.                                                                                                                                     |
|16|**Open-source scope**             |Aster is general-purpose. Application-specific extensions (approval gates, step budgets) should be interceptors, not core. Maintain this separation.                                                                      |
|17|**Fory IDL service block codegen**|Implement `foryc` extension for service definitions. Coordinate with Apache Fory upstream.                                                                                                                                |
|18|**Latency/locality hints**        |Define whether the framework standardizes latency or locality signals as inputs to endpoint selection strategies, or leaves them implementation-defined.                                                                  |
|19|**Enterprise ACL extensions**     |Per-service write boundaries (team A can write `services/payments/*` but not `services/inventory/*`), separation between contract publishing and endpoint lease management, approval workflow for breaking schema changes.|

-----

## Appendix A: Dependency Summary

### Python (Phase 1)

|Component         |Dependency                         |Role                               |
|------------------|-----------------------------------|-----------------------------------|
|Transport         |`iroh-python` (custom PyO3 wheel)  |QUIC, NAT traversal, E2E encryption|
|Serialization     |`pyfory` (Apache Fory)             |XLANG, NATIVE, ROW serialization   |
|Compression       |`zstandard`                        |Frame-level payload compression    |
|Async             |`asyncio` (stdlib)                 |Coroutine scheduling               |
|Introspection     |`typing`, `inspect` (stdlib)       |Decorator metadata from type hints |
|Registry          |`iroh-python` (docs, gossip, blobs)|Service discovery, schema sync     |
|Metrics (optional)|`opentelemetry`                    |Traces and metrics                 |

No `protoc`. No `protobuf`. No external codegen beyond `foryc` (optional).

### Future Languages

|Language|Transport               |Serialization              |Compression     |Async          |
|--------|------------------------|---------------------------|----------------|---------------|
|Rust    |`iroh` (native)         |`fory-rs`                  |`zstd`          |`tokio`        |
|JVM     |`iroh-jvm` (JNI)        |`org.apache.fory:fory-core`|`zstd-jni`      |Coroutines / CF|
|.NET    |`Iroh.Native` (P/Invoke)|`Apache.Fory`              |`ZstdSharp`     |`Task<T>`      |
|Go      |`iroh-go` (CGo)         |`github.com/apache/fory/go`|`klauspost/zstd`|goroutines     |
|JS/TS   |`@Aster/iroh` (NAPI)    |`fory-js`                  |`fzstd`         |`Promise`      |

-----

## Appendix B: Fory Serialization Mode Reference

Aster exposes the three Fory serialization protocol families described in §5.
`NATIVE_COMPATIBLE` is not a separate protocol in Aster. Schema evolution is
handled by the content-addressed contract registry (§11.3), not by Fory's
`compatible` mode.

|Mode  |Wire Compatible   |Schema Evolution                |Random Access               |Arrow Integration            |Languages                                           |
|------|------------------|--------------------------------|----------------------------|-----------------------------|----------------------------------------------------|
|XLANG |All Fory-supported|Yes (via content-addressed registry)|No                          |No                           |Java, Python, Rust, C++, Go, JS, C#, Swift          |
|NATIVE|Same language only |Via content-addressed registry      |No                          |No                           |Per-language (Python pickle-compat, Java JDK-compat)|
|ROW   |Java, Python, C++ |N/A                             |Yes (zero-copy field access)|Yes (Arrow RecordBatch/Table)|Java, Python, C++ (others per Fory roadmap)         |

-----

## Appendix C: Change Log

|Version|Changes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
|-------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|0.7.2  |Spec status refresh after Python implementation progress: marked Python exemplar as implemented through Phase 7, updated interceptor statuses, updated Phase 1 roadmap rows to reflect completed Python work, corrected stale enum references (`SerializationMode.JAVA`/`SerializationMode.PYTHON` → `NATIVE`), corrected stale IDL serialization example (`java` → `native`), corrected Phase 1 Fory roadmap text to remove `NATIVE_COMPATIBLE`, and refreshed Appendix D pyfory verification items based on completed spike tests and implementation status. |
|0.7.1  |Editorial pass: removed self-referential framing (“this is intentional”, “this is a deliberate design choice”) throughout. §3.2 Stream-per-RPC: removed two bullets restating QUIC properties already covered in §2.1 (HOL blocking, cheap streams); retained Aster-specific design consequences (no multiplexing complexity, clean lifecycle). §6.7 Streaming Error Recovery: trimmed rationale paragraph to core argument; removed sentences restating §3.2 stream-per-RPC scoping.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
|0.7.0  |§6.2 StreamHeader: added Phase 1 blocker TODO for canonical contract encoding dependency on `contract_id`. §6.5 Status Codes: added application-tier status code guidance (100+ range) with concrete examples. §6.6 ALPN and global: replaced all stale xRPC references with Aster. §6.8.2 Deadline propagation: trimmed to single paragraph. §8.4 Responsibility Split: deleted (redundant with §3.1.1). §3.1.3 Consequences for Implementations: deleted (redundant with §3.1.1–3.1.2). §10.2 Connection Health: rewritten to confirm QUIC keep-alive covers long-lived streams with no application heartbeat required. §11.8–11.12: compressed from five sections to three (§11.8 Publication and Resolution Flows, §11.9 Endpoint Selection, §11.10 Compatibility/Staleness/Failure). Appendix B: rewritten to match three actual Fory protocols (XLANG, NATIVE, ROW); removed NATIVE_COMPATIBLE row.                                                                               |
|0.6.0  |§5.3 XLANG Mode rewritten: numeric type IDs replaced by canonical tag string scheme (`"{dotted.package}/{TypeName}"`), `_aster/*` namespace reserved for framework types, hash-derived numeric IDs demoted to local optimisation, eager tag validation at class definition time specified. §5.5 ROW Mode: TODOs resolved; §5.5.1 confirms identical length-prefix framing for ROW payloads; §5.5.2 confirms ROW mode in all streaming patterns with schema hoisting (`ROW_SCHEMA` flag) defined. §6.1 Stream Framing: `ROW_SCHEMA` flag (bit 3, `0x08`) added; reserved bits updated from 3–7 to 4–7. §8.3 Local Client rewritten: `Transport` structural Protocol defined in `transport/base.py`; interceptor obligation on `LocalTransport` specified; `wire_compatible` mode documented. §15 Package structure: `transport/base.py` added. §16.1 Blocking questions closed.                                                                                                          |
|0.5.0  |§6.6 ALPN resolved: single `aster/1` per wire version, rationale documented. §6.7 Streaming error recovery: new section, QUIC-layer handling, retry responsibility defined. §6.8 Deadline semantics: new section, per-call absolute epoch, no framework-level propagation, cancellation-via-stream-lifecycle rationale. §9.2 CircuitBreakerInterceptor added with state machine. §10.1 Bootstrap: EndpointId stability via stable secret key documented, “out-of-band” removed. §11.6 Health status values formalised with routing behaviour table. §11.9 Publisher flow updated with `starting`/`draining` transitions. §11.10 Named load balancing strategies defined (round_robin, least_load, random). §11.12 Registry partition tolerance policy added. §14 OTel moved to Phase 2. §15 circuit_breaker.py added to package structure. §16 ALPN question closed; new questions added for multi-registry federation, OTel schema, channel promotion rules, enterprise ACL extensions.|
|0.4.0  |Initial internal draft.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |

-----

## Appendix D: Python Implementation Readiness Checklist

Before starting the Python Aster implementation, the following items must be
confirmed or resolved. This checklist captures the practical proof points
identified during spec review.

### D.1 Transport Layer (iroh-python)

| Item | Status | Notes |
|------|--------|-------|
| `aster_transport_core` as sole Python backend | ✅ Done | PyO3 wraps core directly, no C ABI path |
| Docs: create, set_bytes, get_exact, share, join | ✅ Done | `test_docs.py` passes |
| Docs: query_key_exact, query_key_prefix, read_entry_content | ✅ Done | Multi-author filtering support |
| Blobs: add_bytes, read, ticket, download | ✅ Done | `test_blobs.py` passes |
| Blobs: add_bytes_as_collection, create_collection_ticket | ✅ Done | sendme-compatible |
| Blobs: download_collection (name→data pairs) | ✅ Done | Required for contract fetch |
| Gossip: subscribe, broadcast, receive | ✅ Done | `test_gossip.py` passes |
| Connections: open_bi, accept_bi, streams | ✅ Done | `test_net.py` passes |
| Datagrams: send, read, max_datagram_size, buffer_space | ✅ Done | Phase 1b |
| Monitoring: remote_info, connection_info | ✅ Done | Phase 1b |
| Hooks: endpoint hooks callback lifecycle | ⚠️ Partial | Core adapter done; Python callback dispatch is placeholder |
| Secret key export/import | ✅ Done | Stable EndpointId support |

### D.2 Serialization (Apache Fory / pyfory)

| Item | Status | Notes |
|------|--------|-------|
| pyfory XLANG mode available in Python | ✅ Verified | Verified in spike tests and Python Aster implementation |
| pyfory ROW mode available in Python | ✅ Verified | Verified in spike tests and Python Aster implementation |
| Canonical XLANG profile implementable in pyfory | ❓ Verify | Deterministic field-order, no ref tracking, standalone |
| Golden vector spike: Python + Rust produce identical bytes | ❓ TODO | Pre-implementation proof required |
| Tag-based type registration in pyfory | ✅ Verified | `@fory_type(tag=...)` maps to pyfory namespace/typename registration |

### D.3 Contract Identity

| Item | Status | Notes |
|------|--------|-------|
| Canonical XLANG profile produces deterministic bytes | ❓ TODO | Requires golden vector tests |
| BLAKE3 hashing available in Python | ✅ Available | `blake3` PyPI package |
| TypeDef / ServiceContract serialization in pyfory | ❓ TODO | Framework-internal types need pyfory registration |
| Iroh collection build + import from Python | ❓ TODO | Need `Collection.store()` equivalent via PyO3 |
| ArtifactRef write to docs from Python | ✅ Feasible | Uses existing `doc.set_bytes()` |
| Collection fetch + verify from Python | ❓ TODO | Need `download_collection` + blake3 verify |

### D.4 Decisions to Lock

| Decision | Recommendation | Status |
|----------|---------------|--------|
| Python Phase 1 includes docs query APIs | Yes | Needed for registry reads |
| Python Phase 1 includes monitoring/remote-info | Yes | Already exposed |
| Python Phase 1 includes hooks | Defer full hooks; mark experimental | Callback plumbing incomplete |
| Operational metadata format in docs entries | JSON initially | Switch to XLANG only if determinism needed |
| Contract collection member order | Not significant for identity | `contract_id` comes from `contract.xlang` bytes |
| Immutable artifact caching policy | Cache indefinitely by content hash | Standard for content-addressed data |
| Failure: docs alias exists but collection fetch fails | Surface as `UNAVAILABLE` to caller | Consumer retries per policy |
| Failure: collection contents don't hash to `contract_id` | Reject bundle, surface `DATA_LOSS` | Integrity check is mandatory |
| Security for artifact fetch | Public immutable by default | Authenticated blob refs for private registries |

-----

*End of specification.*
