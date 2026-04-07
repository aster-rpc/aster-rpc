# Aster — Audit Methodology

This document defines how to structure spec compliance and security audits
for the Aster codebase, so that gaps like "code exists but is never called"
are caught systematically rather than by luck.

## Principles

1. **Audit flows, not functions.** A function that exists but is never called
   from the intended entry point is unimplemented. Trace each spec flow
   end-to-end: entry point → intermediate calls → final effect.

2. **Audit both directions.** For every spec requirement, check that (a) the
   code does what the spec says, and (b) the code doesn't do things the spec
   doesn't say (unauthorized behaviors).

3. **Audit the gaps between layers.** Most bugs live at boundaries: Python ↔
   Rust, transport ↔ codec, server ↔ handler, admission ↔ registry. Verify
   that data crosses each boundary correctly.

4. **Audit security at every deserialization point.** Every `json.loads`,
   `codec.decode`, `struct.unpack`, and `bytes.fromhex` is an attack surface.
   Each needs: size limit, type validation, timeout.

## Audit Structure

### Phase 1: Requirement Extraction

Read each spec document and extract concrete, testable requirements:

```
REQ-ID  | Spec §  | Statement                                    | Testable?
T-1     | §4.3    | Frame size MUST NOT exceed 16 MiB            | Yes
T-2     | §11.4.3 | Startup MUST publish contracts to registry    | Yes
T-3     | §3.2.2  | Admission response MUST include registry_ticket | Yes
```

Rules:
- One requirement per row (split compound sentences)
- Mark MUST/SHOULD/MAY per RFC 2119
- Flag "testable?" — untestable requirements need spec revision

### Phase 2: Flow-Level Verification

For each end-to-end flow in the spec, trace the actual code path:

```
Flow: Producer Startup → Contract Publication
  Entry: AsterServer.__aenter__() → start()
  Steps:
    1. [✓] Create IrohNode with ALPNs           → high_level.py:234
    2. [✓] Compute contract_id for each service  → high_level.py:254
    3. [✓] Verify against manifest if present     → high_level.py:268
    4. [✓] Publish to registry doc + blobs        → high_level.py:310
    5. [✓] Generate read-only share ticket        → high_level.py:395
  Gaps: None (as of 2026-04-06)
```

**Every step must have a file:line reference.** If you can't find the line,
the step isn't implemented.

### Phase 3: Function-Level Audit

For each requirement from Phase 1:

| Status | Meaning |
|--------|---------|
| PASS   | Code exists, is reachable from the intended entry point, and behaves correctly |
| PARTIAL| Code exists but is incomplete, has caveats, or isn't fully reachable |
| FAIL   | Code missing, unreachable, or behaves incorrectly |
| N/A    | Not applicable to this implementation |

Rules:
- **PARTIAL is not a pass.** Log exactly what's missing.
- **"Code exists" is not a pass.** Verify it's called in the right flow.
- **Dead code is a FAIL.** If publication.py exists but start() doesn't call it, that's a FAIL for the publication requirement.

### Phase 4: Security Audit

For each deserialization point (every place untrusted bytes → structured data):

```
DESER-ID | File:Line | Source    | Format | Limits | Validation | Risk
D-1      | codec.py:444 | Network | Fory   | ✓ decomp limit | ✓ tag validation | LOW
D-2      | consumer.py:108 | Network | JSON | ✓ count cap | ✗ depth limit | MEDIUM
```

Check:
- [ ] Size limit before parsing?
- [ ] Type validation after parsing?
- [ ] Timeout on the I/O that feeds this deserialization?
- [ ] What happens with malformed input? (crash, hang, truncate, reject)
- [ ] Can this be reached without authentication?

### Phase 5: Cross-Reference

Verify consistency between:
- Spec document ↔ implementation
- Spec document ↔ spec document (no contradictions between Aster-SPEC.md and Aster-ContractIdentity.md)
- Test coverage ↔ requirements (every MUST has a test)
- Security limits ↔ limits.py constants ↔ tests

## Audit Cadence

| Event | Audit type |
|-------|-----------|
| Before each alpha/beta/release | Full audit (all 5 phases) |
| After any spec change | Phase 1 + 2 for affected sections |
| After any security-sensitive change | Phase 4 for affected files |
| Every commit | Automated: Claude security hook (pre-commit) |

## Audit Output Format

The audit produces a single markdown file (`ffi_spec/spec_audit.md`) with:

1. **Summary table** — PASS/PARTIAL/FAIL/N/A counts per spec section
2. **Flow verification** — one section per end-to-end flow with step-by-step trace
3. **Requirement detail** — one row per requirement with status + evidence
4. **Security findings** — ranked by severity with fix status
5. **Known gaps** — issues acknowledged but deferred, with justification

## Lessons Learned

### From the 2026-04-06 audit gap

**What happened:** The contract publication pipeline (`publication.py`,
`publisher.py`) was implemented and tested in isolation, but never wired
into `AsterServer.start()`. The spec audit marked publication as PARTIAL
("pipeline exists") instead of FAIL ("never executed in a real flow").

**Root cause:** The audit checked "does a function exist?" but not "is
the function called from the right entry point?"

**Fix:** Phase 2 (flow-level verification) now requires a file:line
reference for every step. No line reference = not implemented.

### From the decompression bomb finding

**What happened:** `zstandard.ZstdDecompressor.decompress(data, max_output_size=N)`
was assumed to enforce a hard limit. In reality, `max_output_size` is a
buffer allocation hint in python-zstandard, not a security enforcement.
Data larger than the hint is still decompressed successfully.

**Root cause:** API assumption not verified by test.

**Fix:** Switched to streaming decompression with explicit byte counting.
Added `test_max_output_size_enforced` that creates a decompression bomb
and verifies it's rejected.

**Principle:** Never trust a library parameter name — test the boundary.
