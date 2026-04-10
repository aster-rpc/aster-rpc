# Session Protocol

**Status:** Stub -- to be filled after chaos tests confirm invariants.

**Reference:** `bindings/python/aster/session.py` SessionServer + SessionStub

## What this flow covers

The wire protocol for session-scoped services where multiple RPC calls
share a single bidirectional QUIC stream. Covers both server-side
dispatch and client-side call multiplexing.

## Sections to write

### 1. Session opening
- Client sends StreamHeader with `method=""`, `service=<name>`, `callId=<session_id>`
- Server checks scope discriminator: `method=="" + scoped='session'` match; mismatch -> FAILED_PRECONDITION with actionable error message
- Server instantiates a fresh service class per session with `peer` parameter
- Server enters CALL frame loop

### 2. Per-call framing
- Client sends CALL frame (flag 0x10) with CallHeader (`method`, `callId`, `deadlineEpochMs`, metadata)
- Client sends request data frame(s)
- Server dispatches to handler based on pattern (unary, server_stream, client_stream, bidi_stream)
- Server sends response data frame(s)
- Server sends TRAILER frame with RpcStatus per call (streaming patterns)
- **Unary special case:** spec says no success trailer required (Python omits it; TS currently sends it -- to be reconciled)

### 3. Client-stream and bidi end-of-input
- Client signals end-of-input with explicit TRAILER(OK) frame -- NOT `send.finish()`
- `finish()` is reserved for session close
- Server must validate that EoI trailer has status=OK (not just any TRAILER flag)

### 4. Cancellation
- Client sends CANCEL frame (flag 0x20) to cancel the in-flight call
- Server MUST respond with exactly one CANCELLED trailer -- unconditionally
- Server cancels the handler task, drains any pending response frames
- Client drains frames until it sees the CANCELLED trailer, then can issue next CALL
- **Known gap (G2):** TS server currently does `continue` on CANCEL without sending trailer

### 5. Session close
- Client calls `send.finish()` to close the send side
- Server's frame reader returns null (EOF), exits the CALL loop
- Server calls `on_session_close()` lifecycle hook if present on the service instance
- Server calls `send.finish()` to close its send side

### 6. Lock semantics (client-side)
- Python SessionStub holds an async lock per session
- Lock is acquired before sending CALL frame, released after reading the complete response
- **Known gap (G1):** if network drops mid-response, lock releases and next call interleaves

### 7. Auth interceptors within sessions
- CapabilityInterceptor runs per CALL, not just per session
- Auth denial writes error trailer and continues the session loop (doesn't kill the session)
- Peer attributes come from PeerAttributeStore, populated at admission time

## Invariants for new implementations

_(To be confirmed by chaos tests, then documented here)_

## Bugs this flow exposed

- TS server had no session handler at all -- returned UNIMPLEMENTED on method=""
- TS server accepted one-shot calls to session-scoped methods (B1 bug)
- Python session server passed default ForyCodec even when client requested JSON
- Unary trailer mismatch caused second call to read stale trailer (TS server) or deadlock (Python server, if trailer expected)
