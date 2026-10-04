# C ABI rename map (ABI 2.x -> 3.0) and 3.x / 4.x changelog (current: 4.3)

ABI 3.0.0 renames the entire C ABI of `aster_transport_ffi` to the single
`aster_` / `ASTER_` prefix. No backward compatibility: a binding must switch
every symbol, pass `ASTER_ABI_VERSION_MAJOR` (3) to `aster_runtime_new`, and
should check `aster_abi_version() >> 16 == 3` at load.

**ABI 4.0** (see "4.0 (removals)" below) removed the reactor, the framed
`aster_call_*` client, the 3.2-deprecated node factories and bare endpoints.
The 3.x sections and the rename table still name those symbols as a record of
3.x; they no longer exist.

Header: `ffi/aster_ffi.h` (was `ffi/iroh_ffi.h`). Library file name unchanged
(`libaster_transport_ffi.{so,dylib}`, `aster_transport_ffi.dll`).

## Signature / semantic changes (beyond the rename)

| Old | New | Change |
|---|---|---|
| `int32_t iroh_runtime_new(const iroh_runtime_config_t*, iroh_runtime_t*)` | `int32_t aster_runtime_new(uint32_t expected_abi_major, const aster_runtime_config_t*, aster_runtime_t*)` | New first argument. Mismatch returns `ASTER_STATUS_ABI_MISMATCH` (12) and sets the last-error message; nothing is created. A non-NULL config must have `struct_size == sizeof(aster_runtime_config_t)` (16), else `ASTER_STATUS_INVALID_ARGUMENT`. |
| `uint32_t iroh_abi_version_major/minor/patch(void)` | `uint32_t aster_abi_version(void)` | One packed value `(major << 16) \| (minor << 8) \| patch`. Macro `ASTER_ABI_VERSION` is the header's packed value. |
| `const char* iroh_status_name(iroh_status_t)` | `const char* aster_status_name(int32_t)` | Raw status int (no UB on unknown values); unknown -> `"ASTER_STATUS_UNKNOWN"`. |
| `int32_t iroh_hook_before_connect_respond(rt, inv, iroh_hook_decision_t)` | `int32_t aster_hook_before_connect_respond(rt, inv, uint32_t decision)` | Same width; values other than ALLOW/DENY -> `ASTER_STATUS_INVALID_ARGUMENT`. |
| `iroh_endpoint_config_t.relay_mode`: unknown value silently = default | `aster_relay_mode_t`; unknown -> `ASTER_STATUS_INVALID_ARGUMENT` | `ASTER_RELAY_MODE_STAGING` = 3 now named. |
| `iroh_doc_share` / `iroh_doc_share_with_addr` `mode`: 0 = read, anything else = write | `aster_doc_share_mode_t`: 0 read, 1 write, other -> `ASTER_STATUS_INVALID_ARGUMENT` | |
| `ASTER_EVENT_KIND_CALL/CONNECTION_CLOSED/RESPONSE_WRITABLE/RESPONSE_CLOSED` (reactor `event_kind`, uint8) | `ASTER_REACTOR_EVENT_CALL/CONNECTION_CLOSED/RESPONSE_WRITABLE/RESPONSE_CLOSED` | Values unchanged (0..3). Renamed so they cannot be confused with the completion-queue `ASTER_EVENT_*` values of `aster_event_kind_t`. |

## New in 4.3 (additive)

ABI 4.3.0 (tranche F5, last part) adds the **streaming consumer relay**:
relay HTTP to a peer's exposed service with a streamed request body and a
streamed response, full duplex. Nothing in 4.2 changed shape; a 4.2 binding
keeps working. Sources: `ffi/src/expose.rs` (core:
`aster::binding_support::relay`). Tests: `ffi/tests/expose.rs` (`relay_stream_*`:
round trip, stable failure codes, full-duplex lifetime contract, timeouts and
runtime-thread rule, a readiness-driven poll loop, argument validation) and
`ffi/tests/abi_contract_test.c` (`test_relay_stream`, a C-only round trip).
Design: `docs/_internal/bindings/upcall-design.md` §6.4.

| Function | Notes |
|---|---|
| `aster_http_relay_options_init(*out)` | `aster_http_relay_options_t` (24 bytes): `max_chunk` (0 = 1 MiB), `upload_capacity` (0 = 8). |
| `aster_expose_relay_stream(rt, conn, c4, opts*, *upload, *response)` | `C4` with body mode `Stream`; returns `aster_http_upload_t` + `aster_http_response_t` at once, the exchange already running. `aster_status_t`. |
| `aster_http_upload_write(rt, up, buf, len, timeout_ms)` / `_try_write(.., *queued)` / `_finish(rt, up)` / `_abort(rt, up, msg, len)` | `aster_http_body_status_t`, as the 4.2 body calls. A write takes at most `max_chunk`; a timed-out write queued nothing; `finish` / `abort` wake a parked write (`FINISHED`). |
| `aster_http_response_wait(rt, resp, timeout_ms, buf, cap, *len)` | The head as UCv1 `C5` (body mode `Stream`), sticky. `BUFFER_TOO_SMALL` (17) sets `*len`. |
| `aster_http_response_read(rt, resp, buf, cap, timeout_ms, *len, *eof)` | Waits for the head first if needed. `timeout_ms` 0 never blocks (any thread; `TIMEOUT` = nothing ready). |
| `aster_http_upload_ready` / `aster_http_response_ready(rt, h, user_data, *op)` | One-shot `ASTER_EVENT_HTTP_RELAY_READY` (109): `error_code` = the next non-blocking call's result, `status` = its `aster_status_t`. |
| `aster_http_response_close(rt, resp)` / `aster_http_upload_free` / `aster_http_response_free` | `close`/`free` the response before its end abandons the exchange (an unfinished upload is reset); freeing an unfinished upload aborts it. |
| `aster_http_body_status_to_status(code)` | Body/relay code → `aster_status_t` (`DENIED` → `PERMISSION_DENIED`, `UNKNOWN_SERVICE` → `NOT_FOUND`, `RESET`/`READ` → `STREAM_RESET`, `CONNECTION_CLOSED`/`FINISHED` → `ALREADY_CLOSED`, `OVERLOADED` → `CONNECTION_REFUSED`, ...). |

**New values:** `aster_http_body_status_t` 10 `UNKNOWN_SERVICE`, 11 `DENIED`,
12 `OVERLOADED`, 13 `RESET`, 14 `BAD_HEAD`, 15 `CONNECTION_CLOSED`, 16
`UPLOAD_FAILED`, 17 `BUFFER_TOO_SMALL` (relay calls only); `aster_status_t` 14
`UNAUTHENTICATED`, 15 `PERMISSION_DENIED`, 16 `CREDENTIAL_EXPIRED` (reserved
for the R4 auth model; only a denied relay reports `PERMISSION_DENIED` today);
`aster_event_kind_t` 109 `ASTER_EVENT_HTTP_RELAY_READY`.

**Semantic changes (changelog):** an `ASTER_EVENT_ERROR` of
`aster_expose_relay_request` now has `status` from the mapping above and the
relay code in `error_code` (was `INTERNAL` / `-1`). On the wire, a serving
node now resets a refused http-relay stream with `0x41530004` (unknown
service), `0x41530005` (denied) or `0x41530006` (stream cap) instead of
dropping it silently; consumers of any version still see a failure.

## New in 4.2 (additive)

ABI 4.2.0 (tranche F5, expose part) puts the R9 expose surface on the C ABI:
publish a local service to Aster peers (a loopback socket, or an HTTP handler
the binding answers by upcall) and relay HTTP to a peer's exposed service.
Upcall kinds 48 / 49 / 50 are now issued by the C ABI. Nothing in 4.1 changed
shape; a 4.1 binding keeps working. Sources: `ffi/src/expose.rs`; tests:
`ffi/tests/expose.rs` (a polling "foreign" thread decodes `C0` / `C4` / `C7`
and encodes `C5` / `C6` / `C8` / `CF` by hand, checked against
`conformance/vectors/upcalls.json`; real QUIC between two nodes: inline and
streamed request/response with backpressure, authorize deny and failure mode,
route policy grant / deny / malformed / fail-open, 500 / 501 / 502 / 504,
`ChunkTooLarge` / `TooLarge` / 413 / 502 limits, consumer cancel before and
after the head, socket targets, argument validation, `BlockingOnRuntime`) and
`ffi/tests/abi_contract_test.c`. Design: `docs/_internal/bindings/upcall-design.md`
§4 and §6.

**Expose handle** (`aster_expose_t`; needs the `expose` feature of `aster`,
now enabled by this crate):

| Function | Notes |
|---|---|
| `aster_expose_new(rt, node, *out)` / `aster_expose_free(rt, expose)` | A registry plus its reactor. The node is only validated; the registry binds to connections you feed it. Free drops the handle; connections already fed keep their services (`unexpose` first to stop them). |
| `aster_expose_unexpose(rt, expose, service_id)` | Idempotent. |
| `aster_expose_serve(rt, node, expose, alpns, ud, *op)` | Accept loop (`Node::serve_expose`): feeds connections whose ALPN is in `alpns` (empty = all) until the node closes, then completes with `ASTER_EVENT_ERROR`. Do not combine with `aster_accept` / `aster_node_accept_aster` on the node. |
| `aster_expose_handle_connection(rt, expose, conn)` | The composable primitive for a binding's own accept loop (`aster_accept`). The connection handle stays the binding's. |
| `aster_expose_socket(rt, expose, service_id, "ip:port", authorizer_upcalls)` | L4 splice to a loopback server. `authorizer_upcalls` != 0: kind 48 (`C0` -> `CF`) per `(connection, service)`, failure mode applies. 0 admits **every** peer on the node's ALPN (as `aster_lease_serializer_memory_new(.., 0, ..)` authorizes everyone). |
| `aster_expose_http_foreign(rt, expose, upcalls, bodies, service_id, opts*)` | Kind 48 admission + kind 49 per request (`C4` -> `C5`), bodies by `stream_id`. `opts` NULL = defaults. Failures become `500` (`ERROR`), `501` (`UNIMPLEMENTED`), `502` (bad `C5`, inline body over the cap), `503`, `504` (timeout of the kind-49 policy; head only). |
| `aster_expose_route_policy_foreign(rt, expose, upcalls)` | `serve_routes` with kind 50 (`C7` -> `C8`); granted routes land in an internal `EdgeRouter` (the C ABI has no public edge listener). |

**Body table: separate `aster_http_bodies_t`, not folded into
`aster_upcalls_t`.** `HttpBodies` is its own Rust object (one per runtime,
shared by every foreign service, independent of any upcall table); handler
threads call it while the poll thread owns the queue; and keeping it out
leaves the whole 4.1 upcall surface untouched. `aster_http_bodies_new(rt, *out)`,
`aster_http_bodies_free`, `aster_http_bodies_open_count(rt, bodies, *u64)`
(leak checks).

