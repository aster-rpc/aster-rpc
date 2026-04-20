# Fory Untyped Encode/Decode — Upstream Investigation

**Date:** 2026-04-20
**Status:** investigation complete, recommendations below
**Context:** Python's `aster.dynamic.DynamicTypeFactory` synthesizes dataclasses at runtime so Fory XLANG will accept them. This doc answers: does Fory natively support this, and can we avoid the workaround?

## TL;DR

- **Java Fory and pyfory already have this capability.** Java via `UnknownStruct extends LazyMap` (`java/fory-core/.../serializer/UnknownClass.java:108`) + `UnknownStructSerializer` (`UnknownClassSerializers.java:63`). Python via `make_dataclass` in `pyfory/meta/typedef_decoder.py:166` consumed by `StructSerializer`'s typedef-driven path (`pyfory/struct.py:370`). Both require **MetaShare mode enabled** and rely on the inline TypeDef on the wire.
- **Rust Fory and Go Fory do NOT have this.** Rust explicitly errors ("Cannot serialize unknown remote type - type not registered locally", `rust/fory-core/src/resolver/type_resolver.rs:320`). Go requires `RegisterNamedStruct` before any encode.
- **JavaScript/C# Fory**: no matches found — treat as "doesn't have it" until verified.
- **Recommendation:** retire Aster's Python `DynamicTypeFactory` in favour of pyfory's built-in path, use Java `UnknownStruct` directly in our Java binding, and **contribute the missing implementations to Fory upstream for Go / Rust / JS / C#** when we need them. This is a good upstream pitch because Fory's own contributor guide (`docs/_internal/fory/CLAUDE.md`) explicitly calls out "maintain cross-language consistency" as a hard rule.

The previous investigation summary (an agent-generated audit) concluded "Fory does not natively support untyped/generic encode-decode. No language implementation offers this." That was wrong — the agent missed `UnknownStruct` entirely. This doc is the corrected record.

## What Fory actually has

### Java — full support via `UnknownStruct`

`java/fory-core/src/main/java/org/apache/fory/serializer/UnknownClass.java:108`:

```java
class UnknownStruct extends LazyMap implements UnknownClass {
  final TypeDef typeDef;

  public UnknownStruct(TypeDef typeDef) {
    this.typeDef = typeDef;
  }
}
```

`UnknownStructSerializer` (`UnknownClassSerializers.java:63-240`) does both sides:

- **Write** (line 143+): pulls values from the `LazyMap` by qualified field name, writes type ID, writes inline TypeDef, writes each field using the standard per-type field serializers. Supports XLANG mode (line 150 `if (config.isXlang())`).
- **Read** (line 226+): constructs a fresh `UnknownStruct(typeDef)`, reads each field from the buffer, stuffs it into the LazyMap. Caller gets back a `Map<String, Object>` with a TypeDef attached.

Hard requirement: `Preconditions.checkArgument(typeResolver.getConfig().isMetaShareEnabled())` (line 77). MetaShare is a public builder option — `ForyBuilder.withMetaShare(true)` (`ForyBuilder.java:375`).

**How it gets triggered today:** implicitly, when Fory decodes a payload carrying a class that isn't registered in the current JVM — `NativeTypeDefDecoder.java:92` falls back to `UnknownStruct.class`. There's no public API that says "give me an UnknownStruct on purpose," but the machinery is all there. Exposing a public entry point is a small lift, not a design change.

### Python — full support via synthesized dataclass

`python/pyfory/meta/typedef_decoder.py:146-166`:

```python
typename = f"UnknownStruct{user_type_id if user_type_id != NO_USER_TYPE_ID else type_id}"
...
if type_cls is None:
    _generated_class_count += 1
    field_definitions = [(field_info.name, Any) for field_info in field_infos]
    class_name = typename.replace(".", "_").replace("$", "_")
    type_cls = make_dataclass(class_name, field_definitions)
```

Coupled with `pyfory/struct.py:370`:

```python
self._fields_from_typedef = field_names is not None and serializers is not None
if self._fields_from_typedef:
    self._field_names = list(field_names)
    self._serializers = list(serializers)
    ...
```

When an unknown type comes in, pyfory synthesizes a dataclass whose fields are `Any` and whose serializers come from the TypeDef — essentially the same outcome as Aster's `DynamicTypeFactory`, except inside Fory and driven by the on-wire schema rather than by Aster's manifest. There's a safety valve at `_generated_class_count >= MAX_GENERATED_CLASSES` (line 156) to stop a malicious producer from flooding the process with synthesized classes.

