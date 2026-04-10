# Aster Implementation Flows -- Index

Master guide for understanding the Aster codebase and implementing a new
language binding. Read this first, then follow the dependency order below.

## Reading order

The flows have dependencies. Read them in this order:

```
1. transport_layer.md          -- How QUIC streams become framed RPC
2. codec_negotiation.md        -- How server and client agree on serialization
3. error_trailer_encoding.md   -- The codec-match invariant for error handling
4. consumer_admission_handshake.md  -- Gate 1: credential presentation
5. capability_interception.md  -- Gate 3: role-based access control
6. producer_accept_loop.md     -- Server connection routing and dispatch
7. session_protocol.md         -- Session-scoped services (stream-per-instance)
8. publish_contracts.md        -- Contract identity and registry publication
```

Dependency graph:

```
transport_layer ──> codec_negotiation ──> error_trailer_encoding
       │                    │
       ▼                    ▼
consumer_admission    producer_accept_loop ──> session_protocol
       │                    │
       ▼                    ▼
capability_interception   publish_contracts
```

## Assumptions and prerequisites

Before starting a new binding, make these decisions:

### Codec choice

Check whether your language has a working **Fory XLANG** implementation.
Fory XLANG is the default binary codec -- it's faster and more compact than
JSON, and it's what Python uses natively.

- **If Fory XLANG is available and stable** (e.g. Python, Java): use it as
  the default codec. Set `serializationMode=0` in StreamHeader. Advertise
  `serialization_modes: ["xlang"]` in your ServiceSummary. You should also
  support JSON as a fallback (server sniffs first byte).
- **If Fory XLANG is not available or not mature** (e.g. TypeScript, Go):
  use JSON (`serializationMode=3`). Advertise `serialization_modes: ["json"]`.
  Your server only needs to handle JSON. This is a fully supported mode --
  not a degraded path.
- **Either way, your server must sniff the first byte** of the StreamHeader
  to detect what the client sent. A Python client connecting to a JSON-only
  server will discover the mismatch through the `serialization_modes` field
  in the admission response and auto-select JSON.

### iroh FFI surface

Your binding needs iroh's QUIC transport. Options:

- **Rust (native):** Use iroh crates directly.
- **Python:** Uses PyO3 (Rust -> Python FFI). See `bindings/python/rust/`.
- **Other languages:** Use the C FFI at `ffi/src/lib.rs` with the header
  at `ffi/iroh_ffi.h`. Java uses Panama, Go uses cgo.

The FFI provides: endpoint creation, connection accept/connect, bidi
stream open/accept, read_exact/write_all on streams.

### What the framework handles vs what you implement

| Layer | Provided by iroh FFI | You implement |
|-------|---------------------|---------------|
| QUIC transport | Yes | -- |
| Connection open/accept | Yes | Routing by ALPN |
| Stream open/accept | Yes | -- |
| Byte read/write | Yes | -- |
| Frame encoding/decoding | -- | Yes (see framing.json) |
| Protocol types (StreamHeader etc.) | -- | Yes (see protocol-payloads.json) |
| Codec (JSON or Fory) | -- | Yes |
| Service registry | -- | Yes |
| Interceptors | -- | Yes |
| Admission/trust | -- | Yes |

## New binding implementation plan

If you are implementing Aster in a new language, follow these phases:

### Phase 1: Wire compatibility (week 1)

Get the framing and protocol types right. This is the foundation -- if
the bytes are wrong, nothing else works.

1. Implement `write_frame()` and `read_frame()` per the framing spec
2. Validate against `conformance/vectors/framing.json` (9 encode, 3 decode, 3 error vectors)
3. Implement JSON codec for StreamHeader, CallHeader, RpcStatus
4. Validate against `conformance/vectors/protocol-payloads.json` (10 vectors)
5. Implement a basic unary client: connect, send StreamHeader + request, read response + trailer

**Checkpoint:** Your client can talk to a Python server for a simple echo call.

### Phase 2: Server (week 2)

Build the server-side dispatch loop.

1. Accept connections, route by ALPN
2. Read StreamHeader, sniff codec (first byte), look up service
3. Dispatch to handler by method + pattern
4. Write response + trailer
5. Handle all early-return error paths with correct codec (error_trailer_encoding.md)

**Checkpoint:** Python client can call your server. Run the unary-echo conformance scenario.

### Phase 3: Admission and auth (week 3)

Wire the trust layer.

1. Consumer admission: load credential, present to server, parse response
2. PeerAttributeStore: bridge admission attributes to dispatch
3. CapabilityInterceptor: evaluate requires against caller roles
4. Gate 0 hook loop: connection-level filtering

**Checkpoint:** Authenticated Python client can call your server with role-based access.

### Phase 4: Sessions (week 3-4)

Add session-scoped service support.

1. Session discriminator (method="" detection)
2. SessionServer: CALL frame loop, 4 pattern dispatchers
3. SessionStub: client-side lock, call multiplexing
4. Cancellation, deadline enforcement, EoI validation

**Checkpoint:** Run the full cross-language matrix against Python. All patterns pass.

### Phase 5: Hardening (week 4)

Apply all the security and robustness measures.

1. Deadline enforcement with MAX_HANDLER_TIMEOUT_S upper bound
2. Metadata validation (MAX_METADATA_ENTRIES, MAX_METADATA_VALUE_LEN, etc.)
3. Decompression bomb protection (MAX_DECOMPRESSED_SIZE)
4. Client-stream item cap (MAX_CLIENT_STREAM_ITEMS)
5. Default DeadlineInterceptor
6. Run chaos tests, concurrent tests, and soak tests

