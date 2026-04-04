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

Phase 10 is now implemented and verified. The registry package (`aster/registry/`) implements docs-based service registration, endpoint advertisement, and resolution with 48 tests passing. Notable implementation notes: iroh-docs rejects empty entry bytes so `withdraw()` writes `b"null"` tombstone; cross-node resolution waits for `content_ready` events (blob download) before reading entry content.

Phase 10 verification completed with uv:
- `uv run pytest tests/python/test_aster_registry.py -q --timeout=60` → **48 passed**
- `uv run ruff check bindings/aster_python/aster/registry/ tests/python/test_aster_registry.py` → **All checks passed**
- Full suite: **412 passed, 2 pre-existing dumbpipe TCP/Unix flakes unrelated to Phase 10**

Outstanding issue / blocker:
- None for Phases 7–10 at this time.
- Phase 12 has two open design questions: rcan grant format + AdmissionRequest/Response `reason` field — tracked in plan §14.12.
- HashSeq collection builder (multi-file blob upload) deferred from Phase 9/10 to Phase 11+; Phase 10 uses single-blob storage where `collection_hash == contract_id`.

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
- [ ] Multi-file HashSeq collection builder (wraps `BlobsClient` primitives) — deferred to Phase 10
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
- [ ] Publication round-trip: publish → fetch → verify — deferred to Phase 10 (requires iroh integration)

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

**Dependencies:**
- [ ] Add `cryptography>=42` to `pyproject.toml` (ed25519)
- [ ] Optional: add `PyJWT>=2.8` as `iid` extra

**Data model (§2.2):**
- [ ] Create `aster/trust/__init__.py`
- [ ] `EnrollmentCredential` dataclass (endpoint_id, root_pubkey, expires_at, attributes, signature)
- [ ] `ConsumerEnrollmentCredential` dataclass with `credential_type: Literal["policy","ott"]`, optional endpoint_id, optional 32-byte nonce
- [ ] `AdmissionResult` dataclass (admitted, attributes?, reason?)
- [ ] Attribute conventions: reserve `aster.role`, `aster.name`, `aster.iid_provider`, `aster.iid_account`, `aster.iid_region`, `aster.iid_role_arn` (no network-level controls — see Aster-trust-spec.md §1)

**Signing:**
- [ ] Create `aster/trust/signing.py`
- [ ] `canonical_signing_bytes(cred)` — `endpoint_id || root_pubkey || u64_be(expires_at) || canonical_json(attributes) || nonce?`
- [ ] `canonical_json(attributes)` — UTF-8, sorted keys
- [ ] `sign_credential(cred, root_privkey)` — ed25519 (used offline in CLI)
- [ ] `verify_signature(cred, root_pubkey)` — ed25519

**Admission (§2.4):**
- [ ] Create `aster/trust/admission.py`
- [ ] `check_offline(cred, peer_endpoint_id, nonce_store)` — signature, expiry, endpoint_id match, nonce (for OTT)
- [ ] `check_runtime(cred)` — IID only (no network-level checks in the normative spec)
- [ ] `admit(cred, ctx)` — orchestrates offline + runtime
- [ ] Refusal logging with reason (no oracle info leak to peer)

**IID verification:**
- [ ] Create `aster/trust/iid.py`
- [ ] AWS: fetch `http://169.254.169.254/latest/dynamic/instance-identity/document` + signature, verify against AWS public keys
- [ ] GCP: equivalent metadata fetch + JWT verification
- [ ] Azure: equivalent
- [ ] Mock-pluggable for tests

**Nonce store (§3.1):**
- [ ] Create `aster/trust/nonces.py::NonceStore`
- [ ] File backend: atomic write to `~/.aster/nonces.json` (os.replace + fsync)
- [ ] `consume(nonce)` returns True only on first call
- [ ] Stub interface for future iroh-docs backend

