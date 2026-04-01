# Implementation Status

**Last Updated:** 2026-04-01

## Phase 1: Core + FFI Refactoring

### Status: COMPLETE

---

## Tasks

### Phase 1.1: iroh_transport_core Changes

| Task | Status | Notes |
|------|--------|-------|
| Arc-wrap `CoreNode` | ✅ DONE | 2026-04-01 |
| Arc-wrap `CoreNetClient` | ✅ DONE | 2026-04-01 |
| Arc-wrap `CoreConnection` | ✅ DONE | 2026-04-01 |
| Add `relay_urls` to `CoreEndpointConfig` | ✅ DONE | Custom relay URL support |
| Add `enable_discovery` to `CoreEndpointConfig` | ✅ DONE | 2026-04-01 |
| Add `export_secret_key()` to `CoreNetClient` | ✅ DONE | 2026-04-01 |
| Add `export_secret_key()` to `CoreNode` | ✅ DONE | 2026-04-01 |
| Update `build_endpoint_config()` for custom relay | ✅ DONE | 2026-04-01 |
| Verify datagram support completeness | ✅ DONE | Already implemented |

### Phase 1.2: iroh_transport_ffi Rewrite

| Task | Status | Notes |
|------|--------|-------|
| Set up cbindgen.toml | ✅ DONE | 2026-04-01 |
| Add `rand` dependency | ✅ DONE | 2026-04-01 |
| Add `bytes` dependency | ✅ DONE | 2026-04-01 |
| Build HandleRegistry<T> | ✅ DONE | Arc-backed handle storage |
| Build BridgeRuntime | ✅ DONE | 2026-04-01 |
| Implement event queue | ✅ DONE | EventOwned/EventInternal system |
| Implement Runtime lifecycle | ✅ DONE | iroh_runtime_new, iroh_runtime_close |
| Implement Endpoint API | ✅ DONE | create, close, id, addr |
| Implement Connection API | ✅ DONE | connect, accept, close, datagram |
| Implement Stream API | ✅ DONE | open_bi, accept_bi, open_uni, accept_uni |
| Implement Stream read/write | ✅ DONE | write, finish, read, read_to_end, stop |
| Implement Blobs API | ✅ DONE | add_bytes, read |
| Implement Docs API | ✅ DONE | create, create_author, set_bytes, get_exact, share |
| Implement Gossip API | ✅ DONE | subscribe, broadcast, recv |
| Implement operation cancellation | ✅ DONE | iroh_operation_cancel |
| Implement poll_events | ✅ DONE | iroh_poll_events |
| Implement buffer_release | ✅ DONE | iroh_buffer_release |
| Implement typed handle free functions | ✅ DONE | node_free, endpoint_free, etc. |
| **Compile successfully** | ✅ DONE | 2026-04-01 |
| Generate C header with cbindgen | ✅ DONE | 2026-04-01 |
| Write Rust integration tests | ✅ DONE | 39 tests passing |

### Phase 1.3: Testing

| Task | Status | Notes |
|------|--------|-------|
| FFI Integration tests | ✅ DONE | 39 tests passing (Phase 1b tests added 2026-04-01) |
| Handle free safety tests | ✅ DONE | Included in tests |
| Echo roundtrip tests | ✅ DONE | Added endpoint_addr_info + add_node_addr FFI |
| Cross-language validation | ⬜ TODO | Python/Java bindings pending |

---

## Phase 1b: Datagram Completion, Hooks & Monitoring

### Status: IMPLEMENTED (v0.97.0-compatible subset)

> See `ffi_spec/FFI_PLAN_PATCH.md` for the original target design.
>
> Phase 1b has been re-scoped and implemented against the actual `iroh v0.97.0` API surface,
> using the upstream examples as authoritative references:
> - `auth-hook.rs` → builder-time `EndpointHooks` with `before_connect` / `after_handshake`
> - `monitor-connections.rs` → `ConnectionInfo` observation via `after_handshake` hook
> - `remote-info.rs` → userland `RemoteMap` aggregation over `ConnectionInfo`, `paths().stream()`, `closed()`
> - `screening-connection.rs` → protocol-level `ProtocolHandler::on_accepting` (separate from endpoint hooks)

### Phase 1b.1: Datagram Completion

| Task | Status | Notes |
|------|--------|-------|
| Core: Add `max_datagram_size()` to `CoreConnection` | ✅ DONE | Returns `Option<usize>` |
| Core: Add `datagram_send_buffer_space()` to `CoreConnection` | ✅ DONE | Returns `usize` |
| FFI: Add `iroh_connection_max_datagram_size()` | ✅ DONE | Synchronous query |
| FFI: Add `iroh_connection_datagram_send_buffer_space()` | ✅ DONE | Synchronous query |
| Tests: Datagram size/buffer integration tests | ✅ DONE | 4 tests added (39 total FFI tests) |

