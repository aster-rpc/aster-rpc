# Remaining Work for Java / Go / .NET (2026-04-13)

Snapshot taken after commits `be3cc7a` (registry async FFI) and `d8b19ea`
(per-language registry data models + pure-function FFI). Goal of this doc:
enumerate exactly what each non-Python binding still needs to reach
AsterServer / AsterClient parity with Python and TypeScript.

Python is the reference implementation; TypeScript is the current
work-in-progress sibling. The columns below reflect the state of the three
FFI-backed languages: Java, Go, .NET.

## Capability matrix

| # | Capability                                          | Python | TS  | Java     | Go       | .NET     |
|---|-----------------------------------------------------|--------|-----|----------|----------|----------|
| 1 | AsterConfig (TOML loading + env resolution)         | ✅     | ✅  | ✅       | ✅       | ✅       |
| 2 | Interceptors (9 interceptor types)                  | ✅     | ✅  | ✅       | ✅       | ✅       |
| 3 | Registry — pure-function FFI wrappers               | ✅     | —   | ✅       | ✅       | ✅       |
| 4 | Registry — async doc-backed FFI wrappers            | ✅\*   | —   | ❌       | ❌       | ❌       |
| 5 | Decorators `@rpc` / `@service` / `@stream`          | ✅     | ✅  | ❌       | ❌       | ❌       |
| 6 | Contract manifest submission to registry doc        | ✅     | ✅  | ❌       | ❌       | ❌       |
| 7 | Hooks (before_connect / after_connect wrappers)     | ✅     | ✅  | ❌       | ❌       | ❌       |
| 8 | Reactor wrapper (create / submit / poll / destroy)  | ✅     | ✅  | ✅       | ✅       | ✅       |
| 9 | AsterServer (endpoint + reactor + interceptors)     | ✅     | ✅  | partial  | partial  | partial  |
|10 | AsterClient (endpoint + resolve + interceptors)     | ✅     | ✅  | ❌       | ❌       | ❌       |
|11 | Session-scoped services                             | ✅     | ✅  | ❌       | ❌       | ❌       |
|12 | Fory codec wired in                                 | ✅ v0.16 | ✅ | ❌       | ❌       | ❌       |
|13 | JSON / raw-bytes codec fallback                     | ✅     | ✅  | ❌       | ❌       | ❌       |

\* Python registry layer currently uses its own async doc I/O; it has
  not yet been switched over to the new `aster_registry_resolve` /
  `_publish` / `_renew_lease` / `_acl_*` async FFI ops landed in
  `be3cc7a`. That switchover is item C in
  `session-instructions-registry-rust.md` and is a separate work item.

"partial" for AsterServer = the class exists and ties endpoint + reactor
together, but it does not yet drive registry publish, contract manifest
submission, or session-scoped routing because those pieces are missing.

## Cross-cutting prerequisites (do these once, all bindings benefit)

- **Fory v0.16 dependency.** Python uses `pyfory` 0.16.x; Java/Go/.NET
  must use the same major to keep wire compatibility. Add:
  - Java: `org.apache.fory:fory-core:0.16.x` to `bindings/java/pom.xml`.
  - Go: `github.com/apache/fory/go/fory` (pin to v0.16 tag).
  - .NET: `Apache.Fory.NET` v0.16.x to `bindings/dotnet/src/Aster/Aster.csproj`.
  - Once added, write a small `Codec` indirection (Fory by default,
    raw-bytes / JSON for tests) and route AsterServer/Client through it.

- **Decorators (`@rpc`, `@service`, `@stream`).** These exist in
  Python (`bindings/python/aster/decorators.py`) and were just landed in
  TypeScript. They are the gating dependency for items 6, 9 (full),
  10, and 11 — without a way to declare contracts in the host language,
  there is nothing to publish, nothing to route, and nothing to scope a
  session to. Build these in each language before AsterServer/Client.

