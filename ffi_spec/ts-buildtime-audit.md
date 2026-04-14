# TS binding build-time lift: status and follow-ups

Scope: `bindings/typescript/packages/aster/src/` — runtime-dynamic
behavior that the `aster-gen` CLI lifts to build time via the
TypeScript compiler API. This document was originally an audit / plan;
it's now a status log describing what shipped and what's still
deferred.

## What shipped

The `aster-gen` scanner is in `src/cli/gen.ts`. It walks the user's
`tsconfig.json` program, finds `@Service` / `@WireType` /
`@Rpc` / `@ServerStream` / `@ClientStream` / `@BidiStream` classes,
resolves field and parameter types via `TypeChecker`, and emits a
single `rpc.generated.ts` file with:

- A `WIRE_TYPES` literal (topologically sorted, leaves first) carrying
  tag, ctor reference, full `WireFieldShape[]`, a pre-built
  `fieldNameSet` / `nestedTypes` / `elementTypes` trio for the JSON
  shape validator, and a placeholder `foryTypeInfo` field.
- A `SERVICES` literal carrying ctor reference, name/version/scope,
  ordered `methods` with `requestType` / `responseType` constructor
  references resolved from the AST (no more `@Rpc({ request, response })`
  boilerplate), `acceptsCtx` derived from parameter types (not
  `Function.length`), and per-method pre-derived `requestFields` /
  `responseFields` matching the `ManifestField` shape.

Users wire the generated file in once at startup:

```ts
import { AsterServer, registerGenerated } from '@aster-rpc/aster';
import { SERVICES, WIRE_TYPES } from './rpc.generated.js';

registerGenerated({ SERVICES, WIRE_TYPES });
const server = new AsterServer({ services: [new MissionControlService()] });
```

`registerGenerated` stamps `SERVICE_INFO_KEY` / `WIRE_TYPE_KEY` onto
each class constructor and populates the wire-shape registry + the
method-fields registry. From that point on, the existing runtime paths
(`ServiceRegistry.register`, `_buildManifest`, `JsonCodec.decode`
validation) consume the generated data instead of reflecting.

Running the scanner: `bunx aster-gen` (CLI), or via the
`@aster-rpc/aster/vite-plugin` / `@aster-rpc/aster/webpack-plugin`
wrappers that call the programmatic `generate()` API from
`src/cli/gen.ts`.

## Design decisions locked in this session

1. **Standalone TS CLI, not a Python shell-out.** The `cli/aster_cli/`
   Python command doesn't know about TS. The scanner lives inside
   `@aster-rpc/aster` as a `bin` entry so TS users install one package
   and get runtime + codegen + native transport. See discussion in
   `session-instructions-mode2-inline-params.md`.

2. **Runtime reflection stays as a fallback, with warn-once logs.**
   Gutting it would break all current users and force Fory JS to have
   a declarative schema form it doesn't yet expose. Every reflection
   site (`codec.ts:introspectClass`, `codec.ts:walkTypeGraph`,
   `runtime.ts:_buildManifest:extractFields`) now:
   - Prefers the generated registry (`getWireShape(cls)` /
     `getGeneratedMethodFields(service, version, method)`).
   - Falls back to the old `new cls()` + `Object.keys` path with a
     `console.warn` / `logger.warn` on first hit per class.
   - Carries a `TODO(aster-gen):` comment with a removal trigger
     pointing back to this document.

3. **Name-only decorator detection.** The scanner matches decorator
   identifiers (`Service`, `Rpc`, `WireType`, etc.) without trying to
   resolve them to `@aster-rpc/aster` — cleaner under path aliases,
   monorepo resolution, and when fixtures don't have a `node_modules`
   symlink. A user decorator with a colliding name would fail
   loudly at the type-mapping step, not silently corrupt output.

4. **Brand types (`src/brand.ts`) use plain string keys** (`readonly __asterBrand: 'i64'`)
   rather than a `unique symbol` so the scanner can detect them via
   `checker.getPropertyOfType(t, '__asterBrand')` from a context that
   doesn't have the same symbol in scope.

5. **Cyclic wire types are a hard error for v1.** The scanner walks
   the type graph with a DFS that detects cycles and throws
   `ScanError`. Proper SCC support (Rust core has it via
   `core/src/contract.rs:tarjan_scc`) would need the TS side to
   either mirror the algorithm or call into the NAPI binding per
   type — deferred until a real-world cycle shows up.

6. **Contract ID is left at the runtime path.** The TS runtime
   computes `contract_id` via
   `contractIdFromContract(fromServiceInfo(info))` which inlines
   zero-byte type hashes (identity.ts:213) — stable per service but
   not cross-language equivalent. The scanner **does not** recompute
   it. That's a separate piece: see "Follow-up: cross-language
   contract_id" below.

## Lift candidates from the original audit

Original numbering kept for cross-reference with commits.

### L1 + L3 + L7. Service prototype scan, `SERVICE_INFO_KEY` lookup, session instance factory — **DONE**

The scanner emits `SERVICES` with method metadata; `registerGenerated`
stamps `SERVICE_INFO_KEY` onto the constructor; the existing
`getServiceInfo()` + `ServiceRegistry.register()` path picks up the
populated info without any change. `SessionServer`'s
`serviceInfo.instance.constructor` factory path is untouched (still
works) and can be replaced with a generated `factory` reference in
a follow-up once session-scoped service usage grows.

### L2. `acceptsCtx` from `Function.length` — **DONE**