### Rust — no support

`rust/fory-core/src/resolver/type_resolver.rs:320-350` — six different error sites all saying "Cannot serialize unknown remote type - type not registered locally" / "Cannot deserialize ...". Registration is mandatory.

### Go — no support (inferred)

`go/fory/tests/generator_xlang_test.go:208` uses `fory.RegisterNamedStruct(instance, tag)` before encode. No `UnknownStruct` equivalent found in the Go tree. A dynamic path likely needs a new `reflect.StructOf`-based encoder — but Go's reflect struct is weakly typed (public fields only, string struct tags), so the UX wouldn't be great without an upstream wrapper.

### JS / TS / C# — not found

Grep turned up nothing in `javascript/packages/` or `csharp/`. Haven't exhaustively audited, but the pattern is clear: Java + Python have it, compiled-language bindings don't.

## Wire format implications

The capability hinges on the TypeDef being on the wire. That happens in **COMPATIBLE mode with MetaShare enabled** (not in SCHEMA_CONSISTENT mode, where the wire is schema-driven and carries no TypeDef).

The XLANG type IDs involved:
- `NAMED_STRUCT` (29) — compatible mode inline TypeDef, identified by namespace+name
- `NAMED_COMPATIBLE_STRUCT` (30) — same, carrying extra compatibility metadata
- `STRUCT` (27), `COMPATIBLE_STRUCT` — no TypeDef inline, not usable for dynamic decode

Aster today advertises `["xlang", "json"]`; we'd need to confirm our Fory config runs in COMPATIBLE + MetaShare mode so the TypeDef actually ships on the wire. If we're in SCHEMA_CONSISTENT mode, we'd be writing stripped payloads that no dynamic decoder can unpack — this should be checked before we commit to the plan.

## What this means for Aster

### Immediate wins (no upstream work needed)

