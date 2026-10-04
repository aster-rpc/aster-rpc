# Aster Go FFI Bindings

**Package:** `github.com/aster-rpc/aster-go`
**Target:** Go 1.25+ (Fory Go 1.7.6 requires it)
**ABI contract:** Rust FFI via CGO, header `ffi/aster_ffi.h`, ABI 4.x (`aster_*` symbols)
**Native library:** `libaster_transport_ffi.dylib` / `.so` / `.dll`

## Status (ABI 4.0, dynamic dispatch, 2026-10-04)

**Working at RPC parity for open-gate interop.** Go is in the `tests/interop/`
matrix (Rust / Python / TypeScript / Java / .NET / Go, 6×6 = 36/36, identical
`InteropEcho` contract id `c5aa690b…`). The binding refuses a native library
whose ABI major differs (4).

- **Codec:** Apache Fory Go **1.7.6** (`xlang`, `compatible`, no ref tracking),
  safe for concurrent use (a pool of Fory instances, no global lock). Payload types implement `aster.WireTyped`
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
  `Memory*` / `Persistent*` are convenience wrappers over `aster_node_start`
  (ABI 4.0 removed `aster_node_memory*` / `aster_node_persistent`). ABI 4.0
  also removed bare endpoints: `NewEndpoint` starts a node from the
  `EndpointConfig` (mapped onto `AsterConfig`) and `Endpoint` is a view of it
  (`Endpoint.Node()`); `Accept` uses `aster_accept`, which takes connections
  from the node's ALPN queue.
- **Gate 0:** `NewGate` (`aster_gate_*`: trust mode, admission ALPNs, reject
  code/reason, admit/revoke/is-admitted/should-allow). Without a gate, a node
  with `EnableHooks` delivers `*AdmissionRequest`s on `Node.Admissions()`;
  answer with `Allow()` / `Deny(code, reason)` (`aster_admission_respond`).
- **Server (ABI 3.5 dynamic dispatch):** `StartServer(ctx, ServerOptions{Config,
  Services, Gate, Interceptors, ...})` registers each service as its JSON
  registration form (`DynamicServiceJSON`; contract id from
  `aster_dynamic_service_contract_id`) with `aster_server_*`; Rust's
  `aster::rpc::Server` decodes headers, routes, enforces Gate 3 (`Requires`),
  sessions (graveyard, per-connection cap), deadlines
  (`min(header, method Timeout, HandlerTimeout)`) and per-method
  `MaxFrameSize`, and serves `aster.ops.NodeInfo` (`DisableNodeInfo`,
  `Topology`). Go drains `aster_server_poll` on one goroutine, runs each call
  on its own goroutine and answers by call id (`aster_dispatch_respond` /
  `send` / `finish`; QUEUE_FULL reserves capacity and parks until
  `RESPONSE_WRITABLE`). Methods: `Unary`, `ServerStream`, `ClientStream`,
  `BidiStream` (generic, `context.Context` first, streams as
  `iter.Seq2[*Req, error]` + `send func(*Resp) error`); options `Idempotent`,
  `Timeout`, `MaxFrameSize`, `Requires(RequireRole/AnyOf/AllOf)`. The handler
  context carries the call deadline and ends on a peer cancel (streaming) or
  deadline. Session-scoped services: `NewSessionService(name, v, factory,
  ...)`, instance via `Session[T](ctx)`, closed (`io.Closer`) when the
  session ends. `SetPeerAttributes` feeds Gate 3 (attribute bridge);
  `CallInfo.Attributes` / `Metadata` are read lazily from Rust.
  `OnConnectionClosed`, `OnNonRPC` (accepted non-`aster/1` ALPN connections,
  owned by the callback). Every call ends with exactly one AEv1 `RpcStatus`
  (`*RpcError` keeps its code, deadline → `DEADLINE_EXCEEDED`, panics and
  other errors → `INTERNAL`).
