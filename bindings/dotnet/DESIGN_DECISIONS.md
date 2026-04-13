# .NET Binding Design Decisions

**Package:** `Aster`
**Target:** .NET 9+
**ABI contract:** Rust FFI via source-generated P/Invoke (`LibraryImport`)
**Native library:** `libaster_transport_ffi.dylib` from `iroh-python/target/release`
**Build:** `dotnet build`, cross-platform via .NET runtime

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

**Decision:** One background `Task` per `Runtime` calls `iroh_poll_events` in a non-blocking loop. Events are dispatched to `ConcurrentDictionary<ulong, TaskCompletionSource<Event>>`.

**Rationale:**
- Matches the Rust async model where tokio runs the event loop
- `TaskCompletionSource<Event>` provides idiomatic .NET async/await integration
- No thread-per-operation overhead — single poller handles all ops on a runtime
- Native event batch processing (`iroh_poll_events` returns multiple events per call)

**Comparison with other bindings:**

| Language | Poller Implementation |
|----------|---------------------|
| **Go** | One goroutine per `Runtime` calls `iroh_poll_events`, dispatches to `chan *Event` |
| **Java** | `IrohPollThread` (platform thread) calls `iroh_poll_events`, dispatches to `ConcurrentHashMap<opId, CompletableFuture>` |
| **.NET** | `Task.Run` (background task) calls `iroh_poll_events`, dispatches to `ConcurrentDictionary<opId, TaskCompletionSource>` |

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

**Decision:** Rust owns all buffer memory until `.NET` explicitly releases via `iroh_buffer_release`.

**Pattern:**
1. Native call returns buffer handle via event
2. .NET copies data to managed memory (`Marshal.Copy`)
3. .NET calls `iroh_buffer_release(runtime, bufferHandle)` to free

**Comparison:**

| Language | Buffer Ownership |
|----------|----------------|
| **Go** | Rust owns → Go copies via `C.Go()` → calls `iroh_buffer_release` |
| **Java** | Rust owns → Java reads via `MemorySegment` → calls `iroh_buffer_release` |
| **.NET** | Rust owns → .NET copies via `Marshal.Copy` → calls `iroh_buffer_release` |

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
            Native.iroh_runtime_destroy(_runtime);
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

### 6. Endpoint Configuration — 144-Byte Struct

**Decision:** `EndpointConfig` struct must be exactly 144 bytes to match Rust's `iroh_endpoint_config_t`.

**Fields (16 fields, 144 bytes total):**
```csharp
[StructLayout(LayoutKind.Sequential)]
public struct EndpointConfig
{
    public uint struct_size;             // 0
    public uint relay_mode;             // 4
    public Bytes secret_key;            // 8 (16 bytes)
    public BytesList alpns;             // 24 (16 bytes)
    public BytesList relay_urls;        // 40 (16 bytes)
    public uint enable_discovery;       // 56
    public uint enable_hooks;          // 60
    public ulong hook_timeout_ms;       // 64
    public Bytes bind_addr;             // 72 (16 bytes)
    public uint clear_ip_transports;   // 88
    public uint clear_relay_transports; // 92
    public uint portmapper_config;      // 96
    public Bytes proxy_url;            // 100 (16 bytes)
    public uint proxy_from_env;        // 116
    public Bytes data_dir_utf8;         // 120 (16 bytes)
} // 144 bytes total
```

**Why size matters:** Missing fields cause internal errors (kind=99 events) because Rust interprets garbage data as invalid configuration.

**Comparison:**

| Language | Config Struct Size |
|----------|------------------|
| **Go** | `unsafe.Sizeof(iroh_endpoint_config_t{})` = 144 |
| **Java** | `IrohLibrary.IROH_ENDPOINT_CONFIG.byteSize()` = 144 |
| **.NET** | `Marshal.SizeOf<EndpointConfig>()` = 144 |

All three bindings verify struct sizes via ABI contract tests.

---

### 7. Event Kinds — Cross-Language Constants

**Decision:** `EventKind` enum values must match Rust's `iroh_event_kind_t` exactly.

**Critical values:**
```csharp
EventKind.None = 0
EventKind.EndpointCreated = 3
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
