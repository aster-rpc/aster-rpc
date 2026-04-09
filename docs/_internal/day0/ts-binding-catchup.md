# TypeScript Binding Catch-up Plan

**Date:** 2026-04-09  
**Goal:** Bring TS bindings to parity with Python changes from the past 48 hours,
then verify Chapter 7 of the Mission Control guide works.

## Changes already applied to TS (in this session)

- [x] `TicketCredential::Registry` -> `RegistryRead([u8; 32])` (native/src/ticket.rs)
- [x] Consumer admission: `registryTicket` -> `registryNamespace` (trust/consumer.ts)
- [x] Wire format: snake_case keys for Python interop (consumer.ts serialization)
- [x] `AsterClient.registryTicket` -> `registryNamespace` (high-level.ts)
- [x] Registry client docstrings updated

## Changes that need TS equivalents

### Core/Rust changes (already compiled into TS native)

| Change | Core file | TS native file | Status |
|--------|-----------|---------------|--------|
| `RegistryRead([u8; 32])` | core/src/ticket.rs | native/src/ticket.rs | Done |
| `download_collection_hash` | core/src/lib.rs | native/src/blobs.rs | Needs adding |
| `download_hash` | core/src/lib.rs | native/src/blobs.rs | Needs adding |
| `join_and_subscribe_namespace` | core/src/lib.rs | native/src/docs.rs | Needs adding |

### Framework changes

| Change | Python file | TS equivalent | Status |
|--------|-----------|--------------|--------|
| Dead code removal (bootstrap, rcan) | trust/*.py | trust/*.ts | Check if TS has same dead code |
| `PeerAttributeStore` | peer_store.py | NEW | Need to build |
| `build_call_context` with attributes | interceptors/base.py | server.ts | Need to add |
| `any_of` / `all_of` | capabilities.py | NEW | Need to build |
| `AsterClient.proxy()` | high_level.py | high-level.ts | Need to build |
| `ForyCodec.decode` generic unwrap | codec.py | codec.ts | Check if needed |
| `@rpc -> None` rejection | decorators.py | decorators.ts | Need to add |
| Graceful Ctrl+C shutdown | high_level.py | high-level.ts | Need to add |

### API docs

| Item | Python | TS | Status |
|------|--------|-----|--------|
| Public surface module | aster/public.py | Need equivalent | TODO |
| pdoc generation | docs/python/ | typedoc for docs/typescript/ | TODO |
| `@wire_type` gotchas | codec.py docstring | codec.ts JSDoc | TODO |

## Verification checklist for Chapter 7

Once TS bindings are updated:

1. Start Python Mission Control server (Chapter 1-4 control.py)
2. From TS: `AsterClient.connect("aster1...")`
3. Consumer admission succeeds (snake_case wire format)
4. `client.proxy("MissionControl")` returns proxy
5. `proxy.getStatus({ agent_id: "ts-1" })` returns correct response
6. `proxy.ingestMetrics(stream)` client streaming works
7. Fory XLANG types serialize correctly between TS and Python

## Priority order

1. **Add `proxy()` to TS AsterClient** -- needed for Chapter 7
2. **Verify admission wire format** -- snake_case keys E2E
3. **Add native methods** (download_collection_hash, join_and_subscribe_namespace)
4. **Add PeerAttributeStore + capability functions** -- needed for auth
5. **Generate TS API docs** with typedoc
6. **Cross-verify** Python and TS documented surfaces match

## Estimated effort

Items 1-2 are Day 0 blockers for Chapter 7 (~1 hour).
Items 3-4 are needed for feature parity (~2 hours).
Items 5-6 are docs polish (~30 min).
