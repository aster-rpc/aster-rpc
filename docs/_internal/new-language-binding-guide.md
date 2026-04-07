# New Language Binding Guide

How to add a new language binding for Aster. This is the playbook we followed for TypeScript and the process we'll follow for Java, Go, Swift, etc.

## Priority Order for New Languages

| Priority | Language | Binding Tech | Rationale |
|----------|----------|-------------|-----------|
| 1 | **Java/Kotlin** | JNI via C FFI (`ffi/`) | Enterprise adoption. Android. The C FFI was designed for Java FFM. |
| 2 | **Go** | CGo via C FFI | Infrastructure/DevOps ecosystem. CLI tools. Kubernetes operators. |
| 3 | **Swift** | C FFI (bridging header) | Apple ecosystem. iOS/macOS native apps. |
| 4 | **C#/.NET** | P/Invoke via C FFI | Enterprise Windows. Unity game engine. |
| 5 | **Zig/C** | Direct C FFI | Systems programming. Embedded. |

**Why this order:**
- Java/Go cover the largest server-side ecosystems not yet reached
- Swift covers the largest mobile ecosystem not yet reached
- The C FFI (`ffi/src/lib.rs`, 85+ functions, 83 tests) was specifically built for non-Python consumers — Java/Go/Swift/C# all use it directly
- TypeScript used NAPI-RS (Rust→JS) because it's faster than going through C FFI for Node.js

## The Four Phases

### Phase 1: Transport Layer (FFI Surface)

**Goal:** Wrap every FFI/core function so the new language can create nodes, connect, send/receive streams, and use blobs/docs/gossip.

**What to implement:**

```
IrohNode
  ├── memory(), persistent()
  ├── memoryWithAlpns(), persistentWithAlpns()
  ├── connect(nodeId, alpn) → Connection
  ├── acceptAster() → Connection
  ├── nodeId(), nodeAddr(), exportSecretKey()
  ├── addNodeAddr(other)
  ├── takeHookReceiver() → HookReceiver
  ├── hasHooks()
  ├── blobsClient() → BlobsClient
  ├── docsClient() → DocsClient
  ├── gossipClient() → GossipClient
  └── close()

Connection
  ├── openBi() → (SendStream, RecvStream)
  ├── acceptBi() → (SendStream, RecvStream)
  ├── openUni() → SendStream
  ├── acceptUni() → RecvStream
  ├── sendDatagram(data), readDatagram()
  ├── remoteNodeId(), connectionInfo(), maxDatagramSize()
  └── close(code, reason)

SendStream: writeAll(data), finish()
RecvStream: readExact(n), readToEnd(maxLen)

BlobsClient
  ├── addBytes(data) → hash
  ├── read(hash) → data
  ├── createTicket(hash), downloadBlob(ticket)
  ├── addBytesAsCollection(name, data), createCollectionTicket(hash)
  ├── has(hash), blobStatus(hash)
  ├── tagSet/tagGet/tagDelete/tagListPrefix
  ├── blobObserveComplete(hash), blobObserveSnapshot(hash), blobLocalInfo(hash)
  └── (total: 15 methods)

DocsClient
  ├── create() → DocHandle
  ├── join(ticket) → DocHandle
  ├── joinAndSubscribe(ticket) → (DocHandle, EventReceiver)
  └── createAuthor() → authorId

DocHandle
  ├── setBytes(author, key, value), getExact(author, key)
  ├── queryKeyExact(key), queryKeyPrefix(prefix)
  ├── readEntryContent(hash)
  ├── share(mode), shareWithAddr(mode)
  ├── subscribe() → EventReceiver
  ├── startSync(peers), leave()
  ├── setDownloadPolicy(policy), getDownloadPolicy()
  └── docId()

GossipClient: subscribe(topic) → TopicHandle
TopicHandle: broadcast(data), recv() → data

HookReceiver: recvBeforeConnect(), respondConnect(), recvAfterHandshake(), respondHandshake()
```

**Tests to write:**
- Create in-memory node, verify nodeId is 64-char hex
- Two nodes: addNodeAddr, connect, openBi, write+read roundtrip
- Blobs: addBytes → read roundtrip, verify BLAKE3 hash
- Docs: create → setBytes → getExact roundtrip
- Docs: subscribe → receive insert event
- Docs: download policy get/set
- Gossip: subscribe → broadcast → recv roundtrip

**Verification:** All transport tests pass. The new language can create nodes and exchange data over QUIC.

