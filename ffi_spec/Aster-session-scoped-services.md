---
title: "Aster Session-Scoped Services"
sidebar_label: "Session-Scoped Services"
sidebar_position: 4
description: "Session-scoped service model for Aster -- per-stream instances, sequential call semantics, in-band cancellation, and wire protocol extensions"
---

# Aster Spec Addendum: Session-Scoped Services

**Version:** 0.7.2 (tracking toward 1.0)
**Status:** Pre-release (0.1-alpha)
**Last Updated:** 2026-04-06
**Applies to:** Spec v0.7.1+
**Sections affected:** §6.1, §6.3, §7, §8, §16

-----

## 1. Motivation

Aster's default service model is stateless: each RPC opens an independent QUIC
stream, the server dispatches to a shared service instance, and no state
accumulates between calls. This is the right default for most services.

However, some workloads are inherently conversational. An agent task session, a
multiplayer game lobby, a collaborative editing context, or a stateful
negotiation protocol all share a pattern: the consumer opens a logical session,
makes a sequence of typed calls against shared mutable state, and eventually
closes the session. Today, developers working around this either maintain
external session stores keyed by metadata tokens, or collapse their typed
methods into a single `@bidi_stream` with envelope types and manual dispatch.

Session-scoped services give developers the ergonomics of typed multi-method
services with the lifecycle semantics of a persistent stream — no external
state store, no envelope boilerplate, no manual dispatch.

-----

## 2. Design

### 2.1 Core Idea

A session-scoped service instantiates a new class instance per QUIC stream. The
stream remains open across multiple sequential RPC calls. Each call is
dispatched to a typed method on that instance, exactly as in the stateless
model. The stream lifecycle is the session lifecycle — when the stream closes,
the instance is torn down.

### 2.2 Scoping Modes

The `@service` decorator accepts a `scoped` parameter:

| `scoped=`            | Instance per       | Lifetime                | State across calls |
|----------------------|--------------------|-------------------------|--------------------|
| `"shared"` (default) | One for all peers   | Server lifetime         | None (stateless)   |
| `"stream"`           | QUIC stream         | Stream open → close     | Yes (instance vars)|

`"shared"` is the existing behaviour. `"stream"` is the new mode described in
this addendum. No other scoping modes are introduced.

### 2.3 Sequential Call Semantics

Calls within a session stream are **strictly sequential**. Only one method
invocation is in flight at any time. This is enforced by a client-side async
lock: when the consumer issues a call, the lock is acquired; when the response
(or final streaming item) is received, the lock is released and the next queued
call proceeds.

This eliminates the need for:

- **Correlation IDs.** There is only ever one outstanding request-response
  exchange on the stream — the response is unambiguously correlated to the
  most recent request.
- **Multiplexing / demultiplexing.** Frames are never interleaved. The stream
  carries one call at a time in the same request → response sequence as
  stream-per-RPC.
- **Concurrency control on the server.** The handler processes one call at a
  time. Instance state (`self.*`) is accessed single-threaded with no locking
  required.

If the consumer needs parallelism, it opens multiple sessions — each is an
independent stream with an independent instance.

-----

## 3. Service Definition

### 3.1 Decorator Surface (Python)

```python
from aster import service, rpc, server_stream, client_stream, bidi_stream
from aster import SerializationMode
from dataclasses import dataclass
from typing import AsyncIterator

@service(name="AgentControl", version=1, scoped="session",
         serialization=[SerializationMode.XLANG])
class AgentControlSession:

    def __init__(self, peer: EndpointId):
        # Called once when the session stream is accepted.
        # peer is the remote endpoint's cryptographic identity.
        self.peer = peer
        self.task_count = 0
        self.active_task: str | None = None

    async def on_session_close(self):
        # Optional lifecycle hook. Called when the stream closes
        # (consumer finish(), connection drop, or QUIC reset).
        # Use for cleanup: release resources, flush logs, etc.
        ...

    @rpc(timeout=30.0, idempotent=True)
    async def assign_task(self, req: TaskAssignment) -> TaskAck:
        self.task_count += 1
        self.active_task = req.task_id
        return TaskAck(accepted=True)

    @rpc
    async def cancel_task(self, req: CancelRequest) -> CancelAck:
        self.active_task = None
        return CancelAck(cancelled=True)

    @server_stream
    async def step_updates(self, req: TaskId) -> AsyncIterator[StepUpdate]:
        async for update in self.agent_loop.run(req):
            yield update

    @bidi_stream
    async def approval_loop(
        self, requests: AsyncIterator[ApprovalRequest]
    ) -> AsyncIterator[ApprovalResponse]:
        async for req in requests:
            yield ApprovalResponse(approved=req.risk_level != RiskLevel.CRITICAL)
```

