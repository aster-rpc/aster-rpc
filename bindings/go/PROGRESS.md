# Go FFI Binding Progress

**Package:** `github.com/aster-rpc/aster-go`
**Target:** Go 1.24+
**ABI contract:** Rust FFI via CGO, header at `iroh_ffi.h`
**Native library:** `libaster_transport_ffi.dylib` / `.so` / `.dll`
**Build:** Standard `go build`, cross-platform via `GOOS`/`GOARCH` + CGO
**Status:** Phase 1 in progress — library scaffolding compiles; tests require standard Go (this environment has `CGO_TEST_DISALLOW`)

---

## Modern Go FFI Approach

Go has no FFM equivalent — **cgo is the only native interop path**. This is a strength (mature, well-understood) not a limitation.

Key cgo practices used in this binding:

| Concern | Approach |
|---------|----------|
| Goroutine/poller | One background goroutine per `Runtime` calls `iroh_poll_events` in a non-blocking loop |
| Event dispatch | Per-op `chan *Event` — idiomatic Go, no `sync.Map` needed |
| Handle lifecycle | Exported types with `Close()` methods; `runtime.SetFinalizer` as backstop |
| Memory ownership | Rust owns buffers until `iroh_buffer_release`; Go copies data immediately then releases |
| Cancellation | `context.Context` + `operation_cancel`; goroutine-safe channel close |
| Struct encoding | `//#cgo` directives for platform-specific `LDFLAGS`; `unsafe.Offsetof` for field access |
| Error model | Go-native errors with `fmt.Errorf` and `%w` wrapping; no exception hierarchy |

**Why channels over callbacks:** Go has no safe upcall mechanism across cgo boundaries. The poller goroutine receives native events and dispatches them over Go channels — this is the idiomatic equivalent of the Java `CompletableFuture`.

---

## Phase 1 — Foundation (CGO plumbing)

- [x] `go.mod` — module definition (`github.com/aster-outdoor/aster-go`)
- [x] `iroh.go` — C struct mirrors, `//#cgo` directives, all FFI function declarations from `iroh_ffi.h`
- [x] Struct layout validation — `unsafe.Sizeof` / `unsafe.Offsetof` tests for `iroh_event_t`, `iroh_runtime_config_t`, `iroh_bytes_t`, etc. (written, cannot run in this env — has `CGO_TEST_DISALLOW`)
- [x] Ownership smoke tests — null buffer/string release, invalid operation cancel (written, cannot run)
- [x] `runtime.go` — `Runtime` type, background poller goroutine, handle registry, sync helpers
- [x] Sync FFI helpers — version queries, `StatusName`, `LastErrorMessage`

**Build verified** with:
```bash
CGO_CFLAGS="-I$(pwd)/../../ffi" CGO_LDFLAGS="-L$(pwd)/../../target/release/deps -l aster_transport_ffi" go build ./...
```

**Exit criterion:** Can create a runtime, submit an op, receive an event on a channel.

---

## Phase 2 — Node and Endpoint

- [x] `Node` — `iroh_node_memory_with_alpns`, `iroh_node_close`, `iroh_node_id`, `iroh_node_addr_info`
- [x] `Endpoint` — `iroh_endpoint_create`, `iroh_endpoint_close`, `iroh_endpoint_id`
- [x] Incoming connection handler — `iroh_node_accept_aster` registered with runtime as inbound handler
- [x] `Endpoint.Connect(ctx, nodeID, alpn)` — returns `*Connection`
- [x] `Endpoint.Accept(ctx)` — returns `*Connection`
- [x] `Node.Memory(ctx)`, `Node.MemoryWithAlpns(ctx, alpns)`, `Node.Persistent(ctx, path)`, `Node.Close()`
- [x] `Node.Connections()` — channel of incoming connections (`AcceptedAster`)
- [x] `Endpoint.New(ctx, runtime, config)` — creates endpoint
- [x] `Endpoint.ExportSecretKey()` — exports 32-byte secret key seed

---

## Phase 3 — Connection and Streams

