# Fory cross-binding configuration

Aster uses Apache Fory (`fory-core` for Java, `pyfory` for Python, similar for
the other bindings) as its canonical XLANG codec. Getting Fory to round-trip
payloads across bindings requires two things to be identical on every producer
and consumer: the Fory **build config** and the per-type
**(namespace, typename) registration key**.

This note captures the decisions and the Fory quirks that shaped them, so the
next engineer picking this up doesn't rediscover them from a hash-mismatch
error.

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

## Field identity: names, not IDs (except for framework types)

The field-level contract across bindings — and the one that bit us hardest —
is how per-field identifiers feed the schema fingerprint. Fory offers two
modes: IDs (`@ForyField(id=N)` / `pyfory.field(id=N)`) and names
(absent annotation, pick whichever default the binding provides). Aster
picks **names** for user types; only the three framework-internal types
(`StreamHeader`, `CallHeader`, `RpcStatus`) use IDs.

**Why names**: in Fory's name-based fingerprint, Python uses the raw
attribute name while Java and TypeScript auto-snake-case. Python convention
is already snake_case, so `agent_id` (Python) and `agentId` (Java,
snake-cased to `agent_id`) converge automatically. Users never pick IDs;
there are no IDs to accidentally mismatch. See
[`Aster-ContractIdentity.md` §11.3.2.3](../../ffi_spec/Aster-ContractIdentity.md)
for the full spec.

**Why IDs for framework types**: `StreamHeader` / `CallHeader` / `RpcStatus`
are defined by Aster-SPEC §5 and their wire IDs are pinned across all
bindings regardless of field renames. These types are small and
version-stable; manual ID agreement is tractable.

**Why not IDs everywhere**: mixing ID-based and name-based types in the same
contract is impossible to catch at compile time. We hit exactly this during
Phase 3c — Java MC types had `@ForyField(id=N)`, Python MC types didn't, and
cross-binding RPC silently hashed differently on every call. Stripping user
IDs makes the mistake unrepresentable.

### Caveats of the name-based strategy

- **Double-capital abbreviations.** Fory Java's snake-caser splits `userID`
  to `user_i_d`, while a Python author would spell the same field `user_id`.
  Mismatches happen only on non-idiomatic Java identifiers; style guides
  already steer developers away from this.
- **Unicode identifiers.** Fory's snake-caser is ASCII-only. Non-ASCII field
  names match only when both sides spell them identically. Aster bindings
  don't attempt NFC normalization at Fory fingerprint time — producers using
  Unicode identifiers SHOULD register a custom Fory name resolver.
- **Schema-metadata overhead on first message.** Fory transmits struct
  schemas on first send per connection. Name-based schemas are ~N bytes per
  field larger than ID-based. Amortized across the connection lifetime.
- **Generic container element types MUST match.** Python's bare `list` or
  `dict` fingerprints differently from `list[str]` / `dict[str, str]`. The
  snake-case-name convergence only saves you on the *field name*; the
  *field type* is part of the fingerprint and must agree. Declare generics
  precisely in Python (`list[str]` not `list`) when targeting cross-binding
  contracts.

## Quirk: Java Fory's 2-arg `register(cls, tag)` splits on `.`, not `/`

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

## What to do when you see "Hash X is not consistent with Y for type T"

1. **Config drift.** Confirm both bindings are built from the same Aster
   commit — drift in `ForyCodec` / `ForyConfig` causes this.
2. **Field type mismatch.** Confirm the declared field types match across
   bindings. Python `list` ≠ `list[str]`, Python `int` ≠ `pyfory.int32`.
   Bare generic containers are a common culprit.
3. **Naming-convention mismatch.** Confirm Java field names snake-case to
   the Python names (`agentId` ↔ `agent_id`). Double-capital
   abbreviations (`userID`) fail because Java's snake-caser mangles them.
4. **Leaked `@ForyField(id=N)` / `pyfory.field(id=N)`** on a user type. All
   three bindings must agree on ID-vs-name; mixing modes hashes differently.
   Strip the annotation and re-test.
5. **Compare the fingerprints directly.** Print the text fingerprint from
   each side (`compute_struct_fingerprint` in pyfory and
   `Fingerprint.computeStructFingerprint` in Java Fory) and diff them. The
   string that differs tells you which knob is off.
