# Remaining Work for Java / Go / .NET (2026-04-13)

Snapshot taken after the wide-and-shallow evening pass (commits `8d8f136`
through `e46a894`). Goal of this doc: enumerate exactly what each
non-Python binding still needs to reach AsterServer / AsterClient parity
with Python and TypeScript.

Python is the reference implementation; TypeScript is the current
work-in-progress sibling. The columns below reflect the state of the three
FFI-backed languages: Java, Go, .NET.

## Capability matrix

| # | Capability                                          | Python | TS  | Java     | Go       | .NET     |
|---|-----------------------------------------------------|--------|-----|----------|----------|----------|
| 1 | AsterConfig (TOML loading + env resolution)         | ✅     | ✅  | ✅       | ✅       | ✅       |
| 2 | Interceptors (9 interceptor types)                  | ✅     | ✅  | ✅       | ✅       | ✅       |
| 3 | Registry — pure-function FFI wrappers               | ✅     | —   | ✅       | ✅       | ✅       |
| 4 | Registry — async doc-backed FFI wrappers            | ✅\*   | —   | ✅       | ✅       | ✅       |
| 5 | Decorators `@rpc` / `@service` / `@stream`          | ✅     | ✅  | ❌       | ❌       | ❌       |
| 6 | Contract manifest submission to registry doc        | ✅     | ✅  | ❌       | ❌       | ❌       |
| 7 | Hooks (before_connect / after_connect wrappers)     | ✅     | ✅  | ✅\*\*   | ✅\*\*   | ✅\*\*   |
| 8 | Reactor wrapper (create / submit / poll / destroy)  | ✅     | ✅  | ✅       | ✅       | ✅       |
| 9 | AsterServer (endpoint + reactor + interceptors)     | ✅     | ✅  | partial  | partial  | partial  |
|10 | AsterClient (endpoint + resolve + interceptors)     | ✅     | ✅  | ❌       | ❌       | ❌       |
|11 | Session-scoped services                             | ✅     | ✅  | ❌       | ❌       | ❌       |
|12 | Fory codec wired in                                 | ✅ v0.16 | ✅ | ✅ v0.16 | ✅ v0.16 | ✅ v0.16 |
|13 | JSON / raw-bytes codec fallback                     | ✅     | ✅  | ✅ raw   | ✅ raw   | ✅ raw   |

\* Python registry layer currently uses its own async doc I/O; it has
  not yet been switched over to the new `aster_registry_resolve` /
  `_publish` / `_renew_lease` / `_acl_*` async FFI ops landed in
  `be3cc7a`. That switchover is item C in
  `session-instructions-registry-rust.md` and is a separate work item.

\*\* Hook wrappers ship as the minimum FFI release path only:
  `Hooks.RespondBeforeConnect` / `RespondAfterConnect` (.NET),
  `IrohHook.respond{Before,After}Connect` (Java), and
  `RespondBeforeConnect` / `RespondAfterConnect` (Go). The actual
  subscribe + dispatch loop that turns `IROH_EVENT_HOOK_*` events into
  user callbacks is left for AsterServer to wire on top of these
  primitives — the hook surface area beyond the respond functions
  overlaps with what AsterServer needs anyway.

"partial" for AsterServer = the class exists and ties endpoint + reactor
together, but it does not yet drive registry publish, contract manifest
submission, hook dispatch, or session-scoped routing because those
pieces are missing.

## Cross-cutting prerequisites (do these once, all bindings benefit)

- **Fory v0.16 dependency.** ✅ DONE in `8d8f136`. All three bindings
  declare Apache Fory 0.16:
  - Java: `org.apache.fory:fory-core:0.16.0` in `pom.xml`.
  - Go: `github.com/apache/fory/go/fory v0.16.0` in `go.mod`.
  - .NET: `Apache.Fory 0.16.0` in `Aster.csproj`.
  Codec indirection landed separately in `e46a894` (see item 12/13
  notes below).

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
  2. A call to `aster_registry_publish` (exposed as of `be3cc7a`,
     now wrapped per-binding as of `7fb4b3d`) with the artifact JSON
     for that manifest.
  No new FFI symbols are required.

