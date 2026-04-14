# Why `contract_id` uses content hashes (in lay terms)

**Audience:** engineers touching bindings, spec, or onboarding.
**Related:** `ffi_spec/Aster-ContractIdentity.md` §11.3 for the normative spec.

## The one-sentence version

A service's `contract_id` is the BLAKE3 hash of everything that defines it —
including every field of every type it sends on the wire — so that if two
producers think they're implementing "the same" service but secretly disagree
on a field's type, their `contract_id`s *cannot* collide, and the mismatch is
caught at handshake instead of silently producing garbage at decode time.

## The alternative we did not pick

The obvious simpler scheme is **name-based references**: each type has a
unique tag (e.g. `myapp/User`), and when one type references another, you
write the tag, not a hash. Protobuf, JSON Schema, and most IDL systems work
this way. It's simpler, no Merkle DAG, no chicken-and-egg on cyclic types.

We considered it. We rejected it. Here's why.

## The scenario name-based refs can't catch

Imagine a small team with no central schema registry (which is Aster's world
— the whole point is decentralized P2P without coordination). They agree on
a service via a markdown doc:

> `UserService` has one method `getUser(UserRequest) → UserResponse`.
> `UserRequest { id }`, `UserResponse { id, name, email }`.

Two developers implement it independently:

**Alice (Python):**
```python
@wire_type('myapp/UserRequest')
@dataclass
class UserRequest:
    id: int        # Python int → int64
```

**Bob (TypeScript):**
```ts
@WireType('myapp/UserRequest')
class UserRequest {
  id = '';         // String, because Bob read "id" and thought UUID
}
```

Both classes have the tag `myapp/UserRequest`. The spec doc didn't pin down
whether `id` is a number or a string — it's ambiguous prose.

### With name-based refs

The canonical bytes of `myapp/UserRequest` are essentially just the string
`"myapp/UserRequest"`. Both sides produce identical bytes, identical hashes,
identical `contract_id`s. Alice's server sees Bob's client handshake, the
`contract_id` matches, admission succeeds. Bob sends his first request. On
the wire, his client sent UTF-8 `"alice-123"`; Alice's server reads it as a
varint-encoded int64 and gets `7378028543` or a decode error. The failure
mode is silent and late, and it shows up only when a real request flows.

### With content hashes

The canonical bytes of `myapp/UserRequest` include the shape of every field:
`{id: int64}` on Alice's side, `{id: string}` on Bob's side. Different
bytes, different hashes, different `contract_id`s. Bob's client presents
its `contract_id` at handshake; Alice's server compares it to hers and
rejects the session before a single RPC is sent. The error points at the
`UserRequest` type. Bob fixes his type to `bigint` and tries again. Failure
mode is loud and immediate.

**This is the only property content hashing buys us that name-based refs
can't.** Everything else — uniqueness, deduplication, namespacing — could be
done with careful tag conventions. The detection of silent type drift is
what justifies the extra machinery.

## When does this actually happen?

- **Markdown-driven development.** Spec doc in the repo, client and server
  written independently, sometimes in different languages. This is Aster's
  primary use case.
- **Version drift.** Service upgraded on the producer side; one consumer
  forgets to regenerate its bindings. Content hashing catches the stale
  schema at admission.
- **Generated vs hand-written clients.** `aster-gen` produces one
  `rpc.generated.ts`; a user hand-writes a Python client from the same
  spec. Both paths produce real TypeDef JSONs; both get hashed by Rust;
  both end up with the same `contract_id` iff they really agree.
- **Copy-paste errors.** Someone duplicates a type definition into another
  package, changes one field, forgets to update a tag. Name-based refs make
  the two types look identical at the contract layer; content hashing does
  not.

## When does it *not* matter?

- **Single codebase, single producer.** If you own both sides and generate
  them from one source, the spec-doc-ambiguity problem never arises. You
  could run Aster on name-based refs and nothing would break. The machinery
  is overhead for your use case.
- **Perfect out-of-band coordination.** If every schema change is reviewed
  by both teams before it ships, silent drift can't happen. Content hashing
  is defensive, not essential.

The trade-off is: content hashing costs us some complexity (especially
around cyclic types, see next section) in exchange for making a specific
failure mode impossible rather than "unlikely if everyone is careful."
Aster chose paranoia because its failure mode is P2P-wide wire-level
garbage, not a local stack trace.

