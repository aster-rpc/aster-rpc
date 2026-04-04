# Aster Python Implementation — Progress Checklist

## INSTRUCTIONS

Main plan: [ASTER_PLAN.md](ASTER_PLAN.md) - please read first.

Please progress the tasks in this document one phase at a time and one step at a time. Please keep the `STATUS` section updated with your current status and list any outstanding issues or blockers.

For each step we need to make sure the code passes tests and linting.

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

Plan & checklist rewrite (2026-04-04): Phases 8–13 were re-aligned against the spec corpus (Aster-SPEC.md, Aster-session-scoped-services.md, Aster-ContractIdentity.md, Aster-trust-spec.md). Two new trust phases were added between the registry and conformance phases (Phase 11: Trust Foundations, Phase 12: Producer Mesh), and the old Phase 11 (Testing & Conformance) was renumbered to Phase 13. The rewritten checklist captures normative details previously missing: CANCEL flags-only frame, in-session unary no-trailer semantics, custom canonical encoder (not a pyfory wrapper), SCC-based cycle breaking, full EndpointLease fields + lease_seq monotonicity, 4-state health machine, all 6 gossip event types, ed25519 enrollment credentials, signed producer-gossip envelope, clock drift + self-departure.

Phase 1 bug fixed as prerequisite for Phase 8:
- [x] `write_frame` now permits empty payload when `flags & CANCEL` (spec §5.2) — `aster/framing.py`
- [x] Added test `test_cancel_empty_payload` to `tests/python/test_aster_framing.py`

Outstanding issue / blocker:
- None for Phase 7 at this time.
- **Phase 9 is NOT blocked.** Python is the reference implementation; Phase 9 PRODUCES the first canonical byte vectors (Appendix A + rule-level micro-fixtures per §11.3.2). Vectors commit to `tests/fixtures/canonical_test_vectors.json` AND land in `Aster-ContractIdentity.md` Appendix A as "Python-reference v1". Cross-verification arrives when the Java binding is written. See `ASTER_SPEC_ISSUES.md` §B3 for the bootstrap protocol.
- Phase 12 has two open design questions: rcan grant format + AdmissionRequest/Response `reason` field — tracked in plan §14.12.

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
- [ ] Extend `@service` decorator to accept `scoped: Literal["shared", "stream"]` (default `"shared"`)
- [ ] Propagate `scoped` into `ServiceInfo` (Phase 4) so Phase 9 can read it
- [ ] Validate `service_class.__init__` accepts `peer` parameter when `scoped="stream"`

**Wire protocol:**
- [ ] Implement `CallHeader` read helper `read_call_header(recv) -> CallHeader` (Phase 1 dataclass exists)
- [ ] Validate stream discriminator: `StreamHeader.method==""` ↔ service's `scoped=="stream"`; reject mismatches with `FAILED_PRECONDITION` (§4.1)
- [ ] Reject per-call `serialization_mode` override on session streams (§9.1, `INVALID_ARGUMENT`)

**Server-side:**
- [ ] Implement `SessionServer.run()` loop: instantiate `service_class(peer=verified_endpoint_id)`, loop on CALL frames (§7.2)
- [ ] Populate `CallContext.session_id` from `StreamHeader.call_id` (stable for stream lifetime, §8.2)
- [ ] Populate `CallContext.peer` from verified remote EndpointId (§7.1)
- [ ] In-session **unary** dispatch: success → response payload only, **no trailer**; error → trailer with non-OK status + no response (§4.6)
- [ ] In-session server-stream dispatch: response frames + TRAILER(status=OK) at end
- [ ] In-session client-stream dispatch: read frames until TRAILER(status=OK) EoI, call handler, write response (§4.5 rule 3)
- [ ] In-session bidi dispatch: concurrent read/write; server's response TRAILER signals call complete (§4.5)
- [ ] Mid-call CALL rejection: if a new CALL arrives while handler is running → trailer `FAILED_PRECONDITION` + stream reset (§4.5 rule 5)
- [ ] CANCEL frame handler: separate reader task; on CANCEL → cancel handler task → write trailer `CANCELLED`
- [ ] `on_session_close()` lifecycle hook: fires on (a) clean close, (b) stream error, (c) stream reset, (d) server shutdown, (e) connection loss
- [ ] CANCEL on non-session stream: ignored (may log) (§5.6)

