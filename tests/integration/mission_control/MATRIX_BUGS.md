# Mission Control Matrix — Known Bugs

Cross-language interop bugs surfaced by `run_matrix.sh`. Each entry lists
the failing combo(s), the symptom in the matrix output, and what we know
about the root cause.

The matrix dimensions are server-language × client-language × mode
(dev/auth). py-py and ts-ts are the in-language baselines; the failures
below are the cross-language gaps still to close.

---

## B1. TS server accepts session-scoped methods as one-shot bidi streams

**Combos:** ts-ts (masks the bug), ts-py (surfaces it as a different error).

**Symptom:** A client opens a regular bidirectional QUIC stream targeting
`AgentSession.runCommand` (a `scoped: 'session'` service). The TS server
dispatches it through the normal RPC path instead of rejecting the stream
or requiring the session protocol envelope (StreamHeader with `method=""`,
then per-call CALL frames).

**Why it matters:** Session-scoped services exist precisely so multiple
calls share one stream and per-session state can be tracked. Allowing them
to be invoked as one-shot bidi calls bypasses that contract. It also hides
the absence of a real TS session client (see B5) — ts-ts auth currently
appears green only because the server is too permissive.

**Fix sketch:** When the dispatched method belongs to a `scoped: 'stream'`
service, refuse the call with `FAILED_PRECONDITION` ("session protocol
required") unless the StreamHeader was opened in session mode. The session
protocol path should hand off to `SessionServer.handleSession`.

---

## B2. ts-client receives empty payload reading Python streaming responses

**Combos:** py-ts dev (Ch4 register), py-ts auth (Ch5 edge tailLogs,
Ch5 edge runCommand).

**Symptom:**
```
Ch4 register (gpu): RpcError: [UNKNOWN] JSON Parse error: Unrecognized token ''
```

The TS client decodes a frame whose payload is empty / not JSON. The
`Unrecognized token ''` strongly suggests the frame body is zero bytes,
not malformed JSON.

**Suspected cause:** Frame-length reader on the TS side may be consuming
a trailer or sentinel frame as a data frame, or reading 0 bytes after a
clean stream end without breaking the loop. Needs frame-by-frame trace
against a Python server response.

---

## B3. py-client gets `Expected RpcStatus, got NoneType` from TS server

**Combos:** ts-py dev (Ch4 register), ts-py auth (Ch5 edge runCommand).

**Symptom:**
```
Ch4 register (gpu): Expected RpcStatus, got NoneType
```

The Python client reads frames off a TS server stream and expects an
explicit `RpcStatus` trailer at end of stream. It receives `None`,
meaning the TS server closed the stream without writing the trailer.

**Suspected cause:** TS server's bidi/session response writer omits the
final OK trailer in some pattern (likely session-scoped services or after
a streaming response that completes normally). Plan A's "Fix E (TS
session server trailer format)" lives here.

---

## B4. Python `gen-client` does not register types with the Fory codec

**Combos:** ts-py dev (Ch6 generated client).

**Symptom:**
```
Ch6 generated call: [UNKNOWN] <class 'mc_gen.types.mission_control_v1.StatusRequest'> not registered
```

`aster gen-client` produces Python type stubs but the generated client
constructor never calls `ForyCodec(types=[...])` with the produced types,
so first-call serialisation throws.

**Fix sketch:** Codegen template should collect every TypeDef in the
manifest and pass them to the codec it instantiates, the same way
`_collect_service_types` does for hand-written clients.

---

## B5. TS client has no real session-protocol implementation

**Combos:** Currently masked by B1 in ts-ts; would surface as the only
remaining ts-py failure once B3 is fixed.

**Symptom:** `ProxyClient.method.bidi()` opens a fresh bidi stream per
call instead of multiplexing onto a single session stream. There is no
TS analogue of Python's `SessionStub` / `create_session` flow.

**Fix sketch:** Port `bindings/python/aster/session.py` (~150 lines for
SessionStub + create_session, ignore SessionServer + cancel handling for
the first cut). Wire `AsterClient.client(serviceClass)` to dispatch to
the session path when `info.scoped === 'stream'`, mirroring Python's
high-level client at `bindings/python/aster/high_level.py:1513`.

---

## Status snapshot

| Combo       | Pass / Total | Failing chapters |
|-------------|-------------:|-----------------|
| py-py dev   |          9/9 | —               |
| py-py auth  |          6/6 | —               |
| py-ts dev   |          4/5 | Ch4 (B2)        |
| py-ts auth  |          4/6 | Ch5 edge tailLogs, Ch5 edge runCommand (B2) |
| ts-py dev   |          6/8 | Ch4 (B3), Ch6 (B4) |
| ts-py auth  |          5/6 | Ch5 edge runCommand (B3) |
| ts-ts dev   |          6/6 | —               |
| ts-ts auth  |          6/6 | (B1 still latent) |