- [x] `Connection` — `iroh_open_bi`, `iroh_accept_bi`, `iroh_connection_close`, `iroh_connection_info`
- [x] `Stream` — `iroh_stream_write`, `iroh_stream_finish`, `iroh_stream_read`, `iroh_stream_stop`
- [x] Bidirectional stream — `Connection.OpenBi(ctx)` → `(sendStream, recvStream)`
- [x] Unidirectional stream — `Connection.OpenUni(ctx)`, `Connection.AcceptUni(ctx)`
- [x] Datagram — `Connection.SendDatagram(data)`, `Connection.ReadDatagram(ctx)`
- [x] Read adaptation — `RecvStream.Read(ctx)` → `([]byte, error)` (single frame, copies and releases immediately)
- [x] `Connection.MaxDatagramSize()` — returns max datagram size

---

## Phase 4 — High-level API (Go-idiomatic)

- [x] `Node` factory methods — `Memory(ctx)`, `MemoryWithAlpns(ctx, alpns)`, `Persistent(ctx, path)`
- [x] `NodeID` type — 32-byte array with `String()` hex method
- [x] `Node.Close()` — cancels accept loop, frees handle
- [ ] `Node.ID()` — returns `NodeID` (32-byte array, `String()` hex method)
- [ ] `Connection` graceful close — `FinishAndClose(ctx)` for orderly shutdown
- [ ] `Node.Close()` — cancels accept loop, frees handle, releases all resources

### Go-idiomatic API examples

```go
// Node creation
node, err := aster.Memory(ctx, "aster")
if err != nil {
    return fmt.Errorf("create node: %w", err)
}
defer node.Close()

// Connect to a peer
conn, err := endpoint.Connect(ctx, nodeID, "aster")
if err != nil {
    return fmt.Errorf("connect: %w", err)
}
defer conn.Close(nil)

// Open a bidirectional stream
send, recv, err := conn.OpenBi(ctx)
if err != nil {
    return fmt.Errorf("open bi: %w", err)
}

// Send frames
err = send.Write(ctx, []byte("hello"))
if err != nil {
    return fmt.Errorf("write: %w", err)
}
err = send.Finish(ctx)
if err != nil {
    return fmt.Errorf("finish: %w", err)
}

// Receive frames
for {
    data, err := recv.Read(ctx)
    if errors.Is(err, io.EOF) {
        break
    }
    // process data
}
```

---

## Phase 5b — CQ Test Suite

Mirrors the Rust/Java test plan in [`PHASE_5B_TESTS.md`](./PHASE_5B_TESTS.md).

### 5b.1 — CQ State Machine (Go, pure unit tests)
- [x] `cq_fake.go` — goroutine-safe fake CQ implementation for unit tests
- [x] `cq_test.go` — 10 tests covering state transitions, exactly-once, concurrent dispatch
- [x] State transitions: `submit → complete`, `submit → cancel → complete`
- [x] Exactly-once terminal event, no double-complete, no stale op lookup
- [x] Concurrent submit/complete from 100 goroutines
- [x] Concurrent cancel/complete races
- [x] Concurrent close/submit races

### 5b.2 — Concurrency
- [x] `operation_cancel` races `poll` — goroutine-safe channel dispatch (via mutex-protected FakeCQ)
- [x] `close` races `drain` — registry cleanup while polling
- [ ] `drop(Connection)` while ops pending — all dependent ops reach terminal state

### 5b.3 — Memory Safety
- [ ] Buffer after `release` — verify data was copied before release call
- [ ] Stale handle — submit on closed handle returns error, no crash
- [ ] `iroh_buffer_release` called twice — idempotent or error, not UB

### 5b.4 — Fuzz the ABI
- [ ] `cargo-fuzz` on Rust side (already done in Phase 5b.4)
- [ ] Go-side: round-trip encode/decode of all struct types

### 5b.5 — ABI Contract Harness
- [x] `abi_contract_test.go` in `bindings/go/` — struct sizes, field offsets, enum values, ownership smoke

