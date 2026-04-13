# Remaining Work for Java / Go / .NET (2026-04-13)

Snapshot taken after the wide-and-shallow evening pass (commits `8d8f136`
through `e46a894`). Goal of this doc: enumerate exactly what each
non-Python binding still needs to reach AsterServer / AsterClient parity
with Python and TypeScript.

Python is the reference implementation; TypeScript is the current
work-in-progress sibling. The columns below reflect the state of the three
FFI-backed languages: Java, Go, .NET.

## Capability matrix

| # | Capability                                          | Python | TS  | Java          | Go       | .NET     |
|---|-----------------------------------------------------|--------|-----|---------------|----------|----------|
| 1 | AsterConfig (TOML loading + env resolution)         | ✅     | ✅  | ✅            | ✅       | ✅       |
| 2 | Interceptors (9 interceptor types)                  | ✅     | ✅  | ✅            | ✅       | ✅       |
| 3 | Registry — pure-function FFI wrappers               | ✅     | —   | ✅            | ✅       | ✅       |
| 4 | Registry — async doc-backed FFI wrappers            | ✅\*   | —   | ✅            | ✅       | ✅       |
| 5 | Decorators `@rpc` / `@service` / `@stream`          | ✅     | ✅  | ✅\*\*\*      | ❌       | ❌       |
| 6 | Contract manifest submission to registry doc        | ✅     | ✅  | ❌            | ❌       | ❌       |
| 7 | Hooks (before_connect / after_connect wrappers)     | ✅     | ✅  | ✅\*\*        | ✅\*\*   | ✅\*\*   |
| 8 | Reactor wrapper (create / submit / poll / destroy)  | ✅     | ✅  | ✅            | ✅       | ✅       |
| 9 | AsterServer (endpoint + reactor + interceptors)     | ✅     | ✅  | ✅\*\*\*\*\*  | partial  | partial  |
|10 | AsterClient (endpoint + resolve + interceptors)     | ✅     | ✅  | ✅\*\*\*\*\*  | ❌       | ❌       |
|11 | Session-scoped services                             | ✅     | ✅  | ✅            | ❌       | ❌       |
|12 | Fory codec wired in                                 | ✅ v0.16 | ✅ | ✅ v0.16      | ✅ v0.16 | ✅ v0.16 |
|13 | JSON / raw-bytes codec fallback                     | ✅     | ✅  | ✅ raw        | ✅ raw   | ✅ raw   |

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

\*\*\* Java decorators landed via the `aster-annotations` module
  (`@Rpc`, `@Service`, `@Scope`, `@ServerStream`, `@ClientStream`,
  `@BidiStream`) plus the `aster-codegen-apt` annotation processor,
  which scans the source set and emits a `ServiceDispatcher`
  implementation per `@Service` class. Caveat: `DispatcherEmitter`
  currently emits real bodies for `UNARY` only — server/client/bidi
  stream dispatchers still fall back to `throw new
  UnsupportedOperationException(...)`. Real streaming bodies are open
  work, blocked on the same read-side reactor mpsc that gates
  client-stream / bidi runtime support.

\*\*\*\* (Superseded by \*\*\*\*\* below — left in place so older commit
  messages still parse.)

