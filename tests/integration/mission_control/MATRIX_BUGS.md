# Mission Control Matrix — Known Bugs & Audit Gaps

## Status snapshot (2026-04-20)

Full matrix (12 combos × {dev, auth}): all green. Java + Kotlin joined
the matrix across the 2026-04-18 → 2026-04-20 sessions.

Three pre-existing items closed on 2026-04-19/20:

- **#42 Java server JSON body codec** — `AsterServer.java` now sniffs the
  first header byte (`'{'` ⇒ JSON, else Fory) and threads
  `serializationMode` through request/response/trailer paths. Two JSON
  codecs: `jsonFrameworkCodec` (default Jackson) for camelCase framework
  wire types (`StreamHeader` / `RpcStatus`), and
  `jsonUserCodec = JsonCodec.forUserTypes()` with
  `PropertyNamingStrategies.SNAKE_CASE` for user types — same
  camelCase↔snake_case convergence Fory gives you via name-based
  fingerprint. Service summaries advertise `["xlang", "json"]`. Unblocks
  every ja-py (proxy path) and ja-ts combo.
- **#43 TS server "method not found" regression** — bun 1.2.x invokes
  method decorators with the Stage-1 experimental signature regardless
  of tsconfig. Dual-mode `methodDecorator` in
  `bindings/typescript/packages/aster/src/decorators.ts` handles both
  shapes. Also fixed a stale `"^0.1.2"` pin in
  `examples/typescript/missionControl/package.json` that was resolving
  to a cached 0.1.2 dist instead of the workspace. See
  `memory/project_bun_decorators.md` for the diagnostic pattern.
- **#44 py-ko tailLogs streaming timeout** — Kotlin only had
  `callServerStream: CompletableFuture<List<Resp>>` which drains to
  trailer, but Python/TS `tailLogs` is an infinite generator. Added
  incremental `ServerStreamCall<Resp>` (sibling to `BidiCall`, reader-
  only) + `AsterClient.openServerStream(...)` factory. Kotlin test
  consumes-then-closes in a `try/finally`.

## Status snapshot (2026-04-10)

Full matrix: **58/58 pass** across all 8 combos.

## Audit Gaps

Discovered by spec-vs-implementation audit. 12 gaps identified; all
tested, all either fixed or confirmed not vulnerable.

**Additional fixes applied during audit:**
- `MAX_HANDLER_TIMEOUT_S` (300s) added to both Python and TS limits.
  All handlers now have an upper bound even when the client sets no deadline.
- Client-stream JSON decompression in Python `server.py` was using raw
  `zstandard.decompress()` without size limit — fixed to use `safe_decompress()`.

Ordered by severity.

---

### G1. Session lock + network fault (CRITICAL) — TESTED, NOT VULNERABLE

**Spec:** Aster-session-scoped-services.md §6.4 — lock held for one
request-response exchange.

**Gap:** Python `SessionStub._call_unary` holds an async lock while
reading the response frame. If the QUIC stream drops mid-response, the
lock releases and the next queued call sends a CALL frame while the
server may still be flushing the previous response. Silent stream
corruption.

**Test:** `test_g1_concurrent_calls_after_recv_fault` — injects recv
fault mid-response, verifies second call never returns stale data.
**Result:** Lock correctly serialises; second call fails cleanly.

---

### G2. CANCEL never sends trailer — TS server (CRITICAL) — FIXED

**Spec:** Aster-session-scoped-services.md §5.4 — "exactly one trailer
with status CANCELLED" unconditionally.

**Fix:** TS `SessionServer` now writes CANCELLED trailer on CANCEL frame
(was `continue` without response). Python was already correct.

---

### G3. Session metadata validation (CRITICAL) — TESTED, PYTHON PROTECTED

**Spec:** Aster-SPEC.md §5.2 — metadata subject to size/count limits.

**Gap (TS only):** TS `SessionServer` creates the service instance before
validating StreamHeader metadata. TS gap remains unfixed.

**Python:** `validate_metadata()` runs before handler dispatch in
`_session_loop`. Tests confirm oversized values (>4096 bytes) and excess
entries (>64) produce RESOURCE_EXHAUSTED.

**Tests:** `test_g3_oversized_metadata_rejected`,
`test_g3_too_many_metadata_entries_rejected`.