1. **Python**: `aster.dynamic.DynamicTypeFactory` is essentially redundant. pyfory's built-in path does the same thing using the on-wire TypeDef directly. If we switch Aster's Fory config to COMPATIBLE + MetaShare and accept that the CLI shell will get `dataclass` instances (not a custom factory's dataclasses), the factory can be deleted. Only caller today is `cli/aster_cli/shell/app.py:741` — small migration.

2. **Java binding**: `UnknownStruct` is ready to use. Expose it in our Java binding as "call a service you don't have generated classes for" — the Java side of the CLI parity story. No bytecode emission, no ByteBuddy. Caveat: needs `ForyBuilder.withMetaShare(true)` on both sides of the wire; this is a config change we need to verify is compatible with our existing Java wire.

### Needs upstream contribution

3. **Rust Aster binding**: Rust Fory is a hard blocker for dynamic encode/decode. Needs an `UnknownStruct` equivalent contributed upstream. Given Rust Fory's `TypeResolver` is aggressive about registration, this is a real design change, not a small addition. **Rough sizing: 2-4 weeks of focused upstream work.**

4. **Go Aster binding** (future): similar — needs a `reflect.StructOf`-or-map-backed generic encoder path added to Go Fory. Might actually be easier than Rust because Go's reflect is flexible, but the UX question (struct-of vs map) needs design.

5. **TS / C# / .NET bindings**: verify absence (short audit), then contribute if missing.

### Good upstream pitch

Fory's `CLAUDE.md` (their own agent guidance) hard-rules "Maintain cross-language consistency while respecting language-specific idioms." The framing for an upstream RFC writes itself:

> Java and Python Fory both auto-materialise unknown-class payloads into a map / dynamic dataclass (`UnknownStruct`, `make_dataclass`). Rust, Go, JS, and C# hard-error. This inconsistency breaks cross-language round-trips whenever a peer sends a type not registered locally — a common shape in RPC, event-bus, and gateway scenarios. Proposal: a common `UnknownStruct` abstraction across all languages, surfaced as a public API (`Fory.serializeGeneric`, `Fory.deserializeGeneric`) rather than relying on implicit dispatch on unknown-class lookups.

That's a feature that serves Fory's own stated goals, not just Aster's.

## Recommended plan — SUPERSEDED

The recommendations below were the working plan before we realised Aster already has a better architectural answer: put the dynamic codec in **Rust core**, not in each binding.

**Current plan:** see `docs/_internal/bindings/fory-dynamic-via-rust-core-design.md`. One Rust-core implementation, thin FFI adapters per binding, no per-language `UnknownStruct` ports.

**Why the plan below is retired:**

1. Using pyfory's built-in dynamic path / Java `UnknownStruct` both require COMPATIBLE mode + MetaShare. Aster's baseline is **SCHEMA_CONSISTENT + strict + ref-tracking** (verified 2026-04-20 against `bindings/python/aster/codec.py:58-71` and `bindings/java/.../codec/ForyCodec.java:38-50`). Switching to MetaShare would ship the TypeDef on every message — wasteful when Aster already publishes manifests out-of-band via the registry.
2. Per-binding adapter work multiplies by N bindings, of which only Java and Python have the native `UnknownStruct` infrastructure to build on. Go, Rust, TS, C# would all need bespoke approaches.
3. Rust core already owns analogous cross-language logic (`contract_id`, canonical bytes, signing). A dynamic codec belongs in the same place.

## Previously recommended plan (for posterity)

1. ~~Verify Aster's Fory config (Java + Python): are we in COMPATIBLE mode with MetaShare?~~ **Answer: no, we're SCHEMA_CONSISTENT. This plan was premised on the wrong mode assumption.**
2. ~~Python: migrate CLI shell off `aster.dynamic.DynamicTypeFactory` onto pyfory's built-in dynamic path.~~ — retire `dynamic.py` in favour of the Rust-core codec instead.
3. ~~Java: expose `UnknownStruct` through the Aster Java CLI / dynamic-client path.~~ — replaced by a new Java `DynamicForyCodec` backed by Rust core FFI.
4. ~~Go / Rust / TS / C# bindings: write the upstream RFC + PR before shipping.~~ — no upstream Fory work needed. Rust core emits XLANG bytes directly using Rust Fory's public primitives (`Writer`, `WriteContext`, `FieldInfo`, etc., all public at `rust/fory-core/src/lib.rs:195-203`).
5. ~~Open a tracking issue on Fory upstream.~~ — maybe later, once the Aster-side implementation is proven. Not a blocker.

## Open questions

- Confirm Aster's current Fory config mode (COMPATIBLE vs SCHEMA_CONSISTENT, MetaShare on/off) in both Python and Java. Check `bindings/python/aster/codec.py`, `bindings/java/.../codec/ForyCodec.java`.
- Measure the wire-size cost of MetaShare if we're not already running it. TypeDef is shared per-session so the cost is amortised, but the first message on a connection is heavier.
- Check `MAX_GENERATED_CLASSES` limit in pyfory — does it apply per-Fory-instance or per-process? Matters for long-lived servers.
- Swift / Scala / Dart / Kotlin Fory bindings: not audited in this pass. Kotlin likely rides on Java Fory and inherits its support; Swift / Scala / Dart unknown.

## Source files cited

- `docs/_internal/fory/java/fory-core/src/main/java/org/apache/fory/serializer/UnknownClass.java:108` — `UnknownStruct` definition
- `docs/_internal/fory/java/fory-core/src/main/java/org/apache/fory/serializer/UnknownClassSerializers.java:63-240` — `UnknownStructSerializer` encode/decode
- `docs/_internal/fory/java/fory-core/src/main/java/org/apache/fory/serializer/UnknownClassSerializers.java:77` — MetaShare requirement
- `docs/_internal/fory/java/fory-core/src/main/java/org/apache/fory/meta/NativeTypeDefDecoder.java:92` — implicit UnknownStruct fallback
- `docs/_internal/fory/java/fory-core/src/main/java/org/apache/fory/config/ForyBuilder.java:375` — `withMetaShare(boolean)`
- `docs/_internal/fory/python/pyfory/meta/typedef_decoder.py:146-166` — dynamic dataclass synthesis
- `docs/_internal/fory/python/pyfory/struct.py:355-403` — typedef-driven StructSerializer
- `docs/_internal/fory/rust/fory-core/src/resolver/type_resolver.rs:320-350` — Rust hard errors on unknown types
- `docs/_internal/fory/go/fory/tests/generator_xlang_test.go:208` — Go's register-first pattern
- `docs/_internal/fory/CLAUDE.md` — "Maintain cross-language consistency" hard rule