\*\*\*\*\* Java AsterServer / AsterClient now cover ALL FOUR RPC
  shapes end-to-end: unary, server-streaming, client-streaming,
  and bidi-streaming. Read-side support landed in `a148efb` (Rust +
  FFI: per-stream frame reader task, `aster_reactor_recv_frame`
  entry point, `FLAG_END_STREAM = 0x40` wire flag) and `07a2b4c`
  (Java side: `Reactor.recvFrame` FFM wrapper,
  `ReactorRequestStream` SPI helper, `AsterServer` ClientStream /
  BidiStream wiring, `AsterClient.callClientStream` /
  `callBidiStream`). The Mission Control example exercises every
  shape (8/8 E2E tests green: 3 unary + 1 server-stream + 1
  server-stream-with-filter + 1 client-stream + 1 bidi + the two
  session-scope variants). Caveats: bidi is BUFFERED — all requests
  are sent before any response is read (sufficient for ping-pong
  services like `runCommand` because the server's send-side mpsc
  queues responses until the client drains, but not true
  interleaved bidi). True interleaved bidi needs a `BidiCall<Req,
  Resp>` API with explicit `send` / `complete` plus a
  `Flow.Publisher<Resp>` for incremental response delivery.
  Manifest publish + hook event dispatch are still pending.

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
4. ~~Build `@Rpc` / `@Service` / `@Stream` annotation processors~~ ✅
   commits B/C/D + `f8de78d` G.1. `aster-annotations` defines the
   six annotation types, `aster-codegen-apt` scans the source set
   via `AsterAnnotationProcessor` and emits one
   `{Service}$AsterDispatcher` per `@Service`, and
   `aster-codegen-ksp` is scaffolded for the future Kotlin path.
   Open follow-up: `DispatcherEmitter` only emits real bodies for
   `UNARY`; SERVER_STREAM / CLIENT_STREAM / BIDI_STREAM each emit a
   stub that throws `UnsupportedOperationException`. Hand-written
   dispatchers (the Mission Control sample, the Echo test fixture)
   stand in until real streaming bodies are emitted.
5. Wire contract manifest submission into `AsterServer.start()`. The
   manifest is built at startup and exposed via `AsterServer.manifest()`
   but not yet published to the registry doc — see the doc comment on
   `AsterServer` itself. **Still open.**
6. ~~Build `AsterClient`: endpoint + connection cache + registry resolve
   + interceptor chain + Fory codec.~~ ✅ commits F + `f8de78d`
   G.1/G.2-core. Endpoint + connection cache + Fory codec are wired,
   unary `call(...)` and server-stream `callServerStream(...)` are
   end-to-end green over real QUIC. Open follow-ups: registry-resolve
   short-circuit (caller currently passes a `NodeAddr` directly),
   client-stream + bidi invocation (gated on the read-side reactor
   mpsc described below), and a `Flow.Publisher` variant of
   `callServerStream` that delivers frames incrementally instead of
   buffering into a `List<Resp>`.
7. ~~Add session-scoped service support~~ ✅ `f8de78d` G.2-core.
   `SessionKey` is `(peerId, streamId, implClass)`, the reactor's
   `aster_reactor_call_t` carries `stream_id` (layout grew 80→88),
   `AsterServer.Builder.sessionService(implClass, factory)` registers
   per-call factories, and `InMemorySessionRegistryTest` proves
   concurrent sessions from the same peer no longer collapse onto one
   instance.

#### Java open items not in the original list (added 2026-04-13)

These came up during the G.1 / G.2-core / Mission Control push and
are tracked here so the next session can pick one up cold:

- ~~**Reactor read-side mpsc + client-stream / bidi-stream support.**~~
  Landed 2026-04-13 across two commits: `a148efb` (Rust + FFI)
  and `07a2b4c` (Java side + MC parity). New `FLAG_END_STREAM = 0x40`
  wire flag, per-stream frame reader task in `core/src/reactor.rs`,
  new `aster_reactor_recv_frame` FFI entry point, sealed
  `Reactor.RecvFrame` result type, `ReactorRequestStream` SPI helper,
  `AsterServer.dispatchCall` ClientStream / BidiStream cases wired,
  `AsterClient.callClientStream` and `callBidiStream` shipped (both
  buffered shape — see open-item below for true interleaved bidi),
  `MissionControl.ingestMetrics` + `AgentSession.runCommand`
  implemented in the example. 8/8 E2E tests green.

