# Aster Python Implementation — Progress Checklist

## INSTRUCTIONS

Main plan: [ASTER_PLAN.md](ASTER_PLAN.md) - please read first.

Please progress the tasks in this document one phase at a time and one step at a time. Please keep the `STATUS` section updated with your current status and list any outstanding issues or blockers.

For each step we need to make sure the code passes tests and linting.

## STATUS

Pre-requisites complete. Phase 1 complete (42/42 tests passing). Phase 2 complete (47/47 tests passing, 89/89 total).

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

- [ ] Create `aster/transport/__init__.py`
- [ ] Create `aster/transport/base.py` — `Transport` protocol
- [ ] Create `aster/transport/base.py` — `BidiChannel` class (send/recv/close + async context manager)
- [ ] Create `aster/transport/iroh.py` — `IrohTransport` (opens QUIC stream per call)
- [ ] Implement `IrohTransport.unary()`
- [ ] Implement `IrohTransport.server_stream()`
- [ ] Implement `IrohTransport.client_stream()`
- [ ] Implement `IrohTransport.bidi_stream()`
- [ ] Create `aster/transport/local.py` — `LocalTransport` (asyncio.Queue-based)
- [ ] Implement `LocalTransport` with full interceptor chain
- [ ] Implement `wire_compatible` flag on `LocalTransport`
- [ ] Tests: IrohTransport unary round-trip over real Iroh connection
- [ ] Tests: LocalTransport unary round-trip
- [ ] Tests: BidiChannel for both transports
- [ ] Tests: `wire_compatible=True` catches missing type tags

---

## Phase 4: Service Definition Layer

- [ ] Create `aster/decorators.py` — `@service(name, version, serialization, scoped, ...)`
- [ ] Create `aster/decorators.py` — `@rpc(timeout, idempotent, serialization)`
- [ ] Create `aster/decorators.py` — `@server_stream`
- [ ] Create `aster/decorators.py` — `@client_stream`
- [ ] Create `aster/decorators.py` — `@bidi_stream`
- [ ] Create `aster/service.py` — `MethodInfo` dataclass
- [ ] Create `aster/service.py` — `ServiceInfo` dataclass
- [ ] Create `aster/service.py` — `ServiceRegistry` (register, lookup)
- [ ] Implement type introspection from method signatures (`typing.get_type_hints`, `inspect`)
- [ ] Implement eager Fory type validation at decoration time (XLANG mode)
- [ ] Tests: decorate a test service, verify `ServiceInfo` and `MethodInfo`
- [ ] Tests: missing `@fory_tag` raises `TypeError`
- [ ] Tests: `ServiceRegistry` lookup by name

---

## Phase 5: Server Implementation

- [ ] Create `aster/server.py` — `Server.__init__(endpoint, services, interceptors)`
- [ ] Implement connection accept loop (per-connection task spawning)
- [ ] Implement stream dispatch (read `StreamHeader`, validate, route)
- [ ] Implement unary dispatch (read request → call handler → write response)
- [ ] Implement server-stream dispatch (read request → iterate handler → write frames + trailer)
- [ ] Implement client-stream dispatch (read frames until finish → call handler → write response)
- [ ] Implement bidi-stream dispatch (concurrent read/write tasks)
- [ ] Implement `Server.drain(grace_period)` for graceful shutdown
- [ ] Implement error handling (handler exceptions → RpcStatus trailer)
- [ ] Implement unknown service/method → `UNIMPLEMENTED` status
- [ ] Tests: echo service (unary)
- [ ] Tests: counter service (server-stream)
- [ ] Tests: aggregation service (client-stream)
- [ ] Tests: bidi echo service
- [ ] Tests: handler exception → proper trailer
- [ ] Tests: graceful shutdown

---

## Phase 6: Client Stub Generation

- [ ] Create `aster/client.py` — stub class generation from `ServiceInfo`
- [ ] Implement `create_client(service_class, connection, transport, interceptors)`
- [ ] Implement `create_local_client(service_class, implementation, wire_compatible, interceptors)`
- [ ] Implement per-call metadata override
- [ ] Implement per-call timeout override
- [ ] Implement unary stub method
- [ ] Implement server-stream stub method (async iterator)
- [ ] Implement client-stream stub method
- [ ] Implement bidi-stream stub method (async context manager)
- [ ] Tests: client ↔ server unary round-trip
- [ ] Tests: client ↔ server streaming round-trip
- [ ] Tests: local client round-trip
- [ ] Tests: metadata and timeout propagate to `StreamHeader`
- [ ] Tests: `wire_compatible=True` catches serialization issues

