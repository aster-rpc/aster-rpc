# Aster Go FFI Bindings

**Package:** `github.com/aster-rpc/aster-go`
**Target:** Go 1.25+ (Fory Go 1.7.6 requires it)
**ABI contract:** Rust FFI via CGO, header `ffi/aster_ffi.h`, ABI 3.x (`aster_*` symbols)
**Native library:** `libaster_transport_ffi.dylib` / `.so` / `.dll`

## Status (ABI 3.4 catch-up, 2026-10-04)

**Working at RPC parity for open-gate interop.** Go is in the `tests/interop/`
matrix (Rust / Python / TypeScript / Java / .NET / Go, 6×6 = 36/36, identical
`InteropEcho` contract id `c5aa690b…`). The binding refuses a native library
whose ABI major differs (3).

- **Codec:** Apache Fory Go **1.7.6** (`xlang`, `compatible`, no ref tracking),
  safe for concurrent use. Payload types implement `aster.WireTyped`
  (`AsterWireType() string { return "namespace/Name" }`) and are registered by
  name automatically. **`[]byte` fields must be tagged `fory:"type=bytes"`**:
  untagged, Fory Go writes them as `list<uint8>`, not the `binary` every other
  binding uses (the contract builder refuses an untagged `[]byte`; use
  `aster:"uint8_list"` to mean `list<uint8>`).
- **Envelope / compression:** AEv1 `StreamHeader` / `CallHeader` / `RpcStatus`
  and zstd come from the C ABI (`EncodeStreamHeader`, `DecodeRpcStatus`,
  `ZstdMaybeCompress`, `EncodePayloadFrame`, ...); `conformance/vectors/
  envelope.json` and `compression.json` pass.
- **Contract ids:** `Service.ContractID()` builds the TypeDef / ServiceContract
  JSON from Go types and lets Rust canonicalise and hash it
  (`aster_canonical_bytes`, `aster_blake3_hex`, `aster_contract_id`).
  `int32` → `varint32`, `int64`/`int` → `varint64`, `uint32` → `var_uint32`
  (a `fory:"encoding=fixed"` tag selects the fixed forms), pointers are optional.
- **Node:** `AsterConfig` mirrors `aster_config_t`; `ConfigFromEnv()` /
  `ConfigFromTOML(path)` resolve through Rust `aster::AsterConfig` plus the
  binding-level `[trust]` / `[connect]` / `[logging]` keys. `StartNode(ctx, cfg,
  NodeOptions{Gate, ExtraALPNs})` uses `aster_node_start`; `Node.Ticket`,
  `Connect`, `Accept`, `AddNodeAddr`, `SetGate`, `MetricsPrometheus`, `Docs()`.
  The old `Memory*` / `Persistent*` / `NewEndpoint` factories are deprecated (ABI 4.0
  removes them).
- **Gate 0:** `NewGate` (`aster_gate_*`: trust mode, admission ALPNs, reject
  code/reason, admit/revoke/is-admitted/should-allow). Without a gate, a node
  with `EnableHooks` delivers `*AdmissionRequest`s on `Node.Admissions()`;
  answer with `Allow()` / `Deny(code, reason)` (`aster_admission_respond`).
- **Server:** `StartServer(ctx, ServerOptions{Config, Services, Gate,
  Interceptors})` serves `Unary`, `ServerStream`, `ClientStream` and
  `BidiStream` methods (generic, `context.Context` first, streams as
  `iter.Seq2[*Req, error]` + `send func(*Resp) error`). Header deadlines become
  context deadlines; every call ends with exactly one AEv1 `RpcStatus` trailer
  (`*RpcError` keeps its code, deadline → `DEADLINE_EXCEEDED`, panics and other
  errors → `INTERNAL`). The default gate is `TrustOpenDev` when
  `AllowAllConsumers` is set, else `TrustProtected`. QUEUE_FULL is handled by
  reserving response capacity (exercised by `TestRPC_QueueFullBackpressure`).
- **Client:** `StartClient` / `NewClient`, `ConnectTicket`; `Call`,
  `CallServerStream`, `CallClientStream`, `CallBidiStream` (generic package
  functions; the deadline comes from `ctx`, rounded up to whole seconds;
  `WithMetadata`, `WithSessionID`). Unary uses the pooled `aster_call_unary`
  fast path, streaming calls a dedicated substream. Consumer admission is not
  performed (open gate / admitted peers only, as for .NET and Java).
- **Errors:** `errors.Is` against `ErrNotFound`, `ErrInvalidArgument`,
  `ErrVerificationFailed`, ... (status codes) and `ErrRpcNotFound`,
  `ErrRpcUnimplemented`, ... (RPC codes); `errors.As` to `*RpcError` /
  `*IrohError`. Native failures carry `aster_last_error_message`.
- **ABI 3.3 data plane:** `Blobs.DownloadHashFrom` (+ `FetchReport`),
  `DownloadHash`, `ReadRange`, `GCRunOnce`; `Tags.DeletePrefix`;
  `Docs.AuthorityReport` / `Open` / `ImportWriteNamespace` /
  `ImportReadNamespace` / `RetireLocalWriteAuthority`; `Doc.AuthorityReport` /
  `SetBytesDurable` / `EntryExists` / `EntryContentStatus` /
  `ShutdownForRoleSwap`; `AuthorityFingerprint`; `Connection.Paths` /
  `SelectedPath`; `SendStream.SetPriority` / `Priority`. Streams also adapt to
  `io.Reader` / `io.WriteCloser` (`RecvStream.Reader`, `SendStream.Writer`).