- ~~**True interleaved bidi API.**~~ Landed 2026-04-13 in `1b164d7`.
  New `BidiCall<Req, Resp>` AutoCloseable in
  `bindings/java/aster-runtime/.../client/BidiCall.java` exposing
  `send(Req)` / `recv()` / `complete()` / `cancel()` / `close()`.
  `AsterClient.openBidiStream(...)` returns
  `CompletableFuture<BidiCall<Req, Resp>>`. Internal model: a
  dedicated `aster-bidi-reader` virtual thread reads response
  frames off the QUIC stream and pushes them onto a
  `LinkedBlockingQueue<Object>`; `recv()` blocks on `take()`;
  end-of-stream is signalled via a static `END_SENTINEL`. The
  buffered `callBidiStream` stays as a convenience for batched
  ping-pong shapes. Mid-implementation gotcha worth recording: the
  first attempt put `stream.sendAsync(headerFrame).get()` inside
  the BidiCall constructor invoked from `.thenApply(stream → new
  BidiCall(...))`, which deadlocked because the executor thread
  delivering the send completion was the same one blocked on the
  `.get()`. Fix: BidiCall constructor is now I/O-free, the header
  send happens in `openBidiStream` as a pure CompletableFuture
  chain. The constructor's javadoc documents this so it doesn't
  regress. New test
  `MissionControlE2ETest#interleavedBidiRunCommandPingPong`.