---

## Phase 7: Interceptors & Middleware

- [ ] Create `aster/interceptors/__init__.py`
- [ ] Create `aster/interceptors/base.py` — `CallContext` dataclass
- [ ] Create `aster/interceptors/base.py` — `Interceptor` ABC (`on_request`, `on_response`, `on_error`)
- [ ] Implement interceptor chain runner (ordered execution, short-circuit on error)
- [ ] Wire interceptors into server dispatch
- [ ] Wire interceptors into client stubs
- [ ] Create `aster/interceptors/deadline.py` — `DeadlineInterceptor`
- [ ] Create `aster/interceptors/auth.py` — `AuthInterceptor`
- [ ] Create `aster/interceptors/retry.py` — `RetryInterceptor`
- [ ] Create `aster/interceptors/circuit_breaker.py` — `CircuitBreakerInterceptor` (CLOSED → OPEN → HALF-OPEN)
- [ ] Create `aster/interceptors/audit.py` — `AuditLogInterceptor`
- [ ] Create `aster/interceptors/metrics.py` — `MetricsInterceptor` (optional OTel dependency)
- [ ] Tests: deadline enforcement (cancels handler on expiry)
- [ ] Tests: retry behavior (idempotent methods on `UNAVAILABLE`)
- [ ] Tests: circuit breaker state transitions
- [ ] Tests: interceptors run on LocalTransport calls

---

## Phase 8: Session-Scoped Services

- [ ] Extend `@service` decorator to accept `scoped="stream"` parameter
- [ ] Create `aster/session.py` — `create_session(service_class, connection, transport)`
- [ ] Implement session client stub with internal `asyncio.Lock`
- [ ] Implement `CALL` frame writing (client side)
- [ ] Implement `CALL` frame reading (server side)
- [ ] Implement `CANCEL` frame sending (client side)
- [ ] Implement `CANCEL` frame handling (server side — cancel handler, send `CANCELLED` trailer)
- [ ] Implement server-side session loop (instantiate class, loop on CALL frames, dispatch)
- [ ] Implement `on_session_close()` lifecycle hook (fires on all termination paths)
- [ ] Implement client-side: `break` from async iteration sends `CANCEL`
- [ ] Implement client-side: `session.close()` sends `finish()`
- [ ] Implement LocalTransport session support
- [ ] Tests: session with multiple sequential calls (instance state persists)
- [ ] Tests: cancellation mid-stream (CANCEL frame)
- [ ] Tests: session close lifecycle (`on_session_close` fires)
- [ ] Tests: sequential call semantics enforced (async lock)

---

## Phase 9: Contract Identity & Publication

- [ ] Create `aster/contract/__init__.py`
- [ ] Create `aster/contract/identity.py` — `FieldDef` dataclass (`@fory_tag("_aster/FieldDef")`)
- [ ] Create `aster/contract/identity.py` — `EnumValueDef` dataclass
- [ ] Create `aster/contract/identity.py` — `UnionVariantDef` dataclass
- [ ] Create `aster/contract/identity.py` — `TypeDef` dataclass
- [ ] Create `aster/contract/identity.py` — `MethodDef` dataclass
- [ ] Create `aster/contract/identity.py` — `ServiceContract` dataclass
- [ ] Implement canonical XLANG profile serialization (field-order enforcement, no ref tracking)
- [ ] Implement `compute_type_hash(type_def) -> str` (BLAKE3 hex)
- [ ] Implement `resolve_type_graph(service_class) -> dict[str, TypeDef]` (bottom-up hashing)
- [ ] Implement self-reference handling (`type_kind="self_ref"`)
- [ ] Implement `compute_contract_id(contract) -> str` (BLAKE3 hex)
- [ ] Create `aster/contract/manifest.py` — `ContractManifest` dataclass
- [ ] Implement `ContractManifest` construction from `ServiceInfo`
- [ ] Create `aster/contract/publication.py` — `publish_contract(node, service_class, registry_doc)`
- [ ] Implement contract collection building (Iroh collection via `BlobsClient`)
- [ ] Implement `ArtifactRef` write to docs
- [ ] Implement contract fetching and verification (`blake3(contract.xlang) == contract_id`)
- [ ] Tests: hash stability (same input → same hash)
- [ ] Tests: changing a type changes the `contract_id`
- [ ] Tests: self-referencing types
- [ ] Tests: contract publication round-trip (publish → fetch → verify)