- **ABI 3.4 pure functions:** typed ids (`ParseID`, `FormatID`, `ValidateID`,
  `SecretKeyPublicKey`, `NamespaceSecretID`), HPKE (`HPKESeal` / `HPKEOpen`,
  identity-derived X25519), attestation (`AttestRootNode`,
  `VerifyAttestationChain`, text form), sealed grants (`GrantSeal` /
  `GrantOpen`, namespace grants, `GrantRecord`, `ReconcileGrant`);
  `conformance/vectors/grants.json` passes.

Fixed in the catch-up: completions that arrived before their operation was
registered were dropped (calls could hang); `ReadDatagram` rejected the
`DATAGRAM_RECEIVED` completion; ticket encode/decode and the canonical/contract
helpers panicked on empty input and used fixed buffers; the hand-rolled ticket
JSON parser mis-read `null`; the two blob-collection tests built invalid JSON /
expected no error for invalid JSON; status 13 (`VERIFICATION_FAILED`) was
unnamed; `Node` had no `Docs()`.

Known gaps:

- Session-scoped services are not served (`StartServer` rejects them); client
  interceptors are not wired; no `go generate` codegen yet.
- Unary cancellation stops waiting but does not cancel the native call.
- `CallBidiStream` waits for the request iterator to finish before returning.
- The payload codec serialises through one Fory instance under a mutex.
- `hostile_race_test.go` / `TestSoak_Short` have no peer and only check "no
  crash"; `TestSoak_Runs` (30 min) is skipped unless `ASTER_GO_SOAK=1`.

## Build

```bash
# Build the native library (debug) from the repo root
cargo build -p aster_transport_ffi

cd bindings/go
export CGO_CFLAGS="-I$(pwd)/../../ffi"
export CGO_LDFLAGS="-L$(pwd)/../../target/debug -laster_transport_ffi"
export DYLD_LIBRARY_PATH="$(pwd)/../../target/debug"   # LD_LIBRARY_PATH on Linux

go vet ./...
go test ./...                       # all cgo tests (RPC, node, vectors, data plane)
go run ./cmd/aster_server_test      # legacy raw reactor echo

# Pure-Go unit tests (no native library)
CGO_ENABLED=0 go test ./...
```

cgo does not track changes to `ffi/aster_ffi.h` in Go's build cache: after an
ABI bump run `go test -a ...` once (a stale header shows up as
`TestABI_VersionMatchesHeader` failing).

The interop leg lives in `tests/interop/go/` (its own module, `replace`d onto
this one); `tests/interop/run.py` builds it with an rpath to the debug library.

## Source Files

| File | Purpose |
|------|---------|
| `go.mod` | Module definition |
| `iroh.go` | Status / event-kind / reactor-event constants from `aster_ffi.h` (`//go:build cgo`) |
| `cmem.go` | C-memory arena + `aster_bytes_t` helpers for building FFI structs |
| `reactor.go` | `Reactor` — poll (event kinds), submit with QUEUE_FULL capacity reservation |
| `server.go` | `Server` — raw reactor poll loop + byte-level handler (legacy; use `AsterServer`) |
| `config.go` | `AsterConfig` ↔ `aster_config_t`, `ConfigFromEnv` / `ConfigFromTOML` |
| `node_start.go` | `StartNode`, admission requests, connect/accept/ticket helpers |
| `gate.go` | `Gate` — Gate-0 policy (`aster_gate_*`) |
| `envelope.go` | AEv1 envelope, zstd, framing (via the C ABI) |
| `codec.go` | `Codec`, `ForyCodec` (Fory 1.7.6), `WireTyped` |
| `contract.go` | Contract identity: TypeDef / ServiceContract JSON → Rust hash |
| `rpc_service.go` | `Service`, `Unary` / `ServerStream` / `ClientStream` / `BidiStream`, `CallInfo` |
| `rpc_server.go` | `AsterServer` — typed dispatch, deadlines, RpcStatus trailers |
| `rpc_client.go` | `AsterClient`, `RpcConnection`, `Call*` functions |
| `dataplane.go` | ABI 3.3 blobs / tags / docs / paths / priority wrappers |
| `pure.go`, `grants.go` | ABI 3.4 ids, HPKE, attestation, sealed grants |
| `types.go` | Event types and event kind constants (`//go:build !cgo`) |
| `error.go` | Typed errors with `errors.Is`/`errors.As` support (`//go:build cgo`) |
| `runtime.go` | `Runtime` type — poller goroutine, event dispatch, sync FFI helpers |
| `node.go` | `Node` type — address, secret key, close; deprecated memory/persistent factories |
| `endpoint.go` | `Endpoint` type — create, connect, accept, close |
| `connection.go` | `Connection` type — bi/uni streams, datagrams |
| `stream.go` | `SendStream` / `RecvStream` types — write/read/finish |
| `cq_fake.go` | Goroutine-safe fake CQ for unit testing (`//go:build !cgo`) |
| `cq_test.go` | 8 CQ state machine unit tests (`//go:build !cgo`) |
| `abi_contract_test.go` + `abi_layout.go` | ABI contract tests — header struct sizes, ABI version, status names, endpoint config layout (`//go:build cgo`) |
| `hostile_race_test.go` | 7 hostile race integration tests (`//go:build cgo`) |
| `soak_test.go` | 2 soak/leak tests (`//go:build cgo`) |
| `rpc_test.go`, `node_test.go`, `dataplane_test.go`, `vectors_test.go` | RPC (4 patterns, errors, deadlines, gate, interceptors, QUEUE_FULL), config/node/gate/admission, data plane, conformance vectors |

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
