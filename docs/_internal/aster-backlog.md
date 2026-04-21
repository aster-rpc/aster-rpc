# Aster Backlog

Tracked items that are deferred on purpose — not bugs-in-flight, not today's work. Each entry is self-contained enough that a future agent (or future-you after a month away) can pick it up without re-deriving context.

When adding an entry, include:

- **Why** — the motivation / what problem this solves.
- **What** — the concrete change required.
- **Where** — file paths and functions. Grep-able.
- **Blockers** — other backlog items or external dependencies that gate this.
- **Origin** — short pointer to where the decision to defer was made (session date + commit or doc link).

When an item is completed, move it to the `## Done` section at the bottom with the commit hash that resolved it, or just delete it if it's no longer relevant.

---

## Open

### Java dynamic proxy: migrate to canonical-bytes decoder

**Why.** Java today uses `UnknownStruct` as the dynamic-decode fallback. Like Python, this is a different path from what TS will use under Path B. Converge on the shared Rust-core decoder via JNI for architectural cleanliness and to eliminate per-binding codec drift.

**What.** Mirror the Python migration above on the Java side. Expose `decode_type_def_bytes` / `decode_service_contract_bytes` through the existing JNI layer. Rewire Java's dynamic-proxy path to build the type graph from canonical TypeDefs rather than relying on `UnknownStruct` alone.

**Where.**
- `bindings/java/aster-runtime/src/main/java/io/aster/runtime/` — search for `UnknownStruct` call sites.
- `bindings/java/aster-runtime/src/main/java/io/aster/runtime/contract/` — where the manifest/contract clients live.
- JNI glue — wherever `canonicalBytesFromJson` is exposed today.

**Blockers.** Rust-core canonical reader (Path B). Not urgent — Java parity work is already ahead of Python for dynamic contracts, so this is consolidation, not a fix.

**Origin.** 2026-04-21 session, Phase 7 convergence plan.

---

### Cross-binding matrix: Fory 0.17 is not wire-compatible across its own language implementations

**Why.** Every same-binding combo is green. **Every cross-binding combo is red.** It's not "TS is broken"; it's that pyfory 0.17, @apache-fory/core 0.17, and fory-java 0.17 each produce internally-consistent NAMED_COMPATIBLE_STRUCT bytes that **none of the others can read**. The memory confirms the cross-binding matrix was green on `main` before the Fory 0.17 upgrade, so this is a 0.17 regression in all three implementations at once.

**Matrix snapshot (2026-04-21, `fory-017-upgrade` branch, Aster-level wrappers spec-aligned):**

| Combo | Pass/Total | Symptom |
|-------|-----------|---------|
| py-py-dev | **10/10** | — |
| ts-ts-dev | **7/7** | — |
| ja-ko-dev | **6/6** | — (Fory Java round-trips itself cleanly) |
| py-ko-dev | 0/5 | `DeserializationException: read objects are: [null]` |
| ja-py-dev | 3/9 | `Buffer out of bound: 2 + 21 > 4` |
| py-ts-dev | 1/5 | `Out of bounds access` in TS Fory reader |
| ts-py-dev | 2/9 | `Meta share read context must be set when compatible mode is enabled` |
| ts-ko-dev | 0/5 | `DeserializationException: read objects are: []` |

All primitive-type mismatches are already resolved per the xlang type mapping spec in commit `51615ac` (`pyfory.int32 == spec "varint32"`, etc.). Our publisher / consumer / Path B code uses spec-correct names. The break is below the Aster wrapper layer, inside the Fory implementations' `NAMED_COMPATIBLE_STRUCT` wire format.

**Empirical reproduction (all runs on Fory 0.17 with `xlang=true, compatible=true, ref=true`):**