All four RPC patterns (`@rpc`, `@server_stream`, `@client_stream`,
`@bidi_stream`) are supported within a session-scoped service. The decorator
semantics are identical to the stateless model — the only difference is that
`self` is persistent across calls and private to this stream.

### 3.2 Lifecycle Hooks

| Hook                  | When called                              | Required |
|-----------------------|------------------------------------------|----------|
| `__init__(self, peer)`| Stream accepted, before first call       | Yes      |
| `on_session_close()`  | Stream closed (any cause)                | No       |

`on_session_close` is called regardless of how the stream ends: consumer
`finish()`, connection drop, QUIC reset, or server-initiated shutdown. It is a
cleanup hook, not a signal to send further data.

-----

## 4. Wire Protocol

### 4.1 Session Stream Header

The first frame on a session stream uses the existing `HEADER` flag (bit 2,
`0x04`). The `StreamHeader` payload carries the service and contract identity
as usual. A new field signals session scoping:

```text
StreamHeader {
    service: string
    method: string              // Empty string "" for session-scoped streams
    version: int32
    contract_id: string
    call_id: string             // Session ID (unique per session, not per call)
    deadline_epoch_ms: int64    // 0 for session open; per-call deadlines on call headers
    serialization_mode: uint8
    metadata_keys: list<string>
    metadata_values: list<string>
}
```

When `method` is an empty string and the service's contract declares
`scoped="session"`, the server treats this as a session stream. The `call_id`
serves as the session identifier for logging and tracing.

### 4.2 Per-Call Header

After the initial `StreamHeader`, each call within the session is introduced by
a **call header frame**. This is a lightweight frame with a new flag — bit 4
(`0x10`): `CALL` — whose payload is always Fory XLANG-serialized:

```text
CallHeader {
    method: string              // e.g. "assign_task"
    call_id: string             // Unique per call (for tracing)
    deadline_epoch_ms: int64    // Per-call deadline, 0 = none
    metadata_keys: list<string>
    metadata_values: list<string>
}
```

The `CALL` flag distinguishes call headers from data payloads. A session stream
alternates between call headers and their associated request/response frames.

### 4.3 Flags Byte (Updated)

| Bit | Mask   | Name         | Meaning                                            |
|-----|--------|--------------|----------------------------------------------------|
| 0   | `0x01` | `COMPRESSED` | Payload is zstd-compressed                         |
| 1   | `0x02` | `TRAILER`    | Trailing status frame                              |
| 2   | `0x04` | `HEADER`     | Stream header (first frame)                        |
| 3   | `0x08` | `ROW_SCHEMA` | Fory row schema (first data frame of ROW stream)   |
| 4   | `0x10` | `CALL`       | Per-call header within a session stream             |
| 5   | `0x20` | `CANCEL`     | Cancel the current in-flight call (see §5)          |
| 6–7 |        | Reserved     | Must be zero                                       |

### 4.4 Session Stream Lifecycle