`aster_http_foreign_options_t` (48 B): `struct_size` u32 @0 (must equal
`sizeof`), `flags` u32 @4 (0), `inline_request_max` u64 @8, `max_request_body`
u64 @16, `max_response_body` u64 @24, `max_chunk` u64 @32, `response_buffer`
u32 @40, `reserved` u32 @44 (0). Every field 0 = the Rust default (64 KiB / no
cap / no cap / 1 MiB / 8), so "never inline" is not expressible (use 1 to
inline only empty bodies); `aster_http_foreign_options_init(*out)` fills it.

**Body I/O** (`aster_http_body_*`, each takes `rt` first like every other C
call, which the sketch omitted). Returns `aster_http_body_status_t`: `0` OK,
`1..9` = `BodyError::code()` (stable): `UNKNOWN_STREAM` 1, `CANCELLED` 2,
`TOO_LARGE` 3, `CHUNK_TOO_LARGE` 4, `FINISHED` 5, `TIMEOUT` 6, `READ` 7,
`INVALID_TRAILERS` 8, `BLOCKING_ON_RUNTIME` 9. **These do not use
`aster_status_t`** (its 1 and 2 would collide with body codes 1 and 2), so
argument errors are `INVALID_ARGUMENT` -1 (NULL pointer with a length, zero
`cap`) and `UNKNOWN_HANDLE` -2 (unknown runtime or bodies table), detail in
`aster_last_error_message`.

| Function | Notes |
|---|---|
| `aster_http_body_read(rt, bodies, stream_id, buf, cap, timeout_ms, *out_len, *out_eof)` | `read_blocking`; at most `min(cap, max_chunk)` bytes. `*out_eof` = 1 (and `*out_len` 0) ends the body; short reads are normal. `timeout_ms` 0 polls once. A timeout consumes nothing. |
| `aster_http_body_write(rt, bodies, stream_id, ptr, len, timeout_ms)` | `write_blocking`; waits while the buffer is full. A timeout queues nothing. |
| `aster_http_body_try_write(rt, bodies, stream_id, ptr, len, *out_queued)` | `try_write`; `*out_queued` 0 = full, nothing queued. |
| `aster_http_body_finish(rt, bodies, stream_id, trailers, len)` | `trailers` = optional UCv1 `C6` (NULL / 0 = none); a malformed one is `INVALID_TRAILERS` and leaves the body open. |
| `aster_http_body_abort(rt, bodies, stream_id, msg, len)` | Logged, not sent; the relay resets the stream. |

