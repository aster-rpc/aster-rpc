# Fory cross-binding configuration

Aster uses Apache Fory (`fory-core` for Java, `pyfory` for Python, similar for
the other bindings) as its canonical XLANG codec. Getting Fory to round-trip
payloads across bindings requires three things to be identical on every
producer and consumer: the Fory **build config**, the per-type
**(namespace, typename) registration key**, and the per-field **tag-ID
annotations** that drive the schema-hash.

This note captures the decisions and the two Fory quirks that bit us during
the Python ↔ Java Option B work, so the next engineer picking this up
doesn't rediscover them from a hash-mismatch error.

## The baseline Fory config (both bindings)

|        | Python (`pyfory.Fory`) | Java (`Fory.builder()`) |
|--------|------------------------|--------------------------|
| XLANG  | `xlang=True`           | `.withLanguage(XLANG)`    |
| Refs   | `ref=True`             | `.withRefTracking(true)`  |
| Strict | `strict=True` *(default, set explicitly)* | `.requireClassRegistration(true)` |

Aster sets all three on both sides — see
[`bindings/python/aster/codec.py`](../../bindings/python/aster/codec.py) and
[`bindings/java/aster-runtime/.../codec/ForyCodec.java`](../../bindings/java/aster-runtime/src/main/java/site/aster/codec/ForyCodec.java).

Rationale for each:

- **XLANG** — the whole point of picking Fory. Without it each binding uses
  its native format and nothing cross-talks.
- **Refs** — duplicate objects serialize once and circular structures survive
  decode. Matters for trees / graphs that appear in real RPC payloads
  (policy rules, config snapshots, etc.).
- **Strict** — unknown types raise at encode time instead of being smuggled
  through. Without this an attacker could get the peer to deserialize
  arbitrary classes — a textbook deserialization-gadget vector.

**The build config MUST match between sender and receiver.** Fory embeds a
per-struct schema hash in every XLANG payload, and the hash algorithm reads
several of these flags. Drift → `Hash X is not consistent with Y for type
T` on the receiver and the connection is dead.

## Quirk #1: Java Fory's 2-arg `register(cls, tag)` splits on `.`, not `/`

Aster's wire-tag convention (shared with Python's `@wire_type`) is
`"namespace/Typename"`, e.g. `"_aster/RpcStatus"`. Apache Fory Java's 2-arg
`register(Class<?>, String tag)` interprets the tag as a fully-qualified
class name and splits on the **last `.`** to produce
`(namespace, typename)`. Passing `"_aster/RpcStatus"` therefore registers
as `(ns="", tn="_aster/RpcStatus")` — not at all what we want. pyfory's
`Fory.register_type(cls, namespace=..., typename=...)` has no such quirk
because it takes the pair as separate args.

**Mitigation:** everyone calls
[`site.aster.codec.ForyTags.register(fory, cls, tag)`](../../bindings/java/aster-runtime/src/main/java/site/aster/codec/ForyTags.java)
on the Java side. It splits the tag on `/` and invokes Fory's explicit
3-arg `register(cls, namespace, typename)` form. The Python side already
does this inside `@wire_type` so no wrapper is needed there.

## Quirk #2: Java Fory snake-cases field names in the schema fingerprint

The schema hash that SCHEMA_CONSISTENT XLANG embeds in each payload is a
hash of a text fingerprint — a sorted list of
`<field-id>,<type-id>,<ref>,<nullable>;` tuples, one per field. When a
field carries no `@ForyField(id=N)` annotation, Java Fory substitutes its
**snake-cased** field name (`detailKeys` → `detail_keys`) for the
`<field-id>` slot. pyfory in the same situation uses the raw field name
(`detailKeys`). So a Java `RpcStatus` and a pyfory `RpcStatus` with
byte-identical field layouts produce different fingerprints and different
hashes, and the receiver rejects the payload.

This is upstream behaviour — it's in
`org.apache.fory.serializer.struct.Fingerprint.computeStructFingerprint`.
We don't know for certain why Fory-Java does the snake-case conversion;
the most plausible explanation is that its authors wanted Java types
(which use `camelCase` by convention) to produce the same fingerprint as
their Python / Rust / Go siblings (which use `snake_case`), as a
convenience for users who keep their type definitions in lockstep across
languages. **In practice this breaks — the other bindings don't do the
same normalization, so the "convenience" silently breaks any payload
with non-snake-case fields.** File an upstream issue if this still
happens in a later Fory release; for now we work around it.

**Mitigation:** annotate every field with a stable tag ID on every
binding. When an ID is present, Fingerprint uses the ID string instead of
the (snake-cased or raw) field name, and the two bindings produce
identical fingerprints. This is also recommended upstream for stable wire
identity across field renames.

In practice:

- **Framework wire types** (`StreamHeader`, `CallHeader`, `RpcStatus`)
  carry `@ForyField(id = N)` / `pyfory.field(N)` on every field in both
  bindings. IDs run 0..n-1 in declaration order and MUST stay in sync
  across the two.
- **User types declared in hand-written code** must add the same
  annotations. The Mission Control example under
  `bindings/java/aster-examples-mission-control/` does this.
- **User types generated by `aster contract gen-client`** get the IDs
  automatically: the Python codegen emits `pyfory.field(N)` where N is
  the 0-based index of the field in the producer's published manifest.
  The TypeScript codegen has an equivalent (TODO: confirm + link when
  the TS binding follows suit).

## What to do when you see "Hash X is not consistent with Y for type T"

1. Confirm both bindings are built from the same Aster commit — config
   drift in `ForyCodec` / `ForyConfig` causes this.
2. Confirm the type has `@ForyField(id = N)` / `pyfory.field(N)` on every
   field on both sides, with identical IDs for each field.
3. Confirm the declared field types match (Java `int` ↔ Python
   `pyfory.int32`, Java `long` ↔ Python `pyfory.int64`, etc. — a bare
   Python `int` hashes as int64, not int32).
4. If all three are identical, print the fingerprint from each side (see
   `compute_struct_fingerprint` in pyfory and `Fingerprint
   .computeStructFingerprint` in Java Fory) and diff them. The string
   that differs tells you which knob is off.
