# .NET Binding Design Decisions

**Package:** `Aster`
**Target:** .NET 9+
**ABI contract:** Rust FFI via source-generated P/Invoke (`LibraryImport`)
**Native library:** `libaster_transport_ffi.{dylib,so}` / `aster_transport_ffi.dll` from `target/release` (falls back to `target/debug`; override with `-p:NativeLibraryPath=...`)
**C header:** `ffi/aster_ffi.h`, ABI 6.x (`aster_*` symbols; `aster_runtime_new(6, ...)`, `aster_abi_accept(6)`)
**Build:** `dotnet build`, cross-platform via .NET runtime

## STATUS (ABI 6.0, 2026-10-15)

- **ABI:** needs `libaster_transport_ffi` ABI 6.x (`Native.AbiVersionMajor` = 6,
  `Native.RequiredAbiMinor` = 0; 6.0 is breaking: `aster_config_t` grew from 280 to 288 bytes (`blob_zstd_level`, `reuse_last_port`; `AsterConfig.BlobZstdLevel` / `ReuseLastPort`), and `Native.EnsureAbi` performs the `aster_abi_accept(6)` handshake before a config is filled; 5.1 added Enrollment from configuration and the lease grantor (`Native.Enrollment.cs`); 5.0 removed the Fory attestation functions and is otherwise 4.17: owned raw WebTransport and media controls, with 4.12 for explicit public listeners and certificate helpers (4.11 added owned authenticated HTTPS/WebTransport listeners, 4.10 added independently owned Doc event receivers, 4.6 added observability and health, and 4.5 added cancel tokens); 4.0 removed the reactor, framed calls, the node factories and
  bare endpoints, all unused here since the 3.5 port). Struct
  mirrors of the 3.1–3.5 types are in `src/Aster/NativeStructs.cs`, P/Invokes in
  `src/Aster/Native.V3.cs` / `Native.Dispatch.cs` (3.5); sizes are asserted in `NodeTests.Native_Struct_Sizes_Match_Header`.
- **Codec:** Apache.Fory **1.7.6-aster.1** (exact fork pin; FORY-CS-001 metadata-cache
  publication correction, unchanged 1.7.6 wire format). Restore uses
  `NuGet.Config`, mapping `Apache.Fory` to the public Aster
  Forgejo feed. Downstream projects must include that feed and source mapping
  in their own NuGet configuration; package dependencies do not carry feed
  configuration. `ForyCodec`: `ThreadSafeFory`, compatible on, ref tracking
  off; Fory C# is always xlang. Types register under Aster wire tags
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
  `AdmissionRequest.Allow/Deny`), tickets (`AsterTicket`), Prometheus metrics (`Node.MetricsPrometheus()` / `AsterObservability.MetricsText()`, from `aster_metrics_prometheus`; null until an ops listener gave the pipeline a pull reader).
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
  cancellable token stays on the pooled path with a native cancel token
  (`aster_rpc_unary_cancellable`, ABI 4.5), so cancelling the token aborts the native call;
  streaming calls are cancelled with `aster_call_cancel`. Rust surfaces a client cancel to the
  server handler for every pattern, unary included. Blocking request reads (`aster_dispatch_recv`) wait
  in bounded slices that re-check the handler token. *Historical:* the old `Reactor.cs` path
  over `aster_reactor_*` and framed `aster_call_*` (both removed in ABI 4.0) is gone.
- **Data plane (3.3):** blobs read-range / provider fetch with report / GC / status, tag prefix
  delete, docs open / import / authority / retire / durable set / entry status, connection
  paths, stream priority. **Pure functions (3.4):** `Aster.Crypto.Ids`, `Hpke` (`Attestation` removed in ABI 5.0),
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
  upload; a clean end lets the upload finish in the background (the pump owns the upload handle and frees it when it ends; disposal never waits on a stalled source, and a source that ignores its token keeps only the pump alive); an early answer the handler gives
  without reading makes `Upload` complete `false`. Relay failures are `ExposeRelayException` (stable
  code in `RelayStatus`, `Status` via `aster_http_body_status_to_status`); a denied relay, streaming or
  buffered, is `Status.PermissionDenied`. `Unauthenticated` / `CredentialExpired` are reserved for R4
  and not returned today.