**Client-side:**
- [ ] Implement `create_session()` returning session stub with internal `asyncio.Lock`
- [ ] Each generated stub method acquires lock for entire request/response cycle
- [ ] Client-side unary call: acquire lock → write CALL + request → read response payload → release lock (no trailer on success)
- [ ] Client-side server-stream: write CALL + request → read until TRAILER → release lock
- [ ] Client-side client-stream: write CALL + request frames → write TRAILER(status=OK) EoI → read response → release lock
- [ ] Client-side bidi: write CALL + concurrent read/write → wait for server TRAILER → release lock
- [ ] Client cancellation (`break` from iterator or task.cancel): send CANCEL flags-only frame → drain response frames until trailer (expect status=CANCELLED) → release lock (§5.5)
- [ ] `session.close()` → `send_stream.finish()` (does NOT send CANCEL or TRAILER)
- [ ] Retry interceptor semantics in session: retry idempotent calls on same stream; stream reset → abort session, no retry (§9.4)

**LocalTransport:**
- [ ] Implement LocalTransport session support (asyncio.Queue pair per session, preserve interceptor chain + lifecycle)

**Tests:**
- [ ] Multi-call session with state persistence
- [ ] CANCEL mid-unary + mid-stream, drain-until-trailer semantics
- [ ] Client close → `on_session_close` fires
- [ ] Stream reset → `on_session_close` fires
- [ ] Connection drop → `on_session_close` fires
- [ ] Server shutdown → `on_session_close` fires
- [ ] Sequential lock enforcement (concurrent method calls serialized)
- [ ] Mid-call CALL rejection → FAILED_PRECONDITION + stream reset
- [ ] In-session unary success = no trailer frame present on wire
- [ ] In-session unary error = trailer only, no response payload
- [ ] Client-stream EoI: explicit TRAILER frame on wire, not `finish()`
- [ ] Per-call serialization override rejection (INVALID_ARGUMENT)
- [ ] LocalTransport session parity with IrohTransport

---

## Phase 9: Contract Identity & Publication

**Spec refs:** Aster-ContractIdentity.md §11.2, §11.3 (normative), §11.4, §11.5, Appendix A, Appendix B, session addendum Appendix A. Plan: §11.

**Discriminator enums (§11.3.3 — fixed IDs, normative):**
- [ ] `TypeKind` IntEnum: PRIMITIVE=1, CONTAINER=2, TYPE_REF=3, SELF_REF=4, INLINE=5
- [ ] `ContainerKind` IntEnum: LIST=1, MAP=2, SET=3
- [ ] `TypeDefKind` IntEnum: STRUCT=1, ENUM=2, UNION=3
- [ ] `MethodPattern` IntEnum: UNARY=1, SERVER_STREAM=2, CLIENT_STREAM=3, BIDI_STREAM=4
- [ ] `CapabilityKind` IntEnum: NONE=0, REQUIRED=1
- [ ] `ScopeKind` IntEnum: SHARED=0, STREAM=1

**Canonical encoder (§11.3.2 — custom code, NOT fory.serialize wrapper):**
- [ ] Create `aster/contract/canonical.py` with byte-level writers:
  - [ ] `write_varint(w, value)` (unsigned LEB128)
  - [ ] `write_zigzag_i32(w, value)` (VARINT32, NOT fixed-width)
  - [ ] `write_zigzag_i64(w, value)` (VARINT64, NOT fixed-width)
  - [ ] `write_string(w, s)` (`<len_varint><utf8>`)
  - [ ] `write_bytes(w, b)` (`<len_varint><raw>`)
  - [ ] `write_bool(w, v)` (0x00/0x01)
  - [ ] `write_list_header(w, n)` (`0x0C` then `<len_varint>`)
  - [ ] `write_null_flag(w)` (`0xFD` per Appendix A.2)
  - [ ] `write_present_flag(w)` (`0x00`)