## The cyclic-type wrinkle (and why `SELF_REF` exists)

Content hashing works by bottom-up topological order: leaves first, then
types that reference leaves, etc. Each type's hash is a function of its
canonical bytes, which include the hashes of its referenced types.

Self-referential types break this. Consider:

```ts
@WireType('fs/Entry')
class Entry {
  name = '';
  children: Entry[] = [];  // ← references itself
}
```

To hash `Entry`, you need the canonical bytes of `Entry`, which include
the `type_ref` of the `children` field, which is supposed to be the hash
of `Entry` — which you don't have yet. Chicken and egg.

The spec breaks this with `SELF_REF`: within a strongly-connected component
of the reference graph, back-edges (edges that close a cycle) encode the
target by **wire tag** (`fs/Entry`) instead of by hash. The rest of the
SCC stays as `REF`-by-hash. Cross-SCC references are still content-hashed.

### Worked example

For `Entry { name, children: Entry[] }`:

- Reference graph has one cycle: `Entry → Entry` (the `children` field).
- SCC = `{Entry}`, contains one self-edge, so `children` is a back-edge.
- `Entry`'s TypeDef:
  - field `name`: `type_kind: primitive, type_primitive: "string"`
  - field `children`: `type_kind: self_ref, self_ref_name: "fs/Entry",
    container: list`
- Canonical bytes = these two fields, written in the canonical XLANG format.
- Hash = `BLAKE3(canonical bytes)`.

No chicken-and-egg: `children`'s type is encoded as the string `"fs/Entry"`,
not as `Entry`'s (not-yet-computed) hash. The type is still content-
addressed because the hash covers every *field*, including `name`. If you
change `name: string` to `name: int64`, the hash changes.

### What SELF_REF sacrifices

Inside a cycle, the content-addressing property is slightly weaker: two
types with the same tag, the same non-cyclic fields, and a differently-
shaped self-reference (say, a self-reference via a `set` instead of a
`list`) would hash to *different* values — that's still caught. But two
types with the same tag, the same fields, and a self-reference at the
*same position* but pointing to a conceptually-different "self" would
collide. In practice this is not a realistic drift mode — you can't have
two different `fs/Entry` types in the same binary.

### Status across bindings

- **Rust core:** Tarjan SCC + spanning-tree back-edge detection lives in
  `core/src/contract.rs` and is the ground truth.
- **Python:** implements its own Tarjan in `bindings/python/aster/contract/
  identity.py`. **Latent bug**: uses Python-specific `__module__.__qualname__`
  as `self_ref_name` instead of the wire tag. This means two Python services
  with the same wire shape but different module paths get different
  `contract_id`s, and *no* Python service can produce a `contract_id` that
  matches TS or Java for a cyclic type until this is fixed. Follow-up.
- **TypeScript:** as of the work commit-named in this doc, `aster-gen`
  implements Tarjan SCC at build time and uses the wire tag in
  `self_ref_name` (language-neutral, the spec-correct choice). Cross-
  language parity with Python cyclic types is blocked on the Python fix
  above.
- **Java:** delegates hashing to Rust core via JNI, so it inherits Rust's
  behavior automatically.

## Why not move everything to Rust core?

Because Python and TypeScript both need to *build the TypeDef inputs* from
their native type systems before the Rust canonical encoder can hash them.
That build step (walking a Python dataclass tree, walking a TS @WireType
AST) is inherently per-language. The hashing itself is already in Rust.

A future consolidation would expose an `aster_build_contract_id(service
JSON, type descriptors JSON)` FFI that does the whole pipeline — walk,
SCC, hash, assemble — in Rust, given a language-neutral description of
the types. Python and TS would become thin adapters. Worth doing once a
second binding wants cyclic-type support and we're tired of two
implementations.

## TL;DR for a new contributor

1. `contract_id = BLAKE3(canonical bytes of ServiceContract)`, recursively.
2. This catches "same tag, different fields" drift that name-based refs
   would miss. That's the *only* reason it's not just a name.
3. For cyclic types, we use `SELF_REF` (by wire tag) to break the
   chicken-and-egg. This is handled at build time by `aster-gen` in TS,
   by Tarjan SCC in Python, and by the Rust core everywhere else.
4. If you touch the canonical encoder, make sure you understand §11.3
   first. A one-byte change in canonical XLANG output invalidates every
   published `contract_id` in the wild.
