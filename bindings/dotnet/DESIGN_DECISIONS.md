# .NET Binding Design Decisions

**Package:** `Aster`
**Target:** .NET 9+
**ABI contract:** Rust FFI via source-generated P/Invoke (`LibraryImport`)
**Native library:** `libaster_transport_ffi.{dylib,so}` / `aster_transport_ffi.dll` from `target/release` (falls back to `target/debug`; override with `-p:NativeLibraryPath=...`)
**C header:** `ffi/aster_ffi.h`, ABI 4.x (`aster_*` symbols; `aster_runtime_new(4, ...)`)
**Build:** `dotnet build`, cross-platform via .NET runtime

## STATUS (ABI 4.3, 2026-10-04)

- **ABI:** needs `libaster_transport_ffi` ABI 4.x (`Native.AbiVersionMajor` = 4,
  `Native.RequiredAbiMinor` = 3; 4.0 removed the reactor, framed calls, the node factories and
  bare endpoints, all unused here since the 3.5 port). Struct
  mirrors of the 3.1–3.5 types are in `src/Aster/NativeStructs.cs`, P/Invokes in
  `src/Aster/Native.V3.cs` / `Native.Dispatch.cs` (3.5); sizes are asserted in `NodeTests.Native_Struct_Sizes_Match_Header`.