## Done in the wide-and-shallow evening pass (2026-04-13)

The four mechanical items below landed across Java/Go/.NET in one
session, in the order shown. Each one is its own commit so they can be
reviewed independently. None of them required any design call — the
shape was dictated by the existing FFI surface and the Python/TS
references.

1. **`8d8f136` — Fory v0.16 dependency.** Three lockfile/manifest edits.
2. **`7fb4b3d` — Async doc-backed registry FFI wrappers.** ~1200 lines
   across the three bindings: native method handles, high-level Registry
   async classes (`RegistryAsync.java`, `RegistryAsync.cs`, new methods
   on `Doc` in Go), event-pump dispatch for kinds 80–84. Fully wraps
   resolve / publish / renew_lease / acl_add_writer / acl_remove_writer
   / acl_list_writers. The persistent `ResolveState` on the bridge
   means round-robin rotation and stale-seq filtering survive across
   calls — bindings do not need their own state.
3. **`92f7297` — Hook responder wrappers.** Native declarations of
   `iroh_hook_before_connect_respond` / `iroh_hook_after_connect_respond`
   plus thin host-language helpers (`Hooks.cs`, `IrohHook.java`,
   `hooks.go`). The actual event subscribe + dispatch loop is left for
   AsterServer to wire on top.
4. **`e46a894` — Minimal Codec indirection.** A `Codec` interface plus
   `RawBytesCodec` (pass-through, mode `"raw"`) and `ForyCodec` (Apache
   Fory v0.16 with `xlang=true`, mode `"fory-xlang"`) per binding. The
   underlying `Fory` instance is exposed via a public accessor so
   downstream decorators can register types via the language-native
   API surface.

### Caveats / known follow-ups from the evening pass

- **`.NET ForyCodec.Decode` uses reflection.** It dispatches to the
  generic `Fory.Deserialize<T>(byte[])` overload via
  `MethodInfo.MakeGenericMethod(...).Invoke(...)`. If
  `Apache.Fory v0.16` exposes a non-generic
  `object Deserialize(byte[], Type)` (or similar), the reflection
  dispatch in `bindings/dotnet/src/Aster/Codec/ForyCodec.cs` should be
  replaced before any hot path (e.g. AsterClient request encode/decode)
  starts calling it. Cost of the reflection lookup adds up at RPC rates.
- **No `JsonCodec` shipped tonight.** The matrix shows item 13 as
  "raw" only — JSON fallback is still missing in all three bindings.
  Cheap to add (System.Text.Json / Jackson / encoding/json), but not
  on tonight's path.
- **The .NET / Java FFM ABI question** raised during the evening pass
  (whether `iroh_bytes_t` passed by value via `(ADDRESS, JAVA_LONG)`
  pairs lines up with the SysV register convention) is settled for the
  new async registry ops — they follow the same pattern as the existing
  `aster_registry_filter_and_rank` handle, which builds and works
  cleanly. Worth a once-over of the older `iroh_doc_*` Java handles
  that use three consecutive `ADDRESS` arguments for three consecutive
  `iroh_bytes_t` parameters; that pattern is potentially ABI-fragile
  on x86_64 SysV but is out of scope for this doc.

## Per-binding work to reach Day 0

The order below is intentional — earlier items unblock later ones.
Items struck through landed in the wide-and-shallow evening pass; the
remaining items all depend on decorators landing first.

### Java (`bindings/java/`)
1. ~~Add Fory v0.16 dependency to `pom.xml`; create
   `com.aster.codec.Codec` indirection.~~ ✅ `8d8f136` (dep) +
   `e46a894` (`com.aster.codec.{Codec,RawBytesCodec,ForyCodec}`).
2. ~~Add `IrohLibrary` method handles for the six new
   `aster_registry_*` async ops (`resolve`, `publish`, `renew_lease`,
   `acl_add_writer`, `acl_remove_writer`, `acl_list_writers`) and
   high-level wrappers in `com.aster.registry.Registry` that submit the
   op and pump the event queue for kinds 80–84.~~ ✅ `7fb4b3d`. Lives in
   `com.aster.registry.RegistryAsync`; uses CompletableFuture.