**Checkpoint:** Run the full chaos test suite against your binding.

### Phase 6: Contract identity (week 4-5)

Implement contract hashing for the registry.

1. Canonical XLANG encoding of ServiceContract
2. BLAKE3 hash
3. Validate against `conformance/vectors/contract-identity.json` (4 golden vectors)

**Checkpoint:** Contract IDs match Python's output byte-for-byte.

## Conformance vectors

| File | What it tests | Vector count |
|------|---------------|--------------|
| `conformance/vectors/framing.json` | Frame envelope encode/decode/error | 15 |
| `conformance/vectors/protocol-payloads.json` | StreamHeader, CallHeader, RpcStatus payloads (JSON + XLANG) | 10 |
| `conformance/vectors/contract-identity.json` | Canonical encoding + BLAKE3 hashes | 4 |
| `conformance/scenarios/unary-echo.yaml` | End-to-end unary round-trip | 2 calls |
| `conformance/scenarios/server-stream.yaml` | End-to-end server streaming | - |

## Naming conventions

### Wire-compatible (must be identical across bindings)

These names appear in serialized bytes. Changing them breaks cross-language
interop.

| Category | Convention | Examples |
|----------|-----------|----------|
| StreamHeader fields | camelCase | `callId`, `deadlineEpochMs`, `serializationMode`, `metadataKeys` |
| CallHeader fields | camelCase | `callId`, `deadlineEpochMs`, `metadataKeys` |
| RpcStatus fields | camelCase | `code`, `message`, `detailKeys`, `detailValues` |
| Admission request JSON | snake_case | `credential_type`, `root_pubkey`, `endpoint_id`, `expires_at` |
| Admission response JSON | snake_case | `serialization_modes`, `admitted` |
| wire_type tags | `namespace/TypeName` | `mission_control/StatusRequest` |
| ALPN strings | literal | `aster/1`, `aster-consumer-admission/1` |
| Frame flags | numeric | COMPRESSED=0x01, TRAILER=0x02, HEADER=0x04, CALL=0x10, CANCEL=0x20 |
| StatusCode values | integer | OK=0, CANCELLED=1, NOT_FOUND=5, INTERNAL=13, etc. |
| SerializationMode | integer | XLANG=0, NATIVE=1, ROW=2, JSON=3 |

### Language-adaptable (can follow language conventions)

These names are internal to each binding. Adapt to the language's idiom.

| Category | Python example | TS example | Go example |
|----------|---------------|------------|------------|
| Class/struct names | `SessionServer` | `SessionServer` | `SessionServer` |
| Method names | `_session_loop` | `handleSession` | `sessionLoop` |
| Private fields | `self._codec` | `this.codec` | `s.codec` |
| Config types | `AsterConfig` | `ServerOptions` | `Config` |
| Decorators/annotations | `@service`, `@rpc` | `@Service`, `@Rpc` | struct tags |
| Error types | `RpcError` | `RpcError` | `RpcError` |
| Interceptor names | `DeadlineInterceptor` | `DeadlineInterceptor` | `DeadlineInterceptor` |
| Test helpers | `_make_session_pipes()` | `createSession()` | `makeSession()` |

## Security invariants (apply to every binding)

These are non-negotiable. A binding that skips any of these has a
security vulnerability:

1. **Error trailers use the client's codec** -- not the server's default
2. **EoI trailers must be status=OK** -- non-OK EoI = reject, not silent accept
3. **Bidi reader errors propagate** -- not converted to silent EOF
4. **Decompression capped at 16 MiB** -- streaming decompression, don't trust content-size
5. **Client-stream items capped at 100K** -- prevents memory exhaustion
6. **Handler timeout capped at 300s** -- even with no client deadline
7. **Metadata validated before dispatch** -- size, count, key/value lengths
8. **CANCEL produces CANCELLED trailer** -- unconditionally, always
9. **Nonce replay rejected** -- OTT credentials consumed exactly once
10. **Auth fires before dispatch** -- before reading request payload

## Flow documents

| Document | What it covers |
|----------|---------------|
| [transport_layer.md](transport_layer.md) | QUIC streams, ALPNs, framing, connection lifecycle |
| [codec_negotiation.md](codec_negotiation.md) | Codec sniffing, SerializationMode, per-stream invariant |
| [error_trailer_encoding.md](error_trailer_encoding.md) | Codec-match invariant, early-return paths, RpcStatus format |
| [consumer_admission_handshake.md](consumer_admission_handshake.md) | Credential loading, admission wire format, attribute bridging |
| [capability_interception.md](capability_interception.md) | Role-based access control, PeerAttributeStore, requires evaluation |
| [producer_accept_loop.md](producer_accept_loop.md) | Server ALPN routing, connection lifecycle, Gate 0 |
| [session_protocol.md](session_protocol.md) | Session opening, CALL framing, patterns, cancellation, deadlines |
| [publish_contracts.md](publish_contracts.md) | Contract identity, canonical encoding, registry publication |

## Reference implementations

| Language | Status | Notes |
|----------|--------|-------|
| Python | Production-ready | Reference implementation. 1025 tests pass. |
| TypeScript | Feature-complete | JSON-only codec. 58/58 matrix. |
| Java | In progress | FFI layer via Panama. |
| Go | In progress | FFI layer via cgo. |
