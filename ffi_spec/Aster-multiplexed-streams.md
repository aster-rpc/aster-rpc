# Aster Multiplexed Streams — Design Spec

**Status:** Draft
**Date:** 2026-04-13
**Scope:** Unify the client/server stream lifecycle around a single multiplexed-stream primitive owned by `core`. Replace today's two parallel lifecycles (one-shot SHARED streams and session-multiplexed SESSION streams) with one model that bounds resource use, fixes the SHARED stream-per-call perf cliff, and lets streaming methods on a session run in parallel with other calls on the same session.

---

## Table of Contents

1. [Motivation](#1-motivation)
2. [The Unifying Insight](#2-the-unifying-insight)
3. [Stream Categories and Bounds](#3-stream-categories-and-bounds)
4. [Scenario Walkthrough](#4-scenario-walkthrough)
5. [Backpressure and Timeout Policy](#5-backpressure-and-timeout-policy)
6. [Wire Format Changes](#6-wire-format-changes)
7. [Server Dispatch Changes](#7-server-dispatch-changes)
7.5. [Connection Lifecycle and Memory Bounds](#75-connection-lifecycle-and-memory-bounds)
8. [FFI Surface (Sketch)](#8-ffi-surface-sketch)
9. [Configuration](#9-configuration)
10. [Metrics](#10-metrics)
11. [Migration Plan](#11-migration-plan)
12. [Open Questions](#12-open-questions)
13. [Non-Goals](#13-non-goals)

---

## 1. Motivation

Today the client transport has two stream lifecycles, each with a footgun:

**SHARED (`RpcScope.SHARED`) — stream-per-call.** Every unary call opens a fresh QUIC bidi stream via `open_bi`, sends a `StreamHeader`, receives the response and trailer, closes the stream. This pays the open/close yields on every call. Benchmarks show this dominates per-call latency for SHARED traffic from a single client to a single producer, because the call rate is gated by the stream open/close cost, not by the wire round-trip.

**SESSION (`RpcScope.SESSION`) — one stream per session, single-file.** The session holds a lock that serialises every call on its single stream. A long-running server-streaming or bidi method on a session blocks every other call on the same session for its entire duration. Users hit this and reach for `SHARED`, losing the session affinity they actually wanted.

Both problems have the same root cause: the stream lifecycle is rigidly tied to the scope, so neither scope can pool streams or open extra streams when the situation warrants. They are not two independent features; they are two symptoms of the same constraint.

---

## 2. The Unifying Insight

**Every stream is multiplexed. One-shot is just multiplex-of-one.**

Today, `SessionServer.run()` already implements a `while True: read CallHeader → dispatch → write trailer → loop` over a single QUIC stream. The wire format already has a `CallHeader` frame (`bindings/python/aster/protocol.py:42`) used for exactly this purpose. The state machine is proven and battle-tested for session traffic.

The proposed change is to make this the *only* stream lifecycle. SHARED becomes "a pool of N stateless multiplexed streams." SESSION becomes "a multiplexed stream bound to a session context." Streaming methods on a session open *additional* multiplexed substreams tagged with the same session id, so they run in parallel with the session's main stream rather than blocking it.

Three consequences fall out for free:

- **SHARED gets pooled by construction.** A pooled SHARED stream is just a multiplexed stream with no session binding. Calls reuse the stream by sending a fresh `CallHeader` per call. The open/close cost amortises over the pool's lifetime.
- **Session streaming methods stop blocking.** A streaming call gets its own substream. The session's main stream keeps serving unary calls in parallel.
- **The architecture stops having two copies of the framing state machine in every binding.** Today, Python, TypeScript, and Java each carry their own implementation of "open stream, write header, frame loop, parse trailer, handle reset." Moving to one multiplexed primitive in `core` lets every binding shed that code in favour of a thin FFI shim.

---

## 3. Stream Categories and Bounds

Stream categories live on a per-peer-connection basis. Each connection to a producer has its own pools and its own QUIC concurrency ceiling.

| Category | Bound | Owner | Notes |
|---|---|---|---|
| SHARED unary pool | `shared_pool_size` (default 8) | core (per connection) | Calls grab a free stream from the pool. If none free and pool < bound, open a new one. If pool is full, queue. LIFO reuse to keep recently-used streams hot. |
| Session main stream | unbounded by us | core (per session) | One per session. Bounded only by the QUIC ceiling. |
| Session unary substreams | `session_pool_size` (default 1) per session | core (per session) | Default 1 means unary calls on a session multiplex serially on the main stream — simple, predictable, ordering-preserving. Users opt into parallelism by raising this. |
| Streaming substreams (server-stream, client-stream, bidi) | unbounded by us | core (per call) | Each streaming call gets its own dedicated substream tagged with the parent session id (or null for SHARED-scope streaming calls). Lives for the call's lifetime. Doesn't count against any pool. |
| **Hard ceiling** | QUIC `max_concurrent_streams` (negotiated at handshake) | transport | Enforced by QUIC. When reached, new stream opens block. Surfaced to callers via the timeout policy in §5. |

**Why no global per-client cap.** QUIC's handshake-negotiated `max_concurrent_streams` is the right place for the ceiling — it is per-connection (which is what actually matters for resource use), is enforced by the transport layer for free, and the peer gets to advertise its tolerance. Adding a separate global per-client accounting layer would duplicate this without adding safety.

**Why streaming substreams don't count against pools.** Streaming calls are inherently bounded by user behaviour: you only get a streaming substream when the user explicitly invokes a streaming method, and they're typically attentive to those because they started them. Counting them against a pool would mean a streaming call on a SHARED service could starve unary calls of pool slots, which is surprising. Better to let the QUIC ceiling be the only bound on streaming substreams.

**No idle eviction in v1.** Streams in any pool live for the connection's lifetime. There is no idle-timeout-based eviction. For long-lived clients with bursty workloads this means up to N idle streams may sit in the pool indefinitely, which is acceptable given each idle stream costs only a few hundred bytes of QUIC state. If metrics later show this becoming a real cost (e.g., long-lived clients accumulating high `pool_streams_open` against low `pool_streams_busy`), idle eviction can be added in a follow-up — the pool primitive in `core` is the right place for it.

---

## 4. Scenario Walkthrough

These are the scenarios that drove the design. Each must behave elegantly.

**Scenario 1: 1000 concurrent SHARED unary calls from one client to one producer.**
Pool size 8. 8 streams open. 992 calls queue. As streams free, queued calls grab them. Bounded, predictable. Fixes today's stream-per-call cliff. ✓

**Scenario 2: 1000 idle sessions.**
1000 main streams open, one per session. Bounded only by QUIC's `max_concurrent_streams`. If the peer's limit is below 1000, the 1000th-or-so `open_session()` call blocks per §5 then errors. Sessions are an intentional resource — the user opened 1000.

**Scenario 3: 1000 sessions, each with 100 concurrent unary calls.**
Default `session_pool_size=1`. Each session has 1 main stream. Its 100 calls multiplex serially on it. **Total: 1000 streams.** Calls within a session run in order, which is what most session users want. Users wanting parallelism within a session raise `session_pool_size` and pay the cost.

**Scenario 4: 1000 sessions, each running one streaming call AND making unary calls in parallel.**
1000 main streams (unary multiplexed on each) + 1000 streaming substreams (one per active streaming call) = **2000 streams.** The streaming call has its own substream tagged with the session id; the main stream keeps serving unary in parallel. **This is the case Proposal A fixes** — today the streaming call would block the unary call.

**Scenario 5: Pathological — 1000 sessions, `session_pool_size=4`, each running a streaming call.**
4000 unary substreams + 1000 streaming substreams = 5000 streams to one peer. Will hit QUIC's `max_concurrent_streams` ceiling. New stream opens block per §5, then surface a typed `PeerStreamLimitReached` error. The user gets backpressure at the call site, not silent latency.

---

## 5. Backpressure and Timeout Policy

When a call needs a stream and no stream is available (pool full, or QUIC ceiling reached), the client **blocks for up to `stream_acquire_timeout_ms`, then errors** with a typed `StreamAcquireTimeoutError` carrying the reason (`pool_full` or `quic_limit_reached`).

Rationale: silent blocking hides the problem and turns latency into a mystery; immediate failure is too brittle for transient bursts. A bounded wait gives the system time to drain naturally while still surfacing real saturation as a clear error the caller can react to.

The typed error lets callers distinguish:

- `pool_full` → "raise `shared_pool_size` / `session_pool_size`, or reduce concurrency"
- `quic_limit_reached` → "the peer is saturated, retry later or open fewer sessions"

Pool acquisition uses a fair queue (FIFO) so that a burst of 1000 calls drains in order rather than starving the first arrivals.

---

## 6. Wire Format Changes

**Existing primitives, no removal:**

- `StreamHeader` — first frame on every stream (`HEADER` flag). Already carries `service`, `method`, `version`, `callId`, `deadline`, `serializationMode`, `metadataKeys/Values`.
- `CallHeader` — per-call header within a multiplexed stream (`CALL` flag). Already carries `method`, `callId`, `deadline`, `metadataKeys/Values`. Used by sessions today.
- `RpcStatus` — trailer frame (`TRAILER` flag), unchanged.

**One addition:**

- `StreamHeader.sessionId: u32` (default `0`). Encoded as a 4-byte little-endian field in the StreamHeader. `0` means "stateless stream, route through the SHARED pool." Any non-zero value means "this stream belongs to the session identified by this id on this `(peer, connection)`." For session streaming substreams, the client sends the parent session's id here.

**Client-side `sessionId` allocation.** The client maintains a per-connection `AtomicU32` counter starting at `0`. Each `open_session()` call does `counter.fetch_add(1) + 1` (so the first session is `1`, the second `2`, and so on). The counter is per-connection, so reopening a connection resets it. This guarantees:

- Monotonicity within a connection (server can rely on it for the graveyard check below).
- No collisions across sessions on the same connection (the counter is the source of truth, no client-side coordination needed).
- Trivial allocation cost (one atomic increment).

`u32` gives ~4.2B sessions per connection before wraparound, which is comfortably out of reach for any realistic workload (and a wraparound would just look like a NOT_FOUND to the server, not a security issue).

**Session creation semantics — Model A (implicit on first arrival).**

Sessions are created **implicitly** the first time a stream arrives at the server with a `sessionId` not currently in the per-connection session map. Specifically, on every inbound stream the server:

1. Looks up `(connection, sessionId)` in the per-connection session map.
2. If found: bind this stream to the existing session context.
3. If not found and `sessionId > last_opened_session_id` for this connection: create the session context (subject to `max_sessions_per_connection`, see §9), bump `last_opened_session_id` to `sessionId`, bind this stream.
4. If not found and `sessionId <= last_opened_session_id`: the session was previously created and has been closed/expired. Emit NOT_FOUND (see below).

There is no separate "open session" RPC. The session's lifecycle starts when the server first sees its `sessionId`. This matches what the current binding does today (implicit create on first call), with `(connection, sessionId)` as the key instead of `(peer, service)`.

**Why monotonic-counter graveyard.** The `last_opened_session_id` per-connection integer is a bounded, exact graveyard for "this session was previously known and has been closed." No HashSet, no TTL eviction, no memory growth — just one `u32` per connection. It works because the client's monotonic counter guarantees fresh sessionIds are always greater than any previously-issued one.

**Unknown / closed `sessionId` handling.** If the server cannot bind a stream's `sessionId` to a session context per the rules above, it writes an `RpcStatus` trailer with code `NOT_FOUND` and message `"session not found"`, then closes the stream. The client maps `NOT_FOUND` on a session-bound call to a typed `SessionNotFoundError` so callers can decide whether to reopen the session and retry, or fail the operation.

This case is for sessions that were *previously created and have been closed/expired by the server* (idle timeout, auth revoked, server tear-down). A `sessionId > last_opened_session_id` always succeeds (subject to limits), so a fresh client never spuriously NOT_FOUNDs.

**Discriminator change.** Today the server uses `header.method == ""` as the signal for "this is a session stream, expect CallHeaders." Going forward, the server uses **the presence of CallHeader frames after the StreamHeader** as the signal for "this is multiplexed." The `method` field on `StreamHeader` becomes optional and is ignored on multiplexed streams; the per-call `method` lives in the `CallHeader`.

For the migration window, the server can accept both signals (empty `method` OR presence of `sessionId`), but since back-compat is not a constraint (per project status), the cleaner end state is: every stream is multiplexed, every call carries a `CallHeader`, `StreamHeader.method` is removed.

---

## 7. Server Dispatch Changes

**Today:** `bindings/python/aster/server.py:641-643` discriminates one-shot vs session streams via `header.method == ""` and rejects mismatched scopes. `SessionServer.run()` (`bindings/python/aster/session.py:244`) implements the multiplexed read loop. One-shot streams have a separate code path that handles a single call and returns.

**Proposed:** drop the discriminator. Every inbound stream goes through a `MultiplexedCallReader` that:

1. Reads the `StreamHeader`.
2. If `sessionId == 0`, dispatches each call statelessly through the service registry (SHARED pool stream).
3. If `sessionId != 0`, performs the lookup-or-create per the Model A rules in §6 (look up `(connection, sessionId)` in the per-connection session map; create if `> last_opened_session_id`; emit NOT_FOUND if `<= last_opened_session_id`), binds the reader to the resolved session context, and dispatches each call into the session instance.
4. Loops on `CallHeader` until the stream closes or a transport error occurs.

`SessionServer` becomes "a `MultiplexedCallReader` bound to a session instance." The stateless SHARED-pool path is "a `MultiplexedCallReader` bound to the registry." One implementation, two bindings.

This refactor is the server-side equivalent of moving the client framing state machine into `core`: one copy of the loop, parameterised over what to do with each decoded call.

**Where session identity lives.** Core does **not** parse the `StreamHeader` payload (it stays Fory-opaque) and does **not** maintain the session map. Both responsibilities live in the binding. Core's only job is the multiplexed call loop and the per-call channel plumbing; the binding parses headers, owns the per-connection session map, runs the lookup-or-create logic, and emits NOT_FOUND trailers when a sessionId fails the graveyard check. This keeps core thin and lets each binding integrate session lifetime with its own object-lifetime model (Java GC, Python refcounts, etc.).

---

## 7.5. Connection Lifecycle and Memory Bounds

Sessions persist across calls; without explicit cleanup their server-side state would grow without bound. This section makes the cleanup contract explicit and bounds the memory cost.

**Sessions are scoped per-connection, not per-peer-identity.** When a peer's QUIC connection drops, every session created on that connection is torn down. If the peer reconnects, they get a fresh connection with `last_opened_session_id` reset to `0` and an empty session map — they call `open_session()` again and start over. There is no resume-across-disconnects in this layer; if a use case needs that, it builds explicit state-serialization on top of the routing layer.

This rule is what bounds memory: the server's per-`(peer, connection)` session map is reaped whole when the connection closes. There is no orphan path.

**Core emits `ConnectionClosed` events.** Today the reactor's `connection_loop` exits silently when `accept_bi` errors. Going forward it MUST emit a `ConnectionClosed` event before exiting so the binding has a hook to reap state. The reactor's dispatch channel becomes:

```rust
pub enum ReactorEvent {
    Call(IncomingCall),
    ConnectionClosed { peer_id: String, info: CoreClosedInfo },
}
```

The FFI poll surface gains a discriminator on the returned slot so bindings can demultiplex `Call` vs `ConnectionClosed`. On `ConnectionClosed`, the binding looks up its per-connection session map, drops every session instance and substream, and frees the map.

**Per-connection session limit.** A single misbehaving (or malicious) peer must not be able to OOM the server by calling `open_session()` in a loop. A configurable cap bounds this:

- `aster.transport.max_sessions_per_connection` (see §9). Default `1024`.
- When the client sends a `StreamHeader` with `sessionId > last_opened_session_id` AND the active-session count for this connection is already at the cap, the server emits an `RpcStatus` trailer with code `RESOURCE_EXHAUSTED` and message `"session limit reached"`, then closes the stream. `last_opened_session_id` is **not** bumped in this case (the session was rejected, so future references to it are NOT_FOUND, not "previously created").
- The cap counts active sessions only; closed sessions don't count even though they're tracked by the graveyard counter.

**Memory bound.** Worst-case per-server memory for session state is:

```
max_connections × max_sessions_per_connection × per_session_state_size
```

All three terms are operator-controlled. With defaults (assume 10k connections, 1024 sessions/conn, 1KB/session), that's ~10GB worst-case — high but bounded, and the per-session-state-size term is what most deployments will tune downward. With a more typical mix (1k conns, 16 sessions/conn, 1KB), it's ~16MB.

**Streaming substreams are not part of this accounting.** They're per-call, lifetime-bounded by the user's iterator/future, and reclaimed when the call ends. They count against the QUIC `max_concurrent_streams` ceiling (§9), not against the session cap.

**Binding responsibilities — summary.** Every binding MUST:

1. Maintain a per-`(peer, connection)` session map keyed by `sessionId`.
2. Maintain a per-connection `last_opened_session_id` integer for the graveyard check.
3. Maintain a per-connection active-session count for the cap check.
4. On every inbound stream's StreamHeader: run the lookup-or-create logic from §6, emitting NOT_FOUND or RESOURCE_EXHAUSTED on the appropriate failure paths.
5. On every `ConnectionClosed` event from core: drop the session map, the counter, and the active-session count for that connection.

These are mechanical, not invented — they're the binding's existing per-stream / per-peer state management generalised to a longer-lived "session" unit.

---

## 8. FFI Surface

**One unified `CallHandle` for both inbound and outbound calls.** A call is a call regardless of whether it was initiated by the local client or accepted from a peer. The per-call operations — send a frame, receive a frame, send a terminal trailer, release — are direction-agnostic at the abstraction level. The existing `aster_reactor_*` family bundles per-call ops with the inbound accept loop; this design separates them.

After this change:

- The **reactor** owns *only* the inbound accept path. Its surface shrinks to `aster_reactor_create` / `_destroy` / `_poll`. `_poll` returns a batch of unified `CallHandle`s for inbound calls (rather than the current `aster_reactor_call_t` with embedded per-call channel state).
- A new **`aster_call_*` family** owns *every* per-call operation, regardless of direction. Server-side handlers call the same `send_frame` / `recv_frame` / `send_trailer` / `release` ops as the client side.
- The existing `aster_reactor_submit_frame`, `aster_reactor_submit_trailer`, `aster_reactor_recv_frame`, `aster_reactor_buffer_release` are **removed**, replaced by their `aster_call_*` equivalents.

Per-project status, back-compat is not a constraint, so this is a clean rename-and-replace — no parallel old/new surfaces to maintain.

| FFI function | Direction | Purpose |
|---|---|---|
| `aster_reactor_create` / `_destroy` / `_poll` | server inbound | Accept loop, batch dispatch via SPSC ring. `_poll` returns `CallHandle`s for inbound calls. Unchanged in shape, simplified internally. |
| `aster_call_acquire(conn, service, method, session_id, deadline_ms, metadata, out_handle)` | client outbound | Acquire a call handle from the connection's stream pool. Lazily opens a new stream up to the pool bound; blocks with timeout when full. `session_id` is empty for SHARED, populated for session-bound calls. |
| `aster_call_send_frame(handle, payload, flags)` | both | Push a frame on the call. Used for client request frames, server response frames, both directions of bidi. |
| `aster_call_send_trailer(handle, status_payload)` | both | Send the terminal trailer. Server-side: ends an inbound call (frees the call slot, signals to release the stream back to the multiplex). Client-side: ends a client-streaming input phase. |
| `aster_call_recv_frame(handle, out_buf, out_flags, timeout_ms)` | both | Pull the next frame on the call. Same `block_on` + per-call mpsc + timeout pattern as the existing `aster_reactor_recv_frame`. |
| `aster_call_release(handle)` | both | Clean up the call. Server-side: returns the underlying multiplexed stream to its pool of accept-side handlers. Client-side: returns the underlying stream to the SHARED or session pool, or closes it for streaming substreams. |
| `aster_call_buffer_release(buffer_id)` | both | Release a payload buffer back to the `BufferRegistry`. Renamed from `aster_reactor_buffer_release` for consistency with the new family. |
| `aster_call_unary(conn, service, method, session_id, header, request, out_response, out_trailer)` | client outbound | Fast path collapsing acquire/send/recv/release for unary calls — the Python `unary_call` shape, generalised across bindings. |

`aster_call_acquire` errors carry the typed reasons from §5: `PoolFull`, `QuicLimitReached`, `Timeout`, `PeerStreamLimitTooLow`, plus generic transport errors.

**Pool primitives in `core`.** `aster_call_acquire` calls into a per-connection pool living in `core/src/lib.rs`:

- Pool keyed by `Option<SessionId>` (None = SHARED, Some = session-bound).
- `acquire_stream(session_id, timeout) → StreamHandle` — LIFO reuse of free streams; lazy growth up to the configured bound (`shared_pool_size` or `session_pool_size`); blocking wait with timeout when full.
- `release_stream(handle)` — return to pool on success, drop on transport error.
- Connect-time clamp against negotiated QUIC `max_concurrent_streams` (per §9).
- Metric emission for the gauges, histograms, and counters in §10.

**Substrate reuse.** The unified `aster_call_*` family inherits the existing FFI substrate that the reactor built:

- `BufferRegistry` for payload-lifetime management — same opaque buffer-id pattern, same release semantics.
- `BridgeRuntime` for the captured tokio runtime handle.
- The `block_on`-with-timeout pattern for translating async tokio operations into sync FFI calls from binding threads.
- The same SAFETY discipline around `Send`-marked descriptor types and raw payload pointers.

**Why unify rather than parallel?** A parallel surface would mean two concepts of "a call" in the FFI, two implementations of `send_frame`, two test suites, two pieces of documentation, and a permanent cognitive cost for every engineer reading the bindings. The unified surface costs slightly more upfront (the bindings' server-side paths migrate at the same time as the client-side ones land) but produces a permanently smaller, simpler architecture: one `CallHandle`, one set of per-call ops, one mental model. The branch strategy mitigates the lockstep migration cost — see §11.

**Cost estimate (revised):**

- `ffi/src/call.rs`: ~500–700 lines new — absorbs the per-call ops formerly in `ffi/src/reactor.rs`.
- `ffi/src/reactor.rs`: ~300–400 lines *deleted* (per-call ops moved to `call.rs`); file shrinks to just `create`/`destroy`/`poll` plus SPSC plumbing.
- `core/src/lib.rs`: ~300–500 lines new (pool primitives, call-handle types).
- `core/src/reactor.rs`: ~100–200 lines changed for the unified `MultiplexedCallReader` (drop the `is_session_call` discriminator, route by `sessionId` field on header). `IncomingCall` type may merge with the new unified `Call` type — additional deletion.
- **Net new Rust:** ~700–1000 lines.
- **Net deleted Rust:** ~400–500 lines from existing FFI and core.
- Per-binding migration: deletion-heavy on both server and client paths.

---

## 9. Configuration

All new keys live under `aster.transport.*` in `AsterConfig` and are configurable per-client. Defaults are chosen so that most users never touch them.

| Key | Type | Default | Description |
|---|---|---|---|
| `aster.transport.shared_pool_size` | `int` | `8` | Maximum number of multiplexed streams per `(connection, SHARED-pool)`. Calls beyond this queue. Validated against the QUIC ceiling at connect time — see below. |
| `aster.transport.session_pool_size` | `int` | `1` | Maximum number of multiplexed streams per session. Default `1` means unary calls on a session run serially. Raise for parallelism within a session. |
| `aster.transport.stream_acquire_timeout_ms` | `int` | `5000` | How long a call waits for a free stream before erroring with `StreamAcquireTimeoutError`. Applies both to pool-full waits and to QUIC-ceiling-reached waits. |
| `aster.transport.max_sessions_per_connection` | `int` | `1024` | Server-side cap on the number of active sessions per inbound connection (see §7.5). When the cap is reached, further `open_session()` attempts from that peer fail with `RESOURCE_EXHAUSTED`. Sized to be generous for normal use but small enough that a single connection can't OOM the server. |

**Grep target.** Engineers should be able to grep `shared_pool_size`, `session_pool_size`, or `max_sessions_per_connection` and land on this section of this document. Bindings document these keys in their own README under the same names.

**Validation.** All four values must be ≥ 1. `shared_pool_size` and `session_pool_size` should be small (typical: 1–32); a warning is logged if either exceeds 64 since it likely indicates a misconfiguration. `max_sessions_per_connection` typical range is 64–16384; warn outside that range.

**Connect-time QUIC ceiling validation.** The QUIC `max_concurrent_streams` ceiling is negotiated at connection establishment and is set by the *peer*, so it cannot be validated at client startup. Instead, the client validates at connect time:

- If `shared_pool_size > negotiated_max_concurrent_streams`, log a warning and clamp the effective pool size to the ceiling minus a small headroom (default 4 streams reserved for sessions and streaming substreams). The client continues to operate, just with a smaller pool than configured.
- If `negotiated_max_concurrent_streams < 2`, the connection is unusable for the multiplexed-streams model (no headroom for any pooling). Hard-error with `PeerStreamLimitTooLow` and refuse to use the connection. This should never happen against a well-configured peer; it indicates a misconfigured producer.
- The clamped effective pool size is exposed as a gauge metric (see §10) so operators can see when pool size is being constrained by the peer.

---

## 10. Metrics

All metrics carry the `peer` label (the producer node id, hex). Pool metrics also carry a `pool` label distinguishing `shared` from `session:<id>` (session metrics may be high-cardinality and should be aggregable to `session` if needed).

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `aster_transport_pool_streams_open` | gauge | `peer`, `pool` | Number of streams currently open in the pool. |
| `aster_transport_pool_streams_busy` | gauge | `peer`, `pool` | Number of streams currently serving a call (open - free). |
| `aster_transport_pool_acquire_wait_seconds` | histogram | `peer`, `pool` | Time a call waited to acquire a stream. Healthy pools have p99 ≈ 0; sustained nonzero p50 means raise the pool size. |
| `aster_transport_pool_acquire_timeouts_total` | counter | `peer`, `pool`, `reason` | Acquisitions that failed via timeout. `reason` is `pool_full` or `quic_limit_reached`. This is the page-on metric. |
| `aster_transport_quic_stream_limit_blocks_total` | counter | `peer` | Number of times a stream open blocked because the QUIC `max_concurrent_streams` ceiling was reached. Tracks how often the peer is saturated. |
| `aster_transport_streaming_substreams_active` | gauge | `peer` | Number of active streaming substreams (server/client/bidi calls in progress). Useful for capacity planning against the QUIC ceiling. |
| `aster_transport_pool_size_effective` | gauge | `peer`, `pool` | The *effective* pool size after connect-time QUIC ceiling clamping (see §9). When this is below the configured `shared_pool_size`, the peer is constraining us. |
| `aster_transport_active_sessions_per_connection` | gauge | `peer` | Number of currently-active sessions on this inbound connection (server-side, see §7.5). Sustained values near `max_sessions_per_connection` mean the cap is constraining the peer; sustained low values mean the cap is sized fine. |
| `aster_transport_session_limit_rejections_total` | counter | `peer` | Server-side `open_session()` attempts rejected with `RESOURCE_EXHAUSTED` because the connection had already hit `max_sessions_per_connection`. A nonzero value means at least one peer is hitting the cap; investigate. |

The `acquire_wait_seconds` histogram is the diagnostic metric — it tells operators *whether* the pool is the bottleneck before they have to guess. The `acquire_timeouts_total` counter is the alerting metric. The `pool_size_effective` gauge is the "is the peer the limit?" metric. The `session_limit_rejections_total` counter is the "is a peer trying to OOM us?" metric.

These metrics live in `core` and are exposed through the existing transport metrics surface (`transport_metrics()` in Python, parity in TS/Java).

**Forward-looking note: consolidating all metrics in `core`.** This document scopes only the new transport pool metrics, but the same logic that argues for moving the streaming state machine into `core` (§2, §11) argues for moving *all* metrics there. Today, each binding replicates its own metrics surface — duplication that drifts and is awkward to keep in parity. A separate workstream (not blocking this design) should pull every metric definition into `core` and have each binding expose the same set via a single FFI call. The new metrics in this spec should be authored in `core` from the outset, so they land in the right place even before the broader consolidation happens.

---

## 11. Migration Plan

The whole change lands on a single feature branch. Because the FFI surface is being unified rather than paralleled (see §8), the bindings' server-side and client-side paths must migrate in lockstep with the core change — there is no intermediate state where the old `aster_reactor_submit_*` ops coexist with the new `aster_call_*` ops in the merged tree. This is fine on a feature branch; it would be expensive on `main`.

The branch strategy keeps the old reactor surface available in `main` for side-by-side comparison if anything goes catastrophically wrong, without paying the cost of maintaining two abstractions in the merged tree.

### Branch objectives (in order)

The branch is structured around four objectives, with a hard gate after Objective 1.

**Objective 1: Core + FFI change + Java end-to-end proof.**
Java is the lead binding because it has the most complete server-side already and is where a meaningful E2E benchmark can be run to confirm the SHARED cliff is actually gone. **This is the gate** — pause and reassess before continuing to Objective 2.

- Pool primitives + call-handle types in `core/src/lib.rs`. Smallest landing-able piece, pure Rust, unit-testable in isolation.
- `core/src/reactor.rs`: unify on `MultiplexedCallReader`. Drop the `is_session_call` discriminator. Route by `sessionId`. Merge `IncomingCall` with the new unified `Call` type where possible.
- Wire format: `StreamHeader.sessionId` field. `StreamHeader.method` becomes optional (per §12.3, removed in a follow-up cleanup PR).
- `ffi/src/call.rs`: new unified call-ops family — `aster_call_acquire`, `aster_call_send_frame`, `aster_call_send_trailer`, `aster_call_recv_frame`, `aster_call_release`, `aster_call_buffer_release`, `aster_call_unary`.
- `ffi/src/reactor.rs`: shrink. Delete `aster_reactor_submit_frame`, `aster_reactor_submit_trailer`, `aster_reactor_recv_frame`, `aster_reactor_buffer_release`. Update `aster_reactor_poll` to return unified `CallHandle`s.
- Java client migration: `AsterClient` and `BidiCall` lose their hand-rolled framing, become thin shims over `aster_call_*`.
- Java server migration: `AsterServer` migrates from `aster_reactor_submit_*` to `aster_call_*`. The session discriminator check is removed.
- **Acceptance:** existing Java test suite passes. `MissionControlE2ETest` passes against the new FFI surface. SHARED benchmark shows the stream-per-call cliff is gone (Scenario 1 from §4). No unexpected regressions on the in-process Java MC benchmark.
- **Gate:** if the benchmark shows the cliff is gone and no regressions, proceed to Objective 2. If the benchmark is ambiguous or regresses elsewhere, stop and investigate before touching Python and TS.

**Objective 2: Python binding migration.**
Mechanical follow-on once Objective 1 is proven. Both server and client paths migrate together because the FFI surface is unified.

- **Client side:** `bindings/python/aster/transport/iroh.py` sheds its streaming state machines (`server_stream`, `client_stream`, `bidi_stream`). Each becomes a thin shim over `aster_call_*`. The unary fast path stays.
- **Server side:** `bindings/python/aster/server.py` migrates from the old reactor per-call ops to `aster_call_*`. The `is_session_stream != is_session_service` discriminator check is removed.
- **Acceptance:** `uv run pytest tests/python/ -v --timeout=30` green. Transport file shrank meaningfully (deletion-heavy migration is the smell test that we did it right).

**Objective 3: TypeScript binding migration.**
Largest deletion of the three since TS does even unary in the framing state machine today. Both server and client paths migrate together.

- **Acceptance:** TS test suite green. Same shrinkage check.

**Objective 4: Go and .NET — compile-only.**
Functional parity for Go and .NET is **out of scope** for this branch. They are admitted to be behind Java today, and finishing them would either bloat the scope or produce half-finished impls that drift. They are tracked as a follow-up.

The bar for this branch is "they still compile and their non-stubbed tests still pass." Concretely:

- **Native declarations updated.** Go `cgo` declarations and .NET P/Invoke declarations point at the new `aster_call_*` symbol names. Otherwise the packages won't link.
- **Higher-level transport code: stub or migrate.** Cheap cases (where the `aster_call_*` mapping is mechanical) can be functionally migrated. Anything else is stubbed with `ErrNotImplemented` (Go) or `NotImplementedException` (.NET) plus a `TODO(multiplexed-streams):` comment so the follow-up is grep-able.
- **Acceptance:** Go and .NET packages compile and link. Existing tests that don't exercise stubbed paths pass. Stubbed paths are skipped or marked expected-failure.

### Cross-cutting work (lands alongside the objectives)

- **Tests.** Cross-binding parity tests for the new pool semantics (Scenarios 1–5 from §4). Perf benchmarks for Scenario 1 confirming the SHARED cliff is gone. Existing session tests should pass unchanged. New tests for the unified call-ops surface exercised from both inbound and outbound paths.
- **Docs.** Update the binding READMEs to point at the config keys in §9 and the metrics in §10. Update `ffi_spec/FFI_API_SURFACE.md` to reflect the unified `aster_call_*` family and the shrunk `aster_reactor_*` family.

### Merge gate

All four objectives complete to their respective acceptance criteria. Java, Python, and TS suites all green. SHARED benchmark shows the cliff gone. Go and .NET compile and link with their stubbed paths skipped. No regressions in existing session or unary suites. Then merge to `main`.

---

## 12. Open Questions

**12.1 — Reactor C-ABI reuse: RESOLVED (2026-04-13).** Assessment complete; outcome documented in §8. Summary: the existing `aster_reactor_*` family is structurally server-side and cannot be reused as-is for client-initiated calls. However, its substrate (`BufferRegistry`, `BridgeRuntime`, per-call mpsc with `block_on`-and-timeout, handle-based opaque identity) is directly applicable. The plan is to add a parallel `aster_call_*` family in a new `ffi/src/call.rs` that mirrors the reactor's shape and shares its substrate. Estimated cost: ~600–800 lines new in `ffi/src/call.rs`, ~300–500 lines new in `core/src/lib.rs` (pool + call-handle types), ~100–200 lines changed in `core/src/reactor.rs` for the unified `MultiplexedCallReader`. Per-binding work is deletion-heavy.

**12.2 — Per-session pool labelling in metrics.** The `pool` label distinguishes `shared` from `session:<id>`. Per-session labels are high-cardinality. **Deferred to implementation:** default to aggregating all session pools under `session` for the `pool` label, with per-session labels behind a verbosity flag if and when an operator needs that detail. No need to lock this down before implementation begins.

**12.3 — `StreamHeader.method` removal timing.** Since back-compat is not a constraint, the field can be removed in the same PR that lands the migration. But keeping it through the migration window simplifies bisecting if something breaks. Decision: keep through migration, remove in a follow-up cleanup PR after all three bindings have shipped the new code.

---

## 13. Non-Goals

**This is not the Python dispatch fan-out perf fix.** The 4× Python/TS unary gap on synthetic benchmarks is caused by Python's single asyncio dispatch loop, not by stream lifecycle. See `docs/_internal/INVESTIGATING_PYTHON_PERF.md` and the `project_ring_buffer_status.md` memory. The two perf threads are independent and should not be conflated. This document's perf wins (eliminating the SHARED stream-per-call cliff) are real but unrelated to the dispatch fan-out problem.

**This is not a session-id-as-authentication mechanism.** The `sessionId` field on `StreamHeader` is a routing key, not a credential. Authentication and authorization for session ownership are handled by the existing trust/identity layer (see `Aster-trust-spec.md`). The server validates that the peer presenting a `sessionId` is the same peer that opened the session originally; this is existing logic, not new in this spec.

**This is not a QUIC tuning document.** The QUIC `max_concurrent_streams` ceiling appears in this spec because the client must react to it gracefully, but tuning the ceiling itself (peer-advertised limits, flow-control windows, congestion control) is out of scope.