### Phase 2: RPC Framework (Language-Idiomatic)

**Goal:** Build the Aster RPC layer in the target language's idioms. This is the largest phase.

**What to implement:**

| Module | Purpose | Key Types |
|--------|---------|-----------|
| **status** | Error codes + typed error hierarchy | StatusCode (0-16), RpcError + 16 subclasses |
| **types** | Shared enums and config types | SerializationMode, RpcPattern, ExponentialBackoff, RetryPolicy |
| **limits** | Security constants + validators | MAX_FRAME_SIZE, validateHexField, validateMetadata |
| **framing** | Wire frame read/write | writeFrame, readFrame, encodeFrame, decodeFrame, 6 flag constants |
| **protocol** | Wire protocol types | StreamHeader, CallHeader, RpcStatus (all @wire_type) |
| **metadata** | Semantic documentation | Metadata class (@wire_type "_aster/Metadata") |
| **codec** | Serialization | JsonCodec, ForyCodec (if Fory exists for the language), walkTypeGraph |
| **decorators** | Service/method definition | @service, @rpc, @server_stream, @client_stream, @bidi_stream, @wire_type |
| **service** | Registry + metadata types | ServiceInfo, MethodInfo, ServiceRegistry |
| **client** | Client stub generation | createClient (Proxy-based or codegen, language-dependent) |
| **server** | QUIC accept loop + dispatch | RpcServer with serve(), 4 pattern handlers |
| **transport/base** | Transport abstraction | Transport interface (unary, serverStream, clientStream, bidiStream, close) |
| **transport/local** | In-process transport | LocalTransport for testing without network |
| **transport/iroh** | Real QUIC transport | IrohTransport over QUIC streams |
| **session** | Session-scoped services | SessionServer (CALL/CANCEL multiplexing) |

**Language-specific design decisions:**
- **Decorators/annotations:** Python uses decorators, TS uses TC39 decorators, Java uses annotations, Go uses struct tags or codegen
- **Client stubs:** Python uses `__getattr__`, TS uses `Proxy`, Java could use dynamic proxies or codegen, Go could use generics or codegen
- **Async model:** Python uses asyncio, TS uses Promises, Java uses CompletableFuture or virtual threads, Go uses goroutines
- **Type graph walking:** Python uses dataclass introspection, TS walks default values — each language needs its own approach

**Tests to write:** Match conformance vectors in `conformance/vectors/` for framing and contract identity. Run LocalTransport tests for all 4 RPC patterns.

### Phase 3: Interceptors + Trust + Registry

**What to implement:**