- **Contract manifest submission.** The Rust core
  (`core/src/contract.rs`) already builds the canonical manifest and
  ContractIdentity. Each binding only needs:
  1. A way to gather decorated services into a manifest (depends on
     decorators).
  2. A call to `aster_registry_publish` (now exposed as of `be3cc7a`)
     with the artifact JSON for that manifest.
  No new FFI symbols are required.

## Per-binding work to reach Day 0

The order below is intentional — earlier items unblock later ones.

### Java (`bindings/java/`)
1. Add Fory v0.16 dependency to `pom.xml`; create
   `com.aster.codec.Codec` indirection.
2. Add `IrohLibrary` method handles for the six new
   `aster_registry_*` async ops (`resolve`, `publish`, `renew_lease`,
   `acl_add_writer`, `acl_remove_writer`, `acl_list_writers`) and
   high-level wrappers in `com.aster.registry.Registry` that submit the
   op and pump the event queue for kinds 80–84.
3. Add hook wrappers around `iroh_hook_before_connect_respond` /
   `iroh_hook_after_connect_respond` and a `HookReceiver`-equivalent
   that surfaces the events from `IROH_EVENT_HOOK_BEFORE_CONNECT` /
   `_AFTER_CONNECT` to user code.
4. Build `@Rpc` / `@Service` / `@Stream` annotation processors (or
   runtime reflection scanners) that produce a contract manifest and
   the per-method dispatch table.
5. Wire contract manifest submission into `AsterServer.start()`.
6. Build `AsterClient`: endpoint + connection cache + registry resolve
   + interceptor chain + Fory codec.
7. Add session-scoped service support (depends on decorators).

### Go (`bindings/go/`)
1. Add Fory v0.16 to `go.mod`; create `aster/codec` package.
2. Add cgo bindings in `bindings/go/registry_ffi.go` for the six new
   async ops, plus high-level wrappers in `bindings/go/registry.go`.
3. Add hook wrappers (cgo + Go-side channels for the event-driven
   model).
4. Build the decorator equivalent — Go has no annotations, so use
   struct tags + a `RegisterService(svc, ContractMeta{...})` builder.
5. Wire contract manifest submission into the existing Go AsterServer
   (`bindings/go/server.go`).
6. Build `AsterClient`.
7. Add session-scoped service support.

### .NET (`bindings/dotnet/src/Aster/`)
1. Add Fory v0.16 NuGet package; create `Aster.Codec` namespace.
2. Add `Native.cs` declarations for the six new async ops and
   high-level wrappers in `Aster.Registry.Registry`.
3. Add hook wrappers and surfacing of hook invocation events.
4. Build `[Rpc]` / `[Service]` / `[Stream]` attributes plus a source
   generator (or reflection scanner) that produces contract manifests.
5. Wire contract manifest submission into `AsterServer.cs`.
6. Build `AsterClient.cs`.
7. Add session-scoped service support.

## Suggested execution order across the three bindings

1. **Fory dependency** in all three (mechanical, parallelizable).
2. **Async registry FFI wiring** in all three (mechanical, mirrors what
   already exists for the pure-function ops).
3. **Hook wrappers** in all three (the FFI is already there; needs
   binding glue only).
4. **Decorators** in all three — biggest design call, build one first
   (Java is probably the cleanest reference because annotation
   processing is a well-trodden path), then mirror in Go and .NET.
5. **Contract manifest submission** — falls out almost free once
   decorators land, since it's just gather-and-publish.
6. **AsterClient** in all three.
7. **Session-scoped services** in all three.
8. (Separately) Python switchover to the async registry FFI ops, then
   delete the per-language Python registry modules — item C in
   `session-instructions-registry-rust.md`.

## What is explicitly NOT blocked / needed

- No new FFI surface beyond what landed in `be3cc7a`. Everything in this
  doc is binding-side glue + host-language ergonomics.
- No new core Rust work. `core::registry`, `core::contract`,
  `core::reactor`, hooks, and the FFI bridge already expose everything
  the bindings need.
- No spec changes. The mandatory filter rules, key schema, and gossip
  event types are all settled in `core/src/registry.rs` and tested.