`read` and `write` block and must not run on a tokio runtime thread (a
binding's own handler / poll threads are fine): the Rust `BlockingOnRuntime`
guard returns `BLOCKING_ON_RUNTIME` (9). `try_write`, `finish` and `abort`
never block.

**Consumer side:**

| Function | Notes |
|---|---|
| `aster_expose_relay_request(rt, conn, c4, ud, *op)` | Buffered `relay_request`. The request is a UCv1 `C4` with an `Inline` body (`service_id` names the peer's service; `peer_id` and `stream_id` are ignored); completes with `ASTER_EVENT_BYTES_RESULT`, payload = `C5` (inline body, release with `aster_buffer_release`), for any HTTP status. Transport failure, refusal (authorize deny) or a response that failed mid-body = `ASTER_EVENT_ERROR`. |
| `aster_expose_request_route(rt, conn, aster_route_spec_t *specs, n, metadata, ud, *op)` / `aster_route_registration_free(rt, reg)` | Needed to drive a remote edge's route policy: `BYTES_RESULT`, payload = `C8` (indexes of the granted specs), `handle` = `aster_route_registration_t`, which keeps the routes live. `aster_route_spec_t` (40 B): `host` @0, `service_id` @16, `port` u32 @32, `protocol` u32 @36 (`aster_route_protocol_t`). |

**Choices and gaps (smallest reasonable reading of the sketch):**

- Streaming `relay_request_streaming` is **not** in the C ABI: it needs a
  sender handle for the upload and a frame-reader handle for the response
  (two new handle kinds, frame/trailer events); the buffered call covers the
  need and this fits no existing stream-handle pattern cleanly. Follow-up.
- The raw L4 consumer of an `aster_expose_socket` service (`open_http_relay`
  speaking a protocol raw) is not in the C ABI either; a socket target is
  reachable from other Aster consumers. The tests prove the splice by the
  target's accept.
- The request-side `C4` for `aster_expose_relay_request` is reused instead of
  a new struct, so a binding needs no extra codec; `version` selects the
  `http` version, HEAD / trailers behave as in design §6.
- `Cancelled` from a body call is what a call parked in `write`
  (buffer full) gets when the consumer goes away (the mid-stream test floods
  the buffer to hit that); a call made after the
  relay has already torn the exchange down finds no handle and returns
  `UNKNOWN_STREAM`. Both mean "stop"; the design text promises `Cancelled`
  for "the next write", which the Rust implementation does not always give.
- An `abort` (and a `max_response_body` crossing) reaches a remote consumer
  as a clean truncated finish until the aster-expose abort fix lands
  (design §6: the consumer sees a failed response). The two tests that
  pin the documented behaviour are `#[ignore = "needs expose-abort-fix"]` in
  `ffi/tests/expose.rs`.
- `aster_expose_free` / `aster_http_bodies_free` do not tear down services or
  open exchanges that already hold their own references.

**New enums / constants:** `aster_http_body_status_t`, `aster_route_protocol_t`;
`ASTER_ABI_VERSION_MINOR` = 2. The `aster_upcall_kind_t` comments for 48-50 no
longer say "not issued by the C ABI".

**Bindings:** nothing wired in Java, Go or .NET (they build against the new
header; .NET `RequiredAbiMinor` stays 0, Go compares the library with the
header's `ASTER_ABI_VERSION`). Java / .NET / Go decode `C4` and encode `C5`
against `upcalls.json`, as for the 4.1 kinds.

## New in 4.1 (additive)

ABI 4.1.0 (tranche F5, upcall part) carries Rust's questions to foreign code:
the R6 upcall mechanism (`aster::binding_support::upcall`, design
`docs/_internal/bindings/upcall-design.md` §4) behind a poll/respond pair,
plus the adapters that turn it into lease, fenced-resource and topology
extension points. Nothing in 4.0 changed shape; a 4.0 binding keeps working.
Sources: `ffi/src/upcall.rs`; tests: `ffi/tests/upcalls.rs` (a polling
"foreign" thread answers through the C functions: foreign serializer with
background renewals, release/revoke/fenced watch, foreign authorizer and its
fail-open/fail-closed fallback, timeout cancel + late respond, ERROR /
UNIMPLEMENTED statuses, fenced mutate incl. outcome-unknown, topology
admission filter batching/caching, `enable_shared`, close → shutdown cancels,
queue capacity, argument validation), `ffi/tests/abi_contract_test.c`.

**Upcall table.** One `aster_upcalls_t` per binding runtime wraps an
`Upcalls` table and a bounded `UpcallQueue`. Payloads are UCv1
(`conformance/vectors/upcalls.json`); the binding decodes requests and
encodes responses itself. No function pointers.

| Function | Notes |
|---|---|
| `aster_upcalls_new(rt, capacity, *out)` | `capacity` 0 = `ASTER_UPCALL_DEFAULT_CAPACITY` (1024) queued upcalls; beyond it upcalls are refused (`Overloaded`: decisions take the failure mode, other kinds error). `CANCEL` is always queued. |
| `aster_upcall_poll(rt, u, aster_upcall_event_t *out, max, timeout_ms) -> uint32_t` | Batched drain; `timeout_ms` 0 = non-blocking. Single consumer; blocks the caller. |
| `aster_upcall_respond(rt, u, invocation_id, status, payload, len)` | `aster_upcall_status_t` (`OK` = UCv1 response, `ERROR` = UTF-8 message, `UNIMPLEMENTED`). Any thread, any order, never blocks. `NOT_FOUND` = not pending (answered, cancelled, timed out) -- ignore. |
| `aster_upcall_set_policy(rt, u, kind, timeout_ms, failure_mode)` / `aster_upcall_set_default_policy(rt, u, timeout_ms, failure_mode)` | `failure_mode` = `aster_hook_failure_mode_t`; applies to decision kinds. Default 5 s, fail closed. |
| `aster_upcall_buffer_release(rt, u, buffer)` | Frees an event's payload (0 = no-op). |
| `aster_upcalls_close(rt, u)` / `aster_upcalls_free(rt, u)` | Pending upcalls fail, each gets `CANCEL` (`SHUTDOWN`); queued events stay drainable until free. |

`aster_upcall_event_t` (40 B): `invocation_id` u64 @0, `kind` u16 @8
(`aster_upcall_kind_t`), `cancel_reason` u8 @10 (`aster_upcall_cancel_reason_t`,
`CANCEL` only), `_p0` @11, `payload_len` u32 @12, `deadline_unix_ms` u64 @16,
`payload_ptr` @24, `buffer` u64 @32. Differs from the design sketch in one
place: a `CANCEL` event carries its reason in `cancel_reason` and no payload
(the Rust event's one reason byte), so it needs no buffer release.

**Leases** (`aster_lease_*`):

| Function | Upcalls | Completion |
|---|---|---|
| `aster_lease_serializer_foreign_new(rt, u, *out)` | 1 `LEASE_TRANSITION`, 2 `LEASE_SNAPSHOT` | -- |
| `aster_lease_serializer_memory_new(rt, authorizer_u, *out)` | 3 `LEASE_AUTHORIZE` when `authorizer_u` != 0 (async `AuthorizedSerializer`, no blocking authorizer) | -- |
| `aster_lease_acquire(rt, ser, resource, holder, ttl_ms, renew_interval_ms, holder_margin_ms, ud, *op)` | via the serializer | `ASTER_EVENT_LEASE_ACQUIRED` (107): payload = UCv1 `TransitionOutcome` (`B2`); `handle` = `aster_lease_t` when granted, 0 when denied |
| `aster_lease_fence(rt, lease, *epoch)` | -- | sync; `ALREADY_CLOSED` once the holder must stop acting |
| `aster_lease_status(rt, lease, *status)` / `aster_lease_watch(rt, lease, ud, *op)` | -- | `aster_lease_status_t`; watch completes with `ASTER_EVENT_LEASE_STATUS` (108), `flags` = status, when the lease leaves `HELD` (the "fence-lost event") |
| `aster_lease_release(rt, lease, ud, *op)` / `aster_lease_free(rt, lease)` | RELEASE / none | `UNIT_RESULT`; free stops renewing without releasing |
| `aster_lease_snapshot(rt, ser, resource, ud, *op)` / `aster_lease_revoke(rt, ser, resource, authority, ud, *op)` | via the serializer | `BYTES_RESULT`: `B4` / `B2` |
| `aster_lease_serializer_free(rt, ser)` | -- | leases keep their own reference |

Timing: `ttl_ms` 0 = defaults (30 s / 10 s / 1 s); with a ttl, `renew` 0 =
ttl/3 and `margin` 0 = ttl/10 capped at 1 s; invalid combinations are
`INVALID_ARGUMENT` up front.

**Fenced resource.** `aster_fenced_resource_foreign_new(rt, u, *out)` (kind 4)
and `aster_fenced_mutate(rt, res, fence_resource, epoch, fence_holder, writer,
op, ud, *op)` → `BYTES_RESULT` with UCv1 `FencedMutateResponse` (`B7`: applied
output or fence rejection); any other failure is `ASTER_EVENT_ERROR` and means
**outcome unknown**. `aster_fenced_resource_free`.

**Topology.** `aster_topology_admitted_new(rt, u, ttl_ms, failure_retry_ms,
max_batch, *out)` (kind 16, cached + batched, never blocks a read);
`_check`, `_set`, `_invalidate` (empty id = clear), `_prefetch` (`UNIT_RESULT`),
`_stats` (`aster_topology_admitted_stats_t`, 32 B), `_free`.
`aster_topology_enable_shared(rt, node, secret[32], admitted (0 = none),
bootstrap, position_refresh_ms, ud, *op)` joins the topology v2 swarm with the
filter installed (`UNIT_RESULT`; needs `enable_monitoring`).

**Not exposed (gaps):** placement providers (kinds 17/32/33 -- the C ABI has no
coordination client), expose authorize / HTTP handler / route policy
(48/49/50 -- wired in 4.2, see "New in 4.2"), Gate 0 over upcalls (64/65 -- the 3.2 hook iterators keep serving
admission), and a read side for the shared topology view (clusters /
serialized snapshot -- needs `SharedView` parts from core). No typed C decode
helpers for UCv1: bindings decode against the vectors (Java does,
`site.aster.upcall.Ucv1`).

**New enums / constants:** `aster_upcall_kind_t`, `aster_upcall_status_t`,
`aster_upcall_cancel_reason_t`, `aster_lease_status_t`,
`ASTER_UPCALL_DEFAULT_CAPACITY`, `ASTER_EVENT_LEASE_ACQUIRED` (107),
`ASTER_EVENT_LEASE_STATUS` (108).

**Bindings:** Java wraps all of it (`site.aster.upcall`, `site.aster.lease`;
`IrohLibrary` unchanged at major 4). Go and .NET only build against the new
header (no wrappers yet; .NET `RequiredAbiMinor` stays 0, Go compares the
library with `ASTER_ABI_VERSION` from the header).

## 4.0 (removals)

ABI 4.0.0 removes what 3.2 and 3.5 deprecated, now that Java, .NET and Go are
on `aster_node_start` and the 3.5 dispatch server / payload client. Nothing
was added or reshaped: a 3.5 binding that used none of the functions below
only has to pass `ASTER_ABI_VERSION_MAJOR` (4) to `aster_runtime_new` and
check `aster_abi_version() >> 16 == 4`. Every remaining struct layout is
unchanged.

**Removed:**

| Kind | Name | Use instead |
|---|---|---|
| functions | `aster_reactor_{create,destroy,poll,submit,submit_frame,submit_trailer,recv_frame,check_cancelled,buffer_release,reserve_response_capacity,cancel_response_reservation,accept_non_rpc}` | `aster_server_*` / `aster_dispatch_*` (3.5); non-RPC ALPNs arrive as `ASTER_DISPATCH_EVENT_NON_RPC` |
| types / constants | `aster_reactor_t`, `aster_reactor_call_t` (112 B), `ASTER_REACTOR_EVENT_{CALL,CONNECTION_CLOSED,RESPONSE_WRITABLE,RESPONSE_CLOSED}`, `ASTER_RECV_FRAME_{OK,END_OF_STREAM,TIMEOUT}` | `aster_dispatch_event_t`, `aster_dispatch_event_kind_t` |
| functions | `aster_call_acquire`, `aster_call_acquire_streaming`, `aster_call_send_frame`, `aster_call_recv_frame`, `aster_call_unary` | `aster_rpc_unary`, `aster_rpc_open` + `aster_call_{send_payload,send_payloads,recv_payload,cancel}` (3.5) |
| constants | `ASTER_CALL_RECV_{OK,END_OF_STREAM,TIMEOUT}`, `ASTER_CALL_ERR_{POOL_FULL,QUIC_LIMIT_REACHED,PEER_STREAM_LIMIT_TOO_LOW,STREAM_OPEN_FAILED,POOL_CLOSED}` | payload-client statuses / AEv1 trailer codes |
| functions | `aster_node_memory`, `aster_node_memory_with_alpns`, `aster_node_persistent` | `aster_node_start` with an `aster_config_t` (`alpns`, `storage_path`) |
| functions | `aster_endpoint_create`, `aster_endpoint_close`, `aster_endpoint_free`, `aster_endpoint_id`, `aster_endpoint_export_secret_key`, `aster_endpoint_addr_info` | a node: `aster_node_start`, `aster_node_close`, `aster_node_free`, `aster_node_id`, `aster_node_export_secret_key`, `aster_node_addr_info` |
| struct | `aster_endpoint_config_t` (152 B) | `aster_config_t` (relay mode/URLs, secret key, ALPNs, hooks, bind address, transports, portmapper, proxy, storage) |

There are no bare endpoints any more: a node started with only custom ALPNs is
the replacement.

**Kept:** `aster_call_release` / `aster_call_discard` (free an `aster_rpc_open`
call; an unfinished one is cancelled -- the two are now identical) and
`aster_call_buffer_release` (leases from `aster_rpc_unary` /
`aster_call_recv_payload`). `aster_endpoint_remote_info`,
`aster_endpoint_remote_info_list`, `aster_endpoint_transport_metrics`,
`aster_add_node_addr` and `aster_connect` keep their names and take node handles
(`aster_endpoint_t` stays as a typedef for them). `ASTER_EVENT_ENDPOINT_CREATED`
(3) / `ASTER_EVENT_ENDPOINT_CREATE_FAILED` (4) stay in `aster_event_kind_t` as
reserved values and are never emitted.

**Behaviour change:** `aster_accept(rt, node, user_data, out_op)` on a node now
takes the next connection from the node's aster-ALPN accept queue (the queue
`aster_node_accept_aster` reads; connections on the node's configured ALPNs
that passed Gate 0) and emits `ASTER_EVENT_CONNECTION_ACCEPTED` (`handle` =
connection, `related` = node, no payload). Before 4.0 it called the endpoint's
accept directly and raced the node's router. Unknown handle -> `NOT_FOUND`.

**Bindings:** Java (`IrohLibrary.EXPECTED_ABI_MAJOR`), .NET
(`Native.AbiVersionMajor`) and Go (`C.ASTER_ABI_VERSION_MAJOR` from the header)
expect 4. Java and Go now wait in `aster_dispatch_recv` /
`aster_call_recv_payload` with finite timeouts and check their own cancellation
between slices instead of `ASTER_TIMEOUT_INFINITE`.

Sources: `ffi/src/call.rs` (what is left of the call family), `ffi/src/lib.rs`
(`aster_accept`); `ffi/src/reactor.rs` and its tests (`reactor_test.rs`,
`reactor_cancel_lifecycle.rs`, `cancel_state_cleanup.rs`) were deleted. Tests:
`ffi/tests/node_start.rs` (`accept_on_node_reads_the_alpn_queue`),
`ffi/tests/abi_contract_test.c`.

## New in 3.5 (additive)

ABI 3.5.0 (tranche F4, additive part) serves bindings' RPC services through
`aster::rpc::Server` (design `docs/_internal/bindings/dynamic-dispatch-design.md`
§4.4, as built) and adds a payload-level client over the pooled
`RpcConnection`. Nothing in 3.0–3.4 changed shape; a 3.4 binding keeps working
(at the time Go and .NET still used `aster_reactor_*` and the framed
`aster_call_*`; both were removed in 4.0). Sources: `ffi/src/server.rs`, `ffi/src/rpc.rs`; tests:
`ffi/tests/dispatch_server.rs` (all four patterns, sessions, peer cancel,
deadlines, `QUEUE_FULL`/reserve, abandon, Gate 3 + attribute bridge, frame
limits, connection close, non-RPC ALPN, argument validation -- every call
through the C functions), unit tests in `server.rs`,
`ffi/tests/abi_contract_test.c`.

**Server.** A binding registers each service as its JSON registration form
(`DynamicService`, design §1.5); Rust decodes headers, routes, runs Gate 3
(`requires`, against an attribute store the binding feeds), keeps sessions per
connection (§7.5 graveyard and cap), enforces deadlines (`min(header, method
default_timeout, handler cap)`) and per-method frame limits (closing the
"no frame-limit resolver" gap of `aster_reactor_create`), serves the baseline
`aster.ops.NodeInfo` (and optionally `aster.net.Topology`), and delivers each
call that passed as one `CALL` event. Calls are answered by `call_id` with
payloads and AEv1 `RpcStatus` bytes -- Rust frames, compresses and encodes.
Response operations never block: `QUEUE_FULL` returns the exact
`required_len`; `aster_dispatch_reserve` it and retry on
`RESPONSE_WRITABLE` (the retry cannot be full again). `call_id`s are allocated
by the server's call table (from 1); a detached call stays there until a
terminal operation, abandon, peer cancel or its deadline.

**k/v list** (metadata, attributes, `aster_call_options_t.metadata`): `u32 LE
count`, then per entry `u32 LE key_len`, key, `u32 LE value_len`, value
(UTF-8); empty = no entries.

| Kind | Name | Notes |
|---|---|---|
| type | `aster_server_builder_t`, `aster_server_t` | `uint64_t` handles |
| enum | `aster_dispatch_event_kind_t` | `CALL`=0, `CANCELLED`=1, `RESPONSE_WRITABLE`=2, `RESPONSE_CLOSED`=3, `SESSION_CLOSED`=4, `CONNECTION_CLOSED`=5, `NON_RPC`=6 |
| enum | `aster_cancel_reason_t` | `PEER`=0 (no trailer), `DEADLINE`=1 (Rust sent `DEADLINE_EXCEEDED`), `INVALID_REQUEST`=2 (Rust sent `INVALID_ARGUMENT`) |
| enum | `aster_method_pattern_t` | `UNARY`=0, `SERVER_STREAM`=1, `CLIENT_STREAM`=2, `BIDI_STREAM`=3 |
| enum | `aster_server_option_t` | `BUILTIN_NODE_INFO`=1 (0/1, default 1), `BUILTIN_TOPOLOGY`=2 (0/1, default 0), `MAX_SESSIONS_PER_CONNECTION`=3 (default 1024), `HANDLER_TIMEOUT_MS`=4 (default 300000; 0 = no cap) |
| macro | `ASTER_DISPATCH_FLAG_FIRST_IS_END` (1), `_NEW_SESSION` (2), `_IROH_AUTHENTICATED` (4), `_HAS_METADATA` (0x100), `_HAS_ATTRIBUTES` (0x200) | `aster_dispatch_event_t.flags` |
| macro | `ASTER_TIMEOUT_INFINITE` (`0xffffffff`) | `timeout_ms` of `aster_dispatch_recv` / `aster_call_recv_payload` |
| struct | `aster_dispatch_event_t` (104 B) | `kind:u8, pattern:u8, reason:u8, _p0, flags:u32, call_id, service_idx:u32, method_idx:u32, session_key, connection_id, connection_handle, deadline_unix_ms:i64, wire_call_id:i32, session_id:u32, request_ptr, request_len:u32, _p2, peer_ptr, peer_len:u32, _p3, buffer`. `request` + `peer` live in one lease, `buffer`. `CALL`: `connection_handle` is borrowed (same for every call of the connection; freed by the server on close). `NON_RPC`: `request` = the ALPN, `connection_handle` is **owned** (free with `aster_connection_free`) |
| struct | `aster_call_options_t` (72 B) | `struct_size, version:i32, service, method, metadata (k/v list), session_id:u32, deadline_ms:u32, serialization_mode:i32 (0 XLANG, 1 NATIVE, 2 ROW), wire_call_id:i32`. The deadline is enforced client-side and sent as whole seconds, rounded up |
| function | `aster_server_builder_new(rt, node, out)` | over a node from `aster_node_start` (`aster/1` among its ALPNs); Gate 0 stays the node's |
| function | `aster_server_add_service_json(builder, json, len, uint32_t* out_idx)` | parsed + validated (`DynamicService::from_json`; unknown keys refused); duplicate `(name, version)` → `INVALID_ARGUMENT`; `out_idx` = `service_idx` |
| function | `aster_server_set_option(builder, key, value)` | unknown key / bad value → `INVALID_ARGUMENT` |
| function | `aster_server_start(rt, builder, ring_capacity, out)` | consumes the builder; ring rounded up to a power of two (0 = 256); a collision with a baseline service → `INVALID_ARGUMENT` |
| function | `aster_server_builder_free(builder)` | for a builder never started |
| function | `aster_server_poll(rt, server, events, max, timeout_ms)` | returns the count; `timeout_ms` 0 = no wait; single consumer; parks on a condvar (no spinning) |
| function | `aster_dispatch_respond(rt, server, call_id, payload, len, status_aev1, status_len, uint64_t* out_required_len)` | terminal data + trailer in one write (unary / client-stream); NULL status = OK; `QUEUE_FULL` / `NOT_FOUND` (call over) / `ALREADY_CLOSED` (connection gone) / `INVALID_ARGUMENT` (malformed status, too large) |
| function | `aster_dispatch_send(…, call_id, payload, len, out_required_len)` | one data frame; payload non-empty |
| function | `aster_dispatch_finish(…, call_id, status_aev1, len, out_required_len)` | trailer; NULL = OK |
| function | `aster_dispatch_reserve(…, call_id, required_len)` / `aster_dispatch_cancel_reservation(…, call_id)` | one reservation per call (a second → `INVALID_ARGUMENT`) |
| function | `aster_dispatch_recv(…, call_id, timeout_ms, out_ptr, out_len, uint64_t* out_buffer)` | next request payload, decompressed: `OK` + buffer ≠ 0 = payload, `OK` + buffer 0 = end of stream, `TIMEOUT`, `NOT_FOUND` = call over. Blocks |
| function | `aster_dispatch_is_cancelled(…, call_id)` | 0 live; 1 cancelled, finished or unknown |
| function | `aster_dispatch_metadata` / `aster_dispatch_attributes(…, call_id, out_ptr, out_len, out_buffer)` | k/v lists (attributes = what Gate 3 saw, sorted) |
| function | `aster_dispatch_abandon(…, call_id)` | `INTERNAL "handler dropped call"` unless already ended (`NOT_FOUND`) |
| function | `aster_dispatch_buffer_release(rt, server, buffer)` | event / recv / metadata leases; 0 = no-op |
| function | `aster_server_set_peer_attributes(server, peer, len, kv, kv_len)` | attribute bridge (design §2.6): what Gate 3 sees for a peer; empty clears |
| function | `aster_server_live_calls(server)` | diagnostics |
| function | `aster_server_destroy(rt, server)` | stops serving; queued events' calls are abandoned; does not close the node |
| function | `aster_dynamic_service_contract_id(json, len, aster_bytes_t* out)` | `contract_id` of a JSON registration form (free with `aster_string_release`) |
| function | `aster_rpc_unary(rt, conn, const aster_call_options_t*, req, len, out_resp_ptr, out_resp_len, out_resp_buffer, int32_t* out_status_code, out_status_ptr, out_status_len, out_status_buffer)` | pooled; one write of header + request. Returns `OK` once the call is over: code 0 + response, or the failure's code + AEv1 status (deadline → `DEADLINE_EXCEEDED`, lost connection / pool timeout → `UNAVAILABLE`). Buffers released with `aster_call_buffer_release` |
| function | `aster_rpc_open(rt, conn, uint8_t pattern, opts, aster_call_t* out)` | server-, client- or bidi-stream; server-stream sends header + request together on the first send |
| function | `aster_call_send_payload(rt, call, payload, len, uint8_t end_stream)` | empty + end = finish; `ALREADY_CLOSED` when the call is over (the trailer says why) |
| function | `aster_call_send_payloads(rt, call, const aster_bytes_t* payloads, count, end_stream)` | client-stream / bidi: several payloads framed into as few writes as possible (64 KiB batches) |
| function | `aster_call_recv_payload(rt, call, timeout_ms, out_ptr, out_len, out_buffer, uint8_t* out_is_trailer)` | payload, or the trailer = AEv1 `RpcStatus` (code 0 on success; client-side failures included); `TIMEOUT` resumes the same read; after the trailer `ALREADY_CLOSED` |
| function | `aster_call_cancel(rt, call)` | `FLAG_CANCEL` + reset; a blocked recv returns `CANCELLED` |
| function | `aster_connection_next_session_id(rt, conn, uint32_t* out)` | from the connection's shared core counter (Rust and binding clients never collide) |

Behaviour change (no signature change): `aster_call_release` / `aster_call_discard`
also free `aster_rpc_open` calls (an unfinished one is cancelled).

**Deprecated in 3.5, removed in 4.0:**
`aster_reactor_{create,destroy,poll,submit,submit_frame,submit_trailer,recv_frame,check_cancelled,buffer_release,reserve_response_capacity,cancel_response_reservation,accept_non_rpc}`,
`aster_reactor_call_t`, and the framed `aster_call_{acquire,acquire_streaming,send_frame,recv_frame,unary}`.

## New in 3.4 (additive)

ABI 3.4.0 (rest of tranche F2) exports the pure functions: typed ids, HPKE,
ownership attestations and sealed grants. Nothing in 3.0–3.3 changed shape; a
3.3 binding keeps working. Sources: `ffi/src/pure.rs`, `ffi/src/grants.rs`;
tests: unit tests in `pure.rs`, `ffi/tests/grant_vectors.rs` (reproduces
`conformance/vectors/grants.json` through the C functions),
`ffi/tests/abi_contract_test.c`.

All functions are synchronous and need no runtime. Raw keys / ids are 32-byte
buffers (`const uint8_t*` in, `uint8_t*` out). Variable inputs are
`aster_bytes_t` by value (`{NULL,0}` = empty); variable outputs are
library-owned `aster_bytes_t` freed with `aster_string_release` (`{NULL,0}`
when empty or on failure). A namespace capability is `(access, key)`:
`ASTER_LOCAL_ACCESS_WRITE` + namespace secret, or `ASTER_LOCAL_ACCESS_READ` +
namespace id.

| Kind | Name | Notes |
|---|---|---|
| enum-value | `ASTER_STATUS_VERIFICATION_FAILED` = 13 | well-formed input failed a security check: envelope does not open, chain does not verify, role ≠ capability. Malformed input stays `INVALID_ARGUMENT` |
| enum | `aster_id_kind_t` | `NODE_ID`=0, `PUBLIC_KEY`=1, `SECRET_KEY`=2, `HASH`=3, `NAMESPACE_ID`=4, `NAMESPACE_SECRET`=5, `AUTHOR_ID`=6 |
| enum | `aster_attestation_role_t` | `NONE`=0, `NODE`=1, `ANCHOR`=2, `INTERMEDIATE`=3 |
| enum | `aster_grant_state_t` | `ACTIVE`=0, `REVOKED`=1 |
| enum | `aster_grant_outcome_kind_t` | `ACTIVE`=0, `REVOKED`=1, `UNOPENABLE`=2, `ABSENT`=3 |
| macro | `ASTER_KEY_SIZE` (32), `ASTER_ID_HEX_LEN` (64) | |
| struct | `aster_attest_options_t` (32 B) | `struct_size, reserved, epoch:i64, not_before:i64, not_after:i64` |
| struct | `aster_verified_chain_t` (96 B) | `struct_size, depth, node[32], anchor[32], intermediates (count × 32 bytes), intermediates_count, storage` |
| struct | `aster_grant_context_t` (104 B) | `struct_size, reserved, app, granter, recipient, resource, path, role` (granter / recipient = node id hex, bound verbatim) |
| struct | `aster_grant_record_t` (72 B) | `struct_size, state, context_version:i32, reserved, role, envelope, reason, storage` |
| struct | `aster_grant_outcome_t` (72 B) | `struct_size, kind, access, drops_capability, payload, reason, role, storage` |
| function | `aster_id_parse(kind, text, out[32])` | exactly 64 hex chars, either case; Ed25519 kinds (`NODE_ID`, `PUBLIC_KEY`, `AUTHOR_ID`) must be a curve point |
| function | `aster_id_format(kind, id[32], out[65])` | lowercase hex + NUL; does not validate |
| function | `aster_id_validate(kind, id[32])` | OK / `INVALID_ARGUMENT` |
| function | `aster_secret_key_public_key(secret[32], out[32])` | Ed25519 public key = node id |
| function | `aster_namespace_secret_id(secret[32], out[32])` | `aster::NamespaceSecret::id` |
| function | `aster_hpke_generate_keypair(out_private[32], out_public[32])`, `aster_hpke_public_key_from_private` | X25519 |
| function | `aster_hpke_seal(public[32], aad, plaintext, out)` / `aster_hpke_open(private[32], aad, envelope, out)` | envelope = Fory `aster.hpke.Envelope`; open failure → `VERIFICATION_FAILED` |
| function | `aster_hpke_x25519_secret_from_identity`, `aster_hpke_x25519_public_from_identity`, `aster_hpke_x25519_public_from_node_id(node_id hex, out)` | libsodium-compatible Ed25519→X25519; bad public key → `INVALID_ARGUMENT` |
| function | `aster_attest_root_node(root_secret, node_secret, options or NULL, out)` | chain wire bytes |
| function | `aster_attestation_chain_to_text` / `_from_text` | `aster.attestation.chain.v1:<base64url>` |
| function | `aster_attestation_verify_chain(chain, anchors (count × 32), count, expected_node, aster_verified_chain_t*)` | any failed check → `VERIFICATION_FAILED`; release with `aster_verified_chain_release` |
| function | `aster_verified_chain_role_of(chain, id, uint32_t* role)` | node wins over anchor if equal |
| function | `aster_grant_context_aad(ctx, out)` | domain `aster/sealed-grant/v1` + 6 fields, each u32-BE length-prefixed |
| function | `aster_grant_key(prefix, recipient, out)` | `<prefix minus trailing '/'>/grants/<recipient>` |
| function | `aster_grant_check_role(role, can_write)` | OK / `VERIFICATION_FAILED` |
| function | `aster_grant_seal(ctx, plaintext, out)`, `aster_grant_seal_to_key(public[32], ctx, plaintext, out)` | identity-derived or explicit X25519 key |
| function | `aster_grant_open(identity_secret, ctx, envelope, out)`, `aster_grant_open_with_key(x25519_private, ctx, envelope, out)` | any mismatch → `VERIFICATION_FAILED` |
| function | `aster_namespace_capability_encode(access, key, out)` / `_decode(bytes, uint32_t* access, key_out[32])` | Fory capability bytes |
| function | `aster_namespace_grant_seal(ctx, access, key, out)` / `aster_namespace_grant_open(identity_secret, ctx, envelope, uint32_t* access, key_out[32])` | role rule checked on both sides |
| function | `aster_grant_record_encode(const aster_grant_record_t*, out)` / `_decode(bytes, aster_grant_record_t*)` / `_release` | read `context_version` before choosing the context |
| function | `aster_grant_reconcile(identity_secret, ctx, const aster_bytes_t* record or NULL, aster_grant_outcome_t*)`, `aster_namespace_grant_reconcile(...)` | every outcome returns OK; NULL record = ABSENT; namespace ACTIVE = `access` + 32-byte key in `payload`; release with `aster_grant_outcome_release` (zeroes the payload) |

## New in 3.3 (additive)

ABI 3.3.0 (tranche F3) brings the data plane to parity with the Rust SDK's
`aster::Blobs`, `aster::Docs`/`Doc` and `aster::Connection`/`SendStream`.
Nothing in 3.0–3.2 changed shape; a 3.2 binding keeps working. Source:
`ffi/src/data.rs`; tests: `ffi/src/data.rs` (encoding, path lowering,
fingerprint), `ffi/tests/data_plane.rs` (two loopback nodes),
`ffi/tests/abi_contract_test.c`.

Async functions take `(…, user_data, aster_operation_t* out_operation)`, finish
on the completion queue, and return `ASTER_STATUS_INVALID_ARGUMENT` (nothing
started) for malformed ids, unknown enum values / flag bits, or a NULL
`out_operation`; unknown handles return `ASTER_STATUS_NOT_FOUND`. Ids are 64-hex
strings unless noted. Failures arrive as `ASTER_EVENT_ERROR` with the message
as payload.

| Kind | Name | Notes |
|---|---|---|
| enum | `aster_blob_format_t` | `RAW`=0, `HASH_SEQ`=1 |
| enum | `aster_fetch_strategy_t` | `WHOLE`=0, `SPLIT_CHILDREN`=1 (needs `HASH_SEQ`; provider order applies per child) |
| enum | `aster_blob_status_t` | `NOT_FOUND`=0, `PARTIAL`=1, `COMPLETE`=2 (same codes as `aster_blobs_status`) |
| enum | `aster_local_access_t` | `WRITE`=1, `READ`=2 (= the fingerprint's access byte) |
| enum | `aster_retire_outcome_t` | `RETIRED`=0, `BUSY`=1 |
| enum-value | `ASTER_PATH_KIND_OTHER` = 3 | custom/future transport path |
| enum-value | `ASTER_EVENT_BLOB_FETCHED`=100, `_DOC_AUTHORITY_REPORT`=101, `_DOC_RETIRE_OUTCOME`=102, `_DOC_OPENED`=103, `_DOC_ENTRY_EXISTS`=104, `_DOC_CONTENT_STATUS`=105, `_STREAM_PRIORITY`=106 | |
| macro | `ASTER_FETCH_TO_STORE` (1) | `flags` bit: keep the blob in the store, return no bytes |
| macro | `ASTER_FETCH_REPORT_VERSION` (1), `ASTER_FETCH_REPORT_HEADER_SIZE` (16), `ASTER_FETCH_PROVIDER_RECORD_SIZE` (44), `ASTER_AUTHORITY_FINGERPRINT_SIZE` (32) | |
| struct | `aster_path_info_t` (72 B) | `struct_size, path_kind, is_selected, peer_port:u16, _pad:u16, peer_host, relay_url, other, rtt_micros` (`UINT64_MAX` = unmeasured); strings live in the list's storage |
| struct | `aster_path_list_t` (32 B) | `struct_size, _pad, items, len, storage` |
| function | `aster_blobs_download_hash_from(rt, node, hash, aster_bytes_list_t providers, format, strategy, flags, …)` | `aster::Blobs::download_hash[_to_store]_from`; ordered failover; → `ASTER_EVENT_BLOB_FETCHED` (`flags` = provider count), payload = fetch report v1 + blob bytes (none with `TO_STORE`) |
| function | `aster_blobs_download_hash(rt, node, hash, provider, format, flags, …)` | single provider; → `ASTER_EVENT_BLOB_DOWNLOADED` with bytes (empty with `TO_STORE`) |
| function | `aster_blobs_read_range(rt, node, hash, offset, len, …)` | → `ASTER_EVENT_BLOB_READ` |
| function | `aster_tags_delete_prefix(rt, node, prefix, …)` | → `ASTER_EVENT_TAG_DELETED`, payload u64 count, `flags` = count (saturated); empty prefix = all tags |
| function | `aster_blobs_gc_run_once(rt, node, …)` | node-wide GC pass → `ASTER_EVENT_UNIT_RESULT` |
| function | `aster_docs_authority_report(rt, node, namespace_id, …)` | → `ASTER_EVENT_DOC_AUTHORITY_REPORT`: `flags` = access, payload = namespace (32) + fingerprint (32); absent namespace = `ASTER_EVENT_ERROR` |
| function | `aster_doc_authority_report(rt, node, doc, …)` | same, namespace taken from the open doc; `event.handle` = doc |
| function | `aster_doc_authority_fingerprint(namespace_id, access, uint8_t out[32])` | sync, pure (`aster::AuthorityFingerprint::of`) |
| function | `aster_docs_retire_local_write_authority(rt, node, namespace_id, …)` | → `ASTER_EVENT_DOC_RETIRE_OUTCOME`: `flags` = outcome, payload = u32 access, u32 0, u64 open references. BUSY is a success event |
| function | `aster_doc_shutdown_for_role_swap(rt, doc, budget_ms, …)` | frees the handle now; leave + close under one deadline → `ASTER_EVENT_UNIT_RESULT` |
| function | `aster_docs_open(rt, node, namespace_id, …)` | → `ASTER_EVENT_DOC_OPENED` (`handle` = doc, `related` = node); absent → same kind, status `NOT_FOUND`, handle 0 |
| function | `aster_docs_import_write_namespace(rt, node, secret (32 raw bytes), …)` / `aster_docs_import_read_namespace(rt, node, namespace_id, …)` | open-or-import; → `ASTER_EVENT_DOC_OPENED` |
| function | `aster_doc_set_bytes_durable(rt, doc, author, key, value, …)` | → `ASTER_EVENT_DOC_SET` (payload = content hash hex) |
| function | `aster_doc_entry_exists(rt, doc, author, key, …)` | tombstones count → `ASTER_EVENT_DOC_ENTRY_EXISTS`, `flags` 1/0 |
| function | `aster_doc_entry_content_status(rt, doc, content_hash, …)` | → `ASTER_EVENT_DOC_CONTENT_STATUS`: `flags` = blob status, payload = u64 size |
| function | `aster_connection_paths(rt, conn, aster_path_list_t*)` / `aster_connection_selected_path(…)` | sync; selected = 0 or 1 entries; release with `aster_path_list_release` |
| function | `aster_path_list_release(aster_path_list_t*)` | frees storage, zeroes; NULL / released = no-op |
| function | `aster_send_stream_set_priority(rt, stream, int32_t, …)` / `aster_send_stream_priority(rt, stream, …)` | → `ASTER_EVENT_UNIT_RESULT` / `ASTER_EVENT_STREAM_PRIORITY` (payload i32 LE) |

**Fetch report v1** (little-endian): `u8 version (1)`, 3 reserved bytes, `u32 N`
providers, `u64 bytes_transferred`, then N 44-byte records in first-attempt
order: 32-byte raw node id, `u32 attempts`, `u32 failures`,
`u32 completed_requests`. Blob bytes follow the last record. A provider can be
both failed and served (e.g. across split children). Chosen over a decoded
`#[repr(C)]` struct because the result already travels as one event payload:
no second allocation or release call, and every record is fixed-size.

Behaviour change (no signature change): `aster_add_node_addr` now accepts a
node handle as well as an endpoint handle.

Not exported (Rust-only or not yet needed): `download_collection_from` (use
`aster_blobs_list_collection` after a `HASH_SEQ` fetch), `Docs::default_author`,
`read_entry_contents_if_complete`.

## New in 3.2 (additive)

ABI 3.2.0 puts the C ABI on the `aster` crate (plan D1, tranche F1): nodes start
from an `aster::AsterConfig` through `aster::Node::start`, and Gate 0 /
admission decisions come from the Rust `GatePolicy`. Nothing in 3.0/3.1 changed
shape; a 3.1 binding keeps working. Source: `ffi/src/node.rs`; tests:
`ffi/src/node.rs` (config round-trip / env / TOML), `ffi/tests/node_start.rs`,
`ffi/tests/abi_contract_test.c`.

| Kind | Name | Notes |
|---|---|---|
| struct | `aster_config_t` (280 B) | `struct_size, relay_mode, relay_urls, secret_key (32 raw bytes or empty), secret_key_file, storage_path, gc_interval_ms, bind_addr, alpns, identity_file, proxy_url, hook_timeout_ms (0 = 5000), hook_failure_mode, enable_hooks, enable_monitoring, local_discovery, clear_ip_transports, clear_relay_transports, portmapper, proxy_from_env, transport_set, transport_initial_mtu, transport_send_fairness, transport_enable_segmentation_offload, transport_max_concurrent_{bidi,uni}_streams, transport_{stream_receive,receive,send}_window, transport_max_idle_timeout_ms, transport_keep_alive_interval_ms, transport_datagram_{receive,send}_buffer_size, storage` |
| enum | `aster_portmapper_t` | `DEFAULT`=0 (unset), `ENABLED`=1, `DISABLED`=2 |
| enum | `aster_trust_mode_t` | `PROTECTED`=0 (default), `OPEN_DEV`=1 |
| macro | `ASTER_TRANSPORT_*` (bits 0..11), `ASTER_TRANSPORT_ALL` | presence bits of `aster_config_t.transport_set`; unknown bits rejected |
| type | `aster_gate_t` | `uint64_t` handle, runtime-scoped |
| function | `aster_config_init(aster_config_t*)` | Rust defaults; never allocates (`storage == NULL`) |
| function | `aster_config_from_env(aster_config_t*)` | `AsterConfig::from_env`; release with `aster_config_release` |
| function | `aster_config_from_toml(aster_bytes_t path, aster_config_t*)` | `AsterConfig::from_file` (file, then `ASTER_*` env); release with `aster_config_release` |
| function | `aster_config_release(aster_config_t*)` | frees `storage`, zeroes pointers; no-op when `storage == NULL` |
| function | `aster_node_start(rt, const aster_config_t*, aster_gate_t gate, user_data, out_op)` | async → `ASTER_EVENT_NODE_CREATED` / `ASTER_EVENT_ERROR`; config validated + copied before return (bad config → `INVALID_ARGUMENT`, nothing started); `gate` ≠ 0 forces hooks on |
| function | `aster_node_set_gate(rt, node, gate)` | attach/detach (0); `UNSUPPORTED` for nodes without hooks |
| function | `aster_node_metrics_prometheus(rt, node, aster_bytes_t* out)` | free with `aster_string_release` |
| function | `aster_gate_new`, `aster_gate_free`, `aster_gate_set_trust_mode`, `aster_gate_set_admission_alpns`, `aster_gate_set_reject(code, reason)`, `aster_gate_admit`, `aster_gate_revoke`, `aster_gate_is_admitted`, `aster_gate_should_allow` | peers are 64-hex node ids (case-insensitive) |
| function | `aster_admission_respond(rt, invocation, decision, reject_code, reason)` | answers `ASTER_EVENT_HOOK_{BEFORE,AFTER}_CONNECT` from nodes and endpoints; DENY of an inbound invocation closes with `reject_code`/`reason` |

Admission events on nodes: a node started with `enable_hooks` and no gate raises
the same `ASTER_EVENT_HOOK_BEFORE_CONNECT` / `ASTER_EVENT_HOOK_AFTER_CONNECT`
events as an endpoint (`event.handle` = node, `event.related` = invocation,
`event.user_data` = the start call's `user_data`, same payload encoding). With a
gate attached, inbound peers are decided in Rust (admitted / admission ALPN /
`OPEN_DEV` → accept, else close with the gate's code + reason) and outbound
dials are allowed, without events.

Behaviour fixes in 3.2 (no signature change): `aster_node_addr_info` now fills
`direct_addresses` (it always returned an empty list); `aster_endpoint_addr_info`
returns `INVALID_ARGUMENT` for a NULL scratch buffer. Handle ids are now unique across
all handle kinds within the process (one counter), so a function taking "endpoint
or node" can no longer confuse an endpoint with a node of the same number.

**Deprecated in 3.2, removed in 4.0:** `aster_node_memory`, `aster_node_memory_with_alpns`,
`aster_node_persistent`, `aster_endpoint_create` (and with it the
endpoint-only `aster_endpoint_*` functions and `aster_endpoint_config_t`). Use
`aster_node_start`; `aster_connect`, `aster_accept`, remote info and transport
metrics already take node handles.

## New in 3.1 (additive)

ABI 3.1.0 adds the AEv1 RPC envelope codec (Aster-SPEC §6.9) and zstd payload
compression (§5.6) from `aster_transport_core`, so FFI bindings stop encoding
the envelope with their own Fory. Nothing in 3.0 changed; a 3.0 binding keeps
working against a 3.1 library. Source: `ffi/src/envelope.rs`; vectors checked
by `ffi/tests/envelope_vectors.rs`.

| Kind | Name | Notes |
|---|---|---|
| struct | `aster_stream_header_t` (96 B) | `struct_size, version, call_id, session_id, deadline:i16, serialization_mode:i8, reserved0:u8, reserved1:u32, service, method, metadata_keys, metadata_values, storage` |
| struct | `aster_call_header_t` (72 B) | `struct_size, call_id, deadline:i16, reserved0:u16, reserved1:u32, method, metadata_keys, metadata_values, storage` |
| struct | `aster_rpc_status_t` (64 B) | `struct_size, code, message, detail_keys, detail_values, storage` |
| function | `aster_{stream_header,call_header,rpc_status}_encode(const T*, aster_bytes_t* out)` | caller sets `struct_size`; `out` freed with `aster_string_release` |
| function | `aster_{stream_header,call_header,rpc_status}_decode(const uint8_t*, size_t, T* out)` | library writes `struct_size` and `storage`; strings/lists point into `storage`; empty = `{NULL,0}`; not NUL-terminated |
| function | `aster_{stream_header,call_header,rpc_status}_release(T*)` | frees `storage`, zeroes pointers; no-op when `storage == NULL` |
| function | `aster_zstd_maybe_compress(data, len, threshold, aster_bytes_t* out)` | `out == {NULL,0}` means send uncompressed |
| function | `aster_zstd_decompress(data, len, aster_bytes_t* out)` | 16 MiB cap; corrupt / over-limit -> `ASTER_STATUS_INVALID_ARGUMENT` |
| macro | `ASTER_COMPRESSION_THRESHOLD` (4096), `ASTER_MAX_DECOMPRESSED_SIZE` (16 MiB), `ASTER_ENVELOPE_TAG_{STREAM_HEADER,CALL_HEADER,RPC_STATUS}` (0xA1..0xA3), `ASTER_ENVELOPE_VERSION` (1) | |

Errors: malformed / non-canonical input, wrong `struct_size`, non-UTF-8
strings and key/value length mismatches all return
`ASTER_STATUS_INVALID_ARGUMENT` with `aster_last_error_message` set.

## New in 3.0

| Kind | Name | Notes |
|---|---|---|
| function | `aster_abi_version` | packed version |
| macro | `ASTER_ABI_VERSION` | packed version of the header |
| enum-value | `ASTER_STATUS_ABI_MISMATCH` = 12 | |
| enum-value | `ASTER_RELAY_MODE_STAGING` = 3 | |
| type | `aster_connection_type_t`: `ASTER_CONNECTION_TYPE_NOT_CONNECTED`=0, `ASTER_CONNECTION_TYPE_CONNECTING`=1, `ASTER_CONNECTION_TYPE_UDP_DIRECT`=2, `ASTER_CONNECTION_TYPE_UDP_RELAY`=3, `ASTER_CONNECTION_TYPE_OTHER`=4 | `connection_type` of `aster_remote_info_t` / `aster_connection_info_t` |
| type | `aster_path_kind_t`: `ASTER_PATH_KIND_NONE`=0, `ASTER_PATH_KIND_DIRECT`=1, `ASTER_PATH_KIND_RELAY`=2 | `aster_transport_snapshot_t.path_kind` |
| type | `aster_doc_share_mode_t`: `ASTER_DOC_SHARE_READ`=0, `ASTER_DOC_SHARE_WRITE`=1 | `mode` of `aster_doc_share*` |
| type | `aster_registry_key_kind_t`: `ASTER_REGISTRY_KEY_CONTRACT`=0, `_VERSION`=1, `_CHANNEL`=2, `_LEASE`=3, `_LEASE_PREFIX`=4, `_ACL`=5 | `kind` of `aster_registry_key` |

All ABI enums are now emitted in the header (previously `iroh_event_kind_t`,
`iroh_relay_mode_t`, `iroh_doc_query_mode_t`, `iroh_download_policy_mode_t`
were missing): `aster_status_t`, `aster_event_kind_t`, `aster_relay_mode_t`,
`aster_hook_failure_mode_t`, `aster_hook_decision_t`, `aster_connection_type_t`,
`aster_path_kind_t`, `aster_doc_query_mode_t`, `aster_download_policy_mode_t`,
`aster_doc_share_mode_t`, `aster_registry_key_kind_t`. Struct fields and
arguments still carry them as `uint32_t` (C enum width is not ABI-stable
across compilers and an out-of-range value would be UB in Rust).

Struct layouts are **unchanged** (pinned in `ffi/src/layout.rs` and
`ffi/tests/abi_contract_test.c`; LP64): `aster_event_t` 80,
`aster_runtime_config_t` 16, `aster_endpoint_config_t` 152,
`aster_connect_config_t` 48, `aster_node_addr_t` 48, `aster_bytes_t` 16,
`aster_bytes_list_t` 16, `aster_remote_info_t` 72, `aster_connection_info_t` 56,
`aster_transport_snapshot_t` 56, `aster_transport_metrics_t` 136,
`aster_reactor_call_t` 112.

Symbols that were already `aster_*` / `ASTER_*` (reactor, call pool,
contract, ticket, registry, canonical/signing, `ASTER_CALL_*`,
`ASTER_RECV_FRAME_*`) are unchanged, except the `ASTER_EVENT_KIND_*` row above.

## Full rename table

One row per symbol; columns `kind | old | new | note`. Mechanical rule:
`iroh_` -> `aster_`, `IROH_` -> `ASTER_`, plus the rows below the first rule.

| kind | old | new | note |
|---|---|---|---|
| macro | `ASTER_EVENT_KIND_CALL` | `ASTER_REACTOR_EVENT_CALL` | reactor rename |
| macro | `ASTER_EVENT_KIND_CONNECTION_CLOSED` | `ASTER_REACTOR_EVENT_CONNECTION_CLOSED` | reactor rename |
| macro | `ASTER_EVENT_KIND_RESPONSE_WRITABLE` | `ASTER_REACTOR_EVENT_RESPONSE_WRITABLE` | reactor rename |
| macro | `ASTER_EVENT_KIND_RESPONSE_CLOSED` | `ASTER_REACTOR_EVENT_RESPONSE_CLOSED` | reactor rename |
| function | `iroh_accept` | `aster_accept` |  |
| function | `iroh_accept_bi` | `aster_accept_bi` |  |
| function | `iroh_accept_uni` | `aster_accept_uni` |  |
| function | `iroh_add_node_addr` | `aster_add_node_addr` |  |
| function | `iroh_blobs_add_bytes` | `aster_blobs_add_bytes` |  |
| function | `iroh_blobs_add_bytes_as_collection` | `aster_blobs_add_bytes_as_collection` |  |
| function | `iroh_blobs_add_collection` | `aster_blobs_add_collection` |  |
| function | `iroh_blobs_add_path` | `aster_blobs_add_path` |  |
| function | `iroh_blobs_add_path_with_named_tag` | `aster_blobs_add_path_with_named_tag` |  |
| function | `iroh_blobs_create_collection_ticket` | `aster_blobs_create_collection_ticket` |  |
| function | `iroh_blobs_create_ticket` | `aster_blobs_create_ticket` |  |
| function | `iroh_blobs_download` | `aster_blobs_download` |  |
| function | `iroh_blobs_has` | `aster_blobs_has` |  |
| function | `iroh_blobs_list_collection` | `aster_blobs_list_collection` |  |
| function | `iroh_blobs_local_info` | `aster_blobs_local_info` |  |
| function | `iroh_blobs_observe_complete` | `aster_blobs_observe_complete` |  |
| function | `iroh_blobs_observe_snapshot` | `aster_blobs_observe_snapshot` |  |
| function | `iroh_blobs_read` | `aster_blobs_read` |  |
| function | `iroh_blobs_status` | `aster_blobs_status` |  |
| function | `iroh_buffer_release` | `aster_buffer_release` |  |
| function | `iroh_connect` | `aster_connect` |  |
| function | `iroh_connection_authorize_tunnel_tcp` | `aster_connection_authorize_tunnel_tcp` |  |
| function | `iroh_connection_close` | `aster_connection_close` |  |
| function | `iroh_connection_closed` | `aster_connection_closed` |  |
| function | `iroh_connection_datagram_send_buffer_space` | `aster_connection_datagram_send_buffer_space` |  |
| function | `iroh_connection_free` | `aster_connection_free` |  |
| function | `iroh_connection_info` | `aster_connection_info` |  |
| function | `iroh_connection_max_datagram_size` | `aster_connection_max_datagram_size` |  |
| function | `iroh_connection_open_tunnel` | `aster_connection_open_tunnel` |  |
| function | `iroh_connection_read_datagram` | `aster_connection_read_datagram` |  |
| function | `iroh_connection_remote_id` | `aster_connection_remote_id` |  |
| function | `iroh_connection_send_datagram` | `aster_connection_send_datagram` |  |
| function | `iroh_connection_transport_snapshot` | `aster_connection_transport_snapshot` |  |
| function | `iroh_doc_del` | `aster_doc_del` |  |
| function | `iroh_doc_event_recv` | `aster_doc_event_recv` |  |
| function | `iroh_doc_free` | `aster_doc_free` |  |
| function | `iroh_doc_get_exact` | `aster_doc_get_exact` |  |
| function | `iroh_doc_id` | `aster_doc_id` |  |
| function | `iroh_doc_leave` | `aster_doc_leave` |  |
| function | `iroh_doc_query` | `aster_doc_query` |  |
| function | `iroh_doc_read_entry_content` | `aster_doc_read_entry_content` |  |
| function | `iroh_doc_set_bytes` | `aster_doc_set_bytes` |  |
| function | `iroh_doc_set_download_policy` | `aster_doc_set_download_policy` |  |
| function | `iroh_doc_share` | `aster_doc_share` |  |
| function | `iroh_doc_share_with_addr` | `aster_doc_share_with_addr` |  |
| function | `iroh_doc_start_sync` | `aster_doc_start_sync` |  |
| function | `iroh_doc_subscribe` | `aster_doc_subscribe` |  |
| function | `iroh_docs_create` | `aster_docs_create` |  |
| function | `iroh_docs_create_author` | `aster_docs_create_author` |  |
| function | `iroh_docs_join` | `aster_docs_join` |  |
| function | `iroh_docs_join_and_subscribe` | `aster_docs_join_and_subscribe` |  |
| function | `iroh_endpoint_addr_info` | `aster_endpoint_addr_info` |  |
| function | `iroh_endpoint_close` | `aster_endpoint_close` |  |
| function | `iroh_endpoint_create` | `aster_endpoint_create` |  |
| function | `iroh_endpoint_export_secret_key` | `aster_endpoint_export_secret_key` |  |
| function | `iroh_endpoint_free` | `aster_endpoint_free` |  |
| function | `iroh_endpoint_id` | `aster_endpoint_id` |  |
| function | `iroh_endpoint_remote_info` | `aster_endpoint_remote_info` |  |
| function | `iroh_endpoint_remote_info_list` | `aster_endpoint_remote_info_list` |  |
| function | `iroh_endpoint_transport_metrics` | `aster_endpoint_transport_metrics` |  |
| function | `IROH_EVENT_BLOB_OBSERVE_COMPLETE` | `ASTER_EVENT_BLOB_OBSERVE_COMPLETE` |  |
| function | `IROH_EVENT_DOC_SUBSCRIBED` | `ASTER_EVENT_DOC_SUBSCRIBED` |  |
| function | `iroh_gossip_broadcast` | `aster_gossip_broadcast` |  |
| function | `iroh_gossip_recv` | `aster_gossip_recv` |  |
| function | `iroh_gossip_subscribe` | `aster_gossip_subscribe` |  |
| function | `iroh_gossip_topic_free` | `aster_gossip_topic_free` |  |
| function | `iroh_hook_after_connect_respond` | `aster_hook_after_connect_respond` |  |
| function | `iroh_hook_before_connect_respond` | `aster_hook_before_connect_respond` |  |
| function | `IROH_HOOK_DECISION_ALLOW` | `ASTER_HOOK_DECISION_ALLOW` |  |
| function | `IROH_HOOK_DECISION_DENY` | `ASTER_HOOK_DECISION_DENY` |  |
| function | `iroh_last_error_message` | `aster_last_error_message` |  |
| function | `iroh_node_accept_aster` | `aster_node_accept_aster` |  |
| function | `iroh_node_addr_info` | `aster_node_addr_info` |  |
| function | `iroh_node_close` | `aster_node_close` |  |
| function | `iroh_node_export_secret_key` | `aster_node_export_secret_key` |  |
| function | `iroh_node_free` | `aster_node_free` |  |
| function | `iroh_node_id` | `aster_node_id` |  |
| function | `iroh_node_memory` | `aster_node_memory` |  |
| function | `iroh_node_memory_with_alpns` | `aster_node_memory_with_alpns` |  |
| function | `iroh_node_persistent` | `aster_node_persistent` |  |
| function | `iroh_open_bi` | `aster_open_bi` |  |
| function | `iroh_open_uni` | `aster_open_uni` |  |
| function | `iroh_operation_cancel` | `aster_operation_cancel` |  |
| function | `iroh_poll_events` | `aster_poll_events` |  |
| function | `iroh_recv_stream_free` | `aster_recv_stream_free` |  |
| function | `iroh_runtime_close` | `aster_runtime_close` |  |
| function | `iroh_runtime_new` | `aster_runtime_new` |  |
| function | `iroh_secret_key_generate` | `aster_secret_key_generate` |  |
| function | `iroh_send_stream_free` | `aster_send_stream_free` |  |
| function | `iroh_status_name` | `aster_status_name` |  |
| function | `IROH_STATUS_NOT_FOUND` | `ASTER_STATUS_NOT_FOUND` |  |
| function | `iroh_stream_finish` | `aster_stream_finish` |  |
| function | `iroh_stream_read` | `aster_stream_read` |  |
| function | `iroh_stream_read_exact` | `aster_stream_read_exact` |  |
| function | `iroh_stream_read_to_end` | `aster_stream_read_to_end` |  |
| function | `iroh_stream_stop` | `aster_stream_stop` |  |
| function | `iroh_stream_stopped` | `aster_stream_stopped` |  |
| function | `iroh_stream_write` | `aster_stream_write` |  |
| function | `iroh_string_release` | `aster_string_release` |  |
| function | `iroh_tags_delete` | `aster_tags_delete` |  |
| function | `iroh_tags_get` | `aster_tags_get` |  |
| function | `iroh_tags_list_prefix` | `aster_tags_list_prefix` |  |
| function | `iroh_tags_set` | `aster_tags_set` |  |
| type | `iroh_buffer_t` | `aster_buffer_t` |  |
| type | `iroh_bytes_list_t` | `aster_bytes_list_t` |  |
| type | `iroh_bytes_t` | `aster_bytes_t` |  |
| type | `iroh_connect_config_t` | `aster_connect_config_t` |  |
| type | `iroh_connection_info_t` | `aster_connection_info_t` |  |
| type | `iroh_connection_t` | `aster_connection_t` |  |
| type | `iroh_doc_query_mode_t` | `aster_doc_query_mode_t` | was missing from the old header |
| type | `iroh_download_policy_mode_t` | `aster_download_policy_mode_t` | was missing from the old header |
| type | `iroh_endpoint_config_t` | `aster_endpoint_config_t` |  |
| type | `iroh_endpoint_t` | `aster_endpoint_t` |  |
| type | `iroh_event_kind_t` | `aster_event_kind_t` | was missing from the old header |
| type | `iroh_event_t` | `aster_event_t` |  |
| type | `iroh_hook_decision_t` | `aster_hook_decision_t` |  |
| type | `iroh_hook_failure_mode_t` | `aster_hook_failure_mode_t` |  |
| type | `iroh_hook_invocation_t` | `aster_hook_invocation_t` |  |
| type | `iroh_node_addr_t` | `aster_node_addr_t` |  |
| type | `iroh_node_t` | `aster_node_t` |  |
| type | `iroh_operation_t` | `aster_operation_t` |  |
| type | `iroh_recv_stream_t` | `aster_recv_stream_t` |  |
| type | `iroh_relay_mode_t` | `aster_relay_mode_t` | was missing from the old header |
| type | `iroh_remote_info_t` | `aster_remote_info_t` |  |
| type | `iroh_runtime_config_t` | `aster_runtime_config_t` |  |
| type | `iroh_runtime_t` | `aster_runtime_t` |  |
| type | `iroh_send_stream_t` | `aster_send_stream_t` |  |
| type | `iroh_status_t` | `aster_status_t` |  |
| type | `iroh_transport_metrics_t` | `aster_transport_metrics_t` |  |
| type | `iroh_transport_snapshot_t` | `aster_transport_snapshot_t` |  |
| macro | `IROH_ABI_VERSION_MAJOR` | `ASTER_ABI_VERSION_MAJOR` |  |
| macro | `IROH_ABI_VERSION_MINOR` | `ASTER_ABI_VERSION_MINOR` |  |
| macro | `IROH_ABI_VERSION_PATCH` | `ASTER_ABI_VERSION_PATCH` |  |
| enum-value | `IROH_DOC_QUERY_KEY_EXACT` | `ASTER_DOC_QUERY_KEY_EXACT` |  |
| enum-value | `IROH_DOC_QUERY_KEY_PREFIX` | `ASTER_DOC_QUERY_KEY_PREFIX` |  |
| enum-value | `IROH_DOC_QUERY_LATEST_EXACT` | `ASTER_DOC_QUERY_LATEST_EXACT` |  |
| enum-value | `IROH_DOC_QUERY_LATEST_PREFIX` | `ASTER_DOC_QUERY_LATEST_PREFIX` | was missing from the old header |
| enum-value | `IROH_DOWNLOAD_POLICY_EVERYTHING` | `ASTER_DOWNLOAD_POLICY_EVERYTHING` | was missing from the old header |
| enum-value | `IROH_DOWNLOAD_POLICY_EVERYTHING_EXCEPT` | `ASTER_DOWNLOAD_POLICY_EVERYTHING_EXCEPT` | was missing from the old header |
| enum-value | `IROH_DOWNLOAD_POLICY_NOTHING_EXCEPT` | `ASTER_DOWNLOAD_POLICY_NOTHING_EXCEPT` | was missing from the old header |
| enum-value | `IROH_EVENT_ASTER_ACCEPTED` | `ASTER_EVENT_ASTER_ACCEPTED` |  |
| enum-value | `IROH_EVENT_AUTHOR_CREATED` | `ASTER_EVENT_AUTHOR_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_BLOB_ADDED` | `ASTER_EVENT_BLOB_ADDED` |  |
| enum-value | `IROH_EVENT_BLOB_COLLECTION_ADDED` | `ASTER_EVENT_BLOB_COLLECTION_ADDED` |  |
| enum-value | `IROH_EVENT_BLOB_COLLECTION_TICKET_CREATED` | `ASTER_EVENT_BLOB_COLLECTION_TICKET_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_BLOB_DOWNLOADED` | `ASTER_EVENT_BLOB_DOWNLOADED` | was missing from the old header |
| enum-value | `IROH_EVENT_BLOB_READ` | `ASTER_EVENT_BLOB_READ` |  |
| enum-value | `IROH_EVENT_BLOB_TICKET_CREATED` | `ASTER_EVENT_BLOB_TICKET_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_BYTES_RESULT` | `ASTER_EVENT_BYTES_RESULT` | was missing from the old header |
| enum-value | `IROH_EVENT_CLOSED` | `ASTER_EVENT_CLOSED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECT_FAILED` | `ASTER_EVENT_CONNECT_FAILED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECTED` | `ASTER_EVENT_CONNECTED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECTION_ACCEPTED` | `ASTER_EVENT_CONNECTION_ACCEPTED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECTION_CLOSED` | `ASTER_EVENT_CONNECTION_CLOSED` | was missing from the old header |
| enum-value | `IROH_EVENT_DATAGRAM_RECEIVED` | `ASTER_EVENT_DATAGRAM_RECEIVED` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_CREATED` | `ASTER_EVENT_DOC_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_DEL` | `ASTER_EVENT_DOC_DEL` |  |
| enum-value | `IROH_EVENT_DOC_EVENT` | `ASTER_EVENT_DOC_EVENT` |  |
| enum-value | `IROH_EVENT_DOC_GET` | `ASTER_EVENT_DOC_GET` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_JOINED` | `ASTER_EVENT_DOC_JOINED` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED` | `ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED` |  |
| enum-value | `IROH_EVENT_DOC_QUERY` | `ASTER_EVENT_DOC_QUERY` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_SET` | `ASTER_EVENT_DOC_SET` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_SHARED` | `ASTER_EVENT_DOC_SHARED` |  |
| enum-value | `IROH_EVENT_ENDPOINT_CREATE_FAILED` | `ASTER_EVENT_ENDPOINT_CREATE_FAILED` | was missing from the old header |
| enum-value | `IROH_EVENT_ENDPOINT_CREATED` | `ASTER_EVENT_ENDPOINT_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_ERROR` | `ASTER_EVENT_ERROR` | was missing from the old header |
| enum-value | `IROH_EVENT_FRAME_RECEIVED` | `ASTER_EVENT_FRAME_RECEIVED` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_BROADCAST_DONE` | `ASTER_EVENT_GOSSIP_BROADCAST_DONE` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_LAGGED` | `ASTER_EVENT_GOSSIP_LAGGED` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_NEIGHBOR_DOWN` | `ASTER_EVENT_GOSSIP_NEIGHBOR_DOWN` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_NEIGHBOR_UP` | `ASTER_EVENT_GOSSIP_NEIGHBOR_UP` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_RECEIVED` | `ASTER_EVENT_GOSSIP_RECEIVED` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_SUBSCRIBED` | `ASTER_EVENT_GOSSIP_SUBSCRIBED` | was missing from the old header |
| enum-value | `IROH_EVENT_HOOK_AFTER_CONNECT` | `ASTER_EVENT_HOOK_AFTER_CONNECT` | was missing from the old header |
| enum-value | `IROH_EVENT_HOOK_BEFORE_CONNECT` | `ASTER_EVENT_HOOK_BEFORE_CONNECT` | was missing from the old header |
| enum-value | `IROH_EVENT_HOOK_INVOCATION_RELEASED` | `ASTER_EVENT_HOOK_INVOCATION_RELEASED` | was missing from the old header |
| enum-value | `IROH_EVENT_NODE_CREATE_FAILED` | `ASTER_EVENT_NODE_CREATE_FAILED` | was missing from the old header |
| enum-value | `IROH_EVENT_NODE_CREATED` | `ASTER_EVENT_NODE_CREATED` |  |
| enum-value | `IROH_EVENT_NONE` | `ASTER_EVENT_NONE` | was missing from the old header |
| enum-value | `IROH_EVENT_OPERATION_CANCELLED` | `ASTER_EVENT_OPERATION_CANCELLED` | was missing from the old header |
| enum-value | `IROH_EVENT_REGISTRY_ACL_LISTED` | `ASTER_EVENT_REGISTRY_ACL_LISTED` |  |
| enum-value | `IROH_EVENT_REGISTRY_ACL_UPDATED` | `ASTER_EVENT_REGISTRY_ACL_UPDATED` |  |
| enum-value | `IROH_EVENT_REGISTRY_PUBLISHED` | `ASTER_EVENT_REGISTRY_PUBLISHED` |  |
| enum-value | `IROH_EVENT_REGISTRY_RENEWED` | `ASTER_EVENT_REGISTRY_RENEWED` |  |
| enum-value | `IROH_EVENT_REGISTRY_RESOLVED` | `ASTER_EVENT_REGISTRY_RESOLVED` |  |
| enum-value | `IROH_EVENT_SEND_COMPLETED` | `ASTER_EVENT_SEND_COMPLETED` | was missing from the old header |
| enum-value | `IROH_EVENT_STREAM_ACCEPTED` | `ASTER_EVENT_STREAM_ACCEPTED` | was missing from the old header |
| enum-value | `IROH_EVENT_STREAM_FINISHED` | `ASTER_EVENT_STREAM_FINISHED` | was missing from the old header |
| enum-value | `IROH_EVENT_STREAM_OPENED` | `ASTER_EVENT_STREAM_OPENED` |  |
| enum-value | `IROH_EVENT_STREAM_RESET` | `ASTER_EVENT_STREAM_RESET` | was missing from the old header |
| enum-value | `IROH_EVENT_STRING_RESULT` | `ASTER_EVENT_STRING_RESULT` | was missing from the old header |
| enum-value | `IROH_EVENT_TAG_DELETED` | `ASTER_EVENT_TAG_DELETED` |  |
| enum-value | `IROH_EVENT_TAG_GET` | `ASTER_EVENT_TAG_GET` |  |
| enum-value | `IROH_EVENT_TAG_LIST` | `ASTER_EVENT_TAG_LIST` |  |
| enum-value | `IROH_EVENT_TAG_SET` | `ASTER_EVENT_TAG_SET` |  |
| enum-value | `IROH_EVENT_UNIT_RESULT` | `ASTER_EVENT_UNIT_RESULT` |  |
| enum-value | `IROH_HOOK_FAILURE_FAIL_CLOSED` | `ASTER_HOOK_FAILURE_FAIL_CLOSED` |  |
| enum-value | `IROH_HOOK_FAILURE_FAIL_OPEN` | `ASTER_HOOK_FAILURE_FAIL_OPEN` |  |
| enum-value | `IROH_RELAY_MODE_CUSTOM` | `ASTER_RELAY_MODE_CUSTOM` | was missing from the old header |
| enum-value | `IROH_RELAY_MODE_DEFAULT` | `ASTER_RELAY_MODE_DEFAULT` | was missing from the old header |
| enum-value | `IROH_RELAY_MODE_DISABLED` | `ASTER_RELAY_MODE_DISABLED` | was missing from the old header |
| enum-value | `IROH_STATUS_ALREADY_CLOSED` | `ASTER_STATUS_ALREADY_CLOSED` |  |
| enum-value | `IROH_STATUS_BUFFER_TOO_SMALL` | `ASTER_STATUS_BUFFER_TOO_SMALL` |  |
| enum-value | `IROH_STATUS_CANCELLED` | `ASTER_STATUS_CANCELLED` |  |
| enum-value | `IROH_STATUS_CONNECTION_REFUSED` | `ASTER_STATUS_CONNECTION_REFUSED` |  |
| enum-value | `IROH_STATUS_INTERNAL` | `ASTER_STATUS_INTERNAL` |  |
| enum-value | `IROH_STATUS_INVALID_ARGUMENT` | `ASTER_STATUS_INVALID_ARGUMENT` |  |
| enum-value | `IROH_STATUS_OK` | `ASTER_STATUS_OK` |  |
| enum-value | `IROH_STATUS_QUEUE_FULL` | `ASTER_STATUS_QUEUE_FULL` |  |
| enum-value | `IROH_STATUS_STREAM_RESET` | `ASTER_STATUS_STREAM_RESET` |  |
| enum-value | `IROH_STATUS_TIMEOUT` | `ASTER_STATUS_TIMEOUT` |  |
| enum-value | `IROH_STATUS_UNSUPPORTED` | `ASTER_STATUS_UNSUPPORTED` |  |
| function | `iroh_abi_version_major` | `aster_abi_version` | REMOVED: use `aster_abi_version() >> 16` |
| function | `iroh_abi_version_minor` | `aster_abi_version` | REMOVED: use `(aster_abi_version() >> 8) & 0xff` |
| function | `iroh_abi_version_patch` | `aster_abi_version` | REMOVED: use `aster_abi_version() & 0xff` |

Not listed: `iroh_buffer_free`, `iroh_doc_create`, `iroh_doc_join` appeared only in stale doc comments of the old header (no such functions; the real ones are `aster_string_release`, `aster_docs_create`, `aster_docs_join`).
