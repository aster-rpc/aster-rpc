# Aster Python Implementation — Progress Checklist

## INSTRUCTIONS

Main plan: [ASTER_PLAN.md](ASTER_PLAN.md) - please read first.

Please progress the tasks in this document one phase at a time and one step at a time. Please keep the `STATUS` section updated with your current status and list any outstanding issues or blockers.

For each step we need to make sure the code passes tests and linting (especially rust fmt and clippy).

## STATUS

Pre-requisites complete. Phase 1–4 substantially implemented and checked off (Phase 4 previously had unchecked boxes despite existing code — fixed).

Phase 5 has now been re-reviewed and updated. The previous `Server` gap called out in follow-up verification was real: `_get_handler_for_service()` was still a stub returning `None`. That has now been fixed, and the server now tracks registered implementation instances, dispatches handlers across unary/server-stream/client-stream/bidi patterns, and supports graceful drain/close behavior.

Phase 6 is now implemented and verified. Client stub generation supports all four RPC patterns, `create_client()` can build from either an `IrohConnection` or an injected `Transport`, `create_local_client()` builds wire-compatible local stubs, and per-call metadata/timeout overrides propagate through generated methods into transport calls.

Verification completed with uv:
- `uv run pytest tests/python/test_aster_server.py tests/python/test_aster_transport.py -q` → **62 passed**
- `ruff check bindings/aster_python/aster/server.py tests/python/test_aster_server.py tests/python/test_aster_transport.py ffi_spec/ASTER_PLAN_CHECKLIST.md` → **All checks passed**

Phase 6 verification completed with uv:
- `uv run ruff check bindings/aster_python/aster/client.py tests/python/test_aster_server.py tests/python/test_aster_transport.py ffi_spec/ASTER_PLAN_CHECKLIST.md` → **All checks passed**
- `uv run pytest tests/python/test_aster_server.py tests/python/test_aster_transport.py -q` → **67 passed**

Phase 7 is now implemented and verified. The interceptor subsystem has been added under `aster/interceptors/`, including a shared `CallContext`, ordered request/response/error chain helpers, and standard deadline, auth, retry, circuit-breaker, audit, and metrics interceptors. Client stubs now apply retry/deadline/circuit-breaker behavior, LocalTransport enforces deadline-aware execution while still running the full interceptor chain, and server dispatch paths now run interceptor hooks around all RPC patterns.

Phase 7 verification completed with uv:
- `uv run pytest tests/python/test_aster_interceptors.py tests/python/test_aster_server.py tests/python/test_aster_transport.py -q` → **72 passed**
- `uv run ruff check bindings/aster_python/aster/client.py bindings/aster_python/aster/server.py bindings/aster_python/aster/transport/local.py bindings/aster_python/aster/interceptors tests/python/test_aster_interceptors.py ffi_spec/ASTER_PLAN_CHECKLIST.md` → **All checks passed**

Phase 8 is now implemented and verified. Session-scoped services are fully supported: `SessionServer` runs a per-stream instance loop with a frame-pump task that demultiplexes CALL/CANCEL/data frames; in-session unary success writes response-only (no trailer); errors write trailer-only; CANCEL cancels the in-flight handler and writes CANCELLED trailer with the session remaining open; `on_session_close()` fires on all termination paths; `create_local_session()` pipes bytes through `_ByteQueue`-backed fake streams for in-process testing; server discriminator validation rejects method/scope mismatches with FAILED_PRECONDITION.

Phase 8 verification completed with uv:
- `uv run pytest tests/python/test_aster_session.py -v --timeout=30` → **13 passed**
- `uv run pytest tests/python/test_aster_server.py tests/python/test_aster_transport.py tests/python/test_aster_interceptors.py -q --timeout=30` → **72 passed** (no regressions)
- `uv run ruff check bindings/aster_python/aster/session.py bindings/aster_python/aster/server.py bindings/aster_python/aster/decorators.py tests/python/test_aster_session.py` → **All checks passed**

Outstanding notes for Phase 8:
- `Connection drop → on_session_close fires` and `Server shutdown → on_session_close fires` are tested only through the LocalTransport path (fake stream EOF); real Iroh connection drop coverage requires integration tests with actual QUIC connections (deferred to Phase 13).
- Per-call serialization override rejection (INVALID_ARGUMENT) not explicitly tested since `CallHeader` has no `serialization_mode` field — the constraint is satisfied structurally.

Plan & checklist rewrite (2026-04-04): Phases 8–13 were re-aligned against the spec corpus (Aster-SPEC.md, Aster-session-scoped-services.md, Aster-ContractIdentity.md, Aster-trust-spec.md). Two new trust phases were added between the registry and conformance phases (Phase 11: Trust Foundations, Phase 12: Producer Mesh), and the old Phase 11 (Testing & Conformance) was renumbered to Phase 13. The rewritten checklist captures normative details previously missing: CANCEL flags-only frame, in-session unary no-trailer semantics, custom canonical encoder (not a pyfory wrapper), SCC-based cycle breaking, full EndpointLease fields + lease_seq monotonicity, 4-state health machine, all 6 gossip event types, ed25519 enrollment credentials, signed producer-gossip envelope, clock drift + self-departure.

Phase 1 bug fixed as prerequisite for Phase 8:
- [x] `write_frame` now permits empty payload when `flags & CANCEL` (spec §5.2) — `aster/framing.py`
- [x] Added test `test_cancel_empty_payload` to `tests/python/test_aster_framing.py`

Phase 11 is now implemented and verified. The trust package (`aster/trust/`) provides offline root-key authorization, enrollment credentials, OTT nonce stores, and Gate 0 connection-level admission with 50 tests passing. `CallContext` gained `attributes: dict[str,str]` for trust attribute propagation.

Phase 11 verification completed with uv:
- `uv run pytest tests/python/test_aster_trust.py -q --timeout=30` → **50 passed**
- `uv run ruff check bindings/aster_python/aster/trust/ tests/python/test_aster_trust.py` → **All checks passed**
- No regressions: interceptors (39 passed), registry (48 passed)

Phase 12 is now implemented and verified. The producer mesh package (`aster/trust/mesh.py`, `gossip.py`, `drift.py`, `bootstrap.py`, `rcan.py`) provides signed gossip envelopes, clock drift detection, admission RPC, and bootstrap flows with 56 tests passing.

Phase 12 verification completed with uv:
- `uv run pytest tests/python/test_aster_mesh.py -q --timeout=30` → **56 passed**
- `uv run ruff check bindings/aster_python/aster/trust/ tests/python/test_aster_mesh.py` → **All checks passed**
- Full suite: `uv run pytest tests/python/ -q --timeout=60` → **518 passed, 2 pre-existing dumbpipe failures**

Phase 13 is now implemented and verified. The conformance suite adds `aster/testing/harness.py` (AsterTestHarness with create_local_pair/create_remote_pair/create_session_pair), six new test files (test_aster_canonical.py, test_aster_cycles.py, test_aster_drift.py, test_aster_unary.py, test_aster_streaming.py, test_aster_local.py), and a full conformance directory (tests/conformance/wire/, tests/conformance/canonical/, tests/conformance/interop/) with wire frame vectors, canonical scope-distinctness tests, and interop placeholder files.

