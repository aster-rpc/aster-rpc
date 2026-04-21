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

### Cross-binding matrix: Fory TS 0.17 is not wire-compatible with pyfory 0.17

**Why.** After exhaustive debugging:

- All primitive-type mismatches between pyfory and @apache-fory/core have been resolved per the xlang type mapping spec. `pyfory.int32 == spec "varint32"`, etc. TS `Type.int32()` vs `Type.varInt32()` map distinct type ids 4 vs 5. Our publisher / consumer / Path B code on both sides uses the correct spec names.
- With the wire-types fully aligned, **pyfory still cannot decode @apache-fory/core-emitted bytes and vice versa** for the same logical struct in `xlang=true, compatible=true, ref=true` mode. Empirically confirmed: pyfory decodes TS-produced `StreamHeader` as all-zero/empty fields (every field falls through to its default), and TS trips `Out of bounds access` when decoding pyfory-produced `RpcStatus` trailers. Field IDs, field name casing, type ids are all individually aligned; the wire layout / TypeMeta framing still diverges.
- Specifically the first ~12 bytes after the xlang bitmap differ between the two bindings for identical logical structs, and the metastring encoding chosen differs (pyfory trips `LOWER_SPECIAL: 30` in older traces; TS runs off the end of the payload).

This is a **Fory upstream issue at 0.17**: the TS binding (`@apache-fory/core@0.17.0-alpha.0`) lacks the wire-compatibility matrix that Java/Python/Rust/Go go through in `integration_tests/idl_tests/`. No JavaScript/TS runner exists there, so the TS binding's xlang output has never been validated against any other binding.

**What.** Realistic options:

1. Wait / track upstream Fory TS fixes. We need JavaScript in `integration_tests/idl_tests/` plus whatever wire alignment work that surfaces.
2. Replace `@apache-fory/core` on the TS side with a thin NAPI wrapper around the Rust core xlang codec (`core/src/contract.rs` already has `canonical_bytes_to_json` / `canonical_bytes_from_json`; extending to runtime xlang encode/decode is a moderate amount of work). Guarantees byte identity because both py and ts route through the same Rust implementation.
3. Fall back to the JSON codec for cross-binding calls and reserve Fory XLANG for same-binding performance. Lossy and perf-costly but unblocks users today.

**Where.**
- Fory upstream: `docs/_internal/fory/javascript/packages/core/` + `integration_tests/idl_tests/`.
- Option 2 sketch: `core/src/contract.rs` + new `core::codec::xlang` module; expose via `bindings/typescript/native/src/codec.rs` (NAPI) and `bindings/python/rust/src/codec.rs` (PyO3).

**Blockers.** Option 1 is upstream; option 2 is a medium rebuild of the encode/decode surface; option 3 is a config change in `_build_dynamic_codec` + `_registerDynamicTypesForService` with known perf regression.

**Origin.** 2026-04-21 session. All our Aster-level wrappers are correct; the break is in the Fory implementations themselves.

---

## Done

- **Python Path B migration (py-py-dev 10/10)** — resolved 2026-04-21 (same session as ts-ts-dev). Three changes landed together: (1) PyO3 wrapper `canonical_bytes_to_json` exposes the Rust-core TypeDef reader to Python; (2) `DynamicTypeFactory.register_from_type_defs` walks the canonical TypeDef graph transitively (topo-sort + hex-hash ref resolution, mirroring TS `registerFromTypeDefs`); (3) the publisher-side bit-width gap is closed — `_resolve_field_type` now recognises pyfory's TypeVar markers (`pyfory.int32` / `int64` / `float32` etc) and emits the correct `type_primitive`. Consumers then reconstruct synthesized dataclasses with matching pyfory TypeVars, so the Fory struct hash lines up with the producer's real class and the Ch3 `ingestMetrics` consistency guard passes. Also aligned TS Fory + pyfory to `compatible=True` (both bindings use the same NAMED_COMPATIBLE_STRUCT layout). Obsoletes the "Manifest bit-width gap" backlog entry: `TypeDef.type_primitive` now carries the bit-width for free.

- **ts-ts-dev matrix (7/7 green)** — resolved 2026-04-21. Four fixes landed in the same session: (1) scanner's `BUILD_ALL_TYPES` was wrapping each per-type block in a shared `for (const entry of WIRE_TYPES)` loop but hardcoding `entry.ctor` — all blocks after the first silently skipped `initMeta` and registered a typeInfo with no `options.creator`, crashing Fory's decoder with `new options.creator()`; (2) `canonicalToManifestField` in `dynamic.ts` dropped the `kind` field, so container defaults fell through to `null` and Fory rejected `tags` as non-nullable; (3) map default was a plain object but Fory calls `.entries()` which only exists on `Map`; (4) `IrohTransport` server-streaming / client-streaming / bidi `send` paths never threaded `opts?.hintType` into `encodeCompressed`. Also fixed `JsonCodec.decode` to fall back to Fory for non-JSON first bytes (matches Python `JsonProxyCodec`), so the scope-mismatch guard's JSON transport survives Fory-encoded error trailers.

- **TS publisher spec parity (`types/{hash}.bin`)** — resolved by commit `e0373ac` (2026-04-21). Scanner now stamps `typeHashHex` + `typeDefBytes` on every `WireTypeShape`; runtime `_publishContracts` walks reachable types and passes the map to `buildCollection`. Cross-binding dynamic clients can now decode TS-published contract collections byte-for-byte.