- **Codec:** Apache.Fory **1.7.6** (`ForyCodec`: `ThreadSafeFory`, compatible on, ref tracking
  off; Fory C# is always xlang). Types register under Aster wire tags
  (`Register<T>("ns/Name")`, split on the last `/` like every binding). JSON mode is gone;
  `ICodec` is generic (`Encode<T>` / `Decode<T>`).
- **Envelope / compression:** AEv1 `StreamHeader` / `CallHeader` / `RpcStatus` and zstd via
  the C ABI (`AsterEnvelope`, `Zstd`, `AsterFraming`). Every server exit path ends with an
  AEv1 `RpcStatus` trailer, now written by Rust from the status bytes .NET passes. Checked against
  `conformance/vectors/{envelope,compression}.json`.
- **Contract identity:** `ServiceDefinition.ContractId` builds TypeDef / ServiceContract JSON
  from the handler types (`[WireType]`, `[WireName]`) and lets Rust canonicalize and hash it.
  `int`/`long` map to `varint32`/`varint64` (same table as Rust/Java); InteropEcho gives the
  shared `c5aa690b…`. Contract-default markers (`contract-defaults.json`) are not supported yet
  (same as Java): every field is `required`.
- **Node:** `Node.StartAsync(AsterConfig, Gate?, extraAlpns)` on `aster_node_start`;
  `AsterConfig` is a record with `FromEnv()` / `FromToml()` resolved by the Rust
  `aster::AsterConfig`. Gate 0 (`Gate`), admission events (`Node.Admissions` +
  `AdmissionRequest.Allow/Deny`), tickets (`AsterTicket`), Prometheus metrics.
  `Node.MemoryWithAlpnsAsync` is gone (use `StartAsync`). `Endpoint` is a connect/accept view
  of a node: since ABI 4.0 has no bare endpoints, `Endpoint.CreateAsync(runtime, builder)` starts
  an in-memory node on that runtime with the builder's ALPNs (`aster_node_start`) and uses the
  node functions; `aster_accept` on a node takes from its ALPN accept queue.
- **RPC (ABI 3.5, F4):** `AsterServer` serves through Rust's `aster::rpc::Server`
  (`aster_server_*` / `aster_dispatch_*`, wrapped by the internal `DispatchServer`). Each
  `ServiceDefinition` registers its JSON registration form (`RegistrationJson`; Rust derives the
  same contract id, checked in `RpcTests`). Rust decodes headers, routes (unknown service →
  `NOT_FOUND`, unknown method → `UNIMPLEMENTED`), enforces Gate 3 (`CapabilityRequirement` on a
  service / method, fed by `AsterServer.SetPeerAttributes`), deadlines (`min(header, method
  DefaultTimeout, HandlerTimeout)`), per-method frame limits and the per-connection session cap,
  and serves `aster.ops.NodeInfo`. One poll thread drains the events: `CALL` → thread pool,
  `CANCELLED` → the handler's `CancellationToken`, `RESPONSE_WRITABLE/CLOSED` → the
  `QUEUE_FULL` → reserve → await path, `SESSION_CLOSED` → session state disposed + `SessionClosed`,
  `CONNECTION_CLOSED` → `ConnectionClosed`, `NON_RPC` → `NonRpcConnection` (closed when nobody
  subscribes). **Session-scoped services are served** (`ServiceScope.Session` +
  `Builder.SessionFactory`, state in `ServerCallContext.Session`; ids from
  `RpcConnection.NextSessionId`). The client (`RpcConnection`) uses `aster_rpc_unary` (pooled) and
  `aster_rpc_open` + `aster_call_{send,recv}_payload(s)` for streaming; a unary call with a
  cancellable token runs on its own stream so the token cancels the native call
  (`aster_call_cancel`). Rust surfaces a client cancel to the server handler for streaming calls
  only (it does not watch unary streams). Blocking request reads (`aster_dispatch_recv`) wait
  in bounded slices that re-check the handler token. *Historical:* the old `Reactor.cs` path
  over `aster_reactor_*` and framed `aster_call_*` (both removed in ABI 4.0) is gone.
- **Data plane (3.3):** blobs read-range / provider fetch with report / GC / status, tag prefix
  delete, docs open / import / authority / retire / durable set / entry status, connection
  paths, stream priority. **Pure functions (3.4):** `Aster.Crypto.Ids`, `Hpke`, `Attestation`,
  `Grants` (checked against `conformance/vectors/grants.json`).
- **Upcalls, leases, topology (ABI 4.1):** `Aster.Upcall.Upcalls` is one table per runtime
  (`aster_upcalls_*`). A **dedicated poll thread** drains `aster_upcall_poll` (100 ms native wait,
  woken at once by `aster_upcalls_close`) so blocking polls never occupy a thread-pool thread; each
  upcall runs as its own `Task` (user code never runs on the poll thread). `CANCEL` events fire the
  handler's `UpcallContext.Cancellation` (reason in `CancelReason`: timeout / abandoned / shutdown).
  A handler exception answers `ERROR`, a kind without a handler `UNIMPLEMENTED`; per-kind timeout and
  failure mode via `SetPolicy`. Event payloads are copied and their buffer released exactly once.
  Dispose order: close the table (pending Rust waiters fail), join the poll thread, cancel + await
  running handlers (5 s bound), free the handle; nothing starts after dispose begins. `Ucv1` is the
  UCv1 codec (leases, fenced, decisions, topology, expose C0/C4-C8, admission; placement is not in
  the C ABI) and passes every valid and invalid vector of `conformance/vectors/upcalls.json`.
  `Aster.Lease`: `NativeLeaseSerializer` (`Foreign` over an `ILeaseSerializer`, or `Memory` with an
  `ILeaseAuthorizer`), `HeldLease` (`CurrentFence()` at send time, `WatchAsync`, `ReleaseAsync` /
  `DisposeAsync` = graceful release, `Dispose` = free without release), `NativeFencedResource`
  (`MutateAsync` throws when the outcome is unknown; a write is never failed open).
  `AdmittedFilterHandle` + `Topology.EnableSharedAsync`. `aster_operation_cancel` only suppresses a
  completion event: cancelling a .NET token stops the wait, not the Rust-side upcall (it lives to
  the kind's timeout).
- **Expose (ABI 4.2):** `Aster.Expose.ExposeRegistry` (`ExposeSocket` with an optional kind-48
  authorizer, `ExposeHttp` = foreign handler + body table, `ServeRoutePolicy`, `ServeAsync`,
  `HandleConnection`, `Unexpose`), `HttpBodies` (`aster_http_body_*`; the native calls block with a
  timeout, so the async wrappers try with timeout 0 and re-poll after 1-8 ms instead of parking a
  pool thread), `ExposeHttpHandler` (request body as a pull-based `Stream`, response inline or an
  `IAsyncEnumerable<ReadOnlyMemory<byte>>` written with backpressure and finished with optional
  trailers; a throwing sequence aborts the body so the consumer sees a failure, never a truncated
  success), consumer side `ExposeClient.RelayRequestAsync` (buffered; no streaming relay in the C
  ABI yet) and `RequestRouteAsync`. HTTP services need an upcall table; a table without an
  authorizer denies (fail-closed default). No new auth surface: the authorizer is the existing kind
  48 decision, Gate 0 is untouched.
- **Streaming consumer relay (ABI 4.3):** `ExposeClient.RelayStreamAsync` returns an
  `ExposeStreamResponse` at the head (full duplex; the upload pump keeps running). The native calls
  are used with timeout 0 and the one-shot `ASTER_EVENT_HTTP_RELAY_READY` events
  (`aster_http_upload_ready` / `aster_http_response_ready`), so no pool thread blocks. Contract (as in
  TS): a body-source failure before the head is thrown as the caller's own exception (the request is
  reset), after the head it faults `Upload`; disposing before the body's clean end resets an unfinished
  upload; a clean end lets the upload finish (disposal waits for it); an early answer the handler gives
  without reading makes `Upload` complete `false`. Relay failures are `ExposeRelayException` (stable
  code in `RelayStatus`, `Status` via `aster_http_body_status_to_status`); a denied relay, streaming or
  buffered, is `Status.PermissionDenied`. `Unauthenticated` / `CredentialExpired` are reserved for R4
  and not returned today.
- **Interop:** joins `tests/interop/` (`tests/interop/dotnet/`), 5×5 matrix all pass.
- `Bytes.FromArray` (unpinned pointer) is removed; use `fixed` + `Bytes.FromPointer`,
  `Bytes.FromPinned`, or the internal `NativeScope`.
- Tests: `dotnet test Aster.sln --filter "FullyQualifiedName!~SoakTests"`; the soak suite
  (~27 min, endpoints probe public relays) stays out of quick runs.
- `SendStream.Reset` throws `NotSupportedException` (the ABI has no send-stream reset).

---

## Key Design Decisions

### 1. Source-Generated P/Invoke (`LibraryImport`) over DllImport

**Decision:** Use `LibraryImport` (source-generated P/Invoke) instead of traditional `DllImport`.

**Rationale:**
- `LibraryImport` generates AOT-compatible bindings at compile time
- Better null-safety checking at the managed/native boundary
- Avoids the fragile `EntryPoint` naming issues with traditional P/Invoke
- Modern .NET 9+ FFI support with proper handling of `bool`, `char`, struct marshalling

**Comparison with other bindings:**

| Language | Approach | Notes |
|----------|----------|-------|
| **Go** | cgo | Mature, well-understood, but requires `//#cgo` directives |
| **Java** | FFM (`Linker`) | `MemorySegment` for struct passing, requires Java 25+ |
| **.NET** | `LibraryImport` | Source-generated, AOT-friendly, no runtime code-gen |

---

### 2. CQ Poller Architecture — One Thread Per Runtime

**Decision:** One background `Task` per `Runtime` calls `aster_poll_events` in a non-blocking loop. Events are dispatched to `ConcurrentDictionary<ulong, TaskCompletionSource<Event>>`.

**Rationale:**
- Matches the Rust async model where tokio runs the event loop
- `TaskCompletionSource<Event>` provides idiomatic .NET async/await integration
- No thread-per-operation overhead — single poller handles all ops on a runtime
- Native event batch processing (`aster_poll_events` returns multiple events per call)

**Comparison with other bindings:**

| Language | Poller Implementation |
|----------|---------------------|
| **Go** | One goroutine per `Runtime` calls `aster_poll_events`, dispatches to `chan *Event` |
| **Java** | `IrohPollThread` (platform thread) calls `aster_poll_events`, dispatches to `ConcurrentHashMap<opId, CompletableFuture>` |
| **.NET** | `Task.Run` (background task) calls `aster_poll_events`, dispatches to `ConcurrentDictionary<opId, TaskCompletionSource>` |

All three bindings follow the same architectural pattern: submit ops → poll for completions → dispatch to per-op continuations.

---

### 3. Async/Await Integration via `TaskCompletionSource`

**Decision:** `Runtime.WaitForAsync(opId)` returns `Task<Event>` by completing a `TaskCompletionSource` when the poller receives the corresponding event.

**Implementation:**
```csharp
internal async Task<Event> WaitForAsync(ulong opId, CancellationToken cancellationToken)
{
    using var registration = cancellationToken.Register(() => CancelOperation(opId));

    using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(30));
    using var linkedCts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken, cts.Token);

    var tcs = new TaskCompletionSource<Event>(TaskCreationOptions.RunContinuationsAsynchronously);
    using var _ = linkedCts.Token.Register(() => tcs.TrySetCanceled());

    if (!_pendingOperations.TryAdd(opId, tcs))
        throw new InvalidOperationException($"Op {opId} already pending");

    try
    {
        return await tcs.Task.ConfigureAwait(false);
    }
    finally
    {
        _pendingOperations.TryRemove(opId, out _);
    }
}
```

**Key insight:** `TaskCreationOptions.RunContinuationsAsynchronously` ensures completions don't block the poller thread.

---

### 4. Memory Ownership — Rust Owns, .NET Copies and Releases

**Decision:** Rust owns all buffer memory until `.NET` explicitly releases via `aster_buffer_release`.

**Pattern:**
1. Native call returns buffer handle via event
2. .NET copies data to managed memory (`Marshal.Copy`)
3. .NET calls `aster_buffer_release(runtime, bufferHandle)` to free

**Comparison:**

| Language | Buffer Ownership |
|----------|----------------|
| **Go** | Rust owns → Go copies via `C.Go()` → calls `aster_buffer_release` |
| **Java** | Rust owns → Java reads via `MemorySegment` → calls `aster_buffer_release` |
| **.NET** | Rust owns → .NET copies via `Marshal.Copy` → calls `aster_buffer_release` |

---

### 5. Runtime Disposal — Thread Pool Dispatch

**Issue:** Rust's tokio runtime panics if dropped from within an async context:

```
Cannot drop a runtime in a context where blocking is not allowed.
This happens when a runtime is dropped from within an asynchronous context.
```

**Decision:** Implement `IAsyncDisposable` and dispatch disposal to `Task.Run`:

```csharp
public async ValueTask DisposeAsync()
{
    if (_handle != IntPtr.Zero)
    {
        // Dispatch to thread pool to avoid async context
        await Task.Run(() =>
        {
            Native.aster_runtime_close(_runtime);
        }).ConfigureAwait(false);
        _handle = IntPtr.Zero;
    }
}
```

**Why this works:**
- `Task.Run()` moves the blocking Rust call to the ThreadPool
- The `await` properly yields the async context before the blocking call
- Rust's tokio runtime is dropped from a proper blocking context, not during async continuation

**Comparison with other bindings:**

| Language | Disposal Approach |
|----------|------------------|
| **Go** | `runtime.SetFinalizer` + explicit `Close()` — goroutines can be cancelled via context |
| **Java** | `Cleaner` backstop + explicit `close()` — throws `IllegalStateException` if called from async context |
| **.NET** | `IAsyncDisposable` + `Task.Run()` dispatch — properly handles async disposal |

---

### 6. ABI Struct Layouts

*Historical:* `EndpointConfig` mirrored `aster_endpoint_config_t` (152 bytes); ABI 4.0
removed that struct with bare endpoints, and `EndpointConfigBuilder` now maps onto
`AsterConfig`.
The ABI structs (`Event` 80, `RuntimeConfig` 16, `ConnectConfig` 48,
`NodeAddr` 48, `Bytes`/`BytesList` 16, `DispatchEventNative` 104, `CallOptionsNative` 72) have their size and
offsets asserted in `AbiContractTests` against `ffi/src/layout.rs`. Never hand-compute offsets — add a field to the struct and a
`Marshal.OffsetOf` assertion instead.

---

### 7. Event Kinds — Cross-Language Constants

**Decision:** `EventKind` enum values must match Rust's `aster_event_kind_t` exactly.

**Critical values:**
```csharp
EventKind.None = 0
EventKind.EndpointCreated = 3   // reserved, never emitted since ABI 4.0
EventKind.Closed = 5
EventKind.Connected = 10
EventKind.ConnectionAccepted = 12
EventKind.StreamOpened = 20
EventKind.StreamAccepted = 21
EventKind.FrameReceived = 22
EventKind.SendCompleted = 23
EventKind.StreamFinished = 24
EventKind.DatagramReceived = 60
EventKind.BytesResult = 91
EventKind.OperationCancelled = 98
EventKind.Error = 99
```

**Why this matters:** Cross-language conformance tests verify that Go, Java, and .NET all interpret the same event sequences identically.

---

## Cross-Language Conformance

The three language bindings must produce identical event sequences for the same operation sequences. See `PHASE_5B_TESTS.md` for the conformance test specification.

**Shared invariants:**
1. One op → at most one terminal event
2. Cancel removes op from every wait structure exactly once
3. No completion can outlive the handle generation it belongs to
4. Dropping a connection resolves all dependent ops

---

## Known Issues

### 1. Async Disposal Panics in Tests

**Symptom:** Tests using `await using var runtime = new Runtime()` hang during cleanup with:
```
thread 'iroh-ffi' panicked at .../tokio-1.50.0/src/runtime/blocking/shutdown.rs:51:21:
Cannot drop a runtime in a context where blocking is not allowed.
```

**Root Cause:** `DisposeAsync()` called from async continuation context triggers Rust panic.

**Fix:** Implement `IAsyncDisposable` with `Task.Run()` dispatch as shown in section 5 above.

**Status:** Fix implemented but tests with async disposal patterns may still exhibit issues. Recommend synchronous disposal (`using var runtime = new Runtime()`) for now until `IAsyncDisposable` pattern is fully validated.

---

## Related Documentation

- [Rust FFI Unsafe Audit](../../ffi/UNSAFE_AUDIT.md) — Rust-side unsafe code analysis
- [Go Binding Progress](../go/PROGRESS.md) — Go binding architecture
- [Java Binding Progress](../java/PROGRESS.md) — Java binding architecture
- [Phase 5b Test Specification](../java/PHASE_5B_TESTS.md) — Test suite specification
