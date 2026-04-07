# Binding Parity Process

How we maintain feature parity across language bindings (Python, TypeScript, Java).

## Principles

1. **Spec is the source of truth.** The wire protocol is defined in `ffi_spec/` docs, not in any binding's code. Python is the reference implementation, but the spec is the authority.

2. **Conformance vectors catch regressions.** The `conformance/vectors/` directory contains canonical byte vectors (framing, XLANG serialization, contract identity hashes) that every binding must produce identically. CI fails if any binding diverges.

3. **Cross-language tests verify interop.** A Python server must work with a TS client (and vice versa). These tests run in CI on every push that touches binding code.

4. **BINDING_PARITY.md is the dashboard.** It tracks what each binding implements. Updated on every feature merge.

## When a Feature/Fix Lands in One Binding

### Step 1: Spec First

If the change affects wire protocol or RPC semantics:
- Update the relevant spec doc in `ffi_spec/`
- If it adds new frame flags, status codes, or canonical encoding rules, update conformance vectors

If the change is binding-internal (e.g., config resolution, health endpoint, logging), skip this step.

### Step 2: Implement in Origin Binding

Make the change in whichever binding you're working in. Add tests.

### Step 3: Update Conformance Vectors

If the change affects:
- **Wire framing** → update `conformance/vectors/framing.json`
- **XLANG serialization** → update `conformance/vectors/xlang-roundtrip.json`
- **Contract identity** → update `conformance/vectors/contract-identity.json`

Run the conformance tests for all bindings to verify no regressions.

### Step 4: File a Parity Issue

Create a GitHub issue:
- Title: `[parity] <feature> — port to <other bindings>`
- Label: `parity`
- Link to the commit/PR in the origin binding
- List which bindings need the change

### Step 5: Update BINDING_PARITY.md

Mark the feature as `done` in the origin binding, `—` in the others.

### Step 6: CI Catches Wire Regressions

Cross-language integration tests run automatically. If a Python change breaks TS interop (or vice versa), CI fails before merge.

## For Larger Features

Before implementing a significant feature (new interceptor, new streaming pattern, new trust mechanism):

1. Write a brief design note in `docs/_internal/` documenting the cross-binding implications
2. Identify which parts are wire-level (must be identical) vs binding-internal (can differ)
3. Add conformance vectors for the wire-level parts before implementing in any binding

## Conformance Test Suite

See `docs/_internal/conformance-suite.md` for details on the vector format, scenario format, and test harness.

## Binding-Specific Decisions

Some features are necessarily different across bindings:

| Area | Python | TypeScript | Java |
|------|--------|------------|------|
| Decorator syntax | `@service` / `@rpc` | `@Service` / `@Rpc` (TC39) | Annotations |
| Client stubs | `setattr` + metaclass | `Proxy` object | Interface codegen |
| Async model | `asyncio` | `Promise` / `async/await` | `CompletableFuture` |
| Config | TOML + env via `tomllib` | TOML + env via `cosmiconfig` | Properties + env |
| Logging | `logging` module | `pino` | SLF4J |
| Health server | `aiohttp` | `node:http` / `Bun.serve` | Netty / Jetty |

These differences are expected and encouraged — each binding should feel native to its ecosystem.

## What Must Be Identical

- Wire framing (4-byte LE length + 1-byte flags + payload)
- XLANG serialization format (Fory cross-language bytes)
- Contract identity hash (BLAKE3 of canonical bytes)
- Status codes (0–16, gRPC-compatible)
- StreamHeader / CallHeader / RpcStatus wire format
- Admission handshake protocol
- Session multiplexing protocol (CALL/CANCEL flags)
