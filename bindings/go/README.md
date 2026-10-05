# Aster Go FFI Bindings

**Package:** `github.com/aster-rpc/aster-go`
**Target:** Go 1.25+ (Fory Go 1.7.6 requires it)
**ABI contract:** Rust FFI via CGO, header `ffi/aster_ffi.h`, ABI 4.x (`aster_*` symbols)
**Native library:** `libaster_transport_ffi.dylib` / `.so` / `.dll`

## Status (ABI 4.4: upcalls, leases, expose, streaming relay, coordination; 2026-10-04)

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
  **Contract defaults** (spec 11.3.2.3): `aster:"default=VALUE"` (or a bare
  `aster:"default"` for the Go zero value / empty container) marks a field
  `required = false` with the canonical default, so the contract id matches
  Rust / Python / TS (`conformance/vectors/contract-defaults.json` passes).
  Supported: string, bool, int32, int64/int, float64 and empty containers;
  other types are an error when the contract is built (see `contract_default.go`).
- **Client interceptors:** `StartClient(ctx, cfg, WithClientInterceptors(...))`
  (or per call `WithInterceptors`) takes the same `Interceptor`s as the server,
  with the Python client's semantics (`client_interceptors.go`): request hooks
  in order, response hooks in order, error hooks in reverse; unary and
  client-streaming calls also go through the retry loop (`CircuitBreakerInterceptor`
  gate and outcome, `RetryInterceptor` for idempotent calls on retryable codes
  with backoff bounded by the context, `DeadlineInterceptor` refusing an expired
  call; the context deadline is what is sent natively); server-stream and bidi
  get the hooks only. `AsterClient.RegisterService` supplies each method's
  idempotent flag; `WithIdempotent` overrides it.
