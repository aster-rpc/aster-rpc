# Session Protocol

Implementation flow for session-scoped services. Multiple RPC calls share
a single bidirectional QUIC stream. The server instantiates one service
instance per session, preserving state across calls.

**Spec:** Aster-session-scoped-services.md

## Wire protocol overview

```
Client                                   Server
   │                                        │
   │── StreamHeader(method="", service=X) ─▶│  ← session opening
   │                                        │── instantiate service(peer=...)
   │── CALL frame (CallHeader) ────────────▶│  ← call 1 start
   │── request data frame ─────────────────▶│
   │◀─ response data frame ────────────────│  ← unary: no trailer
   │                                        │
   │── CALL frame (CallHeader) ────────────▶│  ← call 2 start
   │── request data frame ─────────────────▶│
   │◀─ response data frame(s) ─────────────│  ← server_stream
   │◀─ TRAILER(OK) ────────────────────────│
   │                                        │
   │── send.finish() ──────────────────────▶│  ← session close
   │                                        │── on_session_close()
```

The key discriminator: `StreamHeader.method == ""` signals session mode.
A non-empty method on a session-scoped service triggers
FAILED_PRECONDITION with an actionable error message.

## Session opening

### Server-side discriminator

```python
if header.method == "" and service_info.scoped == "session":
    # Session mode — enter CALL frame loop
elif header.method == "" and service_info.scoped == "shared":
    # Scope mismatch — reject
elif header.method != "" and service_info.scoped == "session":
    # Client tried a one-shot call on a session service — reject
```

The error message must be clear:
`"'AgentSession' is session-scoped: open a session stream (method='') instead of calling method 'register' directly"`

**Python:** `server.py` dispatch logic around line 400.
**TypeScript:** `server.ts` dispatch logic around line 190.

### Service instantiation

The server creates a **fresh instance per session** so each client gets
its own state. The constructor receives `peer=<endpoint_id>`:

```python
instance = service_class(peer=peer)
```

If the constructor fails, write an INTERNAL trailer and close the stream.

**TypeScript note:** TS tries `new ctor()` first; if that fails, falls
back to reusing the registered instance (line 69-73 in session.ts).

## Per-call framing

### Frame flags

| Flag | Value | Meaning |
|------|-------|---------|
| COMPRESSED | 0x01 | Payload is zstd-compressed |
| TRAILER | 0x02 | Frame carries an RpcStatus |
| HEADER | 0x04 | First frame on stream (StreamHeader) |
| ROW_SCHEMA | 0x08 | ROW mode schema hoisting |
| CALL | 0x10 | Session call boundary (CallHeader) |
| CANCEL | 0x20 | Cancel in-flight call |

### CallHeader

Sent as the payload of a CALL frame:

```
method: string        — method name to dispatch
callId: string        — unique ID for this call (UUID)
deadlineEpochMs: int  — 0 = no deadline, else absolute epoch ms
metadataKeys: string[]
metadataValues: string[]
```

### Metadata validation

Before dispatching, validate CallHeader metadata against limits:
- `MAX_METADATA_ENTRIES` (64)
- `MAX_METADATA_KEY_LEN` (256)
- `MAX_METADATA_VALUE_LEN` (4096)
- `MAX_METADATA_TOTAL_BYTES` (8192)

Reject with RESOURCE_EXHAUSTED if exceeded.

**Python:** `validate_metadata()` at `session.py:270`.
**TypeScript:** `validateMetadata()` at `session.ts:99`.

## Pattern dispatch

After reading the CALL frame and request payload, dispatch based on the
method's registered pattern:

### Unary

1. Read one request data frame
2. Run handler
3. Write one response data frame
4. **No success trailer** — the single response IS the complete response

This is a deliberate spec choice. Streaming patterns need trailers to
signal end-of-stream; unary doesn't.

### Server-stream

1. Read one request data frame
2. Run handler (async generator)
3. Write response data frames as they yield
4. Write TRAILER(OK) after the generator exhausts

### Client-stream

1. Read data frames until TRAILER(OK) end-of-input
2. **Validate EoI:** trailer must have status=OK. Non-OK = INTERNAL error.
3. Run handler with collected items
4. Write one response data frame

**Item cap:** `MAX_CLIENT_STREAM_ITEMS` (100,000). Reject with
RESOURCE_EXHAUSTED if exceeded. Prevents memory exhaustion from a
malicious client sending millions of tiny frames.

### Bidi-stream

1. Start reader task (reads frames into a queue)
2. Start handler with async iterator over the queue
3. Write response frames as handler yields
4. After handler completes, check `reader_error`
5. If reader error: write INTERNAL trailer (not OK)
6. If clean: write TRAILER(OK)

**Critical:** Reader errors must NOT be converted to silent EOF. If the
frame reader hits a decode error, that error must be stored and checked
after the handler finishes. This was the G6 silent corruption bug.

## Cancellation

Client sends a CANCEL frame (flag 0x20, empty payload). Server responds
with **exactly one CANCELLED trailer**, unconditionally:

```python
if flags & CANCEL:
    await _write_trailer(send, codec, StatusCode.CANCELLED, "cancelled")
    continue  # session stays open
```

The session remains open after cancellation. Client drains frames until
it sees the CANCELLED trailer, then can issue the next CALL.

**Python:** The unary dispatcher runs handler and cancel-reader as
concurrent tasks in `asyncio.wait`. If CANCEL wins, handler is cancelled.

**TypeScript:** Fixed to send CANCELLED trailer (was `continue` without
response — the G2 deadlock bug).

## Deadline enforcement

All dispatch methods enforce `deadlineEpochMs` from the CallHeader:

- If no deadline is set (0), the server-side upper bound
  `MAX_HANDLER_TIMEOUT_S` (300s / 5 min) applies.
- If the client's deadline is further than MAX_HANDLER_TIMEOUT_S, it is
  clamped to the server max.
- Server returns DEADLINE_EXCEEDED trailer when the deadline fires.

**Python helper:** `_get_deadline_timeout()` at `session.py:151`.
**TypeScript helper:** `getDeadlineMs()` / `withDeadline()` in session.ts.

## Session close

Client calls `send.finish()` to close the send side. Server's frame pump
returns None (EOF), exits the CALL loop. Server fires `on_session_close()`
lifecycle hook if present on the instance.

## Lock semantics (client-side)

Python `SessionStub` holds an `asyncio.Lock` per session. Lock is acquired
before sending the CALL frame and released after reading the complete
response. This serializes calls — only one in-flight call per session.

Confirmed by chaos tests: concurrent callers on one stub produce a
correct linearizable history (`test_linearizable_increment`).

## Auth interceptors within sessions

CapabilityInterceptor runs **per CALL**, not per session. An auth denial
writes an error trailer but **continues the session loop** (doesn't kill
the session). The client can make other calls that pass auth.

Peer attributes come from PeerAttributeStore, populated at admission time.

## Invariants confirmed by chaos tests

- No cross-talk between concurrent sessions (`test_no_crosstalk_between_sessions`)
- Session state isolation (`test_session_state_isolation`)
- Counter monotonicity under contention (`test_monotonicity_under_contention`)
- Linearizable increment under lock contention (`test_linearizable_increment`)
- Session churn does not leak resources (`test_soak_session_churn`)
- EoI trailer status=OK validated (`test_g4_client_stream_non_ok_eoi`)
- Bidi reader errors propagated (`test_g6_bidi_reader_error_not_silent_eof`)
- CANCEL produces CANCELLED trailer (`test_g2_cancel_produces_cancelled_trailer`)
- Deadline enforced (`test_g8_deadline_enforced_in_session`)
- Corrupt payload produces error trailer (`test_g12_corrupt_payload_produces_error_trailer`)

## Naming conventions (wire compatibility)

These field names in CallHeader/StreamHeader are part of the wire protocol:

| Wire name | Notes |
|-----------|-------|
| `method` | Empty string `""` for session opening |
| `callId` | camelCase on wire (both Python and TS) |
| `deadlineEpochMs` | camelCase on wire |
| `metadataKeys` | camelCase on wire |
| `metadataValues` | camelCase on wire |
| `serializationMode` | camelCase on wire, integer value |

RpcStatus fields:

| Wire name | Type |
|-----------|------|
| `code` | Integer (StatusCode enum value) |
| `message` | String |
| `detailKeys` | String array |
| `detailValues` | String array |

Internal names (Python properties, method names) can use snake_case.
The wire names are what the codec sees.

## Implementation checklist for new bindings

- [ ] Detect session mode: `StreamHeader.method == ""`
- [ ] Scope discriminator with clear error messages
- [ ] Fresh service instance per session with `peer` parameter
- [ ] CALL frame loop with metadata validation
- [ ] Four pattern dispatchers (unary, server_stream, client_stream, bidi)
- [ ] Unary: no success trailer
- [ ] Client-stream: validate EoI trailer status=OK
- [ ] Client-stream: enforce MAX_CLIENT_STREAM_ITEMS
- [ ] Bidi: propagate reader errors (not silent EOF)
- [ ] CANCEL: send exactly one CANCELLED trailer
- [ ] Deadline enforcement with MAX_HANDLER_TIMEOUT_S upper bound
- [ ] Per-call auth interceptors (not per-session)
- [ ] `on_session_close()` lifecycle hook
- [ ] Client-side lock serializing calls on one session

## Key files

| Binding | File | Entry point |
|---------|------|-------------|
| Python | `session.py:168` | `SessionServer` class |
| Python | `session.py:189` | `SessionServer.run()` |
| Python | `session.py:221` | `SessionServer._session_loop()` |
| Python | `session.py:357` | `SessionServer._pump_frames()` |
| Python | `session.py:833` | `SessionStub` class |
| Python | `session.py:1143` | `create_local_session()` |
| TS | `session.ts:44` | `SessionServer` class |
| TS | `session.ts:58` | `SessionServer.handleSession()` |