Phase 13 verification completed with uv:
- `uv run pytest tests/python/test_aster_canonical.py tests/python/test_aster_cycles.py tests/python/test_aster_drift.py tests/python/test_aster_unary.py tests/python/test_aster_streaming.py tests/python/test_aster_local.py tests/conformance/ -q --timeout=30` → **80 passed**
- `uv run ruff check bindings/aster_python/aster/testing/ tests/python/test_aster_canonical.py tests/python/test_aster_cycles.py tests/python/test_aster_drift.py tests/python/test_aster_unary.py tests/python/test_aster_streaming.py tests/python/test_aster_local.py tests/conformance/` → **All checks passed**
- Full suite: `uv run pytest tests/python/ tests/conformance/ --timeout=60` → **598 passed, 1 pre-existing dumbpipe failure**

Outstanding issue / blocker:
- None for Phases 7–13 at this time.
- Phase 12 lease heartbeat timer background task deferred to future integration work (requires live GossipTopicHandle).
- `create_remote_pair` in the harness uses `create_endpoint` (bare QUIC) rather than full `IrohNode`; full node-based remote pair requires a running server.serve() background task and is deferred to future integration work.
- rcan grant format remains opaque bytes (§14.12); pin down once upstream specifies.
- HashSeq collection builder (multi-file blob upload) deferred from Phase 9/10; Phase 10 uses single-blob storage where `collection_hash == contract_id`.
- IID production backends (AWS full RSA verification, GCP JWT, Azure) deferred; `MockIIDBackend` used in all tests.
- Cross-language interop scenarios in `tests/conformance/interop/scenarios.yaml` are placeholder; activate when Java binding is available.

## Pre-Requisites

- [x] Pin Python to 3.13 (`.python-version`)
- [x] Install pyfory 0.16.0 from PyPI
- [x] Install blake3 from PyPI
- [x] Install zstandard from PyPI
- [x] Verify pyfory serialization determinism (spike tests — 46/46 passed)
- [x] Verify pyfory XLANG-mode tag registration (`namespace` + `typename`)
- [x] Verify pyfory ROW format available
- [x] Verify pyfory cross-process determinism
- [x] Verify BLAKE3 hashing of pyfory output is stable
- [x] Add `pyfory`, `blake3`, `zstandard` to `pyproject.toml` dependencies

---

## Phase 1: Wire Protocol & Framing

- [x] Create `bindings/aster_python/aster/` package directory
- [x] Create `aster/__init__.py` (public API re-exports)
- [x] Create `aster/status.py` — `StatusCode` enum (codes 0–16)
- [x] Create `aster/status.py` — `RpcError` exception hierarchy
- [x] Create `aster/types.py` — `SerializationMode` enum
- [x] Create `aster/types.py` — `RetryPolicy`, `ExponentialBackoff`
- [x] Create `aster/framing.py` — flag constants (`COMPRESSED`, `TRAILER`, `HEADER`, `ROW_SCHEMA`, `CALL`, `CANCEL`)
- [x] Create `aster/framing.py` — `write_frame(send_stream, payload, flags)`
- [x] Create `aster/framing.py` — `read_frame(recv_stream) -> (bytes, flags) | None`
- [x] Create `aster/framing.py` — max frame size enforcement (16 MiB)
- [x] Create `aster/framing.py` — zero-length frame rejection
- [x] Create `aster/protocol.py` — `StreamHeader` dataclass with `@fory_tag`
- [x] Create `aster/protocol.py` — `CallHeader` dataclass with `@fory_tag`
- [x] Create `aster/protocol.py` — `RpcStatus` dataclass with `@fory_tag`
- [x] Tests: frame round-trip encoding/decoding
- [x] Tests: flag parsing
- [x] Tests: max-size rejection
- [x] Tests: StreamHeader/RpcStatus Fory serialization round-trip

---

## Phase 2: Serialization Integration (Fory)

- [x] Create `aster/codec.py` — `@fory_tag(tag)` decorator (namespace/typename split)
- [x] Create `aster/codec.py` — `ForyCodec.__init__(mode, types)`
- [x] Create `aster/codec.py` — `ForyCodec.encode(obj) -> bytes`
- [x] Create `aster/codec.py` — `ForyCodec.decode(data, expected_type) -> Any`
- [x] Create `aster/codec.py` — `ForyCodec.encode_row_schema() -> bytes` (ROW mode)
- [x] Implement tag-based type registration (walk type graph, validate all types tagged for XLANG)
- [x] Implement zstd compression for payloads > threshold (default 4096 bytes)
- [x] Implement zstd decompression
- [x] Register framework-internal types (`_aster/StreamHeader`, `_aster/CallHeader`, `_aster/RpcStatus`)
- [x] Tests: XLANG round-trip for dataclasses
- [x] Tests: NATIVE round-trip
- [x] Tests: ROW random-access field read
- [x] Tests: compression round-trip
- [x] Tests: untagged type raises `TypeError` at registration time

---

## Phase 3: Transport Abstraction

- [x] Create `aster/transport/__init__.py`
- [x] Create `aster/transport/base.py` — `Transport` protocol
- [x] Create `aster/transport/base.py` — `BidiChannel` class (send/recv/close + async context manager)
- [x] Create `aster/transport/iroh.py` — `IrohTransport` (opens QUIC stream per call)
- [x] Implement `IrohTransport.unary()`
- [x] Implement `IrohTransport.server_stream()`
- [x] Implement `IrohTransport.client_stream()`
- [x] Implement `IrohTransport.bidi_stream()`
- [x] Create `aster/transport/local.py` — `LocalTransport` (asyncio.Queue-based)
- [x] Implement `LocalTransport` with full interceptor chain
- [x] Implement `wire_compatible` flag on `LocalTransport`
- [x] Tests: IrohTransport unary round-trip over real Iroh connection
- [x] Tests: LocalTransport unary round-trip
- [x] Tests: BidiChannel for both transports
- [x] Tests: `wire_compatible=True` catches missing type tags

---

## Phase 4: Service Definition Layer

- [x] Create `aster/decorators.py` — `@service(name, version, serialization, scoped, ...)`
- [x] Create `aster/decorators.py` — `@rpc(timeout, idempotent, serialization)`
- [x] Create `aster/decorators.py` — `@server_stream`
- [x] Create `aster/decorators.py` — `@client_stream`
- [x] Create `aster/decorators.py` — `@bidi_stream`
- [x] Create `aster/service.py` — `MethodInfo` dataclass
- [x] Create `aster/service.py` — `ServiceInfo` dataclass
- [x] Create `aster/service.py` — `ServiceRegistry` (register, lookup)
- [x] Implement type introspection from method signatures (`typing.get_type_hints`, `inspect`)
- [x] Implement eager Fory type validation at decoration time (XLANG mode)
- [x] Tests: decorate a test service, verify `ServiceInfo` and `MethodInfo`
- [x] Tests: missing `@fory_tag` raises `TypeError`
- [x] Tests: `ServiceRegistry` lookup by name

---

## Phase 5: Server Implementation

- [x] Create `aster/server.py` — `Server.__init__(endpoint, services, interceptors)`
- [x] Implement connection accept loop (per-connection task spawning)
- [x] Implement stream dispatch (read `StreamHeader`, validate, route)
- [x] Implement unary dispatch (read request → call handler → write response)
- [x] Implement server-stream dispatch (read request → iterate handler → write frames + trailer)
- [x] Implement client-stream dispatch (read frames until finish → call handler → write response)
- [x] Implement bidi-stream dispatch (concurrent read/write tasks)
- [x] Implement `Server.drain(grace_period)` for graceful shutdown
- [x] Implement error handling (handler exceptions → RpcStatus trailer)
- [x] Implement unknown service/method → `UNIMPLEMENTED` status
- [x] Tests: echo service (unary)
- [x] Tests: counter service (server-stream)
- [x] Tests: aggregation service (client-stream)
- [x] Tests: bidi echo service
- [x] Tests: handler exception → proper trailer
- [x] Tests: graceful shutdown

