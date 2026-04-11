# Aster Go FFI Bindings

**Package:** `github.com/aster-rpc/aster-go`
**Target:** Go 1.23+
**ABI contract:** Rust FFI via CGO, header at `iroh_ffi.h`
**Native library:** `libaster_transport_ffi.dylib` / `.so` / `.dll`

## Build

```bash
# Build the library
cd bindings/go
CGO_CFLAGS="-I$(pwd)/../../ffi" \
CGO_LDFLAGS="-L$(pwd)/../../target/release/deps -l aster_transport_ffi" \
go build ./...

# Run pure-Go unit tests (no cgo required)
CGO_ENABLED=0 go test -v ./...

# Run cgo tests (requires standard Go installation)
CGO_CFLAGS="-I$(pwd)/../../ffi" \
CGO_LDFLAGS="-L$(pwd)/../../target/release/deps -l aster_transport_ffi" \
go test -v ./...
```

## Source Files

| File | Purpose |
|------|---------|
| `go.mod` | Module definition |
| `iroh.go` | Status constants (`//go:build cgo`) |
| `types.go` | Event types and event kind constants (`//go:build !cgo`) |
| `error.go` | Typed errors with `errors.Is`/`errors.As` support (`//go:build cgo`) |
| `runtime.go` | `Runtime` type — poller goroutine, event dispatch, sync FFI helpers |
| `node.go` | `Node` type — memory/persistent creation, connection channel |
| `endpoint.go` | `Endpoint` type — create, connect, accept, close |
| `connection.go` | `Connection` type — bi/uni streams, datagrams |
| `stream.go` | `SendStream` / `RecvStream` types — write/read/finish |
| `cq_fake.go` | Goroutine-safe fake CQ for unit testing (`//go:build !cgo`) |
| `cq_test.go` | 8 CQ state machine unit tests (`//go:build !cgo`) |
| `abi_contract_test.go` | ABI contract tests — struct sizes, field offsets, ownership (`//go:build cgo`) |
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

Each `Runtime` starts one background goroutine that calls `iroh_poll_events` in a non-blocking loop and dispatches events over channels:

```go
func (r *Runtime) pollLoop() {
    for !r.closed.Load() {
        var events [64]C.iroh_event_t
        n := C.iroh_poll_events(C.uint64_t(r.handle), &events[0], 64, 10)
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
