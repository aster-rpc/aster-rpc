# Aster Go FFI Bindings

**Package:** `github.com/aster-rpc/aster-go`
**Target:** Go 1.23+
**ABI contract:** Rust FFI via CGO, header `ffi/aster_ffi.h`, ABI 3.x (`aster_*` symbols)
**Native library:** `libaster_transport_ffi.dylib` / `.so` / `.dll`

## Status (ABI 3.0 port)

**PARTIAL — compiles and vets clean; reactor server path works end to end;
not production-ready.**

- Ported to the ABI 3.0 rename (`aster_*`, `aster_ffi.h`). `NewRuntime` checks
  `aster_abi_version()` and passes `ASTER_ABI_VERSION_MAJOR` to
  `aster_runtime_new`. Status / event-kind / reactor-event / relay-mode /
  hook-decision / doc-query-mode constants come from the header via cgo
  (`iroh.go`, `endpoint.go`, `reactor.go`); `types.go` mirrors them for `!cgo`.
- All FFI structs are built with the cgo-generated header types
  (`C.aster_endpoint_config_t`, `C.aster_connect_config_t`,
  `C.aster_node_addr_t`, `C.aster_reactor_call_t`, ...) and
  `unsafe.Sizeof`; no hand-computed offsets remain (`cmem.go` owns the C
  allocations for config payloads).
- Reactor: `Poll` branches on `event_kind` (call / connection-closed /
  response-writable / response-closed). Submits that return `QUEUE_FULL` use
  `aster_reactor_reserve_response_capacity` and wait for the capacity event
  delivered by `Poll`, then retry the exact submit (same contract as Java).
  `Server` surfaces submit errors (`ServerConfig.OnSubmitError`, default: log)
  and connection closes (`ServerConfig.OnConnectionClosed`).
- `cmd/aster_server_test` (client + reactor server echo round trip) passes.

Known issues:

- `Endpoint.AddrInfo` (used by `cmd/testnode`, `cmd/stream_test`,
  `cmd/datagram_test`) aborts on a debug build of the native library:
  `aster_endpoint_addr_info` writes the `aster_bytes_t` direct-address array
  at an unaligned offset in the caller's buffer (Rust-side bug, `ffi/src/lib.rs`).
- Pre-existing pure-Go parser test failures unrelated to the ABI:
  `TestParseBlobCollectionJSON` (test builds invalid JSON),
  `TestParseBlobCollectionJSON_invalid` (test expects no error for invalid
  JSON), `TestParseTicketJSON` (hand-rolled ticket JSON parser mis-handles
  `null`).
- `hostile_race_test.go` / `TestSoak_Short` have no peer, so they only check
  "no crash"; they are not meaningful coverage. `TestSoak_Runs` (30 min) is
  skipped unless `ASTER_GO_SOAK=1`.
- No session / multiplexed-stream (spec §6) support in `Server`; streaming
  handlers are not wired (only `Reactor.SubmitFrame/SubmitTrailer`).

## Build

```bash
# Build the native library (debug) from the repo root
cargo build -p aster_transport_ffi

cd bindings/go
export CGO_CFLAGS="-I$(pwd)/../../ffi"
export CGO_LDFLAGS="-L$(pwd)/../../target/debug -laster_transport_ffi"
export DYLD_LIBRARY_PATH="$(pwd)/../../target/debug"   # LD_LIBRARY_PATH on Linux

go vet ./...
go test ./...                       # cgo tests
go run ./cmd/aster_server_test      # end-to-end reactor echo

# Pure-Go unit tests (no native library)
CGO_ENABLED=0 go test ./...
```

## Source Files

| File | Purpose |
|------|---------|
| `go.mod` | Module definition |
| `iroh.go` | Status / event-kind / reactor-event constants from `aster_ffi.h` (`//go:build cgo`) |
| `cmem.go` | C-memory arena + `aster_bytes_t` helpers for building FFI structs |
| `reactor.go` | `Reactor` — poll (event kinds), submit with QUEUE_FULL capacity reservation |
| `server.go` | `Server` — reactor poll loop + handler dispatch |
| `types.go` | Event types and event kind constants (`//go:build !cgo`) |
| `error.go` | Typed errors with `errors.Is`/`errors.As` support (`//go:build cgo`) |
| `runtime.go` | `Runtime` type — poller goroutine, event dispatch, sync FFI helpers |
| `node.go` | `Node` type — memory/persistent creation, connection channel |
| `endpoint.go` | `Endpoint` type — create, connect, accept, close |
| `connection.go` | `Connection` type — bi/uni streams, datagrams |
| `stream.go` | `SendStream` / `RecvStream` types — write/read/finish |
| `cq_fake.go` | Goroutine-safe fake CQ for unit testing (`//go:build !cgo`) |
| `cq_test.go` | 8 CQ state machine unit tests (`//go:build !cgo`) |
| `abi_contract_test.go` + `abi_layout.go` | ABI contract tests — header struct sizes, ABI version, status names, endpoint config layout (`//go:build cgo`) |
| `hostile_race_test.go` | 7 hostile race integration tests (`//go:build cgo`) |
| `soak_test.go` | 2 soak/leak tests (`//go:build cgo`) |

## Architecture

```
Go application
    │
    ▼
aster-go library          Idiomatic Go: channels, context.Context, error wrapping
    │
    │ CGO
    ▼
libaster_transport_ffi    Native shared library (Rust cdylib)
    │
    ▼
aster_transport_ffi      Completion queue model, tokio runtime
    │
    ▼
iroh crates              Transport layer
```

### Poller goroutine

Each `Runtime` starts one background goroutine that calls `aster_poll_events` in a non-blocking loop and dispatches events over channels:

```go
func (r *Runtime) pollLoop() {
    for !r.closed.Load() {
        var events [64]C.aster_event_t
        n := C.aster_poll_events(C.uint64_t(r.handle), &events[0], 64, 10)
        for i := 0; i < int(n); i++ {
            r.dispatch(&events[i])
        }
    }
}
```

### Error Model

All FFI calls return typed `*IrohError` errors that support `errors.Is` and `errors.As`:

```go
// Sentinel errors for status code matching
if errors.Is(err, aster.ErrNotFound) { ... }

// Extract the iroh status code
var irohErr *aster.IrohError
if errors.As(err, &irohErr) {
    fmt.Println(irohErr.Code())
}
```