3. ~~Add hook wrappers around `iroh_hook_before_connect_respond` /
   `iroh_hook_after_connect_respond`~~ ✅ `92f7297`
   (`com.aster.hooks.IrohHook`). The `HookReceiver`-equivalent that
   surfaces `IROH_EVENT_HOOK_BEFORE_CONNECT` / `_AFTER_CONNECT` events
   to user callbacks is still pending — to be wired into AsterServer.
4. Build `@Rpc` / `@Service` / `@Stream` annotation processors (or
   runtime reflection scanners) that produce a contract manifest and
   the per-method dispatch table.
5. Wire contract manifest submission into `AsterServer.start()`.
6. Build `AsterClient`: endpoint + connection cache + registry resolve
   + interceptor chain + Fory codec.
7. Add session-scoped service support (depends on decorators).

### Go (`bindings/go/`)
1. ~~Add Fory v0.16 to `go.mod`; create `aster/codec` package.~~ ✅
   `8d8f136` (dep) + `e46a894` (`bindings/go/codec.go` with
   `Codec`, `RawBytesCodec`, `ForyCodec`).
2. ~~Add cgo bindings in `bindings/go/registry_ffi.go` for the six new
   async ops, plus high-level wrappers in `bindings/go/registry.go`.~~
   ✅ `7fb4b3d`. The async wrappers are methods on `Doc`
   (`ResolveAsync`, `PublishAsync`, `RenewLeaseAsync`,
   `AclAddWriterAsync`, `AclRemoveWriterAsync`, `AclListWritersAsync`)
   in `bindings/go/registry_ffi.go`.
3. ~~Add hook wrappers~~ ✅ `92f7297` (`bindings/go/hooks.go`). The
   Go-side channel-based event subscribe + dispatch loop is still
   pending — to be wired into the existing Go `AsterServer`.
4. Build the decorator equivalent — Go has no annotations, so use
   struct tags + a `RegisterService(svc, ContractMeta{...})` builder.
5. Wire contract manifest submission into the existing Go AsterServer
   (`bindings/go/server.go`).
6. Build `AsterClient`.
7. Add session-scoped service support.

### .NET (`bindings/dotnet/src/Aster/`)
1. ~~Add Fory v0.16 NuGet package; create `Aster.Codec` namespace.~~ ✅
   `8d8f136` (dep) + `e46a894` (`Aster.Codec.{ICodec,RawBytesCodec,
   ForyCodec}`). See caveat above re: reflection-based generic
   dispatch in `ForyCodec.Decode`.
2. ~~Add `Native.cs` declarations for the six new async ops and
   high-level wrappers in `Aster.Registry.Registry`.~~ ✅ `7fb4b3d`
   (`Aster.Registry.RegistryAsync`, exposed as a `static partial class`
   sibling of `Registry`).
3. ~~Add hook wrappers~~ ✅ `92f7297` (`Aster.Hooks` static class with
   `HookDecision` enum). Surfacing hook invocation events to user
   callbacks is still pending — to be wired into `AsterServer.cs`.
4. Build `[Rpc]` / `[Service]` / `[Stream]` attributes plus a source
   generator (or reflection scanner) that produces contract manifests.
5. Wire contract manifest submission into `AsterServer.cs`.
6. Build `AsterClient.cs`.
7. Add session-scoped service support.

## Suggested execution order across the three bindings

1. ~~**Fory dependency** in all three.~~ ✅ `8d8f136`
2. ~~**Async registry FFI wiring** in all three.~~ ✅ `7fb4b3d`
3. ~~**Hook wrappers** in all three (FFI release path only).~~ ✅
   `92f7297`. Hook event dispatch loop still to come with AsterServer.
4. **Decorators** in all three — biggest design call, build one first
   (Java is probably the cleanest reference because annotation
   processing is a well-trodden path), then mirror in Go and .NET.
   **← next up.**
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