Scanner reads the second parameter's type from the AST. If it's
`CallContext` (matched by symbol name), `acceptsCtx: true` is emitted
as a literal in the generated `GeneratedMethodDef`. `Function.length`
is no longer consulted for generated services — it's still the
fallback when the decorator path runs alone.

The "emit two dispatcher thunks and drop the ternary" variant from
the original audit is **not done** and probably not worth it:
`handler.call(instance, req, ctx)` is already a direct call, the
branch is JIT-friendly, and there's no measurable hotspot here.

### L4. `walkTypeGraph` / `introspectClass` runtime reflection — **DONE (fallback preserved)**

`codec.ts:introspectClass` now checks `getWireShape(cls)` first. When
the user has run `aster-gen`, it returns the pre-built shape directly
— no `new cls()`, no default-value sniffing, no empty-array / nullable
nested / non-default-constructible limitations. When the scanner
hasn't been run, it falls back to the old path and logs a warning
once per class.

`walkTypeGraph` logs the same fallback warning; users who adopt
`registerGenerated({ ...WIRE_TYPES })` no longer need to call it
because Fory type registration goes through the generated path.

### L5. Fory `buildTypeInfo` callback — **DEFERRED**

The scanner emits `foryTypeInfo: null` in every `WireTypeShape`.
`registerGenerated` skips Fory registration for null entries, so
users wanting Fory still call `ForyCodec.registerTypeGraph(rootTypes,
buildTypeInfo)` at runtime (the status-quo path). Lifting this
requires the Fory JS binding to expose a declarative schema form
that the scanner can emit as a literal — today the runtime API takes
closures that construct typeInfo objects imperatively. Tracked in
"Follow-up: Fory JS declarative schema" below.

### L6. Per-call method lookup + pattern switch — **NOT DONE**

The dispatch path in `server.ts:275` still does
`svcInfo.methods.get(header.method)` on every call followed by a
four-way pattern switch. The generated file now carries enough
information to emit per-service dispatcher functions that switch on
method name directly, but the win is marginal (V8 JITs this
effectively), and the branch simplification would duplicate logic
between `server.ts` and `server2.ts`. Left for later if a profiler
ever flags it.

## Stays runtime (unchanged from original audit)

- `JsonCodec` JSON-walk shape validation (the *walk* is runtime; the
  shape is now generated)
- Interceptor chains
- Deadline race
- Scope/discriminator branching
- Peer attribute lookup
- Zstd compress/decompress decision

## Follow-ups

### Fory JS declarative schema

**Trigger:** Fory JS binding exposes a JSON-ish schema form that
doesn't require a user closure.

**Work:** scanner computes `foryTypeInfo` from the already-walked
`ScanField[]` graph, emits as a literal in `WIRE_TYPES`, and
`registerGenerated` feeds it to `ForyCodec.registerType` directly.
The existing `ForyCodec.registerTypeGraph(rootTypes, buildTypeInfo)`
callback path can then be deleted along with `walkTypeGraph`.

### Cross-language contract_id

**Trigger:** Any TS↔Python or TS↔Java interop test that needs the
same logical service to produce the same `contract_id` across
languages. (Not required today — dynamic clients read the manifest,
not the contract_id.)

**Work:** either (a) a new NAPI entry point
`compute_contract_id_from_service_and_types_json(...)` that takes a
full service description including a transitive `TypeDef` graph and
runs the Tarjan-SCC type resolution in Rust, or (b) port the
resolution logic from `bindings/python/aster/contract/identity.py` to
TS. (a) is strongly preferred — canonical bytes stay in one place.

### SCC handling for cyclic wire types

**Trigger:** First user with a recursive wire type graph
(`type Tree = { children: Tree[] }` or equivalent).

**Work:** the scanner's topo sort currently throws `ScanError` on
cycles. Either mirror `core/src/contract.rs:tarjan_scc` in TS
(medium effort) or let users annotate cycles with a
`@SelfRef('fqn')` marker that the scanner treats as a leaf. Blocked
on a concrete example that lets us pick the ergonomic shape.

### Server dispatch unification

**Trigger:** A design pass on `server.ts` vs `server2.ts` that
decides whether v1 stays or gets folded into v2.

**Work:** both files duplicate the `methods.get` + pattern switch +
`acceptsCtx` branching. Once a generated `dispatchService()` thunk
per service lands, it can be consumed by whichever server path
survives, removing duplication.

## Files touched this session

**New:**
- `src/brand.ts`
- `src/generated.ts`
- `src/cli/gen.ts`
- `src/plugins/vite.ts`
- `src/plugins/webpack.ts`
- `tests/fixtures/sample/*` (scanner fixture)
- `tests/typescript/unit/aster-gen.test.ts` (14 tests, all passing)

**Modified:**
- `src/codec.ts` — `introspectClass` / `walkTypeGraph` now prefer generated registry, warn on fallback
- `src/runtime.ts` — `_buildManifest` prefers `getGeneratedMethodFields`, warns on fallback
- `src/index.ts` — re-exports `brand`, `generated`, plugins
- `package.json` — `bin`, `exports` for vite-plugin / webpack-plugin / gen, TypeScript as optional peerDep

**Not modified (intentional):**
- `src/decorators.ts` — decorators still work standalone, scanner-agnostic
- `src/service.ts` / `src/session.ts` / `src/server.ts` / `src/server2.ts` — runtime dispatch consumes `ServiceInfo` via existing paths, the generated file just populates them differently