```text
Client                                          Server
  │                                               │
  ├─ [HEADER] StreamHeader (method="") ──────────►│  instantiate service class
  │                                               │
  │  ── Call 1: Unary ──────────────────────────  │
  ├─ [CALL] CallHeader (method="assign_task") ───►│
  ├─ Request payload ────────────────────────────►│
  │                              Response payload ◄─┤
  │                                               │
  │  ── Call 2: Server Stream ──────────────────  │
  ├─ [CALL] CallHeader (method="step_updates") ──►│
  ├─ Request payload ────────────────────────────►│
  │                          Response frame 1     ◄─┤
  │                          Response frame N     ◄─┤
  │                          [TRAILER] OK         ◄─┤
  │                                               │
  │  ── Call 3: Unary ──────────────────────────  │
  ├─ [CALL] CallHeader (method="cancel_task") ───►│
  ├─ Request payload ────────────────────────────►│
  │                              Response payload ◄─┤
  │                                               │
  ├─ finish() ───────────────────────────────────►│  on_session_close(), teardown
  │                                               │
```

### 4.5 Call Framing Rules

1. The stream begins with exactly one `HEADER` frame.
2. Each subsequent call begins with exactly one `CALL` frame.
3. After the `CALL` frame, request and response frames follow the same
   sequencing rules as their equivalent stream-per-RPC pattern (§6.3):
   - **Unary:** one request frame, one response frame.
   - **Server stream:** one request frame, N response frames, one trailer.
   - **Client stream:** N request frames, then the client sends a `TRAILER`
     with status `OK` (code 0) to signal end-of-input for this call. The
     server reads until it receives the client's `TRAILER`, then sends one
     response frame. (In stream-per-RPC, `finish()` signals end-of-input; in
     sessions, `finish()` means session close, so an explicit client `TRAILER`
     is required instead.)
   - **Bidi stream:** concurrent request and response frames. The client
     signals end-of-input by sending a `TRAILER` with status `OK` (code 0).
     The server must then send its own `TRAILER` to complete the call. The
     client must wait for the server's `TRAILER` before releasing the session
     lock and sending the next `CALL` frame.
4. `finish()` from the client at a call boundary (not mid-call) signals session
   close. The server calls `on_session_close()` and finishes its side.
5. A `CALL` frame must not appear while a previous call is still in flight.
   Implementations must reject a `CALL` frame received mid-call with
   `FAILED_PRECONDITION` and reset the stream.

### 4.6 Trailer Semantics in Sessions

- **Unary calls** within a session do **not** require a trailer frame. A
  successful unary response is the response payload alone. Errors are signalled
  by a trailer with a non-OK status code instead of a response payload.
- **Streaming calls** use trailers as in the stateless model (§6.3).
- A trailer with a non-OK status code terminates the **current call**, not the
  session. The session remains alive and the server awaits the next `CALL`
  frame. The exception: `INTERNAL` errors may indicate corrupted instance state;
  servers should log a warning but continue unless the implementation determines
  recovery is impossible.

-----

## 5. In-Band Cancellation

### 5.1 Problem

In stream-per-RPC, cancelling a call is simple: reset the QUIC stream
(`RESET_STREAM`). In a session stream, resetting the stream kills the entire
session. Cancellation must therefore be **in-band** — a frame on the stream
that cancels only the current call while preserving the session.

### 5.2 Cancel Frame

A cancel frame is a frame with bit 5 (`0x20`): `CANCEL` set in the flags byte.
The payload is empty (Length = 1, Flags only). Because calls are sequential,
there is at most one call in flight — the cancel is unambiguous.

```text
┌─────────────┬──────────┐
│ Length: 1    │ CANCEL   │
│ (4B LE u32) │ (0x20)   │
└─────────────┴──────────┘
```

### 5.3 Cancellation Lifecycle

```text
Client                                          Server
  │                                               │
  ├─ [CALL] CallHeader (method="step_updates") ──►│  handler starts
  ├─ Request payload ────────────────────────────►│
  │                          Response frame 1     ◄─┤
  │                          Response frame 2     ◄─┤
  ├─ [CANCEL] ───────────────────────────────────►│  handler cancelled
  │                    [TRAILER] CANCELLED        ◄─┤
  │                                               │
  │  (session alive, next call proceeds)          │
  │                                               │
  ├─ [CALL] CallHeader (method="assign_task") ───►│  new call, same instance
  ├─ Request payload ────────────────────────────►│
  │                              Response payload ◄─┤
  │                                               │
```

