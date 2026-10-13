# Aster Go FFI bindings

**Module path:** `sdk.getaster.now/go`

The module path is `sdk.getaster.now/go` (Go 1.25+). This guide uses
`bindings/go` in a source checkout. Public installation is pending HTTPS vanity
discovery and public release tags; the module path alone is not yet a working
public installation source. See [publication setup](../../deploy/go-vanity/README.md).
**Target:** Go 1.25+ (Fory Go 1.7.6 requires it)
**ABI contract:** Rust FFI via CGO, header `ffi/aster_ffi.h`, ABI 4.x (`aster_*` symbols)
**Native library:** `libaster_transport_ffi.dylib` / `.so` / `.dll`

## Preview status (ABI 5.0 minimum)

Go participates in the six-language `tests/interop/` matrix with Rust, Python,
TypeScript, Java and .NET. The binding requires native ABI 5.0 or newer within
major version 5 (`ABIMinorRequired` in `runtime.go`). Aster 0.3 is a preview release.

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
  `aster_node_metrics_prometheus`; `MetricsPrometheus()` replaces it, see
  Observability below.)
  `Memory*` / `Persistent*` are convenience wrappers over `aster_node_start`
  (ABI 4.0 removed `aster_node_memory*` / `aster_node_persistent`). ABI 4.0
  also removed bare endpoints: `NewEndpoint` starts a node from the
  `EndpointConfig` (mapped onto `AsterConfig`) and `Endpoint` is a view of it
  (`Endpoint.Node()`); `Accept` uses `aster_accept`, which takes connections
  from the node's ALPN queue.
- **Observability (ABI 4.6):** the pipeline (OpenTelemetry SDK, exporters, log
  bridge, Prometheus renderer) lives in Rust; the binding starts it and wraps the
  rest. `InitObservability(ObservabilityConfig{...})` (before the first server;
  otherwise `StartServer` / `StartHealth` run the implicit environment-only
  start) and `ShutdownObservability`. Floor: the binding requires ABI 5.0 or newer
  (`ABIMinorRequired`, checked by `CheckABI` / `NewRuntime`).
  - *Logs:* Rust `tracing` events reach `log/slog`. One goroutine on a locked OS
    thread polls `aster_log_poll`; `SetLogger(*slog.Logger)` picks the
    destination (default `slog.Default()`), and the Rust filter follows the
    logger's level (`SyncLogLevel`, re-checked every 5 s; `LogFilter` / `ASTER_LOG`
    pin it). Records carry `logger`, `aster_origin=rust`, `trace_id`, `span_id`,
    `aster_call_id` and the event's fields (`rpc.service`, `rpc.method`, ...);
    TRACE is `aster.LevelTrace` (-8). `NewTraceHandler(h)` wraps your own handler
    to add `trace_id` / `span_id` from the `context.Context` of
    `logger.InfoContext(ctx, ...)`.
  - *Trace context:* inside a handler `ctx` holds the call's server span and the
    caller's W3C baggage (`CurrentTraceID`, `CurrentSpanID`, `CurrentBaggage`,
    `SpanContextFromContext`). `StartSpan(ctx, name)` / `WithSpan` start spans in
    the Rust tracer under it; calls made with that `ctx` send `traceparent` and
    `baggage`. `WithTraceContext(ctx, traceparent, baggage)` continues a trace
    from an HTTP header or a queue message. Never put secrets in baggage: values
    end up in log lines.
  - *Metrics:* `NewCounter`, `NewUpDownCounter`, `NewHistogram`, `NewGauge`
    (scope `aster.app`; `Add` / `Record(ctx, v, attrs...)`, `NewAttrSet` to intern
    attributes). They share the exporter and `/metrics` with the built-in
    `aster.*` metrics. `MetricsPrometheus()` returns the text (needs the
    Prometheus pull reader: a `HealthServer`, or `OTEL_METRICS_EXPORTER=prometheus`).
  - *Health:* `StartHealth(node, &HealthConfig{...})` or
    `ServerOptions{Health: &HealthConfig{}}` serves `/healthz`, `/readyz` and
    `/metrics` (127.0.0.1 unless `Host` says otherwise); a server-attached listener
    is ready once the server serves and not ready first on `Close`.
    `SetReady`, `SetHealthy`, `SetCheck(name, ok)`, `Addr`, `Stop`.
  - `MetricsInterceptor` is gone: the built-in metrics record in Rust.
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
- **Docs and file imports:** `Doc.ID`, `Del`, `StartSync(ctx, peers...)`,
  and `ShareWithAddr` wrap the existing ABI. `GetExact` returns the value in
  `DocEntry.Value` with the requested key/author and its length; use `Query`
  for `ContentHash`, `ContentLen`, and `Timestamp`, then `ReadEntryContent`
  for bytes. Query results contain metadata, not inline values.
  `Blobs.AddPath` copies a file into the store with automatic retention;
  `AddPathWithNamedTag` uses only the named retention root and replaces that
  tag's previous value. Cancellation stops the Go wait but an import already
  started may still finish. Docs live events use `Doc.Subscribe` or atomic
  `Docs.JoinAndSubscribeWithEvents`. The returned `DocSubscription` exposes
  `Recv(ctx)` and must be closed. Timeout preserves subsequent events; a
  completion that wins the cancellation race is returned. Close wakes pending
  receives with `io.EOF`. The old document-only `JoinAndSubscribe` is deprecated.