### 5b.6 — Hostile Race Integration
- [x] `accept_submit_then_close` — submit accept → close → drain — exactly one terminal
- [x] `cancel_and_completion_racing` — submit → cancel races drain — no spurious completion
- [x] `handle_close_after_submit` — no success event for old handle generation
- [x] `many_outstanding_on_cq` — 100 ops, no loss/duplication
- [x] `many_connections_share_cq` — 20 nodes sharing CQ, throughput stable
- [x] `api_surface_stress` — rapid concurrent create/close, no panics

### 5b.7 — Cross-Language Conformance
- [ ] Golden event traces (JSON) — shared with Rust/Java via `ffi/tests/conformance/`
- [ ] Go trace validator — load scenario → execute → compare against golden trace

### 5b.8 — Performance Benchmarks
- [x] `benchmarks_test.go` in `bindings/go/` — 9 benchmarks scaffolded (null buffer, string release, poll, event encode/decode, runtime create/close, ABI version)

### 5b.9 — Soak Test
- [x] `soak_test.go` — churn pattern with metrics tracking (pending ops, final count)
- [x] Assertions: `final_pending == 0`, `max_pending < 100`

---

## Phase 8 — Full API Surface Completion

See [PHASE8.md](PHASE8.md) for the detailed checklist.

### Summary of groups:
1. **Connection extras** — datagrams, remote ID, onClosed (smallest surface)
2. **Blobs** — file transfer (highest value)
3. **Tags** — key→blob mapping
4. **Docs** — content-addressed store with sync (most complex)
5. **Gossip** — pub/sub (independent, well-scoped)
6. **Endpoint extras** — metrics, hooks
7. **Signing/tickets** — utility layer

### Implementation status
- [x] 8.1 — Connection extras (remoteId, datagrams, onClosed)
- [x] 8.2 — Blobs
- [ ] 8.3 — Tags
- [ ] 8.4 — Docs
- [ ] 8.5 — Gossip
- [ ] 8.6 — Endpoint extras
- [ ] 8.7 — Signing and tickets

---

## Phase 6 — Error Handling

- [x] `IrohError` type with `Code()`, `Error()`, `Unwrap()`, `Is()`, `As()`
- [x] Sentinel errors: `ErrNotFound`, `ErrInvalidArgument`, `ErrAlreadyClosed`, etc.
- [x] `wrapError(status)` converts C status to typed Go error with sentinel chain
- [x] `Error()` function returns typed errors supporting `errors.Is` / `errors.As`
- [x] `error.go` — typed error implementation with sentinel error matching

---

## Phase 7 — Build and Distribution

- [ ] `build.gradle.kts` — for consumers using Gradle
- [ ] Cross-platform native lib detection — `GOOS`/`GOARCH` + `cgo LDFLAGS`
- [ ] Publish to GitHub Releases with platform artifacts
- [ ] JitPack or GitHub Packages for Maven/Gradle consumers

### Platform classifiers

| Platform | Classifier | Architecture |
|----------|-----------|--------------|
| macOS ARM64 | `darwin-arm64` | Apple Silicon |
| macOS x64 | `darwin-amd64` | Intel |
| Linux x64 | `linux-amd64` | x86_64 |
| Linux ARM64 | `linux-arm64` | aarch64 |
| Windows x64 | `windows-amd64` | x86_64 |

Go's `GOOS`/`GOARCH` convention differs from Maven. Use a companion module or GitHub Release artifact naming convention.

---

## Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| `chan *Event` per op | Idiomatic Go dispatch — no callbacks across cgo boundary |
| `context.Context` for cancellation | Standard Go pattern; cancel calls `operation_cancel` + closes channel |
| `runtime.SetFinalizer` backstop | Safety net for handle leaks if `Close()` is not called |
| Immediate copy+release | Native buffer copied to Go `[]byte`, then `iroh_buffer_release` called immediately |
| Background poller goroutine | One goroutine per `Runtime` — O(1) threads, not O(ops) |
| Go error wrapping | `fmt.Errorf("accept: %w", err)` — no exception hierarchy |
| No getter/setter methods | Expose fields directly or use simple `Method()` — idiomatic Go |

---

## Out of Scope

- Service stub generation (codegen)
- Fory serialization
- .NET binding (separate effort)
- Kotlin wrapper (separate Kotlin-specific API)