- **Node:** `AsterConfig` mirrors `aster_config_t`; `ConfigFromEnv()` /
  `ConfigFromTOML(path)` resolve through Rust `aster::AsterConfig` plus the
  binding-level `[trust]` / `[connect]` / `[logging]` keys. `StartNode(ctx, cfg,
  NodeOptions{Gate, ExtraALPNs})` uses `aster_node_start`; `Node.Ticket`,
  `Connect`, `Accept`, `AddNodeAddr`, `SetGate`, `Docs()`. (ABI 4.6 removed
  `aster_node_metrics_prometheus`; metrics come back with observability in O10.)
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
  deadline is enforced natively. Every unary call takes the pooled
  single-write fast path; a ctx that can end (`ctx.Done() != nil`) adds a
  cancel token (ABI 4.5, `aster_rpc_unary_cancellable`), so cancelling ctx
  aborts the native call (the pooled stream is discarded with `FLAG_CANCEL` +
  reset and the server's handler sees the cancel). Streaming calls run on
  their own stream (`aster_rpc_open`) and are cancelled with
  `aster_call_cancel`. **Server handlers:** every handler's `ctx` (unary
  included) ends when the client cancels, when the Rust-side deadline passes,
  and when the handler returns. Every native receive wait (client
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

- **ABI 4.1 upcalls, leases, topology** (`upcall.go`, `lease.go`, `ucv1.go`):
  Rust asks Go a question as an event and Go answers by id. `NewUpcalls(rt,
  capacity)` owns one poll goroutine (`aster_upcall_poll`, 100 ms slices) and
  runs each upcall on its own goroutine; the handler's `context.Context` is
  cancelled on a CANCEL event (timeout / abandoned / shutdown) and on `Close`.
  `SetPolicy(kind, timeout, FailureModeOpen|Closed)` / `SetDefaultPolicy`
  (default 5 s, fail closed; decision kinds take the failure mode when Go
  gives no answer). Handlers: `HandleKind` (raw UCv1), `ServeLeaseSerializer`,
  `ServeLeaseAuthorizer`, `ServeFencedResource`, `ServeAdmittedFilter`,
  `ServeExposeAuthorize`, `ServeRoutePolicy`; an error answers ERROR,
  `ErrUpcallUnimplemented` answers UNIMPLEMENTED, a panic answers ERROR.
  `NewForeignLeaseSerializer` / `NewMemoryLeaseSerializer` (+ Go authorizer)
  give `Acquire` -> `*Lease` (`Fence`, `Status`, `Watch`, `Release`, `Close`;
  a refusal is `*LeaseDeniedError`, `errors.Is(ErrLeaseDenied)`),
  `Snapshot`, `Revoke`; `NewForeignFencedResource` (`Mutate`;
  `ErrMutationOutcomeUnknown` when it cannot answer); `NewAdmittedFilter`
  (cached, batched; `IsAdmitted` / `Set` / `Invalidate` / `Prefetch` /
  `Stats`) and `Node.EnableShared`. `aster_operation_cancel` does not stop a
  native operation, so an `Acquire` whose context ends is not cancelled
  natively: a lease granted late is released in the background. The UCv1 codec
  (`Encode*` / `Decode*`, `ErrUCv1`, plain Go, no cgo) passes every
  `conformance/vectors/upcalls.json` case. Not in the C ABI: placement.
- **ABI 4.2 expose** (`expose.go`, `expose_http.go`): `NewExpose(node)`;
  `Serve(alpns...)` (owns the node's accept loop) or `HandleConnection`;
  `ExposeSocket(id, addr, authorizerUpcalls)` (nil authorizer admits every
  peer: only for a gated node); `ExposeHTTP(up, bodies, id, http.Handler,
  opts)` serves relayed requests with a `net/http` handler (small bodies
  inline in one round trip; `Flush`, a large body or a declared `Trailer`
  streams with backpressure; `ExposeInfoFrom(ctx)` gives the peer id; a panic
  is a 500, `http.ErrAbortHandler` aborts the stream); `ExposeRoutePolicy`;
  `Unexpose`. `HTTPBodies` is the body table (`Read` / `Write` / `TryWrite` /
  `Finish` / `Abort`, `*BodyError` with `ErrBody*` sentinels). Consumer side:
  `Connection.RelayRequest` / `RelayHTTP` (buffered) and `RequestRoute`. Close
  the upcall table before the runtime, and `Unexpose` services that must stop
  before closing the registry. The `ExposeInfo` / handler behaviour follows
  `docs/_internal/bindings/upcall-design.md` section 6.

- **ABI 4.3 streaming consumer relay** (`relay_stream.go`):
  `Connection.RelayStream(ctx, RelayStreamRequest{ServiceID, Method, URI,
  Headers, Body io.Reader, OnUploadError})` returns at the response head
  (`RelayResponse.Body` is an `io.ReadCloser`) while the upload keeps
  running. A body-source failure before the head is the call's error; after
  it, `OnUploadError` hears it. `Body.Close` before EOF abandons the exchange
  and resets an unfinished upload; after a clean EOF the upload finishes. ctx
  covers the whole exchange; a source that is an `io.Closer` is closed on
  abandon. Refusals are `ErrRelayDenied` (also `ErrPermissionDenied`),
  `ErrRelayUnknownService` (also `ErrNotFound`), `ErrRelayOverloaded`, ...;
  statuses 14-16 now have `ErrUnauthenticated` / `ErrPermissionDenied` /
  `ErrCredentialExpired`, and a denied buffered `RelayRequest` matches
  `ErrPermissionDenied` too.

- **ABI 4.4 coordination client + placement** (`coordination.go`,
  `coordination_codec.go`, `coordination_codes.go`, `ucv1_placement.go`):
  `OpenCoordinationClient(ctx, node, CoordinationOptions{GroupID, SeedTickets,
  JournalDir, ReadOnly})`. `Mutate` (and `Acquire` / `Renew` /
  `CompareAndSet` / `ReleaseFence` / `ReleaseWithValue` / `Revoke` /
  `SetGrant` / `SetAdmin` / `ChangeMembership` / `CancelMembership`) returns a
  `*ResolvedMutation` you must `Acknowledge` (no new mutation until then);
  `Unacknowledged` / `Resume` redeliver after a reopen (nil = none). A committed
  typed rejection is a result (`Reply.Rejected()`), not an error. Reads,
  `RetireOtherClient`, `RecoveryExportToFile`, `AcquireLease` ->
  `*CoordinationLease` (a denial is `*LeaseDeniedError`), and
  `NewPlacementManager(client, upcalls, topologyNode)` with Go providers
  (`ServePlacementPolicy` / `ServePlacementProvisioner` /
  `ServeTopologySnapshot`; a provider failure fails the pass). Errors are
  `*CoordinationError{Kind, RPCCode, Status}`; `errors.Is` matches
  `ErrCoordinationAwaitingAck` / `JournalLocked` / ... and the mapped status
  (`ErrPermissionDenied` for an RPC refusal by the replicated ACL). Release and
  Close wait for an in-flight CompareAndSet (up to its full drive budget): do
  not model them as prompt. Live tests need the voter fixture binary
  (`cargo build -p aster-transport-napi --features coordination-fixture --bin
  aster-coordination-fixture`, or `ASTER_COORDINATION_FIXTURE`); without it they
  skip.

Fixed in the catch-up: completions that arrived before their operation was
registered were dropped (calls could hang); `ReadDatagram` rejected the
`DATAGRAM_RECEIVED` completion; ticket encode/decode and the canonical/contract
helpers panicked on empty input and used fixed buffers; the hand-rolled ticket
JSON parser mis-read `null`; the two blob-collection tests built invalid JSON /
expected no error for invalid JSON; status 13 (`VERIFICATION_FAILED`) was
unnamed; `Node` had no `Docs()`.

Known gaps:

- No `go generate` codegen yet; no
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
# The rpath is required on macOS: the dylib's install name is @rpath/...,
# and DYLD_LIBRARY_PATH alone did not reach the test binary under `go test`.
export CGO_LDFLAGS="-L$(pwd)/../../target/debug -laster_transport_ffi -Wl,-rpath,$(pwd)/../../target/debug"
export LD_LIBRARY_PATH="$(pwd)/../../target/debug"     # Linux

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
| `ucv1.go`, `upcall_types.go` | UCv1 upcall payload codec and kind / status tables (plain Go, no cgo) |
| `upcall.go` | ABI 4.1 `Upcalls`: poll goroutine, per-kind policy, typed serving, owned-operation helper |
| `lease.go` | ABI 4.1 leases, fenced resource, topology admitted filter, `Node.EnableShared` |
| `coordination*.go`, `ucv1_placement.go` | ABI 4.4 coordination client, D0-DE codec, outcome / reason codes, placement messages |
| `relay_stream.go` | ABI 4.3 `Connection.RelayStream` (full-duplex streaming relay, readiness-driven) |
| `expose.go`, `expose_http.go` | ABI 4.2 expose registry, body table, relay / route requests, `net/http` handler adapter |
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
| `ucv1_test.go`, `upcall_test.go`, `expose_test.go` | UCv1 vectors, upcall / lease / fenced / topology flows, expose flows (`-race -count=20` clean) |
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