- **ABI 3.4 pure functions:** typed ids (`ParseID`, `FormatID`, `ValidateID`,
  `SecretKeyPublicKey`, `NamespaceSecretID`), HPKE (`HPKESeal` / `HPKEOpen`,
  identity-derived X25519), sealed grants (`GrantSeal` /
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
  before closing the registry. `ExposeInfoFrom(ctx)` identifies the authenticated
  peer for each request.

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

Known gaps:

- No `go generate` codegen yet; no
  consumer-admission handler (`OnNonRPC` hands the connection over).
- A client-stream call with no request is refused by the Rust client
  (`INVALID_ARGUMENT`).
- `hostile_race_test.go` / `TestSoak_Short` have no peer and only check "no
  crash"; `TestSoak_Runs` (30 min) is skipped unless `ASTER_GO_SOAK=1`.

## Authenticated HTTPS and WebTransport

ABI 4.11 adds `AsterServer.StartHTTP` over the server's registered RPC services.
Register a typed authenticator on an upcall table from `srv.Node().Runtime()`
before starting the listener. The authenticator validates credentials from
`RPCAuthRequest.Metadata` and returns `RPCAuthResponse{Accepted: true,
Principal: ..., Attributes: ...}`. A successful principal must be nonempty;
`Accepted: false` rejects credentials, while a callback error reports an
authentication backend failure. Resolved attributes apply to that call's Gate 3
checks. An HTTP address is an identity hint; accepted calls retain HTTP/WT
provenance rather than becoming Iroh-authenticated peers.

```go
// auth implements aster.RPCAuthenticator using your credential backend.
// The caller owns srv and closes it after this function returns.
func serveHTTPS(ctx context.Context, srv *aster.AsterServer,
	certPEM, keyPEM []byte, auth aster.RPCAuthenticator) error {
	up, err := aster.NewUpcalls(srv.Node().Runtime(), 256)
	if err != nil { return err }
	defer up.Close()
	if err := up.ServeRPCAuthenticator(auth); err != nil { return err }
	if err := up.SetPolicy(aster.UpcallRpcAuthenticate,
		2*time.Second, aster.FailureModeClosed); err != nil { return err }
	listener, err := srv.StartHTTP(ctx, up, aster.HTTPListenerConfig{
		BindAddr: "127.0.0.1:0",
		CertificatePEM: certPEM, PrivateKeyPEM: keyPEM,
		WebTransport: true,
		MaxPendingAuth: 256, MaxAuthPayloadBytes: 64 << 10,
	})
	// A rare cleanup failure can return an owner together with an error.
	if listener != nil { defer listener.Close() }
	if err != nil { return err }
	fmt.Println("HTTPS:", listener.TCPAddr(), "WebTransport:", listener.QUICAddr())
	<-ctx.Done()
	return listener.Stop(2*time.Second)
}
```

Startup returns after TCP and optional QUIC bind; WebTransport uses the same
bound address and port. `QUICAddr()` is empty when disabled. Empty `BindAddr`
selects `127.0.0.1:0`; zero auth limits select the defaults shown above. Pending
auth capacity is acquired before encoding and remains occupied after queue
polling until completion, timeout or cancellation. Kind 80 always fails closed,
including when its failure policy is set to `FailureModeOpen`.

Stop the listener, close its upcall table, then close the server and any separately
owned runtime. `Stop(grace)` drains until the grace expires, then cancels remaining
work and joins native tasks; `Close()` cancels application work immediately.
The grace bounds application draining, while QUIC protocol/socket cleanup may
extend beyond it. Both release TCP/UDP ports on success. Stop errors preserve the listener owner for retry. Server
shutdown also stops its children, but their Go wrappers still require `Close()`.
The start context is checked before and after synchronous native startup;
cancellation during startup stops a newly created listener before returning.
Auth callbacks should observe their context when canceled.

This surface accepts PEM certificate/key material from the operator or the
certificate helpers below. The Go client
currently speaks Iroh RPC; there is no Go HTTPS/WT client wrapper yet. HTTP clients
can POST Aster frames to `/aster/{service}/{method}`, while WebTransport clients
open `/aster/wt` and use one bidirectional stream per RPC call.

### Explicit public serving and certificate helpers

ABI 4.12 adds a separately named `StartPublicHTTP`; missing authentication on
`StartHTTP` still fails. Public serving takes no upcall table, supplies no
capability attributes, preserves Gate 3 and Iroh-only checks, and rejects all
session-scoped services with `UNAUTHENTICATED` before their factory runs. Its
authentication-limit fields must remain zero. Use it only for services intended
to accept uncredentialed calls. It has the same readiness and joined cleanup
contract as authenticated serving.

```go
cert, err := aster.GenerateWebTransportCertificate("my-service",
    []string{"localhost", "127.0.0.1"}, nil)
if err != nil { return err }
listener, err := srv.StartPublicHTTP(ctx, aster.HTTPListenerConfig{
    CertificatePEM: cert.CertificatePEM, PrivateKeyPEM: cert.PrivateKeyPEM,
    WebTransport: true,
})
if listener != nil { defer listener.Close() }
if err != nil { return err }
// cert.SHA256 hashes the complete DER certificate for serverCertificateHashes.
```

`GenerateSelfSignedCertificate(sans, nodeSecret)` generates an ECDSA certificate;
empty SANs select `localhost`. `GenerateWebTransportCertificate(subject, sans,
nodeSecret)` generates P-256 material with validity at most 14 days and includes
`localhost`. The subject is a display name. Pass nil for an unbound certificate
or exactly 32 bytes of an Aster node secret for a signed TLS public-key binding.
These helpers never emit an unsigned node-identity claim. Protect the returned
private-key PEM and distribute the certificate hash through a trusted channel.

`VerifyCertificateBinding(cert.CertificatePEM, trustedNodeID)` verifies the signed
binding against a node ID learned independently through a trusted channel. It
proves the TLS public-key binding, not the whole certificate, TLS chain,
hostname, validity or trust in that expected ID. Generation is not ACME and
makes no FIPS validation claim. Owned ACME, static-mount/projection configuration
and Go HTTP/WT clients remain outside this surface.

## Observability

Logs into `log/slog`, custom metrics, tracing and the health listener are
covered in the [observability guide](../../docs/aster-observability-getstarted.md).

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

For a staged native archive, use its extracted directory in both `CGO_CFLAGS`
and `CGO_LDFLAGS`; the packaging workflow includes the matching `aster_ffi.h`
beside the library. On macOS, point the rpath to that directory as well.
This supports an external cgo build; public module installation still depends
on publishing a resolvable Go module identity and tags.

cgo does not track changes to `ffi/aster_ffi.h` in Go's build cache: after an
ABI bump run `go test -a ...` once (a stale header shows up as
`TestABI_VersionMatchesHeader` failing).

The interop leg lives in `tests/interop/go/` (its own module, `replace`d onto
this one); `tests/interop/run.py` builds it with an rpath to the debug library.

## Source files

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
| `observability_pure.go` | Observability, plain Go (no cgo): attribute and `traceparent` / baggage codecs, `context.Context` carrier, `TraceHandler`, Rust-record-to-slog mapping |
| `observability.go`, `metrics.go`, `tracing.go`, `health.go` | ABI 4.6 pipeline init / shutdown and the log poller; metric instruments; spans; `HealthServer` |
| `abi_trace.go` | `aster_trace_context_*` wrappers (test support: pin the Go codec to the library) |
| `rpc_server.go` | `AsterServer` — dispatch events → typed handlers, sessions, cancellation, RpcStatus |
| `rpc_client.go` | `AsterClient`, `RpcConnection`, `Call*` functions, `BidiCall` (`aster_rpc_*`) |
| `ucv1.go`, `upcall_types.go` | UCv1 upcall payload codec and kind / status tables (plain Go, no cgo) |
| `upcall.go` | ABI 4.1 `Upcalls`: poll goroutine, per-kind policy, typed serving, owned-operation helper |
| `rpc_http_listener.go` | ABI 4.11 authenticated and 4.12 explicit public HTTPS/WebTransport listeners, bound addresses and joined stop |
| `tls.go` | ABI 4.12 self-signed/WebTransport PEM and DER hash generation, signed TLS public-key binding verification |
| `lease.go` | ABI 4.1 leases, fenced resource, topology admitted filter, `Node.EnableShared` |
| `coordination*.go`, `ucv1_placement.go` | ABI 4.4 coordination client, D0-DE codec, outcome / reason codes, placement messages |
| `relay_stream.go` | ABI 4.3 `Connection.RelayStream` (full-duplex streaming relay, readiness-driven) |
| `expose.go`, `expose_http.go` | ABI 4.2 expose registry, body table, relay / route requests, `net/http` handler adapter |
| `dataplane.go` | ABI 3.3 blobs / tags / docs / paths / priority wrappers |
| `pure.go`, `grants.go` | ABI 3.4 ids, HPKE, sealed grants |
| `types.go` | Event types and event kind constants (`//go:build !cgo`) |
| `error.go` | Typed errors with `errors.Is`/`errors.As` support (`//go:build cgo`) |
| `runtime.go` | `Runtime` type — poller goroutine, event dispatch, sync FFI helpers |
| `node.go` | `Node` type — address, secret key, close; `Memory*` / `Persistent*` wrappers over `StartNode` |
| `endpoint.go` | `Endpoint` — a node-backed view (ABI 4.0): connect, accept, close, remote info, metrics |
| `connection.go` | `Connection` type — bi/uni streams, datagrams |
| `stream.go` | `SendStream` / `RecvStream` types — write/read/finish |
| `cq_fake.go` | Goroutine-safe fake CQ for unit testing (`//go:build !cgo`) |
| `cq_test.go` | CQ state machine unit tests (`//go:build !cgo`) |
| `abi_contract_test.go` + `abi_layout.go` | ABI contract tests — header struct sizes, ABI version, status names, node-backed endpoint (`//go:build cgo`) |
| `ucv1_test.go`, `upcall_test.go`, `expose_test.go` | UCv1 vectors, upcall / lease / fenced / topology flows, expose flows |
| `observability_pure_test.go`, `observability_test.go` | Pure observability cases (also `CGO_ENABLED=0`) and native integration tests: logs with trace ids, spans parented from dispatch, baggage, Prometheus text, health endpoints, shutdown / re-init |
| `hostile_race_test.go` | Hostile race integration tests (`//go:build cgo`) |
| `soak_test.go` | Soak/leak tests (`//go:build cgo`) |
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

### Error model

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

## Temporary custody and verified retained imports (ABI 4.13)

Use an owned scope when fetching or inspecting bytes before deciding to retain
an object. It protects multiple typed roots, including hashes not local yet:

```go
scope, err := node.Blobs().NewCustodyScope()
if err != nil { return err }
defer scope.Close()
if err := scope.Protect(ctx, expectedHash, aster.BLOB_FORMAT_RAW); err != nil { return err }
data, _, err := scope.DownloadHashFrom(ctx, expectedHash, providers,
    aster.BLOB_FORMAT_RAW, aster.FetchWhole, false)
if err != nil { return err }
// Verify application-specific content in data while scope remains open.
if err := scope.Retain(ctx, expectedHash, aster.BLOB_FORMAT_RAW, "verified/object"); err != nil { return err }
```

`AddBytes` and `AddPath` on the scope create temporary roots without automatic
persistent tags. `Read` and `DownloadHashFrom` keep its guards through accepted
work even when `Close` races completion; closing seals new submissions. Keep
scopes open for host-side verification and for reads/fetches submitted through
older APIs outside the scope. `HashSeq` protects the sequence graph without
imposing Collection metadata; scoped reads return the raw sequence bytes.

For an expected hash known in advance, `Blobs.AddBytesVerified` and
`Blobs.AddPathVerified` copy input, verify its store-computed hash, and commit
exactly the supplied named tag only on a match. Mismatch preserves the previous
named root and leaves no anonymous persistent tag. Reusing a name replaces its
root. Existing store durability applies; no stronger fsync/power-loss guarantee
is added.

These operations keep their completion registration after cancellation and wait
for native cleanup. A tag commit already begun returns its actual success/error;
committed success wins a racing canceled context. Fetch cancellation can wait
for downloader/network drain, so it is not a prompt deadline for a stalled peer.
Close each scope after host verification or named retention; disposing the node
does not replace this ownership obligation.

## Locally complete batch Docs reads (ABI 4.14)

```go
values, err := doc.ReadEntryContentsIfComplete(ctx,
    []aster.BlobID{firstHash, missingHash, emptyHash, firstHash})
if err != nil { return err }
// values[i] == nil means missing or partial locally.
// A non-nil slice of length zero is complete empty content.
```

The ordered result preserves duplicates and copies every present value. The
method submits one native operation and performs no download, tag update or
retention. Empty input returns a non-nil empty result. All hashes must contain
exactly 64 hexadecimal characters; an invalid hash rejects the entire request.
Missing or partial blobs at the initial check return nil. Complete inline data
is copied in the store transaction; larger values use validated export. This
is not a global snapshot: GC/deletion before large export may fail the whole
batch, and an I/O/integrity error is never converted to a missing slot. Hold a
PC08 custody scope when host verification needs stable retention.

## Coordination authority provisioning (ABI 4.15)

`CoordinationCanonicalGenesis` and `CoordinationCanonicalJoinPermit` take typed
hex32 document inputs and return native canonical Fory bytes, preserving list
order. `CoordinationGenesisSigningBytes` / `CoordinationJoinSigningBytes` produce
exact preimages for your external Ed25519 signer. Assemble its raw 32-byte public
key and 64-byte signature with `CoordinationSignedDocumentFromParts`.
`CoordinationCanonicalCandidateTicket` derives the native open candidate ticket;
`CoordinationGenesisHash` hashes opaque bytes without verifying them.

Verify an external envelope with `CoordinationVerifyGenesis` or
`CoordinationVerifyJoinPermitForStart`, supplying a key/domain learned through
an independently trusted channel, never the envelope's adjacent key. Join
verification also pins the candidate; compare returned group/genesis against
independent expectations where required. Returned summaries are copied metadata,
not a native seal. Encoding/assembly never grants membership, starts a voter or
creates a group. Native validation does not sort lists or choose a deployment
profile. `CoordinationProvisioningError.Code` is stable; `Detail` is diagnostic.

## Local and shared Topology (ABI 4.16)

`node.Topology()` reads passive local peer metrics with `HasMonitoring`, `Peers`
and `Peer`. Start with `Config.EnableMonitoring = true` to populate them. RTT and
jitter remain nil until sampled; an unknown peer returns nil. Local `RemoteAddr`
is intentionally omitted by baseline RPC.

Call `EnableShared(ctx, secret32, filter, bootstrap, TopologySwarmOptions{...})`
to join the namespace. A nil admission filter admits every secret holder.
Optional uint32 millisecond/coverage values preserve native defaults when nil
and explicit zero when supplied (position refresh zero selects 60s). Canceling
the wait does not roll back joining; the node owns installed publishing work.

`Snapshot(ctx)` returns an owned `TopologyObservation`; defer `Close`. Its
`Clusters`, `ClusterOf` and `Separated` always use the same immutable native view.
Unknown is a real verdict, never evidence of distance. Captured views survive
later observations and node shutdown while retaining their runtime. The live
`Clusters(ctx)`, `MyCluster(ctx)` and `Separated(ctx, a, b)` conveniences each
capture separately and may observe different eventual states. Use one snapshot
for a placement decision; these observations are local evidence, not policy proof.

Baseline `aster.net.Topology` RPC is explicitly opt-in through
`ServerOptions{Topology:true, TopologyRequirement:RequireRole("observer")}` and
requires monitoring. Requirements are enforced natively before handlers; local
reads alone do not expose network layout over RPC.


## Raw WebTransport (ABI 4.17)

`Runtime.StartRawWT(ctx, upcalls, RawWTConfig{...})` binds a separate TCP/QUIC
listener at an exact non-RPC path; `StartPublicRawWT` explicitly opts out of
authentication. Supply PEM material from `GenerateWebTransportCertificate` or
external provisioning. Register `ServeRPCAuthenticator` before authenticated
start. Its metadata includes `aster.webtransport.path` and the original query;
peer addresses are diagnostic hints, not Iroh-authenticated identities.

`listener.Accept(ctx)` returns an owned `RawWTSession`. Use `AcceptBi`,
`AcceptUni`, `OpenBi` and `OpenUni` for raw byte streams, and `ReceiveDatagram` /
`SendDatagram` for bounded unreliable messages. No Aster RPC framing is added.
`MaxDatagramSize` distinguishes unavailable datagrams from a zero size.
Applications implementing first-stream proof of possession must withhold media
until their own proof check succeeds.

Stream `Read(ctx, maxBytes)` is bounded and returns `io.EOF` at FIN. `Write`
writes all supplied bytes, but cancellation may leave a transmitted prefix;
do not automatically retry. `Finish` queues FIN and retains ownership.
`TransportAcked` separately waits for transport receipt; false means reset or
connection loss. `Reset` remains available after FIN. `SetPriority` / `Priority`
use signed Quinn priorities (larger values first), not HTTP urgency values.
Context cancellation immediately requests native cancellation and collects the
terminal completion; a success already committed can win the race.

`Info`, `Stats` and `Closed(ctx)` expose versioned native snapshots.
`CloseConnection(code, reason)` actively closes the underlying QUIC connection,
including any other sessions on it. `Close` also frees the session owner.
Close every stream and session owner, then the listener and borrowed upcall
table, before closing the runtime. Listener `Stop(grace)` joins protocol cleanup;
grace is not a deadline on the socket/driver join. This separate raw listener
does not compose arbitrary foreign HTTP routes or the `/aster/wt` RPC router.
