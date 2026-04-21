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

### Cross-binding matrix: py↔ts Fory interop still red

**Why.** After the Python Path B migration + compatible mode on both sides landed, py-py-dev is 10/10 and ts-ts-dev is 7/7, but the cross-binding combos still fail with *different* errors than before:

- **py-server + ts-client:** `Invalid character value for LOWER_SPECIAL: 30` — TS Fory's metastring decoder trips on a byte value (30) that isn't a valid char in the LOWER_SPECIAL alphabet {a-z, ".", "_", "$", "|"}. Either (a) pyfory writes a metastring with a different encoding than @apache-fory/core 0.17 expects, (b) the TypeMeta framing is out-of-phase (decoder reading at the wrong offset), or (c) the two codecs disagree about when to write the TypeMeta vs when to read it.
- **ts-server + py-client:** `client_stream got OK trailer with no response frame`, `unary_fast_path: write_all failed: sending stopped by peer: error 0`, empty-string scalars in the decoded response. Looks like a mix of encode-side (pyfory refusing to write?) and decode-side (producing wrong shape) problems.

Path B registration itself works on both sides — the debug prints show the Python client resolves all 7 TypeDefs correctly and ends up with the right pyfory TypeVars (`~float64`, `dict[str, str]`, etc.). The interop gap is below the TypeDef layer.

**What.** Debug one combo end-to-end (py-ts first; the LOWER_SPECIAL trace is the most specific). Concrete probes:

- Dump the raw bytes of a single unary response at both (a) post-encode on the Python server and (b) pre-decode on the TS client. Find the first byte that diverges from what Fory's struct layout predicts.
- Compare TS's `readTypeMeta` flow (`gen/struct.ts` NAMED_COMPATIBLE_STRUCT case) against pyfory's XLANG writer. Fory 0.17 may have a protocol drift between bindings — check the `.agents/testing/integration-tests.md` xlang tests for the golden form.
- Check whether pyfory's `compatible=True` actually produces NAMED_COMPATIBLE_STRUCT bytes (the error suggests it's writing something else the TS side doesn't recognise).

**Where.**
- `bindings/python/aster/codec.py` — `ForyConfig.to_kwargs` now sets `compatible=True` for XLANG.
- `bindings/typescript/packages/aster/src/xlang.ts` — `getXlangForyAndType` / `newXlangFory` set `compatible: true`.
- `docs/_internal/fory/javascript/packages/core/lib/gen/struct.ts` — `readTypeInfo` + `read` (line 176-) in the NAMED_COMPATIBLE_STRUCT case.
- Fory upstream xlang tests under `docs/_internal/fory/integration_tests/` may have a reproducer.

**Blockers.** None. Requires cross-binding Fory protocol knowledge; may need upstream Fory work.

**Origin.** 2026-04-21 session, after py-py-dev 10/10 + compatible-mode alignment landed but cross-lang still red.

---

## Done

- **Python Path B migration (py-py-dev 10/10)** — resolved 2026-04-21 (same session as ts-ts-dev). Three changes landed together: (1) PyO3 wrapper `canonical_bytes_to_json` exposes the Rust-core TypeDef reader to Python; (2) `DynamicTypeFactory.register_from_type_defs` walks the canonical TypeDef graph transitively (topo-sort + hex-hash ref resolution, mirroring TS `registerFromTypeDefs`); (3) the publisher-side bit-width gap is closed — `_resolve_field_type` now recognises pyfory's TypeVar markers (`pyfory.int32` / `int64` / `float32` etc) and emits the correct `type_primitive`. Consumers then reconstruct synthesized dataclasses with matching pyfory TypeVars, so the Fory struct hash lines up with the producer's real class and the Ch3 `ingestMetrics` consistency guard passes. Also aligned TS Fory + pyfory to `compatible=True` (both bindings use the same NAMED_COMPATIBLE_STRUCT layout). Obsoletes the "Manifest bit-width gap" backlog entry: `TypeDef.type_primitive` now carries the bit-width for free.

- **ts-ts-dev matrix (7/7 green)** — resolved 2026-04-21. Four fixes landed in the same session: (1) scanner's `BUILD_ALL_TYPES` was wrapping each per-type block in a shared `for (const entry of WIRE_TYPES)` loop but hardcoding `entry.ctor` — all blocks after the first silently skipped `initMeta` and registered a typeInfo with no `options.creator`, crashing Fory's decoder with `new options.creator()`; (2) `canonicalToManifestField` in `dynamic.ts` dropped the `kind` field, so container defaults fell through to `null` and Fory rejected `tags` as non-nullable; (3) map default was a plain object but Fory calls `.entries()` which only exists on `Map`; (4) `IrohTransport` server-streaming / client-streaming / bidi `send` paths never threaded `opts?.hintType` into `encodeCompressed`. Also fixed `JsonCodec.decode` to fall back to Fory for non-JSON first bytes (matches Python `JsonProxyCodec`), so the scope-mismatch guard's JSON transport survives Fory-encoded error trailers.

- **TS publisher spec parity (`types/{hash}.bin`)** — resolved by commit `e0373ac` (2026-04-21). Scanner now stamps `typeHashHex` + `typeDefBytes` on every `WireTypeShape`; runtime `_publishContracts` walks reachable types and passes the map to `buildCollection`. Cross-binding dynamic clients can now decode TS-published contract collections byte-for-byte.