---

### G4. Client-stream EoI not validated (HIGH) — FIXED

**Spec:** Aster-session-scoped-services.md §4.5 — explicit TRAILER(OK)
for end-of-input.

**Fix:** Both Python and TS session servers now validate EoI trailer
status=OK. Non-OK trailers produce INTERNAL error. Python shared
server also fixed.

**Tests:** `test_g4_client_stream_rejects_non_ok_trailer`,
`test_g4_client_stream_non_ok_eoi`.

---

### G5. Decompression bomb (HIGH) — TESTED, NOT VULNERABLE

**Spec:** Aster-SPEC.md §6.1 — max frame size 16 MiB.

**Gap:** Frame size limit enforced at wire level, but decompressed output
isn't capped. A 10 KiB compressed frame can decompress to 100+ MiB.

**Fix:** `ForyCodec._safe_decompress()` uses streaming decompression and
raises `LimitExceeded` when output exceeds `MAX_DECOMPRESSED_SIZE`
(16 MiB). Does not trust the content-size header.

**Test:** `test_g5_decompression_bomb_rejected` — creates a 20 MiB
payload compressed to ~200 bytes, verifies codec rejects and server
returns error trailer (not OOM/crash).

---

### G6. Bidi reader exception → silent EOF (HIGH) — FIXED

**Spec:** Aster-session-scoped-services.md §4.5 — wire errors should
terminate the call.

**Fix:** Both Python and TS session servers now propagate reader errors
instead of converting to silent EOF. Python stores error in
`reader_error` and checks after handler. TS catches in generator
and checks `readerError` after iteration.

**Test:** `test_g6_bidi_reader_error_not_silent_eof`.

---

### G7. TS unary writes OK trailer, Python doesn't (MEDIUM) — FIXED

**Spec:** Aster-session-scoped-services.md §4.6 — "Unary calls within a
session do not require a trailer frame."

**Fix:** TS SessionServer no longer sends OK trailer for session unary
(aligned with Python + spec). Tested via 58/58 matrix.

---

### G8. Deadline not enforced in handlers (MEDIUM) — FIXED

**Spec:** Aster-session-scoped-services.md §9.3 — per-call deadlines in
CallHeader.

**Fix:** All dispatch methods (session AND shared) now enforce deadline.
Added `MAX_HANDLER_TIMEOUT_S` (300s / 5 min) as server-side upper bound
— applied when client sends no deadline or an absurd value.

**Python:** session.py `_get_deadline_timeout()` + server.py
`_handler_timeout()` both clamp to min(remaining, MAX_HANDLER_TIMEOUT_S).

**TypeScript:** session.ts `getDeadlineMs()` + server.ts
`handlerTimeoutMs()` use same logic.

**Tests:** `test_g8_deadline_enforced_in_session`,
`test_g8_shared_handler_has_upper_bound`.

---

### G9. No client-side scope validation (MEDIUM)

**Gap:** If a developer calls `createSession(SharedService)`, the client
sends method="" and the server rejects with FAILED_PRECONDITION.

**Test sketch:** (Already tested by the scope mismatch guard test.)

---

### G10. OTT nonce replay (LOW) — TESTED, NOT VULNERABLE

**Spec:** Aster-trust-spec.md §3.1 — nonces consumed once.

**Result:** `InMemoryNonceStore.consume()` correctly returns False on
replay. Without a nonce store, OTT credentials are denied outright
(not silently accepted). High-level server always creates
`InMemoryNonceStore` when Gate 0 is enabled.

**Tests:** `test_g10_ott_nonce_consumed_on_replay`,
`test_g10_ott_without_nonce_store_is_denied`.

---

### G11. CallHeader metadata not validated — TS session (LOW) — FIXED

**Fix:** TS `SessionServer` now calls `validateMetadata()` on each
CallHeader's metadata before dispatch. Returns RESOURCE_EXHAUSTED
on violation. Python was already protected.

---

### G12. Invalid UTF-8 / corrupt payload crashes handler (LOW) — TESTED, NOT VULNERABLE

**Result:** Corrupt payload in request frame is caught by codec
decode, which raises an exception. Session server's try/except
catches it and returns INTERNAL error trailer. Server does not
crash or hang.

**Test:** `test_g12_corrupt_payload_produces_error_trailer`.
