# Aster — Security Posture

This document describes Aster's threat model, what we've hardened, what
we proactively do on every commit, and what remains open.

**Last updated:** 2026-04-06

## Threat Model

Aster is a peer-to-peer RPC framework. Both sides of every connection are
potentially adversarial:

- **Malicious consumer** — sends crafted RPC requests to a producer
- **Malicious producer** — sends crafted responses/trailers to a consumer
- **Malicious peer in registry** — writes bad entries to shared iroh-docs
- **Malicious gossip participant** — broadcasts crafted gossip messages
- **Local attacker** — writes to ~/.aster/ or env vars

The primary risks are **denial of service** (memory exhaustion, CPU
exhaustion, hangs) and **data confusion** (type confusion, logic errors
from malformed input). **Remote code execution** via deserialization is
the highest-severity risk.

## Defense Architecture (5 Layers)

### Layer 1: Write-only canonical format — no deserialization from untrusted sources

The `contract.bin` canonical XLANG format uses custom varints, zigzag
encoding, and length-prefixed structures. Deserializing these from
untrusted input would create a large attack surface for allocation bombs,
parser confusion, and out-of-bounds reads.

**What we do:**
- `contract.bin` is **only ever passed to `BLAKE3()`** for hash verification
- All readable contract data comes from `manifest.json` (standard JSON)
- The spec explicitly forbids canonical byte deserialization (§11.4.4.1)
- This is documented with a `:::danger` admonition in the spec

**Spec reference:** Aster-ContractIdentity.md §11.4.4.1

### Layer 2: Fory XLANG tag validation — no gadget chains

Every Fory-deserialized type must be registered with `@wire_type`. This
prevents the "gadget chain" attack where an attacker sends a payload that
deserializes to an unexpected type with dangerous side effects.

**What we enforce:**
- The wire tag must match a registered type — unknown tags rejected
- Fields are positional, not named — no field name injection
- No `eval()`, `exec()`, `__reduce__`, or `pickle` in the deserialization path
- `SerializationMode.NATIVE` is restricted to local/in-process transports only

**Risk acknowledged:** Fory itself doesn't limit field counts, string
lengths, or nesting depth. We enforce these at the layer above.

### Layer 3: Size limits at every boundary

All limits are defined in **`aster/limits.py`** — single source of truth.

```
Network → QUIC stream
  → MAX_FRAME_SIZE (16 MiB)          ✅ Enforced in framing.py
    → MAX_DECOMPRESSED_SIZE (16 MiB)  ✅ Enforced in codec.py (streaming decompress)
      → Metadata caps (64 entries, 8 KB)  ✅ Enforced in server.py, session.py
        → Hex field length validation      ✅ Enforced in trust/consumer.py
          → Application code
```

**Implemented limits:**

| Constant | Value | Where enforced |
|----------|-------|----------------|
| `MAX_FRAME_SIZE` | 16 MiB | `framing.py` — wire frame size cap |
| `MAX_DECOMPRESSED_SIZE` | 16 MiB | `codec.py` — streaming decompression with byte counting |
| `MAX_METADATA_ENTRIES` | 64 | `server.py`, `session.py` — StreamHeader/CallHeader |
| `MAX_METADATA_TOTAL_BYTES` | 8,192 | `server.py`, `session.py` — total metadata size |
| `MAX_STATUS_MESSAGE_LEN` | 4,096 | `transport/iroh.py` — RpcStatus message truncation |
| `MAX_SERVICES_IN_ADMISSION` | 10,000 | `trust/consumer.py` — admission response cap |
| `MAX_COLLECTION_INDEX_ENTRIES` | 10,000 | `contract/publication.py` — collection index cap |
| `MAX_MANIFEST_METHODS` | 10,000 | `contract/manifest.py` — manifest method list cap |
| `MAX_MANIFEST_TYPE_HASHES` | 100,000 | `contract/manifest.py` — type hash list cap |
| `MAX_ACL_LIST_SIZE` | 10,000 | `registry/acl.py` — ACL entry cap |
| `HEX_FIELD_LENGTHS` | per-field | `trust/consumer.py` — pubkey=64, nonce=64, sig=128 |