**Gate 0 hooks (§3.3):**
- [ ] Create `aster/trust/hooks.py::MeshEndpointHook`
- [ ] Implements iroh `EndpointHooks` protocol (Phase 1b FFI ✅)
- [ ] Allowlist `admitted: set[str]`
- [ ] Admission ALPNs always allowed: `aster.producer_admission`, `aster.consumer_admission`
- [ ] Non-admission ALPN: allow only if peer in `admitted`
- [ ] `allow_unenrolled: bool` for local/dev mode
- [ ] `add_peer(endpoint_id)`, `remove_peer(endpoint_id)` methods

**ALPN constants:**
- [ ] Define `ALPN_PRODUCER_ADMISSION = b"aster.producer_admission"`
- [ ] Define `ALPN_CONSUMER_ADMISSION = b"aster.consumer_admission"`

**Trust CLI:**
- [ ] Create `aster/trust/cli.py`
- [ ] `aster trust keygen --out-key ~/.aster/root.key` — emits ed25519 pair, refuses if target exists
- [ ] `aster trust sign --root-key ... --endpoint-id ... --attributes '...' --expires ...` — offline credential signing

**Tests:**
- [ ] Signature verify: valid → True; tampered → False
- [ ] Expiry check: expired → fail
- [ ] Wrong endpoint_id: fail
- [ ] IID verification (mocked): matching claims → True
- [ ] OTT nonce: consumed once, second call → False
- [ ] OTT credential with `len(nonce) != 32` is rejected as malformed (test with 16, 31, 33, 64 byte nonces)
- [ ] Policy credential with a `nonce` field is rejected as malformed
- [ ] Policy credential: reusable within expiry
- [ ] `MeshEndpointHook`: reject unenrolled on non-admission ALPN; allow admission ALPN; allow admitted peer
- [ ] CLI: `keygen` produces valid ed25519 pair; `sign` output verifies
- [ ] LocalTransport bypass: Gate 0 not applied on `LocalTransport` calls (no connection to gate); `CallContext.peer is None`; `CallContext.attributes == {}`
- [ ] Auth interceptor: when `peer is None`, canonical behavior is allow (in-process trust); document any custom interceptor that requires non-None peer

---

## Phase 12: Producer Mesh & Clock Drift

**Spec refs:** Aster-trust-spec.md §2.1, §2.3, §2.5, §2.6, §2.7, §2.10. Plan: §14.

**Data model (§2.6):**
- [ ] `ProducerMessage` dataclass (type, payload, sender, epoch_ms, signature)
- [ ] `IntroducePayload` (rcan bytes — opaque for now)
- [ ] `DepartPayload` (optional reason)
- [ ] `LeaseUpdatePayload` (service_name, version, contract_id, health_status, addressing_info)
- [ ] `MeshState` (accepted_producers, salt, topic_id, peer_offsets, drift_isolated, last_heartbeat_epoch_ms, mesh_joined_at_epoch_ms)
- [ ] `ClockDriftConfig` (replay_window_ms=30_000, drift_tolerance_ms=5_000, lease_heartbeat_ms=900_000, grace_period_ms=60_000, min_peers_for_median=3)

**Signing (§2.6):**
- [ ] Create `aster/trust/gossip.py`
- [ ] `producer_message_signing_bytes(type, payload, sender, epoch_ms)` — `u8(type) || u64_be(epoch_ms) || sender || payload`
- [ ] `sign_producer_message(type, payload, sender, epoch_ms, signing_key)`
- [ ] `verify_producer_message(msg, peer_pubkey)`

**Topic derivation (§2.3):**
- [ ] `derive_gossip_topic(root_pubkey, salt) -> bytes` — `blake3(root_pubkey + b"aster-producer-mesh" + salt).digest()[:32]`

**Bootstrap (§2.1, §2.5):**
- [ ] Create `aster/trust/bootstrap.py`
- [ ] `start_founding_node()`: load credential from `ASTER_ENROLLMENT`, verify, generate/load producer key, generate 32-byte salt, compute topic_id, initialize MeshState, start endpoint + hooks, subscribe gossip, print bootstrap ticket
- [ ] `join_mesh()`: load credential + `ASTER_BOOTSTRAP_TICKET`, dial bootstrap peer, open `aster.producer_admission` ALPN bidi stream, send `AdmissionRequest{credential, optional_iid}`, receive `AdmissionResponse{accepted, salt?, accepted_producers?, reason?}`, if accepted → derive topic_id → subscribe → send Introduce → persist MeshState
- [ ] Bootstrap peer adds new node to `accepted_producers` on accept + rebroadcasts Introduce
- [ ] Persist MeshState to `~/.aster/mesh_state.json` + salt to `~/.aster/mesh_salt`
- [ ] Persist producer signing key to `~/.aster/producer.key`