| Module | Purpose | Items |
|--------|---------|-------|
| **interceptors/** | Middleware chain | base (CallContext, apply*), deadline, auth, retry, circuit-breaker, metrics, rate-limit, compression, audit, capability — 10 interceptors |
| **trust/credentials** | Ed25519 signing | generateKeypair, sign, verify, EnrollmentCredential, ConsumerEnrollmentCredential |
| **trust/admission** | Credential verification | verifyConsumerCredential, verifyProducerCredential |
| **trust/consumer** | Client-side admission | performAdmission handshake |
| **trust/producer** | Server-side admission | handleProducerAdmission, serveProducerAdmission |
| **trust/hooks** | Connection gating | ConnectionPolicy, AllowAll, DenyAll |
| **trust/mesh** | Service discovery state | MeshState with persistence (save/load JSON) |
| **trust/nonce** | Replay protection | NonceStore with TTL |
| **trust/rcan** | Role-based access | evaluateCapability, validateRcan |
| **trust/iid** | Cloud identity | verifyIID + AWS/GCP/Azure/Mock backends |
| **trust/clock** | Drift detection | ClockDriftTracker |
| **contract/identity** | BLAKE3 hashing | Delegates to Rust core (all languages use same FFI) |
| **contract/manifest** | Manifest persistence | ContractManifest, manifestToJson/fromJson |
| **contract/publication** | Publish to blobs | buildCollection, publishContract |
| **registry/** | Service discovery | RegistryClient, RegistryPublisher, RegistryACL, RegistryGossip, keys, models |
| **dynamic** | Runtime type synthesis | DynamicTypeFactory from manifests |

### Phase 4: Production Features + Polish

| Module | Purpose |
|--------|---------|
| **config** | Env vars + TOML file + .aster-identity loading |
| **logging** | Structured logging (JSON/text) with request correlation |
| **health** | HTTP health server (/healthz, /readyz, /metrics, /metrics/prometheus) |
| **high-level** | AsterServer (one-liner producer) + AsterClient (one-liner consumer) |
| **metrics** | ConnectionMetrics + AdmissionMetrics counters |
| **Graceful shutdown** | Signal handlers, drain with timeout |

## Completion Checklist

After implementing all phases, verify these before declaring the binding "done":

### Spec Conformance (BINDING_PARITY.md)
- [ ] Frame encoding/decoding matches golden vectors
- [ ] Contract identity matches golden vectors (4 vectors in Appendix B)
- [ ] Status codes match gRPC-compatible values (0-16)
- [ ] StreamHeader/CallHeader/RpcStatus wire format matches
- [ ] Canonical XLANG encoding produces identical bytes (via Rust core)
- [ ] BLAKE3 hashing produces identical bytes (via Rust core)
- [ ] Consumer/producer admission protocol works cross-language

### Implementation Completeness (BINDING_COMPLETENESS.md)
- [ ] All Transport Layer methods implemented and tested
- [ ] All RPC Framework modules implemented
- [ ] All 10 interceptors implemented
- [ ] All Trust & Security modules implemented
- [ ] All Registry modules implemented
- [ ] All Production Features implemented
- [ ] Metadata class with decorator integration

### Cross-Language Interop
- [ ] New-language server + Python client: unary RPC works
- [ ] Python server + new-language client: unary RPC works
- [ ] New-language server + TypeScript client: unary RPC works
- [ ] Streaming patterns work cross-language (at least server_stream)
- [ ] Contract IDs match across all languages for identical service definitions

### Testing
- [ ] Unit tests for all modules
- [ ] E2E tests over real QUIC (at least unary + server_stream)
- [ ] Conformance vector tests (framing + contract identity)
- [ ] Native integration tests (FFI wrapper correctness)

### Documentation
- [ ] Quickstart guide in docs site (`docs/quickstart/<language>.mdx`)
- [ ] Binding reference pages (`docs/bindings/<language>/`)
- [ ] Updated guides with language tabs (`docs/guides/`)
- [ ] Updated BINDING_PARITY.md with new language column
- [ ] Updated BINDING_COMPLETENESS.md with new language column

### CI/CD
- [ ] CI workflow for the new binding (lint, typecheck, test)
- [ ] Build workflow for package distribution
- [ ] Cross-language interop tests in CI

## Key Principle: Spec is Authority, Python is Reference

The wire protocol is defined in spec docs (`ffi_spec/`), not in any one binding's code:
- `Aster-SPEC.md` — core wire protocol
- `Aster-ContractIdentity.md` — contract identity hashing (includes golden vectors)
- `Aster-session-scoped-services.md` — session protocol
- `Aster-trust-spec.md` — trust model

When in doubt about behavior, read the spec. When the spec is ambiguous, Python is the reference implementation. When adding new features, update the spec first.

## File Layout Convention

Each binding follows the same module structure, adapted to language conventions:

```
bindings/<language>/
├── native/              # FFI wrapper (Rust→language bridge)
│   └── src/             # node, net, blobs, docs, gossip, hooks, contract, error
├── <package>/           # Pure-language RPC framework
│   └── src/
│       ├── status       # StatusCode + RpcError hierarchy
│       ├── types        # Shared enums
│       ├── limits       # Security constants
│       ├── framing      # Wire framing
│       ├── protocol     # StreamHeader, CallHeader, RpcStatus
│       ├── metadata     # Metadata class
│       ├── codec        # JsonCodec + ForyCodec
│       ├── decorators   # @service, @rpc, etc.
│       ├── service      # ServiceInfo, MethodInfo, ServiceRegistry
│       ├── client       # Client stub generation
│       ├── server       # QUIC accept loop
│       ├── session      # Session-scoped services
│       ├── dynamic      # Dynamic type synthesis
│       ├── config       # Configuration
│       ├── logging      # Structured logging
│       ├── health       # Health server
│       ├── high-level   # AsterServer, AsterClient
│       ├── metrics      # Connection/admission metrics
│       ├── transport/   # base, local, iroh
│       ├── interceptors/ # 10 interceptors
│       ├── trust/       # credentials, admission, consumer, producer, hooks, mesh, nonce, rcan, iid, clock
│       ├── contract/    # identity, manifest, publication
│       └── registry/    # client, publisher, acl, gossip, keys, models
└── tests/
    ├── unit/            # Per-module tests
    ├── integration/     # E2E over real QUIC
    └── cross-language/  # Interop with other bindings
```