- **Client:** `StartClient` / `NewClient`, `ConnectTicket`; `Call`,
  `CallServerStream`, `CallClientStream`, `CallBidiStream`, `OpenBidiStream`
  (`Send` / `CloseSend` / `Recv`) over the pooled `aster::rpc::RpcConnection`
  (`aster_rpc_unary`, `aster_rpc_open`, `aster_call_*_payload`). The ctx
  deadline is enforced natively. A unary call with a ctx that can end
  (`ctx.Done() != nil`) runs on its own native call (`aster_rpc_open`, as
  .NET does for a cancellable token) so cancelling ctx cancels it natively
  (`aster_call_cancel`: the stream is reset; Rust's server watches for peer
  cancels on streaming calls only, so a unary handler runs on and its answer
  is dropped); a ctx that
  never ends uses the pooled `aster_rpc_unary` fast path. Streaming calls are
  cancelled the same way. Every native receive wait (client
  `aster_call_recv_payload`, server `aster_dispatch_recv`) is bounded (200 ms
  slices) and re-checks the context between slices. Requests of client-stream / bidi calls are
  sent as the iterator yields them (ping-pong works). Sessions:
  `RpcConnection.NewSessionID` (the connection's shared counter) +
  `WithSession`; `WithMetadata`, `WithVersion`. Consumer admission is not
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

- Client interceptors are not wired; no `go generate` codegen yet; no
  consumer-admission handler (`OnNonRPC` hands the connection over).
- A client-stream call with no request is refused by the Rust client
  (`INVALID_ARGUMENT`).
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
go run ./cmd/aster_server_test      # typed server + client echo, bidi ping-pong

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
| `iroh.go` | Status / event-kind constants from `aster_ffi.h` (`//go:build cgo`) |
| `cmem.go` | C-memory arena + `aster_bytes_t` helpers for building FFI structs |
| `dispatch.go` | ABI 3.5 `aster_server_*` / `aster_dispatch_*` wrapper: poll, respond/send/finish with QUEUE_FULL reservation, recv, k/v lists |
| `server.go` | ALPN, frame flags, `EncodeFrame` |
| `config.go` | `AsterConfig` ↔ `aster_config_t`, `ConfigFromEnv` / `ConfigFromTOML` |
| `node_start.go` | `StartNode`, admission requests, connect/accept/ticket helpers |
| `gate.go` | `Gate` — Gate-0 policy (`aster_gate_*`) |
| `envelope.go` | AEv1 envelope, zstd, framing (via the C ABI) |
| `codec.go` | `Codec`, `ForyCodec` (Fory 1.7.6), `WireTyped` |
| `contract.go` | Contract identity: TypeDef / ServiceContract JSON → Rust hash |
| `rpc_service.go` | `Service` / `NewSessionService`, `Unary` / `ServerStream` / `ClientStream` / `BidiStream`, `Requires`, `CallInfo`, `DynamicServiceJSON` |
| `rpc_server.go` | `AsterServer` — dispatch events → typed handlers, sessions, cancellation, RpcStatus |
| `rpc_client.go` | `AsterClient`, `RpcConnection`, `Call*` functions, `BidiCall` (`aster_rpc_*`) |
| `dataplane.go` | ABI 3.3 blobs / tags / docs / paths / priority wrappers |
| `pure.go`, `grants.go` | ABI 3.4 ids, HPKE, attestation, sealed grants |
| `types.go` | Event types and event kind constants (`//go:build !cgo`) |
| `error.go` | Typed errors with `errors.Is`/`errors.As` support (`//go:build cgo`) |
| `runtime.go` | `Runtime` type — poller goroutine, event dispatch, sync FFI helpers |
| `node.go` | `Node` type — address, secret key, close; `Memory*` / `Persistent*` wrappers over `StartNode` |
| `endpoint.go` | `Endpoint` — a node-backed view (ABI 4.0): connect, accept, close, remote info, metrics |
| `connection.go` | `Connection` type — bi/uni streams, datagrams |
| `stream.go` | `SendStream` / `RecvStream` types — write/read/finish |
| `cq_fake.go` | Goroutine-safe fake CQ for unit testing (`//go:build !cgo`) |
| `cq_test.go` | 8 CQ state machine unit tests (`//go:build !cgo`) |
| `abi_contract_test.go` + `abi_layout.go` | ABI contract tests — header struct sizes, ABI version, status names, node-backed endpoint (`//go:build cgo`) |
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