### Phase 1b.2: Endpoint Hooks

| Task | Status | Notes |
|------|--------|-------|
| Core: `CoreHooksAdapter` implementing `EndpointHooks` | ✅ DONE | Channel-based adapter bridges iroh's builder-time hooks to async reply model |
| Core: `CoreHookReceiver` for FFI/Python consumption | ✅ DONE | `before_connect_rx` / `after_handshake_rx` with oneshot reply channels |
| Core: `CoreHookConnectInfo` / `CoreHookHandshakeInfo` types | ✅ DONE | Clean hook event data types matching iroh 0.97.0 surface |
| Core: `CoreAfterHandshakeDecision` enum | ✅ DONE | Accept / Reject with error_code + reason |
| Core: `CoreEndpointConfig.enable_hooks` | ✅ DONE | Builder-time hook installation via config flag |
| Core: `CoreEndpointConfig.hook_timeout_ms` | ✅ DONE | Configurable timeout for hook reply (default 5000ms) |
| Core: `CoreNetClient.take_hook_receiver()` | ✅ DONE | One-time consumption of hook receiver for FFI layer |
| Core: `CoreNetClient.has_hooks()` | ✅ DONE | Query whether hooks are enabled |
| FFI: Reserve hook event kinds | ✅ DONE | `IROH_EVENT_HOOK_BEFORE_CONNECT` (70), `IROH_EVENT_HOOK_AFTER_CONNECT` (71), `IROH_EVENT_HOOK_INVOCATION_RELEASED` (72) |
| FFI: `iroh_hook_decision_t` enum | ✅ DONE | ALLOW (0), DENY (1) |
| FFI: Hook reply functions (`iroh_hook_before_connect_respond`, `iroh_hook_after_connect_respond`) | ⬜ TODO | Core adapter is ready; FFI event-queue wiring deferred to Phase 2 integration |
| Tests: Hook integration tests | ⬜ TODO | Requires FFI hook reply wiring; core adapter is unit-testable now |

**Implementation notes for `iroh 0.97.0`:**
- Hooks are installed at **endpoint builder time** via `Endpoint::builder(...).hooks(adapter).bind()`, matching the iroh 0.97.0 `EndpointHooks` trait exactly.
- The `CoreHooksAdapter` implements `EndpointHooks` with `before_connect(&self, remote_addr, alpn) -> BeforeConnectOutcome` and `after_handshake(&self, conn: &ConnectionInfo) -> AfterHandshakeOutcome`.
- The adapter sends hook events through `mpsc` channels and waits for replies via `oneshot` channels, with a configurable timeout (default-allow on timeout/error).
- The old `set_hooks()` / `clear_hooks()` no-op stubs have been removed from `CoreNetClient` — hooks are now purely builder-time, matching the upstream API.

### Phase 1b.3: Remote-Info & Monitoring