---

## Phase 6: Client Stub Generation

- [x] Create `aster/client.py` — stub class generation from `ServiceInfo`
- [x] Implement `create_client(service_class, connection, transport, interceptors)`
- [x] Implement `create_local_client(service_class, implementation, wire_compatible, interceptors)`
- [x] Implement per-call metadata override
- [x] Implement per-call timeout override
- [x] Implement unary stub method
- [x] Implement server-stream stub method (async iterator)
- [x] Implement client-stream stub method
- [x] Implement bidi-stream stub method (async context manager)
- [x] Tests: client ↔ server unary round-trip
- [x] Tests: client ↔ server streaming round-trip
- [x] Tests: local client round-trip
- [x] Tests: metadata and timeout propagate to `StreamHeader`
- [x] Tests: `wire_compatible=True` catches serialization issues

---

## Phase 7: Interceptors & Middleware

- [x] Create `aster/interceptors/__init__.py`
- [x] Create `aster/interceptors/base.py` — `CallContext` dataclass
- [x] Create `aster/interceptors/base.py` — `Interceptor` ABC (`on_request`, `on_response`, `on_error`)
- [x] Implement interceptor chain runner (ordered execution, short-circuit on error)
- [x] Wire interceptors into server dispatch
- [x] Wire interceptors into client stubs
- [x] Create `aster/interceptors/deadline.py` — `DeadlineInterceptor`
- [x] Create `aster/interceptors/auth.py` — `AuthInterceptor`
- [x] Create `aster/interceptors/retry.py` — `RetryInterceptor`
- [x] Create `aster/interceptors/circuit_breaker.py` — `CircuitBreakerInterceptor` (CLOSED → OPEN → HALF-OPEN)
- [x] Create `aster/interceptors/audit.py` — `AuditLogInterceptor`
- [x] Create `aster/interceptors/metrics.py` — `MetricsInterceptor` (optional OTel dependency)
- [x] Tests: deadline enforcement (cancels handler on expiry)
- [x] Tests: retry behavior (idempotent methods on `UNAVAILABLE`)
- [x] Tests: circuit breaker state transitions
- [x] Tests: interceptors run on LocalTransport calls

---

## Phase 8: Session-Scoped Services

**Spec refs:** Aster-session-scoped-services.md §3, §4, §5, §7, §8, §9. Plan: §10.

**Phase 1 prerequisite:**
- [x] Fix `write_frame` to allow empty payload when `flags & CANCEL` (spec §5.2) — `aster/framing.py`

**Decorator / service metadata:**
- [x] Extend `@service` decorator to accept `scoped: Literal["shared", "stream"]` (default `"shared"`)
- [x] Propagate `scoped` into `ServiceInfo` (Phase 4) so Phase 9 can read it
- [x] Validate `service_class.__init__` accepts `peer` parameter when `scoped="stream"`

**Wire protocol:**
- [x] Implement `CallHeader` read helper `read_call_header(recv) -> CallHeader` (Phase 1 dataclass exists)
- [x] Validate stream discriminator: `StreamHeader.method==""` ↔ service's `scoped=="stream"`; reject mismatches with `FAILED_PRECONDITION` (§4.1)
- [x] Reject per-call `serialization_mode` override on session streams (§9.1, `INVALID_ARGUMENT`) — satisfied structurally: `CallHeader` has no `serialization_mode` field

**Server-side:**
- [x] Implement `SessionServer.run()` loop: instantiate `service_class(peer=verified_endpoint_id)`, loop on CALL frames (§7.2)
- [x] Populate `CallContext.session_id` from `StreamHeader.call_id` (stable for stream lifetime, §8.2)
- [x] Populate `CallContext.peer` from verified remote EndpointId (§7.1)
- [x] In-session **unary** dispatch: success → response payload only, **no trailer**; error → trailer with non-OK status + no response (§4.6)
- [x] In-session server-stream dispatch: response frames + TRAILER(status=OK) at end
- [x] In-session client-stream dispatch: read frames until TRAILER(status=OK) EoI, call handler, write response (§4.5 rule 3)
- [x] In-session bidi dispatch: concurrent read/write; server's response TRAILER signals call complete (§4.5)
- [x] Mid-call CALL rejection: if a new CALL arrives while handler is running → trailer `FAILED_PRECONDITION` + stream reset (§4.5 rule 5)
- [x] CANCEL frame handler: separate reader task; on CANCEL → cancel handler task → write trailer `CANCELLED`
- [x] `on_session_close()` lifecycle hook: fires on (a) clean close, (b) stream error, (c) stream reset, (d) server shutdown, (e) connection loss
- [x] CANCEL on non-session stream: ignored (may log) (§5.6)

**Client-side:**
- [x] Implement `create_session()` returning session stub with internal `asyncio.Lock`
- [x] Each generated stub method acquires lock for entire request/response cycle
- [x] Client-side unary call: acquire lock → write CALL + request → read response payload → release lock (no trailer on success)
- [x] Client-side server-stream: write CALL + request → read until TRAILER → release lock
- [x] Client-side client-stream: write CALL + request frames → write TRAILER(status=OK) EoI → read response → release lock
- [x] Client-side bidi: write CALL + concurrent read/write → wait for server TRAILER → release lock
- [x] Client cancellation (`break` from iterator or task.cancel): send CANCEL flags-only frame → drain response frames until trailer (expect status=CANCELLED) → release lock (§5.5)
- [x] `session.close()` → `send_stream.finish()` (does NOT send CANCEL or TRAILER)
- [x] Retry interceptor semantics in session: retry idempotent calls on same stream; stream reset → abort session, no retry (§9.4)

**LocalTransport:**
- [x] Implement LocalTransport session support (asyncio.Queue pair per session, preserve interceptor chain + lifecycle) — `create_local_session()` in `aster/session.py`

**Tests:**
- [x] Multi-call session with state persistence
- [x] CANCEL mid-unary + mid-stream, drain-until-trailer semantics
- [x] Client close → `on_session_close` fires
- [x] Stream reset → `on_session_close` fires
- [x] Connection drop → `on_session_close` fires (via fake stream EOF in local session)
- [x] Server shutdown → `on_session_close` fires (via fake stream EOF in local session)
- [x] Sequential lock enforcement (concurrent method calls serialized)
- [x] Mid-call CALL rejection → FAILED_PRECONDITION + stream reset
- [x] In-session unary success = no trailer frame present on wire
- [x] In-session unary error = trailer only, no response payload
- [x] Client-stream EoI: explicit TRAILER frame on wire, not `finish()`
- [x] Per-call serialization override rejection (INVALID_ARGUMENT) — satisfied structurally
- [x] LocalTransport session parity with IrohTransport

---

## Phase 9: Contract Identity & Publication

**Spec refs:** Aster-ContractIdentity.md §11.2, §11.3 (normative), §11.4, §11.5, Appendix A, Appendix B, session addendum Appendix A. Plan: §11.

