# Mission Control Matrix — Known Bugs & Audit Gaps

## Status snapshot (2026-04-10)

Full matrix: **58/58 pass** across all 8 combos.

## Audit Gaps — Not Yet Tested

Discovered by spec-vs-implementation audit. Each gap lists the spec
reference, what would break, and the real-world scenario that exposes it.
Ordered by severity.

---

### G1. Session lock + network fault (CRITICAL)

**Spec:** Aster-session-scoped-services.md §6.4 — lock held for one
request-response exchange.

**Gap:** Python `SessionStub._call_unary` holds an async lock while
reading the response frame. If the QUIC stream drops mid-response, the
lock releases and the next queued call sends a CALL frame while the
server may still be flushing the previous response. Silent stream
corruption.

**Scenario:** Client on flaky WiFi makes two sequential session calls.
First call's response is interrupted by a network drop. Second call
interleaves with the server's still-in-flight first response.

**Test sketch:** Open a session, start a unary call, kill the recv stream
mid-response, immediately make another call. Assert the second call
either fails cleanly or succeeds with correct data — never silently
returns the wrong response.

---

### G2. CANCEL never sends trailer — TS server (CRITICAL)

**Spec:** Aster-session-scoped-services.md §5.4 — "exactly one trailer
with status CANCELLED" unconditionally.

**Gap:** TS `SessionServer` receives CANCEL and does `continue` without
writing a CANCELLED trailer. Client's `cancel()` drains frames waiting
for a trailer that never arrives — permanent deadlock.

**Scenario:** Any client that calls `session.cancel()` against a TS
server.

**Test sketch:** Open a session against TS server, start a slow handler,
send CANCEL, assert CANCELLED trailer is received within 5s.

---

### G3. Session instantiation without metadata validation — TS (CRITICAL)

**Spec:** Aster-SPEC.md §5.2 — metadata subject to size/count limits.

**Gap:** TS `SessionServer` creates the service instance before validating
StreamHeader metadata. Oversized metadata creates the session, then each
CALL spins on validation errors while holding memory.

**Scenario:** Malicious client sends 10MB metadata in the StreamHeader.

**Test sketch:** Send a StreamHeader with metadata exceeding limits to a
session service. Assert server rejects before creating the instance.

---

### G4. Client-stream EoI not validated (HIGH)

**Spec:** Aster-session-scoped-services.md §4.5 — explicit TRAILER(OK)
for end-of-input.

**Gap:** Server's client-stream reader breaks on ANY trailer flag, not
just status=OK. A bit-flip that sets TRAILER on a data frame causes
premature EoI — handler returns a result from partial data.

**Scenario:** `ingestMetrics` with 10,000 points, corruption at point
500. Server says "accepted: 500" and client believes it.

**Test sketch:** Send 100 data frames, then a frame with TRAILER flag but
non-OK payload. Assert server rejects (not silently returns partial).

---

### G5. Decompression bomb (HIGH)

**Spec:** Aster-SPEC.md §6.1 — max frame size 16 MiB.

**Gap:** Frame size limit enforced at wire level, but decompressed output
isn't capped. A 10 KiB compressed frame can decompress to 100+ MiB.

**Scenario:** Attacker sends a small zstd frame with pathological ratio.

**Test sketch:** Create a zstd frame that decompresses to > 16 MiB.
Assert server rejects it (not OOM).

---

### G6. Bidi reader exception → silent EOF (HIGH)

**Spec:** Aster-session-scoped-services.md §4.5 — wire errors should
terminate the call.

**Gap:** Python bidi stream reader catches FramingError, logs it, puts
EOF in the queue. Handler thinks client finished, returns success.

**Scenario:** Network corruption during `runCommand` bidi stream — server
returns success on truncated input.

**Test sketch:** Corrupt a frame mid-bidi-stream. Assert server returns
INTERNAL error, not success.

---

### G7. TS unary writes OK trailer, Python doesn't (MEDIUM)

**Spec:** Aster-session-scoped-services.md §4.6 — "Unary calls within a
session do not require a trailer frame."

**Gap:** TS SessionServer writes OK trailer after unary response. Python
doesn't. We worked around this with a 2s drain timeout in the TS client.

**Test sketch:** (Already implicitly tested by the matrix — py-ts and
ts-py both pass.) Add an explicit assertion that the second call in a
session succeeds without delay.

---

### G8. Deadline not enforced in session handlers (MEDIUM)

**Spec:** Aster-session-scoped-services.md §9.3 — per-call deadlines in
CallHeader.

**Gap:** `deadlineEpochMs` is read into CallContext but never used as a
timeout. Slow handler ignores deadline, client times out, server wastes
resources.

**Test sketch:** Set a 2s deadline on a session call to a handler that
sleeps 10s. Assert the call fails within 3s (not 10).

---

### G9. No client-side scope validation (MEDIUM)

**Gap:** If a developer calls `createSession(SharedService)`, the client
sends method="" and the server rejects with FAILED_PRECONDITION.

**Test sketch:** (Already tested by the scope mismatch guard test.)

---

### G10. OTT nonce replay (LOW)

**Spec:** Aster-trust-spec.md §3.1 — nonces consumed once.

**Gap:** If `nonce_store` is None (dev mode), OTT nonces are not checked.
Attacker reuses a captured credential.

**Test sketch:** Present the same OTT credential twice. Assert second
attempt is denied.

---

### G11. CallHeader metadata not validated — TS session (LOW)

**Gap:** TS SessionServer doesn't validate per-call metadata size/count.

**Test sketch:** Send a CALL frame with oversized metadata. Assert
RESOURCE_EXHAUSTED.

---

### G12. Invalid UTF-8 crashes handler (LOW)

**Gap:** Invalid UTF-8 in frame payload causes UnicodeDecodeError that
propagates without sending an error trailer.

**Test sketch:** Send a frame with invalid UTF-8 bytes. Assert server
returns INTERNAL trailer (not crash/hang).