- **Coordination client + placement (ABI 4.4):** `Aster.Coordination` wraps the Rust client (journal, routing, validation and the
  delivery contract stay in Rust). `CoordinationCodec` is the UCv1 D0-DE / BC-BF codec (every vector in `coordination.json` and the
  placement cases of `upcalls.json`). `CoordinationClient.OpenAsync` (durable with a journal dir, or read-only), typed mutations returning a
  `ResolvedMutation` that must be `AcknowledgeAsync`'d before the next (`Dispose` leaves it journaled), `UnacknowledgedAsync` /
  `ResumeAsync`, reads, `AcquireLeaseAsync` (`CoordinationLease`: release and client close are **not prompt**, they wait for an in-flight CAS and the
  journal lock stays held until it ends, so a reopen right after close may see `JournalLocked`), `PlacementManager` + `Upcalls.ServePlacement`
  (kinds 32 / 33, 17 unless a topology node is given; a provider failure fails the pass, never open). Failures are `CoordinationException`
  (`Kind` from `error_code`, `RpcCode` from `flags`, coarse `Status`: RPC 7 = `PermissionDenied`, 16 = `Unauthenticated`); a committed typed
  rejection is a result (`MutationReply.IsRejected`). `aster_operation_cancel` only suppresses the event: cancelling a token never aborts a started
  coordination operation (recover with `UnacknowledgedAsync`). ROOT administers the group but holds no lease/read rights itself: actors need a grant
  (the live tests use workers granted by the ROOT client). Live tests use the TS suite's `aster-coordination-fixture` (build it with
  `cargo build -p aster-transport-napi --features coordination-fixture --bin aster-coordination-fixture`, or set `ASTER_COORDINATION_FIXTURE`); they skip when it is absent.