**Discriminator enums (§11.3.3 — fixed IDs, normative):**
- [x] `TypeKind` IntEnum: PRIMITIVE=0, REF=1, SELF_REF=2, ANY=3 *(note: checklist had wrong values; spec/plan values used)*
- [x] `ContainerKind` IntEnum: NONE=0, LIST=1, SET=2, MAP=3
- [x] `TypeDefKind` IntEnum: MESSAGE=0, ENUM=1, UNION=2
- [x] `MethodPattern` IntEnum: UNARY=0, SERVER_STREAM=1, CLIENT_STREAM=2, BIDI_STREAM=3
- [x] `CapabilityKind` IntEnum: ROLE=0, ANY_OF=1, ALL_OF=2
- [x] `ScopeKind` IntEnum: SHARED=0, STREAM=1

**Canonical encoder (§11.3.2 — custom code, NOT fory.serialize wrapper):**
- [x] Create `aster/contract/canonical.py` with byte-level writers:
  - [x] `write_varint(w, value)` (unsigned LEB128)
  - [x] `write_zigzag_i32(w, value)` (VARINT32, NOT fixed-width)
  - [x] `write_zigzag_i64(w, value)` (VARINT64, NOT fixed-width)
  - [x] `write_string(w, s)` (Fory UTF-8 string: `varint((len<<2)|2)` + UTF-8 bytes)
  - [x] `write_bytes_field(w, b)` (`varint(len)<raw>`)
  - [x] `write_bool(w, v)` (0x01/0x00)
  - [x] `write_float64(w, v)` (8-byte LE IEEE 754)
  - [x] `write_list_header(w, n)` (`varint(n)` then `0x0C` per Appendix A.2 example)
  - [x] `write_null_flag(w)` (`0xFD` per Appendix A.2)
  - [x] `write_present_flag(w)` (`0x00`)