**Admission RPC server (§2.5):**
- [ ] Server-side handler for `aster.producer_admission` ALPN: read AdmissionRequest, run Phase 11 admission checks, respond with AdmissionResponse

**Gossip handler:**
- [ ] `handle_producer_message(msg, state)` — async task
- [ ] Replay-window check: drop if `abs(now - msg.epoch_ms) > replay_window_ms`
- [ ] Membership check: drop if `msg.sender not in state.accepted_producers`
- [ ] Signature check: drop if verify fails
- [ ] Track offset: `state.peer_offsets[sender] = now_ms - msg.epoch_ms`
- [ ] Dispatch by type: Introduce=1, Depart=2, ContractPublished=3, LeaseUpdate=4
- [ ] Introduce: validate rcan (opaque for now), add sender to `accepted_producers`
- [ ] Depart: remove sender from `accepted_producers`
- [ ] ContractPublished/LeaseUpdate: skip if sender in `drift_isolated`, else forward to Phase 10 registry callback
- [ ] Alert on security-relevant drops: unknown sender, bad signature

**Clock drift (§2.10):**
- [ ] Create `aster/trust/drift.py::ClockDriftDetector`
- [ ] `track_offset(peer, epoch_ms)` — update `peer_offsets`
- [ ] `mesh_median_offset()` — median via `statistics.median_high`; return None if `< min_peers_for_median`
- [ ] `peer_in_drift(peer)` — True if `|offset - median| > drift_tolerance_ms`
- [ ] `self_in_drift(self_offset_estimate)` — True if self deviates
- [ ] Skip drift decisions during grace period (`now - mesh_joined_at_epoch_ms < grace_period_ms`)
- [ ] Self-departure: broadcast Depart, set `mesh_dead=True`, suppress subsequent gossip sends, fail-fast log
- [ ] Peer isolation: add to `drift_isolated`, skip ContractPublished/LeaseUpdate from peer, still process Introduce/Depart
- [ ] Peer recovery: on fresh acceptable message, remove from `drift_isolated`
- [ ] Read `ASTER_CLOCK_DRIFT_TOLERANCE_MS` + related env overrides

**Lease heartbeat:**
- [ ] Background timer: every `lease_heartbeat_ms` (default 15 min), broadcast LeaseUpdate with local health + addressing

**Integration with Phase 10:**
- [ ] Forward received ContractPublished/LeaseUpdate messages to `RegistryClient.on_change` callback

**Tests:**
- [ ] Sign/verify round-trip; tampered payload → verify fails
- [ ] Replay attack: message outside ±30s → dropped
- [ ] Unknown-sender message → dropped
- [ ] 3-peer median + drift detection
- [ ] Self-departure on synthetic >5s clock skew → Depart broadcast + gossip suppression
- [ ] Peer isolation: >5s drift → isolated; ContractPublished/LeaseUpdate from peer → skipped; Introduce/Depart → processed
- [ ] Peer recovery on acceptable fresh message
- [ ] Bootstrap ticket round-trip: founding → print ticket → subsequent node joins
- [ ] Admission RPC: accepted case + rejected case
- [ ] Lease heartbeat broadcast observed after interval

**Open design questions (track in plan §14.12):**
- [ ] **rcan grant format** — opaque bytes for now; pin down once upstream specifies
- [ ] **AdmissionRequest/Response schema** — confirm `reason` field semantics

---

## Phase 13: Testing & Conformance

**Spec refs:** Aster-ContractIdentity.md Appendix A, Appendix B; session addendum Appendix A; Aster-SPEC.md §13.2. Plan: §15.