| Direction | Struct | Result |
|-----------|--------|--------|
| TS encode → pyfory decode | `StatusResponse(agent_id='edge-7', status='running', uptime_secs=42)` | pyfory returns `StatusResponse(agent_id='', status='idle', uptime_secs=0)` — every field matched zero wire descriptors, so pyfory fills class-level defaults. `status='idle'` is the class default, not wire data. |
| TS encode → pyfory decode | `StatusResponse` (same schema, pyfory side uses `pyfory.field(id=1..3)` without defaults) | Still all zero-values. Field IDs don't change the outcome. |
| TS encode → pyfory decode | `StreamHeader` (9 fields, 2 lists) | All fields at defaults → server-side dispatcher reports "Missing service name" and returns a `FAILED_PRECONDITION` trailer. |
| pyfory encode → TS decode | `StatusResponse` | **Works** when Aster's type mapping is spec-correct (pyfory's `int` → spec `varint64` → `Type.varInt64()` on TS). |
| pyfory encode → TS decode | `RpcStatus` error trailer | TS trips `RangeError: Out of bounds access` inside `stringWithHeader → readVarUint36Slow`. |

So the drift is **asymmetric**: TS→py decode always returns defaults (no wire field matches); py→TS decode sometimes works (simple structs) and sometimes over-reads the buffer (anything with lists / the protocol headers `StreamHeader` / `CallHeader` / `RpcStatus`). The first ~12 bytes after the xlang bitmap differ between the two bindings for identical logical structs (TS writes different TypeMeta preamble bytes than pyfory).

**Concretely not wire-compatible on Fory 0.17:**
- `StreamHeader` (9 fields incl. `list<string>` and int8/int16/int32 mix) — TS→py fails in both directions
- `CallHeader` (5 fields incl. `list<string>`) — same pattern
- `RpcStatus` (4 fields incl. `list<string>`) — same pattern
- Mission Control `StatusResponse` / `StatusRequest` / `SubmitLogResult` / `Heartbeat` / `Assignment` / `Command` / `CommandResult` / `LogEntry` / `MetricPoint` / `TailRequest` / `IngestResult` — TS→py fails, py→TS sometimes works (no lists) but decoder runs off buffer once a list field is present.