**Validation helpers in `limits.py`:**
- `validate_hex_field(name, value)` — checks length + valid hex chars
- `validate_metadata(keys, values)` — enforces all metadata caps
- `validate_status_message(msg)` — truncates to limit
- `LimitExceeded` exception — maps to `StatusCode.RESOURCE_EXHAUSTED`

### Layer 4: Timeouts at every I/O point

**Implemented:**
- `read_frame(stream, timeout_s=30.0)` — configurable timeout on all
  frame reads, defaults to `DEFAULT_FRAME_READ_TIMEOUT_S` from limits.py
- Consumer admission reads: bounded by `read_to_end(64KB)`
- RPC deadline propagation: per-call deadlines enforced by `DeadlineInterceptor`

**Remaining gaps (tracked):**
- Admission handshake doesn't have an overall timeout (individual reads are bounded)
- Gossip message reads don't have explicit timeouts (rely on QUIC stream limits)

### Layer 5: Principle of least authority for docs/blobs

- Consumer gets **read-only** registry doc ticket — cannot write to producer's registry
- Blob downloads are **content-addressed** — `BLAKE3(data) == expected_hash`
- ArtifactRef integrity: `BLAKE3(fetched contract.bin) == contract_id`
- Collection tickets are **scoped to specific hashes** — no wildcard access

## Proactive Security Measures

### Claude Security Pre-Commit Hook

Every `git commit` on `.py` or `.rs` files triggers an automated Claude
security review via `scripts/security-review.sh` (installed as
`.git/hooks/pre-commit`).

**What it checks:**
- Deserialization from untrusted sources without size limits
- New `json.loads()` / `bytes.fromhex()` / `codec.decode()` without bounds
- Decompression without size enforcement
- Network reads without timeouts
- `SerializationMode.NATIVE` usage with untrusted input
- Hardcoded secrets, credentials, or keys
- `eval()`, `exec()`, `__import__()`, or `pickle` on untrusted data

**Behavior:**
- CRITICAL findings → commit blocked (exit 1)
- HIGH findings → warning printed, commit allowed
- Clean → "✅ Security review clean."
- Bypass: `git commit --no-verify` (for emergencies only)

### Security Test Suite

`tests/python/test_security_limits.py` — 39 tests covering:

| Test class | What it verifies |
|------------|-----------------|
| `TestHexFieldValidation` | Correct/incorrect lengths, invalid chars, empty allowed |
| `TestMetadataValidation` | Entry count cap, key/value length, total bytes |
| `TestStatusMessageValidation` | Truncation at limit, exact boundary |
| `TestDecompressionBomb` | Normal decompression works; 2x-limit bomb rejected |
| `TestFrameReadTimeout` | `read_frame` accepts timeout parameter |
| `TestServerMetadataCap` | `_validated_metadata` truncates oversized lists |
| `TestAdmissionServicesCap` | Services list capped at 10,000 |
| `TestCredentialHexValidation` | Valid credential passes; short pubkey rejected |
| `TestManifestValidation` | Methods capped; type_hashes capped; version coerced to int |
| `TestLimitsConsistency` | Hex lengths even; frame size = 16 MiB; limits reasonable |

These tests run in CI on every push.

### Audit Methodology

Defined in `docs/_internal/audit-methodology.md`. Key principles:

1. **Audit flows, not functions** — trace each spec flow end-to-end
2. **Dead code is a FAIL** — if publication.py exists but start() doesn't call it, that's a FAIL
3. **Every deserialization point audited** — format, source, limits, validation, risk
4. **Never trust library parameter names** — test the boundary (learned from zstd `max_output_size`)

## Deserialization Surface — Current Status

### Fixed (all verified by tests)