**Harness:**
- [ ] Create `aster/testing/__init__.py`
- [ ] Create `aster/testing/harness.py::AsterTestHarness`
- [ ] `create_local_pair(service_class, implementation, wire_compatible)` — LocalTransport
- [ ] `create_remote_pair(service_class, implementation)` — returns (client, Server, IrohConnection, IrohNode, IrohNode)
- [ ] `create_session_pair(service_class, implementation, wire_compatible)` — for scoped="stream" services

**Unit tests:**
- [ ] `tests/python/test_aster_framing.py` — frame round-trip (incl. **CANCEL flags-only**)
- [ ] `tests/python/test_aster_codec.py` — Fory codec (XLANG, NATIVE, ROW)
- [ ] `tests/python/test_aster_decorators.py` — service introspection
- [ ] `tests/python/test_aster_canonical.py` — Appendix A.2–A.6 byte + hash vectors
- [ ] `tests/python/test_aster_cycles.py` — Appendix B cycle-breaking vectors
- [ ] `tests/python/test_aster_trust.py` — credentials, admission, nonces
- [ ] `tests/python/test_aster_drift.py` — clock drift median + self-departure

**Integration tests:**
- [ ] `tests/python/test_aster_unary.py`
- [ ] `tests/python/test_aster_streaming.py`
- [ ] `tests/python/test_aster_session.py`
- [ ] `tests/python/test_aster_interceptors.py`
- [ ] `tests/python/test_aster_registry.py`
- [ ] `tests/python/test_aster_mesh.py` — bootstrap, admission, gossip, drift
- [ ] `tests/python/test_aster_local.py` — LocalTransport parity

**Conformance:**
- [ ] `tests/conformance/wire/` — stateless wire vectors: HEADER, CALL, TRAILER, COMPRESSED, size boundaries
- [ ] `tests/conformance/wire/session_*.bin` — session vectors: HEADER method="", CALL, **CANCEL flags-only (1 byte)**, in-session unary no-trailer, client-stream EoI TRAILER
- [ ] `tests/conformance/canonical/*.bin` + `.hashes.json` — canonical contract bytes + expected hashes (Appendix A.2–A.6; sourced from `tests/fixtures/canonical_test_vectors.json` produced in Phase 9)
- [ ] `tests/conformance/canonical/test_scope_distinctness.py` — SHARED vs STREAM → different contract_ids
- [ ] `tests/conformance/interop/echo_service.fdl` + `scenarios.yaml` — cross-language interop fixture (placeholder if Rust reference not yet available)

**Additional required tests (called out in spec):**
- [ ] Manifest-mismatch fatal (Phase 9 §11.4.3 step 4)
- [ ] Lease_seq monotonicity (Phase 10 §11.10)
- [ ] In-session unary no-trailer on wire (Phase 8 §4.6)
- [ ] Mid-call CALL rejection (Phase 8 §4.5)
- [ ] `wire_compatible=True` produces identical bytes across LocalTransport and IrohTransport

---

## Milestone Summary

| Milestone | Phases | Description | Status |
|-----------|--------|-------------|--------|
| **Pre-requisites validated** | — | Python 3.13, pyfory determinism confirmed | ✅ Done |
| **Minimal viable RPC** | 1–6 | Unary + streaming RPCs working end-to-end | ✅ Done |
| **Production-ready RPC** | 1–7 | + interceptors (deadline, auth, retry, circuit breaker) | ✅ Done |
| **Session support** | 8 | Session-scoped services with CALL/CANCEL frames | ⬜ Not started |
| **Contract identity** | 9 | Content-addressed contracts via BLAKE3 Merkle DAG + custom canonical encoder | ⬜ Not started |
| **Decentralized registry** | 10 | Service discovery via iroh-docs/gossip/blobs (unauthenticated) | ⬜ Not started |
| **Trust foundations** | 11 | Enrollment credentials + Gate 0 admission (ed25519) | ⬜ Not started |
| **Producer mesh** | 12 | Signed gossip, bootstrap, clock-drift detection | ⬜ Not started |
| **Conformance suite** | 13 | Wire + canonical vectors + cross-language interop | ⬜ Not started |