- ~~**Cancellation propagation.**~~ Landed 2026-04-13 in `6539065`.
  - `IncomingCall` gains `cancelled: Arc<AtomicBool>` (Acquire/Release
    ordering). `handle_stateless` and `handle_session` inspect each
    forwarded frame in their tokio::select loops: if `FLAG_CANCEL`
    is set, store true on the flag, drop the request channel (which
    surfaces as EOS to the dispatcher's `RequestStream.receive()`),
    and `continue` instead of forwarding. The dispatcher's eventual
    trailer arrives naturally.
  - New FFI entry point
    `aster_reactor_check_cancelled(runtime, reactor, call_id) -> i32`
    (0 = alive, 1 = cancelled, <0 = error). `RingCall` carries the
    `Option<Arc<AtomicBool>>`; `ReactorState.cancelled_flags` is the
    per-call map; both poll drain sites populate it; submit and
    submit_trailer evict it as terminal cleanup.
  - Java side: `Reactor.checkCancelled(callId)` wraps the FFI;
    `ReactorResponseStream.isCancelled()` now returns the real flag
    instead of `false`; `BidiCall.cancel()` sends an empty
    `FLAG_CANCEL` frame on the underlying QUIC stream — distinct
    from `complete()` (graceful FIN) and `close()` (resource
    teardown).
  - `AgentSession.runCommand` updated to check `out.isCancelled()`
    at the top of each loop iteration AND after `in.receive()`
    returns null (catches "cancellation closed the request channel
    while we were blocked"). Records exit reason in a static
    volatile field for the test to verify.
  - New test
    `MissionControlE2ETest#bidiRunCommandCancellationPropagates`
    proves the full path: client cancel → wire FLAG_CANCEL →
    reactor sets flag → dispatcher's isCancelled() returns true →
    static field records "CANCELLED".
  - Still open: dispatcher's eventual trailer is still OK rather
    than CANCELLED (graceful early exit); reactor doesn't yet set
    `cancelled = true` on transport errors (e.g. peer crash, stream
    reset). Both refinements left for follow-up.

- ~~**block_on inside virtual-thread dispatchers.**~~ Landed
  2026-04-13 in `c315112`. `AsterServer` now owns two executors:
  `callExecutor` (newVirtualThreadPerTaskExecutor, current — hosts
  unary + server-stream) and a new `streamingExecutor`
  (newCachedThreadPool of platform threads named
  `aster-server-streaming`, daemon — hosts client-stream + bidi).
  `dispatchCall` checks `Thread.currentThread().isVirtual()` and
  trampolines streaming dispatch onto the platform-thread executor
  before invoking. The dispatch switch was extracted into a new
  private `runDispatch()` helper so the inline path and the
  trampolined path share one body. Each method has its own try/catch
  so the trampoline doesn't lose RpcError → trailer handling.
  `streamingExecutor.shutdown()` + `awaitTermination` wired into
  `close()` parallel to `callExecutor`. What this does NOT fix:
  block_on still parks ONE platform thread per in-flight streaming
  call — that's what block_on does on a non-runtime thread.
  CachedThreadPool grows on demand and idle threads expire after
  60s. For workloads that need to multiplex thousands of streams
  onto a handful of OS threads, the right fix is a callback-based
  recv API (no block_on at all), which is a bigger FFI change.
- **`DispatcherEmitter` server-stream / client-stream / bidi emit.**
  Currently every streaming kind emits an
  `UnsupportedOperationException` stub. The real bodies are
  straightforward — same shape as the unary emit plus a call to
  `out.send(codec.encode(...))` — but require a design call on the
  user-method signature shape (push `ResponseStream` parameter vs.
  return `Stream<Resp>`). Once landed, the hand-written
  `MissionControlDispatcher` / `AgentSessionDispatcher` /
  `EchoServiceDispatcher` can all be replaced with generated
  equivalents.
- **Manifest submission to the registry doc.** `AsterServer` builds
  the manifest at startup and exposes it via `manifest()` but does
  not yet call `RegistryAsync.publishAsync(...)`. Item 5 above; this
  is the same work item, called out separately so it doesn't get lost.
- **Hook event dispatch.** `IrohHook.respondBeforeConnect` /
  `respondAfterConnect` ship the FFI release path, but the
  `IROH_EVENT_HOOK_BEFORE_CONNECT` / `_AFTER_CONNECT` event
  subscribe + dispatch loop is still pending — tracked under item 3.
- **Cross-language Mission Control demo.** The Java MC server
  (`bindings/java/aster-examples-mission-control`) uses Fory tags
  matching the Python sample (`mission/StatusRequest` etc.) so a
  Python operator → Java MC server smoke test is one
  codec-registration call away. Useful milestone for proving
  cross-language interop end to end.
- **KotlinPoet Flow emitter** in `aster-codegen-ksp` — replace the
  JavaPoet-delegating stub with a native KotlinPoet emitter that
  bridges `Flow<Resp>` ↔ `ResponseStream.send(...)` and `suspend fun`
  ↔ `kotlinx.coroutines.future.future { ... }`. Pure codegen work
  but the gating dependency for an `examples/kotlin/mission-control`
  Gradle subproject.

#### Java milestone reached (2026-04-13, fully expanded)

The Java binding has a working end-to-end Mission Control server in
`bindings/java/aster-examples-mission-control`. Initial milestone
landed in commit `012bcc9` with unary + server-streaming only;
expanded to FULL RPC-pattern parity with the Python sample in
commits `a148efb` (reactor read-side mpsc) + `07a2b4c` (Java side
+ MC client-stream / bidi); benchmarked against TLS gRPC Java in
`3387fe8` + `2a8f5f5`; and the three open caveats from `07a2b4c`
all closed on the same day in three follow-up commits: `1b164d7`
(true interleaved BidiCall), `c315112` (streaming-executor
carrier-pin fix), `6539065` (cancellation propagation). Two
services (shared `MissionControl` + session-scoped `AgentSession`),
all four method shapes covered, plus interleaved bidi and
cancellation:

  - `getStatus` / `submitLog` / `register` / `heartbeat` (unary)
  - `tailLogs` (server-streaming, with level filter)
  - `ingestMetrics` (client-streaming)
  - `runCommand` (bidi-streaming, fake-exec for deterministic tests)
  - `runCommand` via `BidiCall<Req, Resp>` (true interleaved,
    `1b164d7`)
  - `runCommand` cancellation propagation (`6539065`)

Exercised by **10 green Java↔Java E2E tests** in
`MissionControlE2ETest`. Total Java test count: aster-runtime 50 +
MC 10 = **60 across 6 modules**, full `mvn -P fast test` in ~65s.

Run the server locally with:

```
cd bindings/java && mvn -P fast -pl aster-examples-mission-control \
  exec:java -Dexec.mainClass=site.aster.examples.missioncontrol.Server
```

#### Java vs gRPC side-by-side (commits `3387fe8` + `2a8f5f5`, 2026-04-13)

In-process Java↔Java Aster MC benchmark
(`bindings/java/aster-examples-mission-control/.../MissionControlBenchmark.java`)
plus a standalone TLS gRPC Java baseline
(`benchmarks/grpc-java-mission-control/`) using the same
`mission_control.proto` the Python gRPC baseline already had. Both
ports use mvn-driven harnesses so the comparison is on identical
hardware with identical encryption posture (TLS on both sides, in
the gRPC case via a Netty self-signed cert).

Run with:

```
cd bindings/java && mvn -P fast -pl aster-examples-mission-control \
  -am test -Dtest=MissionControlBenchmark -Dsurefire.failIfNoSpecifiedTests=false

cd benchmarks/grpc-java-mission-control && mvn -q exec:java
```

First numbers, M-series Mac, dev profile, 1000-iteration unary
stages, in-process:

| Stage                        | Aster Java       | gRPC Java        | Ratio       |
|------------------------------|------------------|------------------|-------------|
| Unary getStatus (1k seq)     | 1,178 r/s, p50 0.78ms | 2,336 r/s, p50 0.38ms | gRPC 2.0×  |
| Unary submitLog (1k seq)     | 1,533 r/s, p50 0.62ms | 4,490 r/s, p50 0.21ms | gRPC 2.9×  |
| Concurrent 10                | 3,802 r/s        | 2,985 r/s        | Aster 1.3× |
| Concurrent 50                | 8,298 r/s        | 12,765 r/s       | gRPC 1.5×  |
| Concurrent 100               | TIMED OUT        | 17,107 r/s       | gRPC works |
| JVM heap delta               | +224 MB          | +50 MB           | gRPC 4.5×  |

Read of result: we're "in the right ballpark" — within 2-3× of gRPC
on sequential unary, slightly ahead at low concurrency, behind at
higher concurrency, ~4× the heap. Two clear gaps drive most of it:

1. **Stream-open-per-call architecture.** Every Aster unary call
   opens a fresh QUIC bi-stream; gRPC pipelines unaries onto a
   small pool of HTTP/2 streams. The wire format already supports
   session-mode unary (`FLAG_CALL`) — exposing it on `AsterClient`
   is the obvious next perf win and should close most of the
   sequential gap.
2. **Quinn's default `initial_max_streams_bidi=100`.** One-line
   config change in `core/src/lib.rs`'s endpoint builder to lift
   the concurrency ceiling. Surfaces as the TIMED OUT row above.

The buffered client-stream API hits ~27K msg/s on a 10K-batch
ingestion (not in the table because the proto doesn't define a
client-stream method); that path is not bottlenecked.

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
4. **Decorators** in all three — biggest design call, build one first.
   ✅ **Java done** (2026-04-13) via `aster-annotations` +
   `aster-codegen-apt`; `aster-codegen-ksp` scaffolded for Kotlin.
   Go and .NET still to mirror — Java is the working reference for
   how the annotation-processor → ServiceDispatcher pipeline shapes
   up. **← next up for Go / .NET.**
5. **Contract manifest submission** — falls out almost free once
   decorators land, since it's just gather-and-publish. Java's
   `AsterServer` builds the manifest but does not yet publish it;
   one wire-up call away.
6. **AsterClient** in all three. ✅ **Java done** (commits F +
   `f8de78d`) for the unary + server-streaming subset, with
   client-stream / bidi gated on read-side reactor mpsc widening.
7. **Session-scoped services** in all three. ✅ **Java done**
   (`f8de78d` G.2-core).
8. **Java end-to-end milestone**: working Mission Control server in
   `bindings/java/aster-examples-mission-control` with all four RPC
   shapes (commits `012bcc9` + `a148efb` + `07a2b4c`, 2026-04-13).
   Two services, 8/8 E2E tests green. Stretch goal: point a Python
   operator at it for cross-language smoke.
9. ~~**Reactor read-side mpsc + client/bidi streaming**~~ — landed in
   `a148efb` (Rust + FFI) + `07a2b4c` (Java side + MC parity).
   Wire format gained `FLAG_END_STREAM = 0x40`, reactor gained a
   per-stream frame reader task feeding a shared mpsc, FFI gained
   `aster_reactor_recv_frame`, Java gained `Reactor.recvFrame` +
   `ReactorRequestStream` + AsterServer client/bidi wiring +
   `AsterClient.callClientStream` / `callBidiStream`.
10. **DispatcherEmitter streaming emit + true interleaved bidi API**
    — both pure binding work, no Rust changes. The codegen item lets
    us delete the hand-written MC dispatchers; the bidi API item
    replaces the buffered `callBidiStream` for use cases that need
    true streaming responses.
11. (Separately) Python switchover to the async registry FFI ops, then
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