- [ ] Implement `normalize_identifier(s) -> str`: asserts `s.isidentifier()` (UAX #31), returns `unicodedata.normalize("NFC", s)`. Called on all identifier strings (method names, type names, package names, enum/union member names, role names) before encoding.
- [ ] Implement mixed-script warning helper: detect Latin/Cyrillic/Greek script mixing in identifiers; emit `warnings.warn` at registration time (non-fatal).
- [ ] No outer Fory header, no ref meta, no root type meta, no schema hash prefix

**Type dataclasses (§11.3.3):**
- [ ] Create `aster/contract/identity.py`
- [ ] `FieldDef` (id, name, type_kind, primitive_tag, container, element_type_hash, key_type_hash, optional)
- [ ] `EnumValueDef` (name, value)
- [ ] `UnionVariantDef` (id, name, type_hash)
- [ ] `TypeDef` (name, kind, fields, enum_values, union_variants)
- [ ] `CapabilityRequirement` (kind, roles)
- [ ] `MethodDef` (name, pattern, request_type_hash, response_type_hash, idempotent, requires?)
- [ ] `ServiceContract` (name, version, methods, serialization_modes, alpn, **scoped**, requires?)
- [ ] All types decorated with `@fory_tag("_aster/...")` for internal registration

**Canonical writers (per-type):**
- [ ] `write_field_def(w, f)` with zero-value conventions for unused companion fields
- [ ] `write_enum_value_def(w, ev)`
- [ ] `write_union_variant_def(w, uv)`
- [ ] `write_type_def(w, t)` — sorts fields by id, enum_values by value, union_variants by id
- [ ] `write_capability_requirement(w, cr)` — roles NFC-normalized + sorted by Unicode codepoint
- [ ] `write_method_def(w, m)` — handles optional `requires` with NULL_FLAG
- [ ] `write_service_contract(w, c)` — NFC-normalizes method names, sorts methods by Unicode codepoint, handles optional `requires`

**Type graph + cycle breaking (§11.3.4 + Appendix B):**
- [ ] `resolve_type_graph(service_class) -> dict[str, TypeDef]` via `typing.get_type_hints` + `inspect`
- [ ] Implement Tarjan's SCC algorithm over type-reference graph
- [ ] For each SCC size ≥ 2: codepoint-ordered spanning tree rooted at NFC-codepoint-smallest fully-qualified name
- [ ] Back-edges within SCC → encoded as SELF_REF with DFS position
- [ ] Bottom-up hashing over condensation DAG
- [ ] Validate against Appendix B fixtures: direct self-recursion, 2-type mutual, 3-cycle, diamond+back-edge

**Hashing:**
- [ ] `compute_type_hash(type_def) -> bytes` (32-byte BLAKE3 digest)
- [ ] `compute_contract_id(contract) -> str` (48-char hex)
- [ ] `ServiceContract` construction from `ServiceInfo` — propagate `scoped` from `@service(scoped=...)`

**Manifest (§11.4.4):**
- [ ] Create `aster/contract/manifest.py`
- [ ] `ContractManifest` with: identity fields, type_hashes list, **scoped** string, provenance fields (semver, vcs_revision, vcs_tag, vcs_url, changelog), runtime fields (published_by, published_at_epoch_ms)
- [ ] `verify_manifest_or_fatal(live_contract, manifest_path)` — raises `FatalContractMismatch` on mismatch with spec-matching diagnostic (expected, actual, service_name, version, remediation "rerun `aster contract gen`")

**Offline CLI (§11.4.2):**
- [ ] Create `aster/contract/cli.py` with `aster contract gen --service MODULE:CLASS --out .aster/manifest.json`
- [ ] CLI imports service class, computes contract + all type hashes, writes manifest.json (no network, no credentials)
- [ ] Register as console script in `pyproject.toml`: `aster = "aster.cli:main"`

**Publication (§11.4.3 — normative ordering):**
- [ ] Create `aster/contract/publication.py::publish_contract()`
- [ ] Build HashSeq collection: [0]=manifest.json, [1]=contract.xlang, [2..N]=types/{hex(hash)}.xlang
- [ ] Multi-file HashSeq collection builder (wraps `BlobsClient` primitives)
- [ ] Set GC-protection tag `aster/contract/{friendly}@{contract_id}` via `BlobsClient.tag_set` (Phase 1c.1 ✅)
- [ ] Startup verification call **before** any writes (fatal on mismatch)
- [ ] Write order: (1) ArtifactRef at `_aster/contracts/{contract_id}`, (2) version pointer `_aster/services/{name}/versions/v{version}`, (3) optional tag/channel aliases, (4) gossip `CONTRACT_PUBLISHED`, (5) endpoint leases LAST (Phase 10)

**Fetch / verification (§11.4.4):**
- [ ] `fetch_contract(blobs, ref)` delegates download to collection_hash, uses `blob_local_info` for cache hit, `blob_observe_complete` to wait for download (Phase 1d ✅)
- [ ] Verify `blake3(contract.xlang bytes) == contract_id`
- [ ] Parse canonical bytes back into `ServiceContract` instance
- [ ] Optional: verify each TypeDef hash matches manifest's type_hashes

**Golden vector generation (Python IS the reference):**
- [ ] Write `tools/gen_canonical_vectors.py` — constructs fixtures, runs encoder, emits `tests/fixtures/canonical_test_vectors.json` (hex bytes + BLAKE3 hex hashes)
- [ ] Produce Appendix A composite vectors: A.2 (empty ServiceContract), A.3 (enum TypeDef), A.4 (TypeDef with TYPE_REF field), A.5 (MethodDef without requires), A.6 (MethodDef with requires)
- [ ] Produce Appendix B cycle-breaking vectors: direct self-recursion, 2-type mutual, 3-cycle, diamond+back-edge
- [ ] Produce rule-level micro-fixtures (one per §11.3.2 rule — see next block)
- [ ] Commit vectors to `tests/fixtures/canonical_test_vectors.json`
- [ ] Copy vectors into `Aster-ContractIdentity.md` Appendix A as "Python-reference v1, pending cross-verification (Java binding)"

**Rule-level micro-fixtures (risk mitigation against locked-in bugs):**
- [ ] ZigZag VARINT32 edges: values `0, 1, -1, INT32_MAX, INT32_MIN` — pin each byte
- [ ] ZigZag VARINT64 edges: same five values for int64
- [ ] Varint boundaries: `0x7F` (1 byte), `0x80` (2 bytes), `0x3FFF` (2 bytes), `0x4000` (3 bytes)
- [ ] String encoding: empty string `""` vs absent string (NULL_FLAG)
- [ ] Bytes encoding: empty bytes `b""` vs absent bytes
- [ ] List encoding: empty list (header + length 0) vs absent list
- [ ] `CapabilityRequirement` as `None` → NULL_FLAG byte + position locked
- [ ] `CapabilityRequirement` present → presence flag + nested bytes locked
- [ ] Zero-value conventions per TypeKind: PRIMITIVE, REF, SELF_REF, ANY (§11.3.3)
- [ ] `container != MAP` → container_key_* fields zero-valued
- [ ] Codepoint sort stability: two methods named `foo_bar` and `foo_baz` produce deterministic order
- [ ] NFC normalization: method name `café` (NFC: 4 codepoints) and `café` (NFD: 5 codepoints, e + combining acute) normalize to identical canonical bytes → identical contract_id
- [ ] Unicode identifier: Japanese method name `注文する` (`str.isidentifier()` → True) accepted; sorts deterministically by codepoint
- [ ] Scope distinctness: identical ServiceContract except `scoped` → different `contract_id`

**Tests (assertions over committed vectors):**
- [ ] Hash stability (same input → same hash across runs)
- [ ] Byte-equality + hash-equality against Appendix A.2–A.6 committed vectors
- [ ] Byte-equality + hash-equality against Appendix B cycle-breaking vectors
- [ ] All rule-level micro-fixtures pass byte-equality
- [ ] int32/int64 encoded as ZigZag VARINT (not fixed-width) — asserted via micro-fixture
- [ ] NULL_FLAG encoding for absent optional fields — asserted via micro-fixture
- [ ] Changing any type in graph → changes contract_id
- [ ] Manifest mismatch → fatal error with full diagnostic
- [ ] `aster contract gen` CLI produces committable manifest.json offline (no network access)
- [ ] Publication round-trip: publish → fetch → verify

---

## Phase 10: Service Registry & Discovery

**Spec refs:** Aster-SPEC.md §11.2, §11.2.1, §11.2.3, §11.5, §11.6, §11.7, §11.8, §11.9, §11.10. Plan: §12.

**Scope:** Docs-based registry only. Trust (Phase 11) and producer mesh (Phase 12) are separate.

**Data model:**
- [ ] Create `aster/registry/__init__.py`
- [ ] `ArtifactRef` dataclass (§11.2.1): contract_id, collection_hash, optional provider_endpoint_id/relay_url/ticket
- [ ] `EndpointLease` dataclass (§11.6) — **all fields**: service_name, version, contract_id, endpoint_id, **lease_seq**, alpn, feature_flags, relay_url, direct_addrs, load, language_runtime, aster_version, policy_realm, **health_status**, tags, updated_at_epoch_ms
- [ ] `HealthStatus` IntEnum (§11.6): STARTING, READY, DEGRADED, DRAINING
- [ ] `GossipEvent` dataclass with all 6 event types: CONTRACT_PUBLISHED, **CHANNEL_UPDATED**, ENDPOINT_LEASE_UPSERTED, ENDPOINT_DOWN, **ACL_CHANGED**, **COMPATIBILITY_PUBLISHED** (§11.7)

**Key schema:**
- [ ] Create `aster/registry/keys.py` with helpers (`contract_key`, `version_key`, `channel_key`, `tag_key`, `lease_key`, `acl_key`, `config_key`)
- [ ] Key prefixes: `_aster/contracts/`, `_aster/services/{name}/{versions|channels|tags}/`, `_aster/services/{name}/contracts/{cid}/endpoints/`, `_aster/acl/`, `_aster/config/`

**Publisher (§11.6, §11.8):**
- [ ] Create `aster/registry/publisher.py::RegistryPublisher`
- [ ] `publish_contract()` delegates to Phase 9
- [ ] `register_endpoint()` writes initial lease with `health=STARTING`, starts refresh timer
- [ ] `set_health(status)` — bumps `lease_seq`, writes new lease row, emits ENDPOINT_LEASE_UPSERTED gossip
- [ ] `refresh_lease()` — background timer, cadence = `lease_refresh_interval_s` (default 15s from `_aster/config/`)
- [ ] Default `lease_duration_s` = 45
- [ ] `withdraw(grace_period_s)` — graceful state machine: (1) set_health(DRAINING) + lease_seq++, (2) wait grace, (3) delete lease, (4) broadcast ENDPOINT_DOWN

**Client (§11.8, §11.9):**
- [ ] Create `aster/registry/client.py::RegistryClient`
- [ ] `__init__` applies `DownloadPolicy.NothingExcept(["_aster/"])` to registry_doc (Phase 1c.6 ✅)
- [ ] Uses `DocsClient.join_and_subscribe` for race-free subscribe-before-sync (Phase 1c.8 ✅)
- [ ] Two-step `resolve(service_name, version?, channel?, tag?, strategy)`: (1) read pointer key → contract_id, (2) list `_aster/services/{name}/contracts/{cid}/endpoints/*` → candidate leases
- [ ] Mandatory filters (§11.9 normative order): contract_id match, ALPN, serialization_modes, health ∈ {READY, DEGRADED}, lease freshness (`now - updated_at_epoch_ms <= lease_duration_s*1000`), policy_realm
- [ ] Rank survivors by strategy: `round_robin`, `least_load`, `random`
- [ ] Prefer READY over DEGRADED within strategy
- [ ] `resolve_all()` variant returns unranked survivors
- [ ] `fetch_contract(contract_id)` delegates to Phase 9 (`blob_observe_complete` + `blob_local_info`)
- [ ] `on_change(callback)` subscribes to gossip + `DocHandle.subscribe()` InsertRemote events (authoritative backup, §11.10)
- [ ] **`lease_seq` monotonicity**: maintain latest-seen per `(service, endpoint_id)`; reject writes with `lease_seq <= latest` (§11.10)
- [ ] **Lease-expiry eviction independent of gossip** (§11.10 — docs is authoritative)

**ACL (§11.2.3):**
- [ ] Create `aster/registry/acl.py::RegistryACL`
- [ ] Read `_aster/acl/{writers,readers,admins,policy}` keys
- [ ] Reload on `ACL_CHANGED` gossip event
- [ ] `is_trusted_writer(author_id)` — called on every `_aster/*` read
- [ ] Post-read filter: entries from non-writer AuthorIds dropped silently (with log)
- [ ] Admin operations: `add_writer`, `remove_writer` (requires admin AuthorId)
- [ ] Document TODO: true sync-time rejection requires future FFI hook

**Gossip (§11.7):**
- [ ] Create `aster/registry/gossip.py::RegistryGossip`
- [ ] Broadcast methods for all 6 event types: contract_published, channel_updated, endpoint_lease_upserted, endpoint_down, acl_changed, compatibility_published
- [ ] `listen()` async iterator over incoming events

**Tests:**
- [ ] Publish contract + advertise endpoint on node A; resolve + connect from node B
- [ ] Registry doc sync uses `NothingExcept(["_aster/"])` policy
- [ ] `lease_seq` monotonicity: stale writes rejected
- [ ] Lease expiry: consumer evicts without `ENDPOINT_DOWN` gossip
- [ ] Graceful withdraw: DRAINING → grace → delete → ENDPOINT_DOWN
- [ ] Consumer skips STARTING + DRAINING, prefers READY > DEGRADED
- [ ] All 6 gossip event types round-trip
- [ ] Endpoint selection: mandatory filters applied before strategy ranking
- [ ] ACL post-read filter: untrusted-author entries excluded
- [ ] Contract fetch uses `blob_observe_complete` (Phase 1d ✅)

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