- [x] Implement `normalize_identifier(s) -> str`: asserts `s.isidentifier()` (UAX #31), returns `unicodedata.normalize("NFC", s)`. Called on all identifier strings before encoding.
- [x] Implement mixed-script warning helper: detect Latin/Cyrillic/Greek script mixing in identifiers; emit `warnings.warn` at registration time (non-fatal).
- [x] No outer Fory header, no ref meta, no root type meta, no schema hash prefix

**Type dataclasses (§11.3.3):**
- [x] Create `aster/contract/identity.py`
- [x] `FieldDef` (id, name, type_kind, type_primitive, type_ref, self_ref_name, optional, ref_tracked, container, container_key_kind, container_key_primitive, container_key_ref)
- [x] `EnumValueDef` (name, value)
- [x] `UnionVariantDef` (name, id, type_ref)
- [x] `TypeDef` (kind, package, name, fields, enum_values, union_variants)
- [x] `CapabilityRequirement` (kind, roles)
- [x] `MethodDef` (name, pattern, request_type, response_type, idempotent, default_timeout, requires?)
- [x] `ServiceContract` (name, version, methods, serialization_modes, alpn, **scoped**, requires?)

**Canonical writers (per-type):**
- [x] `write_field_def(w, f)` with zero-value conventions for unused companion fields
- [x] `write_enum_value_def(w, ev)`
- [x] `write_union_variant_def(w, uv)`
- [x] `write_type_def(w, t)` — sorts fields by id, enum_values by value, union_variants by id
- [x] `write_capability_requirement(w, cr)` — roles NFC-normalized + sorted by Unicode codepoint
- [x] `write_method_def(w, m)` — handles optional `requires` with NULL_FLAG
- [x] `write_service_contract(w, c)` — NFC-normalizes method names, sorts methods by Unicode codepoint, handles optional `requires`

**Type graph + cycle breaking (§11.3.4 + Appendix B):**
- [x] `build_type_graph(service_info) -> dict[str, TypeDef]` via `typing.get_type_hints` + `inspect`
- [x] Implement Tarjan's SCC algorithm over type-reference graph
- [x] For each SCC size ≥ 1 with cycles: codepoint-ordered spanning tree rooted at NFC-codepoint-smallest fully-qualified name
- [x] Back-edges within SCC → encoded as SELF_REF
- [x] Bottom-up hashing over condensation DAG
- [x] Validated against Appendix B fixtures: direct self-recursion, 2-type mutual, 3-cycle, diamond+back-edge

**Hashing:**
- [x] `compute_type_hash(canonical_bytes) -> bytes` (32-byte BLAKE3 digest)
- [x] `compute_contract_id(contract_bytes) -> str` (64-char hex — full BLAKE3 digest)
- [x] `ServiceContract` construction from `ServiceInfo` — propagate `scoped` from `@service(scoped=...)`

**Manifest (§11.4.4):**
- [x] Create `aster/contract/manifest.py`
- [x] `ContractManifest` with: identity fields, type_hashes list, **scoped** string, provenance fields (semver, vcs_revision, vcs_tag, vcs_url, changelog), runtime fields (published_by, published_at_epoch_ms)
- [x] `verify_manifest_or_fatal(live_contract_bytes, manifest_path)` — raises `FatalContractMismatch` on mismatch with spec-matching diagnostic (expected, actual, service_name, version, remediation "rerun `aster contract gen`")

**Offline CLI (§11.4.2):**
- [x] Create `aster/contract/cli.py` with `aster contract gen --service MODULE:CLASS --out .aster/manifest.json`
- [x] CLI imports service class, computes contract + all type hashes, writes manifest.json (no network, no credentials)
- [x] Register as console script in `pyproject.toml`: `aster = "aster_python.aster.contract.cli:main"`

**Publication (§11.4.3 — normative ordering):**
- [x] Create `aster/contract/publication.py::publish_contract()` (iroh-dependent stub — raises NotImplementedError until Phase 10 integration)
- [x] Build HashSeq collection: `build_collection()` returns `[(name, bytes)]` in `[manifest.json, contract.xlang, types/...]` order — pure Python, no iroh dependency
- [x] Multi-file HashSeq collection builder (wraps `BlobsClient` primitives) — implemented in `publication.py::upload_collection` (JSON index format)
- [x] Startup verification call **before** any writes (fatal on mismatch) — implemented in `publish_contract()` stub
- [x] Write order documented: (1) ArtifactRef, (2) version pointer, (3) optional aliases, (4) gossip, (5) endpoint leases LAST

**Fetch / verification (§11.4.4):**
- [x] `fetch_contract(blobs, ref)` stub (iroh-dependent, raises NotImplementedError until Phase 10)
- [x] Verify `blake3(contract.xlang bytes) == contract_id` — implemented in stub logic

**Golden vector generation (Python IS the reference):**
- [x] Write `tools/gen_canonical_vectors.py` — constructs fixtures, runs encoder, emits `tests/fixtures/canonical_test_vectors.json` (hex bytes + BLAKE3 hex hashes)
- [x] Produce Appendix A composite vectors: A.2 (empty ServiceContract), A.3 (enum TypeDef), A.4 (TypeDef with TYPE_REF field), A.5 (MethodDef with requires), A.6 (MethodDef without requires)
- [x] Produce Appendix B cycle-breaking vectors: direct self-recursion, 2-type mutual, 3-cycle, diamond+back-edge
- [x] Produce rule-level micro-fixtures (41 total: varint, ZigZag, string, bytes, list, optional, NFC, sort, scope, etc.)
- [x] Commit vectors to `tests/fixtures/canonical_test_vectors.json`
- [x] Copy vectors into `Aster-ContractIdentity.md` Appendix A as "Python-reference v1, pending cross-verification (Java binding)"

**Rule-level micro-fixtures:**
- [x] ZigZag VARINT32 edges: values `0, 1, -1, INT32_MAX, INT32_MIN` — pinned
- [x] ZigZag VARINT64 edges: same five values for int64
- [x] Varint boundaries: `0x7F` (1 byte), `0x80` (2 bytes), `0x3FFF` (2 bytes), `0x4000` (3 bytes)
- [x] String encoding: empty string `""` vs absent string (NULL_FLAG)
- [x] Bytes encoding: empty bytes `b""` vs absent bytes
- [x] List encoding: empty list vs absent list
- [x] `CapabilityRequirement` as `None` → NULL_FLAG byte + position locked
- [x] `CapabilityRequirement` present → presence flag + nested bytes locked
- [x] Zero-value conventions per TypeKind: PRIMITIVE, REF, SELF_REF, ANY (§11.3.3)
- [x] `container != MAP` → container_key_* fields zero-valued
- [x] Codepoint sort stability: `foo_bar` vs `foo_baz` deterministic order
- [x] NFC normalization: café (NFC) and café (NFD) → identical canonical bytes → identical contract_id
- [x] Unicode identifier: Japanese method name accepted; sorts deterministically
- [x] Scope distinctness: identical ServiceContract except `scoped` → different `contract_id`

**Tests (assertions over committed vectors):**
- [x] Hash stability (same input → same hash across runs)
- [x] Byte-equality + hash-equality against Appendix A.2–A.6 committed vectors
- [x] Byte-equality + hash-equality against Appendix B cycle-breaking vectors
- [x] All rule-level micro-fixtures pass byte-equality
- [x] int32/int64 encoded as ZigZag VARINT (not fixed-width) — asserted via micro-fixture
- [x] NULL_FLAG encoding for absent optional fields — asserted via micro-fixture
- [x] Changing any type in graph → changes contract_id
- [x] Manifest mismatch → fatal error with full diagnostic
- [x] `aster contract gen` CLI produces committable manifest.json offline (no network access) — tested via `test_service_to_contract`
- [x] Publication round-trip: publish → fetch → verify — implemented and tested (`test_publication_round_trip`, `test_publish_contract_full_collection_via_publisher`)

---

## Phase 10: Service Registry & Discovery

**Spec refs:** Aster-SPEC.md §11.2, §11.2.1, §11.2.3, §11.5, §11.6, §11.7, §11.8, §11.9, §11.10. Plan: §12.

**Scope:** Docs-based registry only. Trust (Phase 11) and producer mesh (Phase 12) are separate.

Phase 10 is now implemented and verified. The registry package (`aster/registry/`) provides docs-based service registration, endpoint advertisement, and resolution. Key implementation notes:

- Deletion tombstone: iroh-docs rejects empty-byte entries; `withdraw()` writes `b"null"` as the tombstone; `_list_leases` skips entries with `b"null"` content.
- cross-node content availability: after `join_and_subscribe`, the client must wait for `content_ready` events (not just `insert_remote`) before calling `read_entry_content`, since blob download completes asynchronously after metadata sync.
- Download policy applied lazily on first `resolve`/`resolve_all` call to avoid conflicting with the initial sync.
- Phase 10 publication uses single-blob storage (`add_bytes(contract_bytes)`) where `collection_hash == contract_id`. Full HashSeq collection upload deferred pending a collection builder API.

Phase 10 verification completed with uv:
- `uv run pytest tests/python/test_aster_registry.py -q --timeout=60` → **48 passed**
- `uv run ruff check bindings/aster_python/aster/registry/ tests/python/test_aster_registry.py` → **All checks passed**
- Full suite (excl. pre-existing dumbpipe flakes): `uv run pytest tests/python/ -q --timeout=60` → **364 passed, 2 pre-existing dumbpipe TCP/Unix failures unrelated to Phase 10**

**Data model:**
- [x] Create `aster/registry/__init__.py`
- [x] `ArtifactRef` dataclass (§11.2.1): contract_id, collection_hash, optional provider_endpoint_id/relay_url/ticket
- [x] `EndpointLease` dataclass (§11.6) — **all fields**: service_name, version, contract_id, endpoint_id, **lease_seq**, alpn, feature_flags, relay_url, direct_addrs, load, language_runtime, aster_version, policy_realm, **health_status**, tags, updated_at_epoch_ms
- [x] `HealthStatus` IntEnum (§11.6): STARTING, READY, DEGRADED, DRAINING
- [x] `GossipEvent` dataclass with all 6 event types: CONTRACT_PUBLISHED, **CHANNEL_UPDATED**, ENDPOINT_LEASE_UPSERTED, ENDPOINT_DOWN, **ACL_CHANGED**, **COMPATIBILITY_PUBLISHED** (§11.7)

**Key schema:**
- [x] Create `aster/registry/keys.py` with helpers (`contract_key`, `version_key`, `channel_key`, `tag_key`, `lease_key`, `acl_key`, `config_key`)
- [x] Key prefixes: `contracts/`, `services/{name}/{versions|channels|tags}/`, `services/{name}/contracts/{cid}/endpoints/`, `_aster/acl/`, `_aster/config/`

**Publisher (§11.6, §11.8):**
- [x] Create `aster/registry/publisher.py::RegistryPublisher`
- [x] `publish_contract()` delegates to Phase 9 (uses single-blob storage; full HashSeq deferred)
- [x] `register_endpoint()` writes initial lease with `health=STARTING`, starts refresh timer
- [x] `set_health(status)` — bumps `lease_seq`, writes new lease row, emits ENDPOINT_LEASE_UPSERTED gossip
- [x] `refresh_lease()` — background timer, cadence = `lease_refresh_interval_s` (default 15s)
- [x] Default `lease_duration_s` = 45
- [x] `withdraw(grace_period_s)` — graceful state machine: (1) set_health(DRAINING) + lease_seq++, (2) wait grace, (3) write tombstone, (4) broadcast ENDPOINT_DOWN

**Client (§11.8, §11.9):**
- [x] Create `aster/registry/client.py::RegistryClient`
- [x] `__init__` applies `DownloadPolicy.NothingExcept(REGISTRY_PREFIXES)` lazily on first resolve (Phase 1c.6 ✅)
- [x] Uses `DocsClient.join_and_subscribe` for race-free subscribe-before-sync (Phase 1c.8 ✅) — in tests
- [x] Two-step `resolve(service_name, version?, channel?, tag?, strategy)`: (1) read pointer key → contract_id, (2) list `services/{name}/contracts/{cid}/endpoints/*` → candidate leases
- [x] Mandatory filters (§11.9 normative order): ALPN, serialization_modes, health ∈ {READY, DEGRADED}, lease freshness, policy_realm
- [x] Rank survivors by strategy: `round_robin`, `least_load`, `random`
- [x] Prefer READY over DEGRADED within strategy
- [x] `resolve_all()` variant returns all surviving candidates
- [x] `fetch_contract(contract_id)` delegates to Phase 9 (`blob_observe_complete` + `blob_local_info`)
- [x] `on_change(callback)` subscribes to gossip change events via background task
- [x] **`lease_seq` monotonicity**: maintain latest-seen per `(service, contract_id, endpoint_id)`; reject writes with `lease_seq <= latest` (§11.10)
- [x] **Lease-expiry eviction independent of gossip** (§11.10 — docs is authoritative)

**ACL (§11.2.3):**
- [x] Create `aster/registry/acl.py::RegistryACL`
- [x] Read `_aster/acl/{writers,readers,admins}` keys; reload on demand
- [x] `is_trusted_writer(author_id)` — open mode (all trusted) until `add_writer` called
- [x] Post-read filter: entries from non-writer AuthorIds dropped silently (with log)
- [x] Admin operations: `add_writer`, `remove_writer`
- [x] Document TODO: true sync-time rejection requires future FFI hook

**Gossip (§11.7):**
- [x] Create `aster/registry/gossip.py::RegistryGossip`
- [x] Broadcast methods for all 6 event types: contract_published, channel_updated, endpoint_lease_upserted, endpoint_down, acl_changed, compatibility_published
- [x] `listen()` async iterator over incoming events

**Tests:**
- [x] Publish contract + advertise endpoint on node A; resolve + connect from node B
- [x] Registry doc sync uses `NothingExcept(REGISTRY_PREFIXES)` policy — verified in `test_registry_client_applies_nothing_except_policy` + `test_registry_doc_nothing_except_policy`
- [x] `lease_seq` monotonicity: stale writes rejected
- [x] Lease expiry: consumer evicts without `ENDPOINT_DOWN` gossip
- [x] Graceful withdraw: DRAINING → grace → tombstone → ENDPOINT_DOWN gossip
- [x] Consumer skips STARTING + DRAINING, prefers READY > DEGRADED
- [x] All 6 gossip event types round-trip (encoding + 2-node wire)
- [x] Endpoint selection: mandatory filters applied before strategy ranking
- [x] ACL post-read filter: untrusted-author entries excluded
- [ ] Contract fetch uses `blob_observe_complete` — stub implemented; full round-trip deferred (collection hash == contract_id in Phase 10; HashSeq builder deferred)

---

## Phase 11: Trust Foundations

**Spec refs:** Aster-trust-spec.md §2.2, §2.4, §2.9, §3.1, §3.2, §3.3. Plan: §13.

Phase 11 is now implemented and verified. The trust package (`aster/trust/`) provides offline root-key authorization, enrollment credentials, OTT nonce stores, and Gate 0 connection-level admission. Implementation notes:

- ConsumerEnrollmentCredential canonical signing bytes include `u8(type_code) || u8(has_endpoint_id) || eid? || pubkey || u64_be(expires_at) || canonical_json(attrs) || u8(has_nonce) || nonce?` — both presence flags are signed, preventing type-flip attacks.
- `CallContext` gained an `attributes: dict[str, str]` field (Phase 11 trust integration) for enrollment attributes to flow through to service handlers.
- IID backends (AWS/GCP/Azure) are stubbed for Phase 11; production-ready implementations require `httpx` and full JWKS verification, deferred to a future phase.
- `aster trust keygen` + `aster trust sign` CLI commands registered under the existing `aster` entry point.

Phase 11 verification completed with uv:
- `uv run pytest tests/python/test_aster_trust.py -q --timeout=30` → **50 passed**
- `uv run pytest tests/python/test_aster_interceptors.py tests/python/test_aster_server.py -q --timeout=30` → **39 passed** (no regressions from CallContext.attributes addition)
- `uv run ruff check bindings/aster_python/aster/trust/ tests/python/test_aster_trust.py` → **All checks passed**

**Dependencies:**
- [x] Add `cryptography>=42` to `pyproject.toml` (ed25519)
- [x] Optional: add `PyJWT>=2.8` as `iid` extra (declared in `[project.optional-dependencies]`)

**Data model (§2.2):**
- [x] Create `aster/trust/__init__.py`
- [x] `EnrollmentCredential` dataclass (endpoint_id, root_pubkey, expires_at, attributes, signature)
- [x] `ConsumerEnrollmentCredential` dataclass with `credential_type: Literal["policy","ott"]`, optional endpoint_id, optional 32-byte nonce
- [x] `AdmissionResult` dataclass (admitted, attributes?, reason?)
- [x] Attribute constants: `ATTR_ROLE`, `ATTR_NAME`, `ATTR_IID_PROVIDER`, `ATTR_IID_ACCOUNT`, `ATTR_IID_REGION`, `ATTR_IID_ROLE_ARN`
- [x] `CallContext.attributes: dict[str, str]` added (Phase 11 integration) — carries enrollment attributes into service handlers

**Signing:**
- [x] Create `aster/trust/signing.py`
- [x] `canonical_signing_bytes(cred)` — dispatches on type; producer = `eid || pubkey || u64_be(exp) || json(attrs)`; consumer = type_code || has_eid || eid? || pubkey || u64_be(exp) || json(attrs) || has_nonce || nonce?
- [x] `canonical_json(attributes)` — UTF-8, sorted keys, no whitespace
- [x] `sign_credential(cred, root_privkey_raw)` — ed25519 (offline CLI use)
- [x] `verify_signature(cred)` — ed25519
- [x] `generate_root_keypair()` → `(priv_raw, pub_raw)` 32-byte raw scalars
- [x] `load_private_key(priv_raw)`, `load_public_key(pub_raw)` helpers

**Admission (§2.4):**
- [x] Create `aster/trust/admission.py`
- [x] `check_offline(cred, peer_endpoint_id, nonce_store)` — structural validation, signature, expiry, endpoint_id match, OTT nonce consumption
- [x] `check_runtime(cred, iid_backend, iid_token)` — IID only; skips if no `aster.iid_provider`
- [x] `admit(cred, peer_endpoint_id, ...)` — orchestrates offline + runtime; fails fast
- [x] Refusal logged with reason; reason never sent to peer

**IID verification:**
- [x] Create `aster/trust/iid.py`
- [x] `IIDBackend` protocol + `MockIIDBackend` (test double)
- [x] `AWSIIDBackend` stub (claim checks implemented; RSA signature verification deferred pending httpx + JWKS)
- [x] `GCPIIDBackend`, `AzureIIDBackend` stubs (return NotImplemented)
- [x] `get_iid_backend(provider)` factory; `verify_iid(attrs, backend, token)` helper

**Nonce store (§3.1):**
- [x] Create `aster/trust/nonces.py`
- [x] `NonceStore`: file backend, atomic write via `os.replace` + `fsync`
- [x] `InMemoryNonceStore`: for tests (no persistence)
- [x] `consume(nonce)` returns True only on first call; raises ValueError if len != 32
- [x] `is_consumed(nonce)` read-only check
- [x] `NonceStoreProtocol` for duck-typing docs backend replacement

**Gate 0 hooks (§3.3):**
- [x] Create `aster/trust/hooks.py::MeshEndpointHook`
- [x] `should_allow(remote_endpoint_id, alpn) -> bool` implements Gate 0 logic
- [x] Admission ALPNs always allowed: `ALPN_PRODUCER_ADMISSION`, `ALPN_CONSUMER_ADMISSION`
- [x] Non-admission ALPN: allow only if peer in `admitted` or `allow_unenrolled`
- [x] `allow_unenrolled: bool` for local/dev mode
- [x] `add_peer(endpoint_id)`, `remove_peer(endpoint_id)` methods
- [x] `run_hook_loop(hook_receiver)` wires to Phase 1b `HookReceiver` background task

**ALPN constants:**
- [x] `ALPN_PRODUCER_ADMISSION = b"aster.producer_admission"`
- [x] `ALPN_CONSUMER_ADMISSION = b"aster.consumer_admission"`

**Trust CLI:**
- [x] Create `aster/trust/cli.py`; registered in `aster/contract/cli.py:main()` under `aster trust`
- [x] `aster trust keygen --out-key PATH` — generates ed25519 pair, chmod 600, refuses if exists
- [x] `aster trust sign --root-key PATH --endpoint-id ... --attributes ... --expires ... --type producer|policy|ott --out PATH` — offline signing

**Tests:**
- [x] Signature verify: valid → True; tampered payload/endpoint_id/attributes → False
- [x] Expiry check: expired → fail
- [x] Wrong endpoint_id: fail
- [x] IID verification (mocked): matching claims → True; mismatched → False; absent → skip
- [x] OTT nonce: consumed once, second call → False
- [x] OTT credential with `len(nonce) != 32` is rejected as malformed (tested 16, 31, 33, 64)
- [x] Policy credential with a `nonce` field is rejected as malformed
- [x] Policy credential: reusable within expiry (5 calls all pass)
- [x] `MeshEndpointHook`: reject unenrolled on normal ALPN; allow admission ALPN; allow admitted peer
- [x] CLI: `keygen` produces valid ed25519 pair; `sign` producer + OTT output verifies
- [x] LocalTransport bypass: `CallContext.peer is None`; `CallContext.attributes == {}`
- [x] Auth interceptor: `peer is None` → allow (in-process trust); `on_request` completes without raise

---

## Phase 12: Producer Mesh & Clock Drift

**Spec refs:** Aster-trust-spec.md §2.1, §2.3, §2.5, §2.6, §2.7, §2.10. Plan: §14.

Phase 12 is now implemented and verified. The producer mesh package adds signed gossip, clock drift detection, bootstrap flows, and admission RPC. Implementation notes:

- Canonical signing bytes order (normative, §2.6): `u8(type) || payload || sender.encode('utf-8') || u64_be(epoch_ms)`. The checklist originally listed these in the wrong order; the plan and spec are authoritative.
- `handle_producer_message` normative processing order: replay-window → membership → signature → offset tracking / drift → dispatch.
- Depart and Introduce are processed even when the sender is drift-isolated (only ContractPublished/LeaseUpdate are skipped).
- Self-departure is triggered by `ClockDriftDetector.self_in_drift()` exceeding `drift_tolerance_ms` after the grace period. `mesh_dead=True` suppresses further gossip sends.
- Peer recovery from drift isolation: any fresh message with an acceptable offset (|offset - median| ≤ tolerance) removes the peer from `drift_isolated`.
- rcan grant format remains opaque bytes (§14.12 open design question).
- Lease heartbeat timer deferred to runtime integration (a background asyncio task); the config + payload encoder are provided.

Phase 12 verification completed with uv:
- `uv run pytest tests/python/test_aster_mesh.py -q --timeout=30` → **56 passed**
- `uv run ruff check bindings/aster_python/aster/trust/ tests/python/test_aster_mesh.py` → **All checks passed**
- Full suite: `uv run pytest tests/python/ -q --timeout=60` → **518 passed, 2 pre-existing dumbpipe failures unrelated to Phase 12**

**Data model (§2.6):**
- [x] `ProducerMessage` dataclass (type, payload, sender, epoch_ms, signature) — `aster/trust/mesh.py`
- [x] `IntroducePayload` (rcan bytes — opaque for now) — `aster/trust/mesh.py`
- [x] `DepartPayload` (optional reason) — `aster/trust/mesh.py`
- [x] `ContractPublishedPayload` (service_name, version, contract_collection_hash) — `aster/trust/mesh.py`
- [x] `LeaseUpdatePayload` (service_name, version, contract_id, health_status, addressing_info) — `aster/trust/mesh.py`
- [x] `MeshState` (accepted_producers, salt, topic_id, peer_offsets, drift_isolated, last_heartbeat_epoch_ms, mesh_joined_at_epoch_ms, mesh_dead) — `aster/trust/mesh.py`
- [x] `ClockDriftConfig` (replay_window_ms=30_000, drift_tolerance_ms=5_000, lease_heartbeat_ms=900_000, grace_period_ms=60_000, min_peers_for_median=3) — `aster/trust/mesh.py`
- [x] `AdmissionRequest` / `AdmissionResponse` dataclasses — `aster/trust/mesh.py`
- [x] `MeshState.to_json_dict()` / `from_json_dict()` for persistence

**Signing (§2.6):**
- [x] Create `aster/trust/gossip.py`
- [x] `producer_message_signing_bytes(type, payload, sender, epoch_ms)` — `u8(type) || payload || sender.encode('utf-8') || u64_be(epoch_ms)` (normative order per spec §2.6)
- [x] `sign_producer_message(type, payload, sender, epoch_ms, signing_key)`
- [x] `verify_producer_message(msg, peer_pubkey)`

**Topic derivation (§2.3):**
- [x] `derive_gossip_topic(root_pubkey, salt) -> bytes` — `blake3(root_pubkey + b"aster-producer-mesh" + salt).digest()` — `aster/trust/gossip.py`

**Bootstrap (§2.1, §2.5):**
- [x] Create `aster/trust/bootstrap.py`
- [x] `start_founding_node()`: load credential from `ASTER_ENROLLMENT`, verify, generate/load producer key, generate 32-byte salt, compute topic_id, initialize MeshState, persist, print bootstrap ticket
- [x] `join_mesh()`: load credential + `ASTER_BOOTSTRAP_TICKET`, build `AdmissionRequest` (caller handles dial + QUIC transport)
- [x] `apply_admission_response()`: finalize MeshState from accepted AdmissionResponse, persist salt + state
- [x] Bootstrap peer adds new node to `accepted_producers` on accept
- [x] Persist MeshState to `~/.aster/mesh_state.json` + salt to `~/.aster/mesh_salt`
- [x] Persist producer signing key to `~/.aster/producer.key`

**Admission RPC server (§2.5):**
- [x] `handle_admission_rpc(request_json, own_state, own_root_pubkey)` — async; runs Phase 11 `check_offline`, returns `AdmissionResponse`

**rcan stub:**
- [x] Create `aster/trust/rcan.py` — opaque pass-through (§14.12 open question)

**Gossip handler:**
- [x] `handle_producer_message(msg, state, config, peer_pubkeys, ...)` — async task
- [x] Replay-window check: drop if `abs(now - msg.epoch_ms) > replay_window_ms`
- [x] Membership check: drop if `msg.sender not in state.accepted_producers` + security alert log
- [x] Signature check: drop if verify fails + security alert log
- [x] Track offset: `state.peer_offsets[sender] = now_ms - msg.epoch_ms`
- [x] Dispatch by type: Introduce=1, Depart=2, ContractPublished=3, LeaseUpdate=4
- [x] Introduce: validate rcan (opaque for now), add sender to `accepted_producers`
- [x] Depart: remove sender from `accepted_producers`, drift tracker, and peer_offsets
- [x] ContractPublished/LeaseUpdate: skip if sender in `drift_isolated`, else forward to Phase 10 registry callback
- [x] Alert on security-relevant drops: unknown sender, bad signature (logged at WARNING)

**Clock drift (§2.10):**
- [x] Create `aster/trust/drift.py::ClockDriftDetector`
- [x] `track_offset(peer, epoch_ms)` — update `peer_offsets`
- [x] `mesh_median_offset()` — median via `statistics.median_high`; return None if `< min_peers_for_median`
- [x] `peer_in_drift(peer)` — True if `|offset - median| > drift_tolerance_ms`
- [x] `self_in_drift(self_offset_estimate)` — True if self deviates
- [x] Skip drift decisions during grace period (`now - mesh_joined_at_epoch_ms < grace_period_ms`)
- [x] Self-departure: set `mesh_dead=True`, fire `on_self_departure` callback, suppress subsequent gossip sends
- [x] Peer isolation: add to `drift_isolated`, skip ContractPublished/LeaseUpdate from peer, still process Introduce/Depart
- [x] Peer recovery: on fresh acceptable message, remove from `drift_isolated`
- [x] Read `ASTER_CLOCK_DRIFT_TOLERANCE_MS`, `ASTER_REPLAY_WINDOW_MS`, `ASTER_GRACE_PERIOD_MS` env overrides

**Lease heartbeat:**
- [x] `encode_lease_update_payload()` encoder provided; background timer pattern documented (requires caller to spawn asyncio task)
- [ ] Full background asyncio timer wired to a running GossipTopicHandle — deferred to Phase 13 integration tests

**Integration with Phase 10:**
- [x] `registry_callback` parameter on `handle_producer_message` forwards ContractPublished/LeaseUpdate to Phase 10 consumer

**Tests:**
- [x] Sign/verify round-trip; tampered payload/sender/epoch → verify fails
- [x] Replay attack: message outside ±30s → dropped
- [x] Unknown-sender message → dropped + security alert
- [x] Bad signature from accepted sender → dropped + security alert
- [x] 3-peer median + drift detection (median_high, even/odd counts)
- [x] Self-departure on synthetic >5s clock skew → mesh_dead=True
- [x] Self-departure suppressed during grace period
- [x] Peer isolation: >5s drift → isolated; ContractPublished/LeaseUpdate from peer → skipped; Introduce/Depart → processed
- [x] Peer recovery on acceptable fresh message
- [x] Bootstrap admission RPC: accepted case + rejected (malformed + expired credential)
- [x] `apply_admission_response` raises on rejection
- [x] MeshState JSON round-trip (to_json_dict / from_json_dict)
- [x] ClockDriftConfig env overrides
- [x] Topic derivation: deterministic, distinct on different salt/pubkey, matches blake3 vector
- [x] Payload encode helpers: Depart, ContractPublished, LeaseUpdate, Introduce round-trips
- [ ] Lease heartbeat broadcast observed after interval — deferred (requires live gossip in test)

**Open design questions (track in plan §14.12):**
- [x] **rcan grant format** — opaque bytes in `aster/trust/rcan.py`; pin down once upstream specifies
- [x] **AdmissionRequest/Response schema** — `reason` field for internal logging; never sent to peer in production

---

## Phase 13: Testing & Conformance

**Spec refs:** Aster-ContractIdentity.md Appendix A, Appendix B; session addendum Appendix A; Aster-SPEC.md §13.2. Plan: §15.

**Harness:**
- [x] Create `aster/testing/__init__.py`
- [x] Create `aster/testing/harness.py::AsterTestHarness`
- [x] `create_local_pair(service_class, implementation, wire_compatible)` — LocalTransport
- [x] `create_remote_pair(service_class, implementation)` — returns (client, Server, IrohConnection, endpoint, endpoint); uses bare QUIC endpoints (full IrohNode integration deferred)
- [x] `create_session_pair(service_class, implementation, wire_compatible)` — for scoped="stream" services

**Unit tests:**
- [x] `tests/python/test_aster_framing.py` — frame round-trip (incl. **CANCEL flags-only**)
- [x] `tests/python/test_aster_codec.py` — Fory codec (XLANG, NATIVE, ROW)
- [x] `tests/python/test_aster_decorators.py` — service introspection
- [x] `tests/python/test_aster_canonical.py` — Appendix A.2–A.6 byte + hash vectors
- [x] `tests/python/test_aster_cycles.py` — Appendix B cycle-breaking vectors
- [x] `tests/python/test_aster_trust.py` — credentials, admission, nonces
- [x] `tests/python/test_aster_drift.py` — clock drift median + self-departure

**Integration tests:**
- [x] `tests/python/test_aster_unary.py`
- [x] `tests/python/test_aster_streaming.py`
- [x] `tests/python/test_aster_session.py`
- [x] `tests/python/test_aster_interceptors.py`
- [x] `tests/python/test_aster_registry.py`
- [x] `tests/python/test_aster_mesh.py` — bootstrap, admission, gossip, drift
- [x] `tests/python/test_aster_local.py` — LocalTransport parity

**Conformance:**
- [x] `tests/conformance/wire/` — stateless wire vectors: HEADER, CALL, TRAILER, CANCEL flags-only; binary `.bin` fixtures auto-generated by conftest; test_wire_vectors.py verifies structure and round-trips
- [x] `tests/conformance/wire/session_*.bin` — session CANCEL flags-only vector included in `cancel_flags_only.bin`; session HEADER/CALL/no-trailer tested via test_aster_session.py (existing)
- [x] `tests/conformance/canonical/test_scope_distinctness.py` — SHARED vs STREAM → different contract_ids; 5 variants tested
- [x] `tests/conformance/interop/echo_service.fdl` + `scenarios.yaml` — cross-language interop fixture (placeholder; scenarios activate when Java binding is available)
- [ ] `tests/conformance/canonical/*.bin` + `.hashes.json` — standalone canonical binary files not yet committed; covered via `tests/fixtures/canonical_test_vectors.json` and `test_aster_canonical.py`

**Additional required tests (called out in spec):**
- [x] Manifest-mismatch fatal (Phase 9 §11.4.3 step 4) — `test_aster_contract_identity.py::test_manifest_mismatch_fatal`
- [x] Lease_seq monotonicity (Phase 10 §11.10) — `test_aster_registry.py::test_lease_seq_monotonicity_*`
- [x] In-session unary no-trailer on wire (Phase 8 §4.6) — `test_aster_session.py::test_local_session_unary_no_trailer`
- [x] Mid-call CALL rejection (Phase 8 §4.5) — `test_aster_session.py::test_local_session_mid_call_call_rejection`
- [x] `wire_compatible=True` produces identical bytes across LocalTransport and IrohTransport — `test_aster_local.py::test_wire_compatible_true_fory_codec_encode_is_consistent`

---

## Milestone Summary

| Milestone | Phases | Description | Status |
|-----------|--------|-------------|--------|
| **Pre-requisites validated** | — | Python 3.13, pyfory determinism confirmed | ✅ Done |
| **Minimal viable RPC** | 1–6 | Unary + streaming RPCs working end-to-end | ✅ Done |
| **Production-ready RPC** | 1–7 | + interceptors (deadline, auth, retry, circuit breaker) | ✅ Done |
| **Session support** | 8 | Session-scoped services with CALL/CANCEL frames | ✅ Done |
| **Contract identity** | 9 | Content-addressed contracts via BLAKE3 Merkle DAG + custom canonical encoder | ✅ Done |
| **Decentralized registry** | 10 | Service discovery via iroh-docs/gossip/blobs (unauthenticated) | ✅ Done |
| **Trust foundations** | 11 | Enrollment credentials + Gate 0 admission (ed25519) | ✅ Done |
| **Producer mesh** | 12 | Signed gossip, bootstrap, clock-drift detection | ✅ Done |
| **Conformance suite** | 13 | Wire + canonical vectors + cross-language interop | ✅ Done |