- **Observability and health (ABI 4.6, O9; design `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md`):**
  everything runs in Rust (`aster-otel`); the binding starts it and bridges. `Aster.Observability`:
  - `AsterObservability.Init(ObservabilityOptions)` / `Flush` / `Shutdown` / `MetricsText()` over
    `aster_observability_*`. Each `Init` that succeeds holds one reference; `Shutdown` drops all of them. A different
    configuration than the running pipeline's throws `IrohException` with `Status.AlreadyExists` (17, new). `AsterServer` and `AsterClient`
    start call `EnsureStarted()` (log bridge + Activity bridge, exporters only when `OTEL_*` asks), so call `Init` first to configure.
    The C ABI cannot force a signal off against the environment: `Traces/Metrics/OtlpLogs = true` sets the flag, null or false leaves it to `OTEL_*` (`FromEnv`) or off.
  - **ILogger bridge** (`AsterLogging`): a background `aster-log-poll` thread runs `aster_log_poll` (256 per batch, 250 ms wait, single poller)
    and logs each record on an `ILogger` of category target-with-dots (`aster.rpc.dispatch`) after an `IsEnabled` check, inside a scope
    with `TraceId` / `SpanId` (the keys `ActivityTrackingOptions` uses), `aster.call_id`, `aster.audit`; state = the event fields + `aster.origin=rust` + `{OriginalFormat}`.
    `AsterLogging.UseLoggerFactory(factory)` sets the factory (none: Warning and above to stderr). Level sync: `SyncLevel()` pushes `warn,aster=<lowest enabled level>` through
    `aster_log_set_filter`; the thread re-checks every 5 s; an explicit `LogFilter` or `ASTER_LOG` pins it. *Deviation:* no `IOptionsMonitor<LoggerFilterOptions>.OnChange` hook
    (that needs `Microsoft.Extensions.Options`; the package reference is `Microsoft.Extensions.Logging.Abstractions` only), so the 5 s re-check and `SyncLevel()` are the sync paths.
  - **Metrics:** `Metrics.Counter/UpDownCounter/Histogram(boundaries)/Gauge`, `Metrics.Attributes(...)` (`aster_attrset_*`). Instruments declare lazily and
    re-declare when a pipeline shutdown freed their handle (`NOT_FOUND`). `autoAttrs` / `cardinalityLimit` are accepted, with no effect yet (as in every binding).
    `Metrics` holds no .NET `System.Diagnostics.Metrics` bridge. The old `MetricsInterceptor` is deleted (the built-ins record in Rust).
  - **Activity bridge** (design §5.3, decision Q9): source `ActivitySource("Aster")` (`Tracing.Source`). *Inbound:* the handler runs inside an `Activity`
    (`<service>/<method> handler`, kind Internal) whose parent is the Rust server span (`aster_dispatch_trace_context`, `ASTER_DISPATCH_FLAG_HAS_TRACE`) and whose
    baggage is the inbound W3C `baggage` (`aster_dispatch_inbound_trace`); so HttpClient, ASP.NET Core, EF Core and `Tracing.StartSpan` children join the trace.
    A call with baggage but no exported span gets a baggage-only carrier (source `Aster.Carrier`, never exported; outbound calls then send `baggage` without `traceparent`).
    *Export:* an `ActivityListener` on source `Aster` (registered by `Init`, removed by `Shutdown`) reports each stopped, recorded activity with `aster_span_record`
    (ids, times, kind, status, tags, events; links are not carried). `Sample` returns `AllDataAndRecorded` only when the parent is sampled (a root is offered to Rust's sampler),
    else `PropagationData`. *Outbound:* every client call reads `Activity.Current` (any source) and sends `traceparent` + `baggage` as call metadata (`Tracing.Inject`); Rust
    parents its client span on it. *One export path:* the OpenTelemetry .NET SDK must not subscribe to source `Aster`; when another listener records an Aster activity whose in-process
    parent is unsampled, the bridge skips it and warns once (a bare remote parent cannot be told apart, so the rule stays: do not subscribe).
    `Tracing.WithTraceContext(traceparent, baggage)` continues a trace that arrived another way; `Activity.RecordException` is an extension in `Tracing`.
  - **`Aster.Health`:** `HealthServer.Start(node, HealthOptions)` over `aster_health_*` (`/healthz`, `/readyz` with `Check(name)`, `/metrics`);
    `AsterServerOptions.Health` starts one on the server's node and attaches it (`aster_server_attach_health`: ready after start, not ready first on dispose; the server owns and stops it).
    The .NET `AsterConfig` has no health fields, so the listener is configured through `HealthOptions` (127.0.0.1 and an ephemeral port by default).
  - Tests: `ObservabilityTests` (xUnit, `-warnaserror`): end to end against a loopback OTLP/HTTP receiver that decodes the exported spans.
- **Interop:** joins `tests/interop/` (`tests/interop/dotnet/`), 5×5 matrix all pass.
- **Authenticated serving (ABI 4.11):** `AsterServer.StartHttpListenerAsync` returns an
  `AsterHttpListener` after native TCP/optional QUIC readiness. The same-runtime
  `Upcalls.ServeRpcAuthenticator` bridges typed UCv1 C9/CA requests and outcomes
  (kind 80), with native bounded pending work, payload limits, deadline and
  fail-closed enforcement. Per-call principal and attributes come from the native
  authentication snapshot; only authenticated Iroh connection identities are cached.
  Startup is serialized with server disposal. Each listener retains its runtime;
  stop joins native cleanup and only then releases that owner. Grace bounds
  application work, not QUIC protocol draining. Cancellation never abandons a
  native start or stop already in progress. ABI 4.12 adds a separately named
  `StartPublicHttpListenerAsync`: empty capability attributes, unchanged Gate 3
  and Iroh-only restrictions, no session factories, no authentication-limit
  configuration. `TlsCertificates` generates copied PEM/hash outputs, optionally
  signed by a 32-byte node secret, and verifies the TLS public-key binding against
  a trusted expected node ID. No unsigned Claim, ACME or FIPS validation is
  implied. A managed WebTransport client remains outside this surface. See the README and
  `HttpListenerTests` for native lifecycle and all HTTP RPC pattern coverage.
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