### 5.4 Server Behaviour on CANCEL

1. The server receives the `CANCEL` frame.
2. The server cancels the handler using the language's native mechanism:
   - Python: `task.cancel()` → raises `asyncio.CancelledError` in the handler.
   - Rust: drop the handler future.
   - Go: cancel the `context.Context`.
   - JVM: cancel the `CompletableFuture` / coroutine `Job`.
3. The server sends a trailer frame with status `CANCELLED` (code 1).
4. The server awaits the next `CALL` frame or `finish()`.

**Race condition with in-flight writes.** At the moment CANCEL arrives, the
server may have already queued or written an OK trailer (or an error trailer
from the handler's own completion). The server is NOT required to retract
that trailer. Instead, the framework guarantees that whenever the server
receives a CANCEL frame for an active call, it will emit **exactly one
trailer with status `CANCELLED`** to terminate that call — even if another
trailer was emitted first. The client treats CANCELLED as the authoritative
terminator (see §5.5).

:::warning
The handler is responsible for leaving `self` in a consistent state after cancellation. The framework delivers the cancellation signal but cannot guarantee cleanup -- this is inherent to cancellation in any system. Developers should use `try/finally` (or language equivalent) in handlers that acquire resources.
:::

### 5.5 Client Behaviour on CANCEL

After sending a `CANCEL` frame, the client must drain and discard any response
frames from the server until it receives a trailer whose status is
`CANCELLED`. This handles two races:

- The server may have already written response frames before processing the
  cancel — the client drops them.
- The server may have already written an OK (or other non-CANCELLED) trailer
  before processing the cancel — the client drops that trailer too and keeps
  reading until CANCELLED arrives.

The CANCELLED trailer is authoritative: once the client reads it, the in-flight
call is terminated (regardless of whether an earlier trailer claimed
success). The client then releases the session lock and the next queued
call may proceed.

### 5.6 CANCEL on Non-Session Streams

The `CANCEL` flag must not be sent on a non-session (stream-per-RPC) stream.
Implementations receiving a `CANCEL` frame on a non-session stream should
ignore it and may log a protocol warning. Stream-per-RPC cancellation remains
`RESET_STREAM`.

-----

## 6. Client API

### 6.1 Session Stub (Python)

```python
from aster import create_session

# Open a session — opens a QUIC stream, sends StreamHeader, server instantiates
session = await create_session(AgentControlSession, connection=conn)

# Typed calls — sequential, async-locked internally
ack = await session.assign_task(TaskAssignment(
    task_id="t1",
    workflow_yaml="...",
    credential_refs=["cred-a"],
    step_budget=100,
))

# Streaming call within the session
async for update in session.step_updates(TaskId(task_id="t1")):
    print(f"Step {update.step_number}: {update.status}")
    if update.status == "stuck":
        break  # consumer stops iterating → CANCEL sent

# Another unary call — same instance, task_count is now 1
ack2 = await session.assign_task(TaskAssignment(...))

# Close the session
await session.close()  # finish() on the stream → on_session_close() on server
```

### 6.2 Multiple Sessions

```python
# Two independent sessions on the same connection
session_a = await create_session(AgentControlSession, connection=conn)
session_b = await create_session(AgentControlSession, connection=conn)

# Fully independent instances on the server
await session_a.assign_task(task_1)  # instance A: task_count = 1
await session_b.assign_task(task_2)  # instance B: task_count = 1

await session_a.close()
await session_b.close()
```

### 6.3 Cancellation from Consumer

Cancellation is exposed through standard language async idioms:

```python
import asyncio

# Cancel a long-running call
task = asyncio.create_task(session.step_updates(TaskId(task_id="t1")))
await asyncio.sleep(5.0)
task.cancel()  # internally sends CANCEL frame, waits for CANCELLED trailer

# Session is still alive
ack = await session.assign_task(...)
```

For streaming calls, breaking out of the async iteration sends a `CANCEL`
frame automatically:

```python
async for update in session.step_updates(TaskId(task_id="t1")):
    if should_stop(update):
        break  # CANCEL frame sent, CANCELLED trailer received
```

### 6.4 Session Lock Semantics

The session stub holds an internal `asyncio.Lock` (or language equivalent). All
public methods acquire the lock before writing to the stream and release it
after the response is fully received. This means:

- Concurrent `await` calls from different coroutines are safe — they queue.
- The developer does not need to manage synchronization.
- Deadlock is impossible: the lock is held only for the duration of one
  request-response exchange.

```python
# These can be launched concurrently — the stub serializes them
async with asyncio.TaskGroup() as tg:
    tg.create_task(session.assign_task(task_1))  # executes first
    tg.create_task(session.assign_task(task_2))  # queued, executes second
```

-----

## 7. Server API

### 7.1 Server Accept Loop (Updated)

The server accept loop gains a branch for session-scoped services:

```text
1. endpoint.accept() → Connection
2. Per connection: loop on connection.accept_bi()
3. Per stream: read first frame (HEADER flag)
4. Read StreamHeader:
   a. If method != "" → stateless dispatch (existing behaviour)
   b. If method == "" and service is scoped="session":
      i.   Instantiate service class: impl = ServiceClass(peer=remote_endpoint_id)
      ii.  Enter session loop:
           - Read next frame
           - If CALL flag → dispatch to impl.{method}(payload)
           - If CANCEL flag → cancel current handler
           - If finish() → call impl.on_session_close(), exit loop
      iii. On stream reset or connection drop → call impl.on_session_close()
```

### 7.2 Implementation (Python)

```python
class Server:
    async def _handle_session_stream(
        self,
        service_class: type,
        send: SendStream,
        recv: RecvStream,
        peer: EndpointId,
        header: StreamHeader,
    ):
        impl = service_class(peer=peer)

        try:
            while True:
                frame = await read_frame(recv)

                if frame is None:
                    # Client called finish() — clean session close
                    break

                if frame.flags & CANCEL:
                    if self._current_handler:
                        self._current_handler.cancel()
                        await write_trailer(send, StatusCode.CANCELLED)
                    continue

                if frame.flags & CALL:
                    call_header = deserialize_call_header(frame.payload)
                    handler = getattr(impl, call_header.method)
                    await self._dispatch_session_call(handler, call_header, send, recv)

        finally:
            if hasattr(impl, "on_session_close"):
                await impl.on_session_close()
```

-----

## 8. Transport Abstraction

### 8.1 LocalTransport

`LocalTransport` supports session-scoped services. `create_session` with a
local transport instantiates the service class directly and routes calls
through the interceptor chain and (optionally) the Fory serialize/deserialize
roundtrip, exactly as described in §8.3 of the main spec.

```python
# In-process session — same API, no network
session = await create_session(
    AgentControlSession,
    transport=LocalTransport(implementation_class=AgentControlSession),
)
ack = await session.assign_task(task)
await session.close()
```

### 8.2 Interceptors

Interceptors run on every call within a session, not once per session open.
The `CallContext` is populated per-call with the method name from the
`CallHeader`. Session-level context (peer identity, session ID) is available
on the context throughout. If rcan authorization is enabled (see Trust Spec) then claims are also included in the `CallContext`

```python
class CallContext:
    service: str
    method: str             # Per-call method name
    call_id: str            # Per-call unique ID
    session_id: str | None  # Non-None for session-scoped calls
    peer: EndpointId
    metadata: dict[str, str]
    deadline: float | None
    is_streaming: bool
```

-----

## 9. Interaction with Existing Spec Concepts

### 9.1 Serialization Modes

All serialization modes (`XLANG`, `NATIVE`, `ROW`) work within session streams.
The serialization mode is declared once on the `StreamHeader` and applies to
all calls on that stream. Per-call serialization override is not supported in
session streams — all calls within a session use the same mode.

### 9.2 Compression

Compression applies per-frame, exactly as in §5.6. Individual call payloads
within a session may be independently compressed.

### 9.3 Deadlines

Per-call deadlines are carried in the `CallHeader.deadline_epoch_ms` field.
There is no session-level deadline. Session lifetime is controlled by the
stream lifecycle (consumer `close()` or connection drop), not by a timer.

### 9.4 Retry

Retry semantics are per-call, not per-session. An idempotent call that fails
within a session may be retried by the `RetryInterceptor` on the same session
stream (the lock ensures sequential execution). A session stream failure
(stream reset, connection drop) is not retried — the session is gone and must
be re-established by the consumer.

### 9.5 ROW Schema Hoisting

ROW mode schema hoisting (§5.5.2) applies per streaming call within a session,
not per session. Each `@server_stream` or `@bidi_stream` call that uses ROW
mode sends its own `ROW_SCHEMA` frame as the first data frame of that call.

### 9.6 ALPN

Session streams use the same `aster/1` ALPN. No new ALPN is introduced.

-----

## 10. Constraints and Non-Goals

### 10.1 What Session-Scoped Services Are Not

- **Not durable sessions.** Session state lives in memory on the server for the
  lifetime of the stream. If the connection drops, the session is gone. Durable
  sessions (surviving reconnects) are an application concern — persist state
  externally and restore on a new session if needed.

- **Not a replacement for stream-per-RPC.** The default stateless model remains
  correct for the majority of services. Session scoping adds a client-side lock
  and a persistent stream — these are costs, even if small. Use session scoping
  only when shared mutable state across calls is a genuine requirement.

- **Not concurrent within a session.**

:::info Design Decision
The sequential call model within a session is a feature, not a limitation. It eliminates an entire class of concurrency bugs on shared instance state. Parallelism is achieved by opening multiple sessions.
:::

### 10.2 Deferred to v2

| Topic                    | Notes                                                             |
|--------------------------|-------------------------------------------------------------------|
| Session handover         | Migrate a session from one server instance to another.            |
| Session resumption       | Reconnect to a session after a transient connection drop.         |
| Session migration        | Move a session across connections on the same server.             |
| Session timeout          | Server-initiated idle timeout for sessions with no calls.        |
| Session metadata events  | Server-to-client push on the session stream between calls.       |
| Concurrent calls         | Opt-in parallelism within a session (requires correlation IDs).  |

-----

## Appendix A: Summary of Wire Changes

| Change                     | Type        | Details                                              |
|----------------------------|-------------|------------------------------------------------------|
| `CALL` flag (bit 4, 0x10)  | New flag    | Introduces per-call header within a session stream   |
| `CANCEL` flag (bit 5, 0x20)| New flag    | In-band cancellation of the current call             |
| `CallHeader` type          | New type    | method, call_id, deadline, metadata                  |
| `StreamHeader.method = ""` | Convention  | Signals session-scoped stream (vs. single-RPC stream)|

**Changes to contract identity (§11.3):** `ServiceContract` gains a `scoped`
field (field 6). A session-scoped service (`scoped = "session"`) and a shared
service (`scoped = "shared"`) with otherwise identical methods produce
**different `contract_id` values**. This prevents a client from accidentally
connecting to a scoped endpoint with a stateless stub or vice versa.

No changes to framing (§6.1), status codes (§6.5), trailer format (§6.4),
ALPN (§6.6), or any existing flag semantics.

-----

## Appendix B: Updated Flags Byte

```text
Bit 0 (0x01): COMPRESSED   — payload is zstd-compressed
Bit 1 (0x02): TRAILER      — trailing status frame
Bit 2 (0x04): HEADER       — stream header (first frame on stream)
Bit 3 (0x08): ROW_SCHEMA   — Fory row schema (ROW mode streams)
Bit 4 (0x10): CALL         — per-call header within a session stream
Bit 5 (0x20): CANCEL       — cancel current in-flight call (session streams only)
Bit 6:        Reserved (must be zero)
Bit 7:        Reserved (must be zero)
```
