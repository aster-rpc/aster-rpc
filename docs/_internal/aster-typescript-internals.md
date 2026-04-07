# Aster TypeScript Binding — Internal Engineering Guide

How the TypeScript binding works, how to extend it, and how it maps to the Python implementation.

## Architecture

Same two-layer pattern as Python:

```
@aster-rpc/aster          Pure TypeScript (~3500 lines, 33 source files)
    |                      Decorators, client, server, interceptors,
    |                      framing, protocol types, contract identity
    |
@aster-rpc/transport      NAPI-RS native addon (~530 lines Rust, 7 files)
    |                      IrohNode, IrohConnection, streams,
    |                      BlobsClient, DocsClient, GossipClient
    |
aster_transport_core       Shared Rust crate (same as Python binding)
```

## Key Design Decisions (vs Python)

| Aspect | Python | TypeScript | Rationale |
|--------|--------|-----------|-----------|
| Decorators | `@service`, `@rpc` (Python metaclass) | `@Service`, `@Rpc` (TC39 Stage 3) | TC39 decorators compile away, no runtime support needed |
| Metadata storage | `__aster_service_info__` attribute | `Symbol.for('aster.service_info')` | Symbols avoid name collisions |
| Client stubs | `setattr` + metaclass | `Proxy` object | Full IDE type inference in TS |
| Async model | `asyncio` | `Promise` / `async/await` | Native JS patterns |
| Streaming | `AsyncIterator` / `AsyncGenerator` | Same (native in JS) | Direct mapping |
| Wire types | `@dataclass` + `@wire_type` | Class + `@WireType` | TS classes with constructor init pattern |
| Serialization | pyfory (XLANG) | `@apache-fory/core` (XLANG) | Same Fory XLANG wire format |
| Config | `tomllib` + env | env vars (cosmiconfig planned) | Minimal for alpha |

## Module Map

| Module | Python equivalent | Lines | Purpose |
|--------|------------------|-------|---------|
| `status.ts` | `status.py` | 195 | StatusCode, RpcError hierarchy |
| `types.ts` | `types.py` | 60 | SerializationMode, RpcPattern, RPC_ALPN |
| `limits.ts` | `limits.py` | 160 | Security constants + validators |
| `framing.ts` | `framing.py` | 190 | Wire framing (4-byte LE + flags + payload) |
| `protocol.ts` | `protocol.py` | 65 | StreamHeader, CallHeader, RpcStatus |
| `codec.ts` | `codec.py` | 80 | JsonCodec (working), ForyCodec (scaffold) |
| `decorators.ts` | `decorators.py` | 190 | @Service, @Rpc, @ServerStream, etc. |
| `service.ts` | `service.py` | 120 | ServiceInfo, MethodInfo, ServiceRegistry |
| `client.ts` | `client.py` | 110 | Proxy-based client stubs |
| `transport/base.ts` | `transport/base.py` | 75 | AsterTransport interface |
| `transport/local.ts` | `transport/local.py` | 140 | In-process transport |
| `transport/iroh.ts` | `transport/iroh.py` | 200 | QUIC transport |
| `interceptors/*.ts` | `interceptors/*.py` | 550 | 9 interceptors |
| `contract/*.ts` | `contract/*.py` | 200 | Canonical encoding, BLAKE3 |
| `session.ts` | `session.py` | 100 | Session multiplexing |
| `config.ts` | `config.py` | 110 | Environment-based config |
| `logging.ts` | `logging.py` | 130 | Structured logging |
| `health.ts` | `health.py` | 115 | HTTP health endpoints |
| `high-level.ts` | `high_level.py` | 120 | AsterServer, AsterClient |

## NAPI-RS Layer

Mirrors `bindings/python/rust/src/` exactly:

| NAPI file | PyO3 equivalent | Wraps |
|-----------|----------------|-------|
| `node.rs` | `node.rs` | CoreNode (memory, persistent, accept, clients) |
| `net.rs` | `net.rs` | CoreConnection, CoreSendStream, CoreRecvStream |
| `blobs.rs` | `blobs.rs` | CoreBlobsClient |
| `docs.rs` | `docs.rs` | CoreDocsClient, CoreDoc |
| `gossip.rs` | `gossip.rs` | CoreGossipClient, CoreGossipTopic |
| `error.rs` | `error.rs` | Error mapping |

Key difference from PyO3: NAPI uses `#[napi]` attributes instead of `#[pymethods]`, returns `Promise<T>` instead of `PyResult<Bound<'py, PyAny>>`, and uses `Buffer` instead of `&[u8]`.

## Testing

113 tests across 7 files:

| Test file | Tests | Covers |
|-----------|-------|--------|
| `framing.test.ts` | 20 | Encode/decode, conformance vectors, stream I/O |
| `status.test.ts` | 10 | StatusCode values, RpcError, fromStatus() |
| `limits.test.ts` | 15 | Constants, validateHexField, validateMetadata |
| `decorators.test.ts` | 15 | @Service, @Rpc, ServiceRegistry |
| `transport.test.ts` | 20 | LocalTransport (all 4 patterns), createClient |
| `interceptors.test.ts` | 23 | All 9 interceptors, chain execution |
| `contract.test.ts` | 10 | Canonical encoding, determinism |

Run: `bun vitest run` (from `bindings/typescript/packages/aster/`)

## ForyCodec Status

The `ForyCodec` class is scaffolded but not implemented. The `JsonCodec` is used for testing.

Full implementation requires:
1. Validate `@apache-fory/core` v0.17.0-alpha.0 XLANG wire compatibility with Python's pyfory
2. Build conformance vectors in `conformance/vectors/xlang-roundtrip.json`
3. Implement type registration using Fory's `Type.*()` builder API
4. Handle zstd compression (threshold: 4KB)

## Extending

### Adding a new interceptor

1. Create `interceptors/my-interceptor.ts` implementing the `Interceptor` interface
2. Export from `index.ts`
3. Add tests in `tests/unit/interceptors.test.ts`

### Adding a new transport

1. Create `transport/my-transport.ts` implementing `AsterTransport`
2. The WASM transport (Phase 8) will follow this pattern
3. Export from `index.ts`