---

## Phase 10: Service Registry & Discovery

- [ ] Create `aster/registry/__init__.py`
- [ ] Define `EndpointLease` dataclass (§11.6)
- [ ] Define `ArtifactRef` dataclass (§11.2.1)
- [ ] Define `GossipEvent` dataclass (§11.7)
- [ ] Create `aster/registry/publisher.py` — `RegistryPublisher`
- [ ] Implement `RegistryPublisher.publish_contract()`
- [ ] Implement `RegistryPublisher.advertise_endpoint()`
- [ ] Implement `RegistryPublisher.refresh_lease()` (timer-based)
- [ ] Implement `RegistryPublisher.withdraw()` (graceful shutdown)
- [ ] Create `aster/registry/client.py` — `RegistryClient`
- [ ] Implement `RegistryClient.resolve(service_name, version, channel)`
- [ ] Implement `RegistryClient.fetch_contract(contract_id)`
- [ ] Implement trusted-author filtering on docs reads (§11.2.3)
- [ ] Implement endpoint selection strategies: `round_robin`, `least_load`, `random`
- [ ] Create `aster/registry/acl.py` — `RegistryACL`
- [ ] Implement `RegistryACL.get_writers()`, `get_readers()`, `get_admins()`
- [ ] Implement `RegistryACL.add_writer()`, `remove_writer()`
- [ ] Create `aster/registry/gossip.py` — `RegistryGossip`
- [ ] Implement `RegistryGossip.broadcast_contract_published()`
- [ ] Implement `RegistryGossip.broadcast_endpoint_lease()`
- [ ] Implement `RegistryGossip.broadcast_endpoint_down()`
- [ ] Implement `RegistryGossip.listen()` (async iterator)
- [ ] Tests: publish contract, resolve from second node
- [ ] Tests: lease expiry removes stale entries
- [ ] Tests: gossip notification round-trip
- [ ] Tests: ACL enforcement (untrusted authors rejected)

---

## Phase 11: Testing & Conformance

- [ ] Create `aster/testing/__init__.py`
- [ ] Create `aster/testing/harness.py` — `AsterTestHarness`
- [ ] Implement `create_local_pair(service_class, implementation, wire_compatible)`
- [ ] Implement `create_remote_pair(service_class, implementation)`
- [ ] Write `tests/python/test_aster_framing.py` — frame unit tests
- [ ] Write `tests/python/test_aster_codec.py` — Fory codec unit tests
- [ ] Write `tests/python/test_aster_decorators.py` — service introspection unit tests
- [ ] Write `tests/python/test_aster_unary.py` — unary RPC integration tests
- [ ] Write `tests/python/test_aster_streaming.py` — all streaming patterns
- [ ] Write `tests/python/test_aster_session.py` — session-scoped services
- [ ] Write `tests/python/test_aster_interceptors.py` — interceptor chain
- [ ] Write `tests/python/test_aster_registry.py` — registry publish/resolve
- [ ] Write `tests/python/test_aster_local.py` — LocalTransport parity with IrohTransport
- [ ] Generate `tests/conformance/wire/` — byte-level wire format vectors
- [ ] Generate `tests/conformance/fory/` — serialization golden vectors

---

## Milestone Summary

| Milestone | Phases | Description | Status |
|-----------|--------|-------------|--------|
| **Pre-requisites validated** | — | Python 3.13, pyfory determinism confirmed | ✅ Done |
| **Minimal viable RPC** | 1–6 | Unary + streaming RPCs working end-to-end | 🟡 Phase 2 complete |
| **Production-ready RPC** | 1–7 | + interceptors (deadline, auth, retry, circuit breaker) | ⬜ Not started |
| **Session support** | 8 | Session-scoped services with CALL/CANCEL frames | ⬜ Not started |
| **Contract identity** | 9 | Content-addressed contracts via BLAKE3 Merkle DAG | ⬜ Not started |
| **Decentralized registry** | 10 | Service discovery via iroh-docs/gossip/blobs | ⬜ Not started |
| **Conformance suite** | 11 | Wire-format test vectors + cross-language readiness | ⬜ Not started |