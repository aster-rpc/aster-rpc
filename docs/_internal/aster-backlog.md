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

### Python ProxyClient: migrate to canonical-bytes decoder

**Why.** Python's dynamic proxy today reads `ContractManifest.methods[*].fields` (flat; no nested TypeDefs). Works for py-py by relying on pyfory's by-name nested-type resolution at runtime. When Path B lands the Rust-core canonical reader for TS, Python should converge onto the same path so we have one source of truth per binding.

**What.** Rewire `bindings/python/aster/runtime.py` `_ensure_manifest` / `ProxyClient._ensure_transport` to fetch the contract collection, iterate `types/*.bin`, decode each via the new Rust-core `decode_type_def_bytes` exposed through PyO3, build the type graph, and register transitively via `DynamicTypeFactory`. Keep the fast-path manifest read for flat types; fall through to canonical decoding when a nested ref appears. (Same hybrid as TS decision (b).)

**Where.**
- `bindings/python/aster/runtime.py` — `_ensure_manifest` (lines ~2300–2400 depending on drift).
- `bindings/python/aster/dynamic.py` — `DynamicTypeFactory`; add transitive walk.
- `bindings/python/rust/src/contract.rs` or sibling — add PyO3 wrapper around `core::contract::decode_type_def_bytes`.

**Blockers.** Rust-core canonical reader (Path B).

**Origin.** 2026-04-21 session, Phase 7 convergence plan.

---

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

### Manifest bit-width gap: carry int/float bit-width on ManifestField

**Why.** `pyfory.int32` / `pyfory.int64` are `TypeVar` markers, not classes. Python's `manifest._classify_type` (`bindings/python/aster/contract/manifest.py:_classify_type`) doesn't recognise them and falls through to `"kind": "string"`. Surfaces two ways:

- `aster contract gen-client --lang python` emits broken `accepted: str = 0` fields (cosmetic; typed clients work because they register real classes directly and never read the generated field kinds).
- `DynamicTypeFactory` can't reconstruct `int32` from the manifest, so a synthesised request/response type produces a different Fory hash than the server's real class. Manifests built from int32-declaring services fail Fory's consistency check on the proxy path.
- Surfaced by Mission Control Ch3 `ingestMetrics` in py-py-dev (1/10 failing test).

**What.** Extend `ManifestField` with an optional `bit_width: int` on `int` / `float` kinds. Teach `_classify_type` to recognise pyfory's TypeVar markers and emit the correct bit width. Teach `DynamicTypeFactory` to reconstruct `int32` / `int64` / `float32` / `float64` from that field. Parallel change on the TS side (`bindings/typescript/packages/aster/src/contract/manifest.ts` ManifestField).

**Where.**
- `bindings/python/aster/contract/manifest.py` — `ManifestField`, `_classify_type`.
- `bindings/python/aster/dynamic.py` — `DynamicTypeFactory.synthesize_for_method` field-kind reconstruction.
- `bindings/typescript/packages/aster/src/contract/manifest.ts` — `ManifestField` interface.
- `bindings/typescript/packages/aster/src/dynamic.ts` — parallel field reconstruction.

**Blockers.** None, but Path B (canonical-bytes decoder) may subsume this entirely: `TypeDef` already carries bit-width via `type_primitive: "int32"` etc. If Path B lands and the dynamic proxy reads TypeDefs directly, the manifest-side fix may become unnecessary. Revisit after Path B ships and see whether the gap still manifests.

**Origin.** 2026-04-21 session, follow-up from `docs/_internal/fory_upgrade/dynamic-proxy-async.md:86-90`.

---

### Ch4 investigation: `"string" argument must be of type string ... Received an instance of Array`

**Why.** ts-ts-dev Ch4 (RunCommand bidi stream) fails with what looks like a native-binding or session-lifecycle crash, not a codec issue. Surfaces only when the proxy path is exercised, which is why it showed up after the async proxy change; root cause may be older. Unrelated to the two Fory-codec failures (Ch1 typeId 29 and Ch3 requestWireTag).

**What.** Reproduce under `ts-ts-dev` Ch4 and capture the full stack trace. Likely culprits to investigate first:

- `joinAndSubscribeNamespace` native call in `AsterClientWrapper.proxy` — check whether a `string[]` vs `string` is being passed to a NAPI arg.
- Session-lifecycle interaction in `SessionProxyClient` — the error may surface during session teardown when the bidi stream closes.
- The error message "Received an instance of Array" suggests a `Buffer.from()` or `String()` call receiving an array where a string is expected.

**Where.**
- `bindings/typescript/packages/aster/src/runtime.ts` — `SessionProxyClient`, `AsterClientWrapper.proxy`.
- `bindings/typescript/napi/src/` — the NAPI side of `joinAndSubscribeNamespace` and related.
- `examples/typescript/missionControl/test_guide/ch4*.ts` — repro harness.

**Blockers.** None. Can proceed in parallel with Path B.

**Origin.** 2026-04-21 session, third of the three follow-ups in `docs/_internal/fory_upgrade/dynamic-proxy-async.md:95`.

---

## Done

- **ts-ts-dev matrix (7/7 green)** — resolved 2026-04-21. Four fixes landed in the same session: (1) scanner's `BUILD_ALL_TYPES` was wrapping each per-type block in a shared `for (const entry of WIRE_TYPES)` loop but hardcoding `entry.ctor` — all blocks after the first silently skipped `initMeta` and registered a typeInfo with no `options.creator`, crashing Fory's decoder with `new options.creator()`; (2) `canonicalToManifestField` in `dynamic.ts` dropped the `kind` field, so container defaults fell through to `null` and Fory rejected `tags` as non-nullable; (3) map default was a plain object but Fory calls `.entries()` which only exists on `Map`; (4) `IrohTransport` server-streaming / client-streaming / bidi `send` paths never threaded `opts?.hintType` into `encodeCompressed`. Also fixed `JsonCodec.decode` to fall back to Fory for non-JSON first bytes (matches Python `JsonProxyCodec`), so the scope-mismatch guard's JSON transport survives Fory-encoded error trailers.

- **TS publisher spec parity (`types/{hash}.bin`)** — resolved by commit `e0373ac` (2026-04-21). Scanner now stamps `typeHashHex` + `typeDefBytes` on every `WireTypeShape`; runtime `_publishContracts` walks reachable types and passes the map to `buildCollection`. Cross-binding dynamic clients can now decode TS-published contract collections byte-for-byte.