| Task | Status | Notes |
|------|--------|-------|
| Core: `CoreMonitor` struct (modeled after `remote-info.rs`) | ✅ DONE | Implements `EndpointHooks` via internal `MonitorHook`, tracks connections via `after_handshake` |
| Core: `RemoteInfoEntry` internal tracking | ✅ DONE | Stores `ConnectionInfo` per-connection, `CoreRemoteAggregate` for historical stats |
| Core: `CoreRemoteAggregate` struct | ✅ DONE | Tracks rtt_min, rtt_max, ip_path, relay_path, bytes_sent/received, last_update |
| Core: Path change tracking via `conn.paths().stream()` | ✅ DONE | Background task per connection updates aggregate stats on path changes |
| Core: Connection close tracking via `conn.closed()` | ✅ DONE | Background task per connection cleans up on close, accumulates final stats |
| Core: `CoreMonitor.remote_info(node_id)` | ✅ DONE | Returns `Option<CoreRemoteInfo>` from live RemoteMap |
| Core: `CoreMonitor.remote_info_iter()` | ✅ DONE | Returns `Vec<CoreRemoteInfo>` of all known remotes |
| Core: `CoreEndpointConfig.enable_monitoring` | ✅ DONE | Builder-time monitor installation via config flag |
| Core: `CoreNetClient.remote_info()` delegates to `CoreMonitor` | ✅ DONE | Returns real data when monitoring enabled, `None` when disabled |
| Core: `CoreNetClient.remote_info_iter()` delegates to `CoreMonitor` | ✅ DONE | Returns real data when monitoring enabled, empty vec when disabled |
| Core: `CoreNetClient.has_monitoring()` | ✅ DONE | Query whether monitoring is enabled |
| Core: Define `CoreRemoteInfo` struct | ✅ DONE | Fully populated from live connection data + aggregates |
| Core: Define `ConnectionType` enum | ✅ DONE | NotConnected, Connecting, Connected(detail) |
| Core: Define `ConnectionTypeDetail` enum | ✅ DONE | UdpDirect, UdpRelay, Other |
| Core: Define `CoreConnectionInfo` struct | ✅ DONE | From `Connection::to_info()`, `selected_path()`, `stats()` |
| Core: `CoreConnection.connection_info()` | ✅ DONE | Real implementation using iroh 0.97.0 `ConnectionInfo` API |
| FFI: `iroh_remote_info_t` struct | ✅ DONE | ABI type exists and tested |
| FFI: `iroh_connection_info_t` struct | ✅ DONE | ABI type exists and tested |
| FFI: `iroh_endpoint_remote_info()` | ✅ DONE | Calls through to real `CoreNetClient::remote_info()` (monitoring auto-enabled for FFI endpoints) |
| FFI: `iroh_endpoint_remote_info_list()` | ✅ DONE | Calls through to real `CoreNetClient::remote_info_iter()` |
| FFI: `iroh_connection_info()` | ✅ DONE | Full implementation tested |
| Tests: Remote-info integration tests | ✅ DONE | 6 tests covering invalid params, struct sizes |

**Implementation notes for `iroh 0.97.0`:**
- The `CoreMonitor` is modeled directly after the `remote-info.rs` upstream example, using `EndpointHooks::after_handshake` to capture `ConnectionInfo`, then spawning background tasks for `conn.paths().stream()` (path change tracking) and `conn.closed()` (close tracking with final stats).
- Monitoring is **automatically enabled** for all FFI-created endpoints (`enable_monitoring: true`).
- The `remote_info()` and `remote_info_iter()` methods now return real data populated from live connection tracking, replacing the previous placeholder stubs that always returned `None`/empty.
- `screening-connection.rs` patterns (`ProtocolHandler::on_accepting`) remain a separate surface not addressed in this phase.

### Phase 1b.3a: Screening / Connection Admission Control

| Task | Status | Notes |
|------|--------|-------|
| Analyze `screening-connection.rs` example | ✅ DONE | Protocol-level via `ProtocolHandler::on_accepting`, separate from `EndpointHooks` |
| Design FFI surface for screening | ⬜ TODO | Deferred — distinct from endpoint hooks, requires protocol-handler integration |

---

## Phase 2: Python Bindings Update

**Status:** NOT STARTED

**Phase 2 readiness assessment:**
- Phase 1b core surfaces (monitoring, hooks adapter, connection-info) are now **implemented and compilable** against iroh 0.97.0.
- The `CoreMonitor` and `CoreHooksAdapter` provide the substrate Python wrappers need.
- Remaining gap: FFI hook reply functions not yet wired through the event queue — but Python (using PyO3 directly over core) does not need the FFI hook reply path.
- **Recommendation:** Phase 2 can proceed. Python should consume `CoreMonitor` and `CoreHooksAdapter` directly via PyO3, not through the FFI C ABI.

### Phase 3: Java FFM Bindings

**Status:** NOT STARTED

---

## Notes

- Using skeleton from `ffi_spec/iroh_bridge.rs` as starting reference
- Full blob/doc/gossip APIs included in Phase 1
- cbindgen setup required for header generation
- Phase 1b was re-scoped against `iroh 0.97.0` and implemented using the upstream examples as authoritative references
- Datagram completion is real and implemented
- **Hook support is now a real channel-based adapter** implementing iroh 0.97.0 `EndpointHooks` at builder time; the old no-op `set_hooks()` / `clear_hooks()` stubs have been removed
- **Remote-info/monitoring is now a real `RemoteMap`-based tracking system** modeled after the `remote-info.rs` example; the old stubs that returned `None`/empty have been replaced with live connection tracking
- Monitoring is auto-enabled for FFI-created endpoints
- `screening-connection.rs` shows a separate acceptance-control surface via `ProtocolHandler::on_accepting`, which should be treated distinctly from endpoint hooks in future planning
- The authoritative upstream target for this repository today is **`iroh v0.97.0`**, including its examples
- All 39 FFI integration tests pass after the Phase 1b implementation