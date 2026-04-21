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

### ts-ts-dev Ch1: `new options.creator()` is undefined (Path B integration bug)

**Why.** After Path B lands end-to-end (commits `0b9cf73`, `831adc3`, `e0373ac`, `197a7f7`, `a9d5654`), ts-ts-dev Ch1 fails with `TypeError: undefined is not a constructor (evaluating 'new options.creator()')` on the **server** side while decoding `StatusRequest`. The client correctly fetches `types/{hash}.bin`, decodes 7 TypeDefs, topo-sorts them, and registers the dynamic classes via `DynamicTypeFactory.registerFromTypeDefs`. Unit tests for this path pass (see `dynamic-fory.test.ts` "canonical hybrid path" case) — the bug is specifically cross-process.

The error pattern suggests one of these:

1. Client's encode writes a namespace/typeName combination the server doesn't have registered (name mismatch — `mission/StatusRequest` vs `mission.StatusRequest`, or similar).
2. Server registered the struct but `options.creator` on the struct typeInfo is `undefined`. In that case Fory's `registerSerializer(typeInfo)` falls through to the "no ForyTypeInfoSymbol on prototype" branch and generates a decoder that does `new undefined()`.
3. The client's encode produces a struct header that points to a nested ref using a name the server resolves differently (dynamic vs scanner-produced `@WireType` tag drift).

**What.** Investigate by:

- Adding a server-side diagnostic that logs `typeInfo.options.creator` after each `codec.registerType` call in `_publishContracts` / BUILD_ALL_TYPES. If `creator` is defined server-side, the bug is encode-side on the client.
- Comparing the exact wire bytes the client produces (capture via `codec.encode(statusRequest)`) against what the server's `BUILD_ALL_TYPES`-registered type expects. The `e2e-quic.test.ts` path works end-to-end in-process, so the golden bytes from that path are a good reference.
- Check if `Type.struct` called in `registerFromTypeDefs` produces a struct with the same `TypeMeta.computeStructHash()` as `BUILD_ALL_TYPES` does for the same logical class. Fory's TypeMeta sorts fields via `groupFieldsByType` (primitives by size desc, then name asc); if our client-side field ordering differs from the server-side, the hash differs → decoder rejects.
- Also investigate Ch3 `Field "tags" is not nullable` — `canonicalToManifestField` in `dynamic.ts` hardcodes `default: undefined`, so `defaultForField` for container types falls through to `null` (should be `[]` for list, `{}` for map). Two separate bugs likely tangled together.

**Where.**
- `bindings/typescript/packages/aster/src/dynamic.ts` — `registerFromTypeDefs`, `foryFieldTypeFromCanonical`, `canonicalToManifestField`.
- `bindings/typescript/packages/aster/src/runtime.ts` — `_registerDynamicTypesForService` (the hybrid integration point).
- `bindings/typescript/packages/aster/src/contract/identity.ts` — `decodeTypeDefBytes` (sanity-check the returned shape).
- Reference working path: `bindings/typescript/packages/aster/src/cli/gen.ts` `BUILD_ALL_TYPES` body (leaves-first, identical shape).
- Reference working path: `tests/typescript/integration/e2e-quic.test.ts` — round-trips structs in-process with Fory XLANG.

**Blockers.** None. This is the direct follow-up to Path B's cross-process validation.

**Origin.** 2026-04-21 session, after `a9d5654` landed. Matrix output captured in the commit message. Ch2 (`read failed before trailer`), Ch4 (`options.creator` again), and scope mismatch guard (`Failed to detect the Fory type`) likely all share the same root cause or are cascading failures from Ch1 — fix Ch1 first, then re-run.

---

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

- **TS publisher spec parity (`types/{hash}.bin`)** — resolved by commit `e0373ac` (2026-04-21). Scanner now stamps `typeHashHex` + `typeDefBytes` on every `WireTypeShape`; runtime `_publishContracts` walks reachable types and passes the map to `buildCollection`. Cross-binding dynamic clients can now decode TS-published contract collections byte-for-byte.
