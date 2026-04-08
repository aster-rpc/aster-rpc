# WASM Consumer Transport — Design Plan

Status: **exploratory / not started**
Date: 2025-04-08

## Goal

Enable browser-based (WASM) consumers to connect to Aster services via iroh's relay transport. Consumers need: RPC calls, contract/service discovery (docs), and schema fetching (blobs).

## Context

- iroh 0.33+ supports WASM compilation (relay-only, no direct QUIC/UDP in browser)
- The TS binding already has a transport abstraction (`AsterTransport` in `packages/aster/src/transport/base.ts`) with `IrohWasmTransport` explicitly planned as "Phase 8"
- The entire TS client stack (client proxy, codec, framing, decorators, service metadata) is transport-agnostic and would work unchanged with a WASM transport

## Architecture

```
Browser consumer
  ├── Aster client (existing TS — client.ts, codec.ts, framing.ts)
  ├── IrohWasmTransport (new — transport/wasm.ts, ~100-200 lines)
  │     └── iroh WASM endpoint (wasm-bindgen/wasm-pack)
  └── Relay connection to service node
```

## Key Challenge: Docs + Blobs in WASM

Consumers need docs and blobs for service discovery (finding contracts, fetching schemas). However iroh's WASM support targets the networking layer — docs and blobs depend on:

- Local storage backends (replica store for docs, content store for blobs)
- Doc sync protocol
- Blob download/verification

These almost certainly don't have browser-compatible storage backends (IndexedDB, etc.) in iroh's WASM build.

### Recommended approach: Discovery-as-a-Service

Instead of running native iroh docs/blobs in the browser, expose discovery as an Aster RPC service:

```
Browser consumer
  └── AsterClient → RegistryService.getContract("EchoService")
                   → RegistryService.listServices()
                   → RegistryService.getSchema(hash)
```

- A `RegistryService` (Python or TS, running on a real node) wraps docs+blobs access behind standard Aster RPC methods
- Browser consumers discover services via RPC calls over the same relay transport they'll use for actual service calls
- No need for iroh docs/blobs runtime in the browser at all
- Falls back naturally to the existing Aster client stack

### Alternative: native WASM docs+blobs

If iroh eventually ships browser-compatible storage backends, the consumer could sync docs and download blobs directly. This would be more decentralized but significantly more complex. Revisit if/when iroh's WASM surface matures.

## Implementation Steps

### Phase 1: Spike — iroh WASM viability (~half day)
- Attempt to compile iroh with `--target wasm32-unknown-unknown`
- Confirm `Endpoint::connect`, `SendStream`, `RecvStream` are available
- Document which APIs are/aren't exposed

### Phase 2: Glue crate (~half day)
- Small Rust crate with `wasm-bindgen` exposing iroh endpoint + streams to JS
- Build with `wasm-pack`

### Phase 3: IrohWasmTransport (~1 day)
- Implement `AsterTransport` in `packages/aster/src/transport/wasm.ts`
- Mirror `iroh.ts` structure but backed by WASM API
- Relay-only connections

### Phase 4: RegistryService (~1 day)
- Define `RegistryService` contract (list services, get contract, get schema)
- Implement server-side in Python (wraps docs+blobs)
- Browser consumer calls it via standard Aster RPC

### Phase 5: End-to-end test (~half day)
- Browser/WASM consumer → relay → Python Aster service
- Discover service via RegistryService, then call it

## Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| iroh WASM API surface incomplete or unstable | Blocks Phase 1 | Spike first before investing further |
| iroh WASM doesn't expose stream API cleanly | Painful glue code | Check 0.33 release notes / source |
| wasm-pack build pipeline complexity | Time sink | Keep glue crate minimal |
| Relay latency for discovery + RPC | UX | Cache contract metadata client-side after first fetch |

## Effort Estimate

- Proof of concept (unary RPC only, no discovery): ~2 days
- Full consumer with RegistryService discovery: ~4-5 days
- Depends heavily on iroh WASM API maturity (Phase 1 spike de-risks this)