| ID | Location | Issue | Status |
|:--:|----------|-------|--------|
| C1 | `codec.py` zstd decompress | No decompressed size limit | ✅ Fixed — streaming decompress with byte counting |
| C2 | `framing.py` read_exact | No timeout on frame reads | ✅ Fixed — configurable timeout, default 30s |
| C3 | `server.py` metadata zip | Unbounded metadata lists | ✅ Fixed — 64 entries / 8KB cap in server + session |
| H1 | `consumer.py` services list | Unbounded ServiceSummary count | ✅ Fixed — capped at 10,000 |
| H2 | `iroh.py` RpcStatus trailer | Unbounded message string | ✅ Fixed — truncated to 4KB |
| H3 | `publication.py` collection index | Unbounded entries list | ✅ Fixed — capped at 10,000 |
| H4 | `consumer.py` hex fields | No length validation | ✅ Fixed — validate_hex_field on all credential fields |
| H5 | `server.py` StreamHeader | Metadata lists unbounded | ✅ Fixed — same as C3 |
| M1 | `acl.py` ACL JSON | No type validation | ✅ Fixed — isinstance check + size cap |
| M3 | `manifest.py` from_json | No field type validation | ✅ Fixed — numeric coercion + list caps |
| M4 | `gossip.py` gossip payload | JSON depth unbounded | ✅ Fixed — payload size check added before `json.loads` |
| M5 | `iid.py` IID HTTP response | No HTTPS pinning, no size limit | ✅ Fixed — 64KB response size limit added |

### Open (tracked for future hardening)

| ID | Location | Issue | Priority | Notes |
|:--:|----------|-------|----------|-------|
| M2 | `bootstrap.py` mesh state | Local file not integrity-checked | Medium | Add HMAC with node key. Low risk in containers (read-only mount). |
| M6 | `interceptors/` | RateLimitInterceptor | Medium | Token-bucket or sliding-window rate limiter per peer. Interceptor skeleton exists; policy configuration TBD. |
| M7 | `server.py` | Graceful drain on SIGTERM | Medium | Server should stop accepting new connections and drain in-flight RPCs before shutdown. |
| M8 | `transport/iroh.py` | Connection retry backoff | Medium | Consumer reconnect uses fixed delay; should use exponential backoff with jitter. |

## Fory XLANG — What It Gives Us and What It Doesn't

**Protection provided by Fory XLANG mode:**
- Tag-based type dispatch — only registered `@wire_type` classes instantiated
- No arbitrary class loading (unlike Python pickle, Java serialization)
- No field-name injection (positional fields)
- No expression evaluation in the deserialization path

**Protection NOT provided (we add it ourselves):**
- Field count limits → enforced by `MAX_METADATA_ENTRIES` and frame size
- String length limits → bounded by `MAX_FRAME_SIZE` (16 MiB per frame)
- Nesting depth limits → bounded transitively by frame size
- Total message size → `MAX_FRAME_SIZE` + `MAX_DECOMPRESSED_SIZE`

**NATIVE mode warning:** `SerializationMode.NATIVE` allows deserialization
of arbitrary Python types. It MUST NOT be used with untrusted network input.
It is safe for `LocalTransport` (in-process, same trust domain). The
pre-commit hook flags any new NATIVE usage with untrusted input.

## Lessons Learned

### zstd `max_output_size` is a buffer hint, not a security boundary

`zstandard.ZstdDecompressor.decompress(data, max_output_size=N)` does NOT
reject payloads that decompress beyond N bytes. It allocates N bytes as a
buffer hint but happily decompresses the full payload.

**Fix:** Switched to `stream_reader()` with manual byte counting — read
in 64KB chunks, sum the total, raise `LimitExceeded` when the cap is hit.

**Principle:** Never trust a library parameter name — test the boundary.

### Publication pipeline existed but was never called

The contract publication code (`publication.py`, `publisher.py`) was
implemented, tested in isolation, and marked PARTIAL in the audit. But
`AsterServer.start()` never called it — effectively dead code.

**Fix:** Added Phase 2 (flow-level verification) to the audit methodology.
Every spec step must have a `file:line` reference proving it executes in
the real flow. No line reference = not implemented.