The following **does** work on both bindings:
- Same-binding round-trips (py→py, ts→ts) — every struct round-trips cleanly with the fixes in commits `e022167`, `f86543e`, `51615ac`.
- Canonical `TypeDef` / `ServiceContract` bytes (distinct wire protocol from Fory's xlang struct bytes, produced by `core::contract::canonical_xlang_bytes` / `canonical_bytes_to_json`). Those round-trip cross-binding.

**What to re-test when upstream ships a fix.** Watch for a Fory release that (a) adds a JavaScript runner to `integration_tests/idl_tests/`, or (b) lists `NAMED_COMPATIBLE_STRUCT` wire-format fixes in its JS package changelog. Then:

1. Revert the `Type.varInt64()` / `Type.varInt32()` / `Type.varUInt32()` / `Type.varUInt64()` mappings in `bindings/typescript/packages/aster/src/dynamic.ts PRIMITIVE_TO_FORY` if upstream spec changes.
2. Run `tests/integration/mission_control/run_matrix.sh --only py-ts-dev` — expect 5/5 pass.
3. Run `--only ts-py-dev` — expect 9/9 pass.
4. Run the full matrix — expect 4/4 combos × 2 modes green.
5. If green, remove the stop-gap from whichever option below we picked (JSON fallback, Rust-core route) and close this entry.

**Realistic short-term options:**

1. **Wait / track upstream Fory TS.** No action from us beyond re-testing on each Fory release.
2. **Route TS through Rust core via NAPI** (see sibling backlog entry "Route TS Fory through Rust core").
3. **JSON fallback for cross-binding calls.** Quick, perf-costly, lossy (Fory's XLANG offers zero-copy + schema evolution JSON doesn't).

**Where.**
- Fory upstream: `docs/_internal/fory/javascript/packages/core/` + `integration_tests/idl_tests/` (no JS runner at 0.17).
- Our Aster-level wrappers confirmed correct: `bindings/python/aster/contract/identity.py` `_pyfory_typevar_primitive`, `bindings/python/aster/dynamic.py` `_PRIMITIVE_TO_PY`, `bindings/typescript/packages/aster/src/dynamic.ts` `PRIMITIVE_TO_FORY`.

**Blockers.** Option 1 is upstream; option 2 is medium effort (sibling entry); option 3 is a config change with known perf regression.

**Also flagged here (not this entry's fix):** `bindings/java/aster-runtime/src/main/java/site/aster/codec/ForyCodec.java` does NOT set `withCompatibleMode(true)`. When Java rejoins the matrix (`ASTER_MATRIX_INCLUDE_JAVA=1`), expect the same incompat unless compatible-mode is enabled there too. One-liner to add, but out of scope until Java is in the matrix again.

**Origin.** 2026-04-21 session. All our Aster-level wrappers are correct; the break is in the Fory implementations themselves.

---

### Route TS Fory through Rust core via NAPI (Fory interop stop-gap)

**Why.** Sibling entry "Fory TS 0.17 is not wire-compatible with pyfory 0.17" documents the blocker. One of the three proposed fixes is to replace `@apache-fory/core` on the TS side with a NAPI wrapper around our Rust core's xlang codec. This guarantees byte identity because the same Rust implementation powers encode + decode on every binding that uses it. Attractive because:

- Rust core already owns contract-identity canonical bytes (`core::contract::canonical_xlang_bytes`), so extending to runtime struct encode/decode is an incremental build.
- Eliminates the per-binding "re-implement Fory" drift we're hitting at 0.17.
- Lines up with the memory item "Rust core migration plan" — the broader trend of consolidating behavior into Rust core.

Cost: we take on maintenance of a Fory xlang encoder/decoder in Rust, and lose the Fory JS JIT codegen path (currently good for perf).

**What.** Four-layer sketch:

1. **Rust core xlang codec.** Add `core::codec::xlang` with a `ForyWriter` / `ForyReader` that can encode/decode NAMED_COMPATIBLE_STRUCT bytes against a `TypeDef`-driven schema. The TypeDef is our canonical form and already drives every other cross-binding decision; using it as the schema here keeps Rust core the single source of truth. Reuses `TypeMeta` struct layout from the spec (`docs/specification/xlang_serialization_spec.md`) rather than re-deriving.

   Surface:
   ```rust
   pub fn encode_xlang(tdefs: &TypeDefGraph, instance: &Value) -> Vec<u8>;
   pub fn decode_xlang(tdefs: &TypeDefGraph, bytes: &[u8], root_tag: &str) -> Value;
   ```
   where `Value` is a shallow typed-JSON analogue (ints, floats, strings, maps, lists, nested structs by tag). Rust→JSON at the FFI boundary; callers on the TS side convert to idiomatic JS objects.

2. **NAPI surface.** `bindings/typescript/native/src/codec.rs`:
   ```rust
   #[napi]
   pub fn fory_xlang_encode(type_defs_json: String, root_tag: String, instance_json: String) -> Result<Buffer>;
   #[napi]
   pub fn fory_xlang_decode(type_defs_json: String, root_tag: String, data: Buffer) -> Result<String>;
   ```
   Uses JSON at the FFI boundary for simplicity (matches the existing `canonical_bytes_to_json` pattern). Future optimization: expose a `Buffer → Buffer` path with a neutral binary representation.

3. **TS ForyCodec shim.** `bindings/typescript/packages/aster/src/codec.ts` new `RustForyCodec` class implements `Codec`. Internally:
   - Lazily builds a `TypeDef` graph from all registered types (either directly from Path B's `CanonicalTypeDef` dicts, or by translating `Type.struct(...)` declarations to TypeDefs for non-Path-B callers).
   - `encode(obj, hintType)` → serialize `obj` to instance JSON, call `fory_xlang_encode`.
   - `decode(payload)` → call `fory_xlang_decode`, parse JSON back to instances (reusing the registered class constructors for shape).

   Gate the swap via an env flag or config option initially: `createXlangCodec({ backend: 'rust' })` vs default `'js'`.

4. **Python parallel.** Same PyO3 surface exposed through `_aster.codec.fory_xlang_encode` / `fory_xlang_decode`. Python's `ForyCodec` gets a similar `backend='rust'` swap. When both bindings use the Rust backend, cross-binding interop is guaranteed.

**Where.**
- `core/Cargo.toml` + `core/src/codec/xlang/{mod,writer,reader,value}.rs` — new module. Reference: `docs/specification/xlang_serialization_spec.md` §3 (NAMED_COMPATIBLE_STRUCT) and `docs/specification/xlang_type_mapping.md`.
- `bindings/typescript/native/src/codec.rs` — NAPI wrapper.
- `bindings/python/rust/src/codec.rs` — PyO3 wrapper (mirror of the NAPI surface).
- `bindings/typescript/packages/aster/src/codec.ts` — `RustForyCodec` shim.
- `bindings/python/aster/codec.py` — parallel `RustForyCodec` shim (optional; pyfory already works).
- `bindings/typescript/packages/aster/src/xlang.ts` — wire the new backend behind a flag.
- `tests/typescript/unit/rust-fory-codec.test.ts` + `tests/python/test_rust_fory_codec.py` — parity tests: same TypeDef + same instance → same bytes.

**Phasing.**

1. **Phase A (spike, ~1 day):** `core::codec::xlang` can encode + decode `StatusResponse` bytes that round-trip through itself. Unit test only — no NAPI yet.
2. **Phase B (~1 day):** NAPI + PyO3 wrappers. Rust↔Rust bytes identical, Rust↔pyfory bytes identical (verified by encoding the same struct via pyfory and `core::codec::xlang`, diffing).
3. **Phase C (~1-2 days):** TS `RustForyCodec` shim + env-flag switchover. `run_matrix.sh --only ts-ts-dev` + `py-ts-dev` + `ts-py-dev` green with the flag on. Keep the js backend as default.
4. **Phase D (later):** Promote to default, remove `@apache-fory/core` dependency.

Phases A+B are the load-bearing ones. If Fory upstream ships a compat fix in that window, we revisit phase C.

**Blockers.** None technical. Depends on whether the team wants to own a Fory xlang implementation in Rust long-term (vs. tracking upstream).

**Origin.** 2026-04-21 session, after proving Fory TS 0.17 / pyfory 0.17 wire incompatibility (commits `e022167`, `f86543e`, `51615ac`).

---

## Done

- **Python Path B migration (py-py-dev 10/10)** — resolved 2026-04-21 (same session as ts-ts-dev). Three changes landed together: (1) PyO3 wrapper `canonical_bytes_to_json` exposes the Rust-core TypeDef reader to Python; (2) `DynamicTypeFactory.register_from_type_defs` walks the canonical TypeDef graph transitively (topo-sort + hex-hash ref resolution, mirroring TS `registerFromTypeDefs`); (3) the publisher-side bit-width gap is closed — `_resolve_field_type` now recognises pyfory's TypeVar markers (`pyfory.int32` / `int64` / `float32` etc) and emits the correct `type_primitive`. Consumers then reconstruct synthesized dataclasses with matching pyfory TypeVars, so the Fory struct hash lines up with the producer's real class and the Ch3 `ingestMetrics` consistency guard passes. Also aligned TS Fory + pyfory to `compatible=True` (both bindings use the same NAMED_COMPATIBLE_STRUCT layout). Obsoletes the "Manifest bit-width gap" backlog entry: `TypeDef.type_primitive` now carries the bit-width for free.

- **ts-ts-dev matrix (7/7 green)** — resolved 2026-04-21. Four fixes landed in the same session: (1) scanner's `BUILD_ALL_TYPES` was wrapping each per-type block in a shared `for (const entry of WIRE_TYPES)` loop but hardcoding `entry.ctor` — all blocks after the first silently skipped `initMeta` and registered a typeInfo with no `options.creator`, crashing Fory's decoder with `new options.creator()`; (2) `canonicalToManifestField` in `dynamic.ts` dropped the `kind` field, so container defaults fell through to `null` and Fory rejected `tags` as non-nullable; (3) map default was a plain object but Fory calls `.entries()` which only exists on `Map`; (4) `IrohTransport` server-streaming / client-streaming / bidi `send` paths never threaded `opts?.hintType` into `encodeCompressed`. Also fixed `JsonCodec.decode` to fall back to Fory for non-JSON first bytes (matches Python `JsonProxyCodec`), so the scope-mismatch guard's JSON transport survives Fory-encoded error trailers.

- **TS publisher spec parity (`types/{hash}.bin`)** — resolved by commit `e0373ac` (2026-04-21). Scanner now stamps `typeHashHex` + `typeDefBytes` on every `WireTypeShape`; runtime `_publishContracts` walks reachable types and passes the map to `buildCollection`. Cross-binding dynamic clients can now decode TS-published contract collections byte-for-byte.
