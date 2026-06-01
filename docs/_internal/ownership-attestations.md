# Ownership Attestations — Provable Root ↔ Node Linkage

**Status:** Design locked for MVP; **v1 core partially implemented in Rust as of 2026-05-30** (single-edge mint + an N-tier verifier — see "Implementation status" below). Wire format is Fory-serialized, schema-versioned, opaque-body-signed with Ed25519 v1. The primary artifact is a **bundled `AttestationChain`** carrying all edges from leaf to anchor — verification is local, no walking. BLS deferred indefinitely (would be a `v2` schema bump if it ever lands). Concepts borrowed from OpenID Federation 1.0; wire format is not.
**Date:** 2026-04-29 (last revised: 2026-05-30)
**Related:**
- [aster-trust-architecture.md](aster-trust-architecture.md) — broader trust architecture (gates, modes, transparency log, FROST)
- [../../ffi_spec/Aster-trust-spec.md](../../ffi_spec/Aster-trust-spec.md) — current implemented trust spec
- [working_ideas/retired.trust-anchor-inversion.md](working_ideas/retired.trust-anchor-inversion.md) — superseded; the two-tier anchor + deployment root mechanism it proposed is now subsumed by the `ANCHOR_AUTHORISED_ROOT` / `ROOT_AUTHORISED_BY_ANCHOR` statement codes in this doc
- [aster-rust.md](aster-rust.md) — the Rust crate that implements this (`core/src/attestation.rs` + `aster` facade)

---

## Implementation status (2026-05-30)

A first implementation in any language landed in Rust: `core/src/attestation.rs`
(behind the `attestation` Cargo feature) with an idiomatic surface in the
`aster` facade crate (`aster/src/attestation.rs`). Wire format uses the Apache
Fory **Rust** crate (`fory-core` / `fory-derive` `1.1.0-rc.1`,
`#[derive(ForyStruct)]`, `register_by_name`); the `Fory` runtime is built once
and shared (`OnceLock`), `xlang(true).compatible(true)`.

**Implemented (v1):**
- The four wire types — `Statement`, `AttestationBody`, `AttestationEdge`,
  `AttestationChain` — registered as `aster.attestation.{statement,body,edge,chain}` / `v1`.
- **Single-edge root↔node minting** — `attest_root_node(root_sk, node_sk, opts)`:
  reciprocal Ed25519, both parties sign `b"aster.attestation.v1\0" + body_bytes`.
- **The N-tier verifier** — `verify_chain(bytes, &trusted_anchors, &expected_node)`:
  pre-parse size gate → per-edge structural checks (version, 2–3 statements,
  positional sig count, 32-byte ids, 64-byte sigs, `code`/`ext` ∈ `0..=0xFFFF`,
  `not_before`/`not_after`) → **positional edge-form grammar** (leaf =
  node-ownership pair `ROOT_OWNS_NODE`/`NODE_OWNED_BY_ROOT` or
  `INTERMEDIATE_OWNS_CHILD`/`NODE_OWNED_BY_PARENT`; middle edges =
  `INTERMEDIATE_OWNS_CHILD`/`INTERMEDIATE_AUTHORISED_BY_PARENT`
  (`intermediate→intermediate`); top = anchor-rooted
  `ANCHOR_AUTHORISED_*`/`*_AUTHORISED_BY_*`; unknown or mis-positioned slot-0/1
  codes rejected) → leaf binds to expected node → bridging
  (`bodies[i+1].statements[1] == bodies[i].statements[0]`) → top anchor ∈ trusted
  set → every signature. Typed `AttestationError`. Exercised with a 3-edge
  (4-tier) chain test; arbitrary depth supported up to `MAX_CHAIN_DEPTH = 8`.
- Statement-registry codes as constants; size limits (`MAX_CHAIN_BYTES` 4096,
  `MAX_EDGE_BYTES` 512, `MAX_BODY_BYTES` 256, `MAX_CHAIN_DEPTH` 8); text encoding
  `aster.attestation.chain.v1:<base64url>`; `public_key(secret)` helper.

**Not implemented / deferred:**
- **Multi-tier minting** — only the root↔node mint helper exists; the *verifier*
  already accepts multi-edge chains, but `anchor→intermediate→node` minting
  (Step 0.5) is not written.
- **Epoch replay enforcement** — `epoch` is carried and signed, but the verifier
  is stateless and does not compare against a cached per-root epoch. Callers that
  need replay protection track it themselves.
- **Trust Mark slot** — the 3rd statement (`0x01xx` codes) is accepted by the
  verifier (2–3 statements) but never minted.
- **Verification is sequential**, not parallel (correctness-first; the spec's
  parallel-verify is a perf optimisation to add later).
- **Trusted-anchor-set storage** — verifier takes a caller-supplied slice; no
  managed mutable/auth-controlled store.
- **Revocation, trust publication** (JWS-at-boundary, `.well-known`, DNS/Pkarr),
  and the `aster_node_chain()` serving endpoint — none built.
- **BLS / v2** — deferred indefinitely.
- **Cross-binding wire compatibility is unproven.** Only Rust implements this
  today, and the Rust Fory crate (`1.1.0-rc.1`) is a different major than the
  repo's `pyfory` (`0.17`) — a Rust-minted chain is **not** expected to verify in
  Python/TS/Java until the Fory versions are aligned. portal-sync is Rust↔Rust.

---

## Design summary

The primary artifact is an **`AttestationChain`** — a Fory-serialized bundle carrying every edge from a leaf identity (a node) up to a trust anchor (a root, or an anchor sitting above the root). Each edge is a signed claim by exactly two parties: a parent and a child, both holding private keys, both signing the same canonical body bytes.

A verifier with a chain in hand:

1. Decodes all edges (CPU only, no I/O).
2. Validates structural well-formedness — each edge's parent identity matches the next edge's child identity, the leaf binds to the expected node, the top edge's parent is in the trusted-anchor-set.
3. Verifies all edge signatures **in parallel** (N edges × 2 sigs).
4. Checks per-edge time bounds (`not_before` / `not_after` if set) and monotonic replay (`epoch`).

No walking. No network. No sequential failure modes on the trust path. Anchor rotation re-issues only the top edge; leaves keep the rest of the chain unchanged. No fixed protocol expiry — deployments choose `not_after = 0` for long-lived stable systems or a finite value for CI-driven re-attestation cadences.

Three properties drive the design:

1. **Determinism** — array-shaped struct, positional fields, no map ordering ambiguity. Either an implementation produces the agreed bytes or it produces nothing valid.
2. **Reciprocal consent** — both the parent and the child must hold private keys and sign each edge. A compromised parent cannot mint a chain through an *existing* child without compromising the child too.
3. **Same primitive top to bottom** — root↔node, anchor↔root, intermediate↔intermediate, all use one edge schema and one verifier path. Chains are just lists of edges.

---

## Why this shape, briefly

Four earlier design directions were considered and rejected during the design sessions; the doc preserves the reasoning so future revisions don't relitigate.

- **JWS-chains / OpenID Federation 1.0** (recommended in 2026-04-29 revision, rejected 2026-05-06): spec is hostile in practice, optimised for European eIDAS IDPs, requires HTTP-fetched `.well-known/openid-federation` resolution, library support thin outside EU IDP shops. Its *concepts* are valuable (multi-anchor resolution, Trust Marks, self-issued + parent-issued split) — borrowed below. Its wire format and chain-walking resolution model are not.
- **BLS12-381 aggregate signatures** (originally pitched as the primitive): for the actual data shapes Aster needs (2-of-2 reciprocal at each edge, 5-tier chains in worst-case enterprise topologies), BLS saves ~32 bytes per edge and adds a new curve, a new crate, mandatory proof-of-possession machinery, and scarce HSM support. Bad trade. Reserved as a future option behind a `v2` schema bump if chain depth or bulk-verify load ever justify it.
- **COSE_Sign envelope**: was carrying weight for the BLS upgrade lever (per-signer `alg`) and an interop-with-FIDO/C2PA story. With BLS deferred and no realistic interop scenario, the envelope is overhead.
- **Parent-pointer chain walking** (the OpenID Federation `authority_hints` model — each artifact names its immediate parent and the verifier walks up one hop at a time, fetching parent attestations from a directory or cache): every step is a sequential network/storage request, none can be parallelised, each step has its own failure mode (cache miss, directory unavailable, partial fetch, stale parent). This is the engineering anti-pattern. We bundle the chain at the producer instead.

What's left is intentionally small: a versioned struct per edge, signed opaquely with a domain separator, wrapped in a chain bundle, using the codec we already ship in every binding.

---

## The problem

Today we have:

- An **offline root key** (one per deployment).
- A **node key** per running node, admitted by the root key.

This works for admission control. But there is no compact, self-contained artifact that lets an outside observer say:

1. "This root key owns / authorised this node," **and**
2. "This node acknowledges it is operated by that root key."

The asymmetry matters. Admission tokens prove the root let the node in, but they don't carry the node's reciprocal claim. And as soon as the "root key" is itself a delegate of something higher (an offline anchor authorising an ephemeral CI-issued deployment root, for example), we need the same shape of artifact one tier up — with no protocol churn.

We want a single, uniform primitive that handles both edges, and a bundling mechanism so a leaf node can present its full chain in one shot.

---

## The primitive

### Schema

Four Fory-serialized types, all `@WireType`-pinned so schema evolution can't quietly invalidate signatures.

```python
@WireType("aster.attestation.statement.v1")
class Statement:
    id: bytes                    # 32 bytes (Ed25519 pubkey == identity)
    code: int32                  # u16 from registry, widened for Fory friendliness
    ext: int32                   # u16 deployment-local nuance, opaque to outside verifiers

@WireType("aster.attestation.body.v1")
class AttestationBody:
    v: int32                     # = 1, version tag
    epoch: int64                 # monotonic counter — replay protection ONLY, not expiry
    not_before: int64            # 0 = no lower bound; else unix seconds (reject before)
    not_after: int64             # 0 = NO EXPIRY; else unix seconds (reject after)
    statements: list[Statement]  # 2 required (parent, child), 3rd optional (Trust Mark slot)

@WireType("aster.attestation.edge.v1")
class AttestationEdge:
    body: bytes                  # opaque Fory bytes of AttestationBody
    signatures: list[bytes]      # positional: signatures[i] is by body.statements[i].id

@WireType("aster.attestation.chain.v1")
class AttestationChain:
    v: int32                     # = 1
    edges: list[bytes]           # opaque AttestationEdge bytes, leaf-first to anchor-last
```

The chain wrapper is **not signed as a unit** — its security comes from each edge being individually signed by both parties. Tampering with an edge invalidates that edge's signatures. The chain itself is just a transport container; its only structural property is the order of edges (leaf-first to anchor-last) and the stipulation that each edge's parent identity matches the next edge's child identity.

### Signing input (per edge)

```
sign_input = b"aster.attestation.v1\0" + body_bytes
```

The leading domain separator string scopes the signature to this artifact type so an Ed25519 sig over `body_bytes` can never be replayed against a tunnel handshake, codec metadata, or any other Aster-internal Ed25519-signing context.

### Producer (root↔node, single-edge chain)

```python
body = AttestationBody(
    v=1,
    epoch=monotonic_counter_for(root_id),       # never decreases for this issuer/child pair
    not_before=0,                               # operator policy; 0 = no lower bound
    not_after=0,                                # operator policy; 0 = NO EXPIRY
    statements=[
        Statement(id=root_pk, code=ROOT_OWNS_NODE,    ext=0),
        Statement(id=node_pk, code=NODE_OWNED_BY_ROOT, ext=0),
    ],
)
body_bytes = fory.serialize(body)
sign_input = b"aster.attestation.v1\0" + body_bytes
edge = AttestationEdge(
    body=body_bytes,
    signatures=[
        ed25519_sign(root_sk, sign_input),
        ed25519_sign(node_sk, sign_input),
    ],
)
chain = AttestationChain(v=1, edges=[fory.serialize(edge)])
artifact_bytes = fory.serialize(chain)
```

### Producer (multi-tier chain)

The CLI assembles chains by stacking edges. Each edge is produced independently with the standard producer flow above; the chain wrapper just orders them.

```python
# Edge 0 (leaf): intermediate ↔ node
leaf_edge = build_edge(
    parent=(int_pk, INTERMEDIATE_OWNS_CHILD, int_sk),
    child=(node_pk, NODE_OWNED_BY_PARENT, node_sk),
)

# Edge 1: anchor ↔ intermediate
upper_edge = build_edge(
    parent=(anchor_pk, ANCHOR_AUTHORISED_INTERMEDIATE,    anchor_sk),
    child=(int_pk,    INTERMEDIATE_AUTHORISED_BY_PARENT, int_sk),
)

chain = AttestationChain(
    v=1,
    edges=[fory.serialize(leaf_edge), fory.serialize(upper_edge)],
)
```

The order is **leaf-first**: `edges[0]` is the edge whose child is the node, `edges[-1]` is the edge whose parent is the trust anchor.

### Verifier

```python
def verify_chain(
    wire_bytes: bytes,
    trusted_anchors: set[bytes],
    expected_node: bytes,
) -> None:
    """
    The ONLY validation entry point. There is no "verify just signatures" path.
    Size gates (0) and structural checks (1–3) MUST run and pass BEFORE
    signature verification (4). Any check failing rejects the whole chain.
    No partial-trust state.
    """
    # 0. Pre-parser size gate — runs BEFORE Fory touches the bytes.
    #    Bounds the parser's allocation surface; forecloses parser-bomb attacks.
    assert len(wire_bytes) <= MAX_CHAIN_BYTES

    chain = fory.deserialize(wire_bytes, AttestationChain)
    assert chain.v == 1
    assert 1 <= len(chain.edges) <= MAX_CHAIN_DEPTH

    # 0b. Post-decode size gates (still pre-crypto)
    for edge_bytes in chain.edges:
        assert len(edge_bytes) <= MAX_EDGE_BYTES

    # 1. Decode all edges (CPU only)
    edges = [decode_edge(b) for b in chain.edges]
    for edge in edges:
        assert len(edge.body) <= MAX_BODY_BYTES
        for sig in edge.signatures:
            assert len(sig) == MAX_SIGNATURE_BYTES   # EXACT, not <=

    bodies = [fory.deserialize(e.body, AttestationBody) for e in edges]
    for body in bodies:
        for stmt in body.statements:
            assert len(stmt.id) == MAX_STATEMENT_ID_BYTES   # EXACT
            assert 0 <= stmt.code <= 0xFFFF
            assert 0 <= stmt.ext  <= 0xFFFF

    # 2. Per-edge structural sanity (gate; MUST pass before step 4)
    for body, edge in zip(bodies, edges):
        assert body.v == 1
        # Time bounds (operator policy; 0 = unbounded)
        if body.not_before != 0:
            assert now() >= body.not_before
        if body.not_after != 0:
            assert now() <= body.not_after
        # Monotonic replay protection (always on; cache key is the chain leaf id)
        assert body.epoch >= cached_epoch_for(chain_leaf_id)
        assert 2 <= len(body.statements) <= 3
        assert len(edge.signatures) == len(body.statements)
        validate_statement_codes(body.statements)

    # 3. Chain structural well-formedness (gate; MUST pass before step 4)
    #    Leaf-first ordering: edges[0] is the leaf, edges[-1] is the top.
    #    leaf binds to expected node:
    assert bodies[0].statements[1].id == expected_node
    #    each upper edge's child == previous edge's parent (bridging identity):
    for i in range(len(bodies) - 1):
        assert bodies[i + 1].statements[1].id == bodies[i].statements[0].id
    #    top edge's parent is in the trusted anchor set:
    assert bodies[-1].statements[0].id in trusted_anchors

    # 4. Crypto — only reached if 0–3 all passed.
    #    Every signature verified, in parallel.
    parallel_for_each(
        ((body, edge) for body, edge in zip(bodies, edges)),
        verify_edge_signatures,
    )

def verify_edge_signatures(body: AttestationBody, edge: AttestationEdge) -> None:
    sign_input = b"aster.attestation.v1\0" + edge.body
    # Positional binding: signatures[i] MUST verify against statements[i].id.
    # If a sig is by the wrong party (or in the wrong slot), the verify call fails.
    for stmt, sig in zip(body.statements, edge.signatures):
        ed25519_verify(stmt.id, sign_input, sig)  # raises on failure
```

The "sign opaque bytes, deserialize after verify" pattern (DSSE-style) means the verifier and signer are bit-identical on what's signed — there's no "did you re-encode the body the same way" bug class. Combined with bundling, no part of trust derivation does I/O.

### Validation discipline (mandatory — read before implementing)

**Signature verification alone is NOT sufficient.** An attacker can present valid edges captured from unrelated chains; every individual signature will verify because they're real signatures by real parties. Without the structural checks (steps 2 and 3 above), the verifier would falsely accept the composition as a coherent chain. The structural checks are the load-bearing security boundary, not the signatures.

#### Canonical orderings (out-of-order = invalid = reject)

The spec mandates strict canonical orderings. Implementations MUST NOT attempt to recover from misordering by sorting, retrying, or otherwise being "liberal in what they accept." Out-of-order input is malformed input and is rejected.

1. **Within an edge** — `edge.signatures[i]` is the signature by `body.statements[i].id`. Positional binding. Slot 0 = parent role's signature; slot 1 = child role's signature; slot 2 (if present) = Trust Mark's signature. Positional binding is enforced cryptographically: `ed25519_verify(statements[i].id, sign_input, signatures[i])` fails if the sig at position i is actually by some other party.

2. **Within a chain** — `chain.edges` is **leaf-first**. `chain.edges[0]` is the edge whose *child* (slot 1) is the node; `chain.edges[-1]` is the edge whose *parent* (slot 0) is in the trusted-anchor-set. Anchor-first or shuffled chain order is rejected by the structural checks (the leaf-binding and anchor-termination assertions both fail).

   Note on natural reading order: humans tend to *describe* a chain top-down ("root authorises int1, int1 authorises int2, int2 authorises node") because that matches construction order. But the wire order is leaf-first; the chain `[edge(root,int1), edge(int1,int2), edge(int2,node)]` written this way is **invalid**. The valid wire order for that chain is `[edge(int2,node), edge(int1,int2), edge(root,int1)]`. This is the same convention as TLS certificate chains and Certificate Transparency.

#### Concrete attack: what skipping the structural check enables

Suppose a verifier (incorrectly) implements only step 4 (signature verification) and the anchor-membership half of step 3 (top edge's parent is in the trust set). An attacker captures two unrelated but legitimate edges from the public network:

- **Edge A**: `(org_foo_root, node_alice)` — Alice operated by org Foo. Signed by both. Captured from Alice's public chain.
- **Edge B**: `(some_other_anchor, completely_different_root)` — from a different deployment's chain, no relation to org Foo. Signed by both. Captured from that deployment's public chain.

Attacker presents `chain = [A, B]` to the broken verifier:

- All signatures verify (A and B are genuine).
- Anchor membership passes (`some_other_anchor` is in the verifier's trust set).
- Without the bridging check (`edges[i+1].child == edges[i].parent`), the verifier wrongly concludes "Alice is anchored at `some_other_anchor`."

The structural check catches this immediately: edge B's child (`completely_different_root`) does NOT equal edge A's parent (`org_foo_root`). Bridging fails. Reject.

This is why the verifier MUST implement structural and signature checks together, in one path, with no fast-path or audit-only path that runs sigs without structure.

#### API contract — single entry point

The core API exposes exactly one verification function: `verify_chain(wire_bytes, trusted_anchors, expected_node) -> Result`. There is no public "verify just signatures" path. There is no "validate structure but skip sigs" path. There is no "give me the parsed body and let me decide" path. Implementations that need to inspect or audit chains MUST still go through the full verifier; partial-trust paths are explicitly forbidden because they invite reuse from contexts where the missing half of the check matters.

Bindings (Python, TypeScript, Java, Kotlin) expose only `verify_chain()`. The internal helpers (`decode_edge`, `verify_edge_signatures`, etc.) are implementation details, not public API.

### Size limits and parser hardening

Hard byte and count bounds on every input element, enforced *before* the parser touches the bytes (where possible) and *before* any signature verification. The verifier MUST treat these as the first line of defense against parser bombs, allocation-amplification attacks, and malformed-input DoS.

**Constants (v1):**

| Constant | Value | Rationale |
|----------|-------|-----------|
| `MAX_CHAIN_BYTES` | 4096 | 8 edges × 512 bytes/edge + chain wrapper framing, rounded up |
| `MAX_EDGE_BYTES` | 512 | Body (~256) + 3 sigs (192) + framing |
| `MAX_BODY_BYTES` | 256 | v + epoch + 3 statements + framing |
| `MAX_SIGNATURE_BYTES` | 64 | Ed25519 sig length — **exact equality**, not upper bound |
| `MAX_STATEMENT_ID_BYTES` | 32 | Ed25519 pubkey length — **exact equality**, not upper bound |
| `MAX_CHAIN_DEPTH` | 8 | Configurable per deployment; default rejects deeper chains as malformed |
| `MAX_PUBLICATION_BYTES` | 8192 | Day-1 trust-publication JWS bytes — pre-fetch parser-bomb gate (see §"Day-1: Trust publication") |

**Exact-equality checks for fixed-size cryptographic blobs:**

- `Statement.id` MUST be exactly 32 bytes. Not `≤ 32`. Reject anything else.
- `signatures[i]` MUST be exactly 64 bytes. Reject anything else.
- `Statement.code` and `Statement.ext` MUST fit in `u16` range (`0..=0xFFFF`), even though the wire type is `int32` for Fory friendliness. Reject larger values.

**Order of enforcement (cheap-first, fail-fast):**

1. **Pre-parser** (O(1)): `len(wire_bytes) <= MAX_CHAIN_BYTES`. Bounds the parser's allocation surface BEFORE any decoder sees the bytes — closes off parser-bomb attacks at the door.
2. **Post-chain-decode** (O(depth)): each edge's bytes within `MAX_EDGE_BYTES`; chain depth within `MAX_CHAIN_DEPTH`; chain version equals 1.
3. **Post-edge-decode** (O(depth)): each body within `MAX_BODY_BYTES`; each signature exactly 64 bytes.
4. **Post-body-decode** (O(depth × 3)): each `Statement.id` exactly 32 bytes; codes within u16 range; body version equals 1.
5. **Structural checks** (O(depth)): per-edge sanity (epoch, code registry membership, signature count); chain bridging (leaf binds to expected node, edges bridge correctly, top is anchored).
6. **Signature verification** (parallel): only if 1–5 all passed.

Steps 1–5 are CPU-only, local, and combined cost is microseconds. Step 6 is where real CPU is spent, and we never reach it on malformed input.

**Why exact-equality on cryptographic blobs:** an Ed25519 sig is always exactly 64 bytes; a pubkey is always exactly 32 bytes. Accepting `≤` instead of `==` would let attackers probe parser/verifier behavior with truncated or padded blobs, which has historically been a source of CVEs in less-strict implementations. There is no legitimate use case for variable-length pubkeys or sigs in v1.

### First concrete instance: root ↔ node

A single-edge chain is the MVP shape. The leaf's `AttestationChain` contains exactly one edge whose body is:

```
body.statements = [
    Statement(id=root_pk, code=ROOT_OWNS_NODE,    ext=0),
    Statement(id=node_pk, code=NODE_OWNED_BY_ROOT, ext=0),
]
edge.signatures = [<64-byte root sig>, <64-byte node sig>]
```

Anyone with the root pubkey in their trusted-anchor-set can verify the chain in a single local check. Either party can present it. No admission-token chase, no transport state.

### Statement encoding

Each statement is **two 16-bit codes**, not a string:

- **`code`** — drawn from a registered, well-known table maintained as part of the trust spec. New entries are additive; old verifiers ignore unknown codes.
- **`ext`** — deployment / implementation custom nuance, opaque to outside verifiers.

Some `code` values may be partitioned into bitwise flags (e.g. role bits, capability bits) rather than enum-style; the registry decides per code. Strings are explicitly **out**: too easy to drift on whitespace, casing, or NFC normalisation, and signature failures from cosmetic differences are the worst kind of bug.

### Schema evolution rule

**Never add a defaulted field to an existing wire version.** Bump the version tag (`aster.attestation.body.v2`, `aster.attestation.edge.v2`, `aster.attestation.chain.v2`, separator string `aster.attestation.v2\0`) and define sibling types. Old verifiers reject v2 cleanly; new verifiers handle both.

This is the upgrade path for any future change — including the BLS option, which would be a v2 schema introducing an `alg`-tagged signature variant. Old v1 chains keep verifying under the v1 path indefinitely.

---

## MVP scope

The smallest end-to-end slice that delivers value:

### Step 0 — node attestation chain in node config

- `aster attest node` CLI command: given a root keypair and a node identity, produces an `AttestationChain` of length 1 (root↔node edge) and writes it into the node's config file.
- Aster runtime loads the chain at node start and exposes it through the existing identity surface. No protocol change to admission flow.
- `@aster` service (the operator-facing handle service hosted on `aster0.net`) consumes chains during handle resolution: `node.handle.aster0.net` returns the node iff `verify_chain()` succeeds against the deployment's anchor set.

What this requires:
- `Statement` + `AttestationBody` + `AttestationEdge` + `AttestationChain` Fory schemas registered in core.
- `attest()` / `verify_chain()` functions in core, exposed across all four bindings.
- `aster attest node` CLI subcommand.
- `@aster` service: chain parser + verifier + deployment anchor-set config.

### Step 0.5 — intermediate-key delegation

- `aster attest intermediate` CLI command: given a root keypair and an intermediate identity, produces an upper edge (`ROOT_AUTHORISED_INTERMEDIATE` ↔ `INTERMEDIATE_AUTHORISED_BY_PARENT`).
- Operator subsequently runs `aster attest node` under the *intermediate* identity; CLI assembles the leaf edge plus the upper edge into an `AttestationChain` of length 2 written into the node's config.
- Verifier requires no change beyond Step 0 — `verify_chain()` already handles arbitrary depth. The "intermediate" is just an identity that appears as a parent in one edge and as a child in another; the chain wrapper carries them both.

What this requires beyond Step 0:
- `INTERMEDIATE_*` codes exposed in the CLI (already in the registry).
- CLI logic to load existing upper edges from a configured store and concatenate them with new leaf edges.
- Configurable max-chain-depth cap in the verifier.

### Out of MVP scope

- Trust Mark slot population (slot 3 stays specified-but-empty in v1 chains).
- Two-artifact (parent-issued + subordinate self-issued) form — MVP ships one-artifact-per-edge only.
- BLS (deferred indefinitely; reserved as `v2` schema).
- Revocation infrastructure — deferred. See §"Revocation (deferred)" for the sketched design and trigger conditions. MVP relies on short epochs + re-attestation. **No half-built hooks** — when revocation lands, it lands as a complete unit.
- Trust publication via signed HTTPS URL — deferred to Day-1. See §"Day-1: Trust publication" for the sketched design (JWS Compact, `.well-known/aster` URL convention, `ASTER_TRUST_ANCHOR=` configuration, `@aster` as convenience default). Day-0 ships static config-file trust sets only. **No half-built hooks** at MVP.
- Cross-deployment replay binding (open question; not blocking MVP).
- Off-path / fetch-on-demand verifiers (audit tools that walk a directory rather than receive a bundle) — not part of the trust path.

---

## Concepts borrowed from OpenID Federation 1.0

The spec itself is overweight for our use, but several of its conceptual moves are right and we lift them. Note the one we deliberately did **not** lift: their `authority_hints` chain-walking resolution model.

### Multi-anchor resolution (trust store pattern)

Verifiers hold a **set** of trusted anchor public keys, exactly like a browser holds ~150 root CAs in its trust store. A chain is valid if it terminates at *any* anchor in the set. This makes:

- **Anchor migration**: cross-sign new anchor against old, ramp clients to add the new anchor to their set, retire old when no consumer depends on it.
- **Federation**: organisations add each other's anchors to their trust sets; nodes are mutually verifiable without a shared root.
- **Per-deployment trust scope**: a deployment can pin only its own anchor and reject all others, or accept a wider set for cross-org use cases.

### Trust Marks (slot 3)

The third statement slot is a **Trust Mark**: a third-party signed assertion attached to an entity, orthogonal to the parent-issued chain. Examples:

- A CI signer attesting "this attestation was emitted by build #X."
- A transparency-log inclusion witness.
- A compliance-attestation issuer ("this node passed audit Y").
- A peer node endorsing a sibling.

Trust Marks are *plural and optional* in concept; the v1 wire shape caps the slot at one per edge. A future revision can lift this since `statements` is already an array. Verifiers ignore Trust Marks they don't have a policy for — the chain validates regardless. Deployment policy can *require* certain Trust Marks be present.

### Why we did NOT borrow Authority Hints / chain walking

OpenID Federation's resolution model is "each Entity Statement points at its issuer; the verifier walks one hop at a time, fetching parent statements from `.well-known/openid-federation` endpoints." This works for them because they're a federated *web* protocol where every entity has an HTTP endpoint and the verifier has no choice but to walk.

Aster has control over how artifacts are produced and shipped: the producer assembles the full chain at attest-time and writes the bundle into the node's config. The verifier receives the chain in one shot and verifies it locally in parallel. Walking parent-by-parent at verification time is the engineering anti-pattern — sequential, network-dependent, and every step has its own failure mode. Bundling beats walking for any system that can produce a chain at a known point in time.

---

## Statement registry

Codes (the first u16) are governed by a versioned registry shipped with `Aster-trust-spec.md`:

- Each code has a stable name, a precise English description, and a version it was introduced at.
- Codes are **never repurposed**. Deprecation = mark deprecated, allocate a successor.
- Some codes may use the 16 bits as bitwise flags rather than as an enum — declared per-code in the registry.
- The `ext` u16 is unrestricted; deployments can use it freely. Verifiers outside that deployment treat it as opaque.

**Registry distribution: compiled-in.** The registry is a compiled-in table in the core Rust crate (and in the bindings). Verifiers ship with the registry baked into the binary; updates require a release. New codes are additive — old verifiers ignore unknown codes (when used as Trust Marks at slot 3) or reject the chain (when used at slot 0 or 1, where the code defines the relationship being attested). Registry governance — who allocates new codes, on what cadence — is an open question (see §"Open questions").

Initial registry entries:

| Code | Name | Meaning |
|------|------|---------|
| `0x0001` | `ROOT_OWNS_NODE` | "I am the root key for the associated node." |
| `0x0002` | `NODE_OWNED_BY_ROOT` | "I am a node operated by the referenced root key." |
| `0x0010` | `ANCHOR_AUTHORISED_ROOT` | (anchor inversion) "I am the anchor that authorised this deployment root." |
| `0x0011` | `ROOT_AUTHORISED_BY_ANCHOR` | (anchor inversion) "I am a deployment root authorised by the referenced anchor." |
| `0x0020` | `ANCHOR_AUTHORISED_INTERMEDIATE` | "I am the anchor that authorised this intermediate." |
| `0x0021` | `INTERMEDIATE_AUTHORISED_BY_PARENT` | "I am an intermediate authorised by the referenced parent identity." (recurses through any depth of intermediate↔intermediate edges) |
| `0x0022` | `INTERMEDIATE_OWNS_CHILD` | "I am the parent of the referenced child identity." (the bridging-identity's downward role) |
| `0x0023` | `NODE_OWNED_BY_PARENT` | "I am a node operated by the referenced parent identity." (terminal-tier counterpart to `INTERMEDIATE_OWNS_CHILD`) |
| `0x0100` | `MARK_BUILD_PROVENANCE` | (Trust Mark) "I observed this attestation emitted by build X." |
| `0x0101` | `MARK_TRANSPARENCY_LOG` | (Trust Mark) "I included this attestation in transparency log Y." |

---

## How this composes with anchor inversion

The edge primitive is identity-agnostic. A chain stacks edges of different shapes:

| Edge type | Slot 1 (parent) | Slot 2 (child) | Slot 3 (optional Trust Mark) |
|-----------|-----------------|----------------|------------------------------|
| Root ↔ node | root id | node id | (later: CI build provenance) |
| Anchor ↔ deployment root | anchor id | deployment-root id | (later: transparency log) |
| Intermediate ↔ intermediate | upper id | lower id | (any) |
| Anchor ↔ node (collapsed, single-edge) | anchor id | node id | deployment-root id (acting as bridging witness) |

A node operated by org Foo presents an `AttestationChain` with edges from leaf upward:
- 1-tier deployment: `[root↔node]`
- 2-tier with anchor inversion: `[root↔node, anchor↔root]`
- 4-tier enterprise: `[int2↔node, int1↔int2, anchor↔int1]`

The verifier logic is identical regardless of depth — same `verify_chain()`, just a different number of edges.

---

## Verification points

`verify_chain()` is called wherever Aster needs to bind a transport-layer endpoint identity (Gate 0, iroh's QUIC pubkey) to an owner identity (Gate 1, the chain's anchored root). The places this happens, in order of frequency:

1. **Connection establishment (RPC layer) — primary verification point.**
   When a consumer opens a QUIC connection to a producer, iroh authenticates the *endpoint* pubkey. That tells the consumer which key it's talking to but not which org operates it. The consumer fetches the producer's `AttestationChain` (queryable via a public service endpoint — `aster_node_chain()` — at a well-known position in every Aster service surface) and runs `verify_chain()` against the consumer's trusted-anchor-set, with `expected_node = endpoint_pubkey`. Without this verification, every connection is operating at Gate 0 only — it knows *which key* but not *which org*. **This is the verification point most likely to be skipped in a naive implementation** ("we already trust the iroh endpoint, why double-check?") and the one that matters most in production.

2. **Mesh admission (producer admits new producer).**
   When a new producer node joins an existing mesh, the existing producer that admits it verifies the joining node's chain. Verifies the new node is operated under an anchor in the existing mesh's trust set.

3. **Discovery registration (DNS / directory publishing).**
   When a producer registers itself for discovery (DNS record, directory entry), the registrar verifies the chain before publishing. Prevents unauthorised registration of a handle the registrant doesn't actually control.

4. **Handle resolution at `@aster`.**
   `@aster`'s service for `node.handle.aster0.net` verifies the chain before resolving the handle. The resolved record is only returned if the chain validates against the deployment's anchor set.

5. **Re-verification on trust-set change.**
   When a verifier's `trusted_anchors` changes (operator config update, Day-1 publication-fetch returning a new set), all cached "this chain validated successfully" results MUST be invalidated. Next verification re-evaluates from scratch. The trust-set change might have removed an anchor that previously rooted a chain.

6. **Audit and off-path monitoring.**
   Third-party watchers, transparency-log monitors, security tooling. Same `verify_chain()` API — no special-case audit path.

### Public chain-fetch endpoint

Every Aster service exposes its current `AttestationChain` at a well-known position in its service surface:

- API: `aster_node_chain() -> AttestationChain`
- Protocol-level: a request that ANY Aster transport supports, separate from the application's RPC contract.

The endpoint is read-only and unauthenticated — the chain is a public artifact (its security comes from signatures, not access control). Anyone who can reach the endpoint can read the chain and verify it locally.

### Re-verification cost

Verification is microseconds per chain (parallel sig verifies + structural checks, no I/O). Re-verifying on every connection establishment is not a performance concern. Caching is an optimisation, not a necessity; if it's used, the cache MUST be invalidated on trust-set change (per #5 above).

---

## Text encoding for human-encountered blobs

Aster blobs that travel through human-readable contexts (config files, log messages, CLI output, error reports) are encoded as **`<typetag>:<base64url>`** — the type tag matches the Fory `@WireType` name, base64url is unpadded.

```
aster.attestation.chain.v1:eyJ2IjoxLCJlcG9jaCI6...
aster.attestation.edge.v1:<base64url>          # rare; usually only inside a chain
aster.revocation.v1:<base64url>                # Day-2+
```

This serves the same purpose as the `aster0` prefix on identities: anyone who encounters such a string in a log, config, or stack trace can immediately tell what type it is and find the spec. Blobs without the prefix are bare bytes (wire format, internal use); blobs with the prefix are the canonical human-readable form.

The `verify_chain()` API takes raw bytes; a thin wrapper (`decode_text_form(text) -> bytes`) strips the prefix and decodes base64url. No special-casing in the verifier — encoding lives in a separate function.

Comparison to other Aster human-encountered forms:

| Artifact | Form | Example |
|----------|------|---------|
| Identity (Ed25519 pubkey) | `aster0` + bech32-encoded pubkey | `aster0xq...` |
| `AttestationChain` (this doc) | `aster.attestation.chain.v1:` + base64url | `aster.attestation.chain.v1:eyJ...` |
| `Revocation` (Day-2+) | `aster.revocation.v1:` + base64url | `aster.revocation.v1:eyJ...` |
| Trust publication (Day-1) | JWS Compact (already self-described by structure + `typ` header) | `eyJhbGciOiJFZERTQSI...` |
| `did:key` alias | `did:key:z6Mk...` | (W3C standard, kept for ecosystem interop) |

Trust publications don't need an Aster-specific prefix because JWS Compact is already self-describing (three dot-separated base64url segments; `typ` header inside identifies the application).

---

## Delegation chains — the enterprise scaling story

Two-tier covers most deployments. Large enterprises don't fit two tiers: there's a corporate anchor, an org-unit signer, a regional CA-equivalent, an environment root, then nodes. The chain primitive scales to this **without adding a new artifact** — just by stacking more edges in the bundle.

### The model

What we currently call a "root key" is just one role an identity can play. The same key can play two roles in two different edges of the same chain:

- In edge **A**: it's the *child* of some higher identity.
- In edge **B**: it's the *parent* of some lower identity.

That's all an "intermediate" is. There's no special intermediate-key type. Concretely:

```
Edge B (intermediate ↔ node) — leaf edge:
    body.statements = [
        Statement(id=intermediate_id, code=INTERMEDIATE_OWNS_CHILD, ext=0),
        Statement(id=node_id,         code=NODE_OWNED_BY_PARENT,    ext=0),
    ]
    signatures = [Ed25519 by intermediate, Ed25519 by node]

Edge A (anchor ↔ intermediate) — upper edge:
    body.statements = [
        Statement(id=anchor_id,       code=ANCHOR_AUTHORISED_INTERMEDIATE,    ext=0),
        Statement(id=intermediate_id, code=INTERMEDIATE_AUTHORISED_BY_PARENT, ext=0),
    ]
    signatures = [Ed25519 by anchor, Ed25519 by intermediate]

Chain = AttestationChain(v=1, edges=[Edge B, Edge A])  # leaf-first
```

`intermediate_id` appears in both — it's the **bridging identity**. The chain's structural-well-formedness check (`edges[i+1].child == edges[i].parent`) confirms the bridge.

Scaling to N tiers: just stack more `INTERMEDIATE_OWNS_CHILD` / `INTERMEDIATE_AUTHORISED_BY_PARENT` edges between the leaf and the anchor edge.

### Verifier semantics for a chain

Already shown above in the `verify_chain()` pseudocode. Summary of properties:

- **All I/O happens before verification** (chain delivered as a single bundle).
- **All signature verifications run in parallel** — N edges × 2 sigs each.
- **Structural check is local and O(depth)** — chain ordering, parent↔child bridging, anchor membership, leaf binding.
- **Failure is single-point**: any check failing rejects the whole chain. No partial-trust state.
- **No retries, no fallbacks, no cache lookups** on the trust path.

A configurable `MAX_CHAIN_DEPTH` defends against pathologically long chains (e.g. 8 by default; tunable per deployment).

### Why this is not a CA chain

Visually similar, structurally different. Worth being explicit:

| | X.509 CA chain | Aster delegation chain |
|---|---|---|
| Direction | Unidirectional: parent issues cert to child | Reciprocal: both endpoints sign each edge |
| Subject consent | Implicit (child requests, parent grants) | Cryptographic (child must hold a private key and sign every edge it appears in) |
| Per-edge artifact | One certificate, one signature | One edge, ≥2 signatures over a single canonical body |
| Constraints | Name constraints, EKU, policy OIDs | `code` + `ext` bits, declared in the registry |
| Resolution | Verifier walks a chain often discovered in-band (TLS) | Producer ships full bundle; verifier checks locally and in parallel |
| Revocation | CRL / OCSP | See §"Revocation (deferred)" — gossip-topic design for sub-epoch response when needed; otherwise relies on operator policy (`not_after` per edge or trust-set updates) |
| Cross-signing | Possible but operationally painful | At the trust-set layer (multi-anchor resolution), not the chain layer. Each chain is single-rooted; verifiers add multiple anchors to their trust set for federation. |

The reciprocal-signing property is the key difference. In a CA, a compromised parent can mint subordinates without those subordinates ever knowing or consenting. Here, the bridging identity must actively sign every edge it appears in — a compromised parent can still mint a fake intermediate from scratch (and then run the chain forward), but it cannot forge a chain through an *existing* intermediate without compromising that intermediate too.

### When BLS aggregation would justify a v2 schema

Today every edge carries 2 Ed25519 sigs. A 5-tier chain = 10 verifications, all parallelisable, on the order of microseconds across cores. Bulk-verifying 1000 chains during directory cache warmup is similarly cheap.

BLS aggregation becomes a real win when:

- **Chain depth** routinely exceeds 5 tiers and a single pairing op for the whole chain beats 2N parallel Ed25519 verifies (very large enterprise topologies).
- **Bulk verification** reaches tens of thousands of chains per directory refresh (P2P scale rather than enterprise scale).
- **Aggregate proofs across an organisation** become a product feature ("here is one signature attesting to all 50 000 of our nodes").

If/when those land, the upgrade path is a **v2 schema bump** introducing an alg-tagged signature variant. v1 verifiers and v2 verifiers run side-by-side during a deprecation window. Existing v1 chains remain valid forever (or until their epoch expires); new v2 chains can opt into BLS where it pays off.

### Multi-anchor / federation / migration happens at the trust-set layer

Each node has exactly one chain. Each identity has exactly one parent. We do **not** support chain-level cross-signing (a node holding multiple alternate chains, an intermediate co-signed by two anchors with both upper edges). The complexity isn't justified — the use cases that motivated cross-signing are all served better elsewhere:

- **Trust-root migration**: handled by the Day-1 trust publication's `successor` mechanism (publication carries the rotation; chains stay single-rooted).
- **Federation across orgs**: handled by multi-anchor trust-set resolution (each verifier holds both orgs' anchors; each org's chains are single-rooted at their own anchor).
- **Tier drift** (intermediate added/removed): operators re-issue affected upper edges and push new chains. No chain-level alternates.

The result is a much simpler chain structure: one chain per node, exactly one parent per identity, no "alternates" for the verifier to reason about.

- **Org-shaped trust topologies.** Companies model their identity graph however they need: tier per business unit, tier per environment, tier per region. The protocol doesn't care.
- **Local autonomy with global verifiability.** A regional intermediate can mint nodes; the chain bundle assembled at provisioning time carries the full path up.
- **Smaller blast radius.** Compromise of an intermediate scopes to its subtree; verifiers walk to the next-up intact tier — except they don't *walk*, they *read*, since the chain is in hand.
- **Same primitive top to bottom.** No "intermediate certificates" type, no CA-specific code paths. The implementation in core is the same `attest_edge()` / `verify_chain()` regardless of tier.

---

## Revocation (deferred to post-MVP, sketched here)

Day-0 ships **without an explicit revocation mechanism**. The combination of short epochs (e.g., 7 days) and anchor-set updates already covers the realistic threats:

| Compromise | How it's handled without explicit revocation |
|---|---|
| Node key leaked | Stops being usable within one epoch. Operator removes node from CI inventory; old chain expires naturally. |
| Intermediate compromised | Attacker can mint chains under the intermediate until its edge expires. Operator stops re-issuing under that intermediate; subtree expires within one epoch. |
| Anchor compromised | Anchor rotation (cross-sign new against old, update anchor sets at consumers) — handled separately, not revocation. |
| Routine trust withdrawal | Stop re-attesting. Expires within one epoch. |

Explicit revocation gains exactly one thing: **emergency response time faster than the epoch.** For MVP-scale deployments, the epoch-bounded window is acceptable. When production scale demands sub-hour response, the design below is the deferred plan — sketched now so a future implementer doesn't reinvent it differently.

### The deferred design: gossip-topic revocation

The P2P architecture makes revocation operationally cleaner than CRL/OCSP. No HTTPS endpoints, no central server, no fail-open behavior when the revocation source is unreachable — iroh-gossip handles distribution.

**Wire format**: same Fory + opaque-body + domain-separator discipline as attestations.

```python
@WireType("aster.revocation.body.v1")
class RevocationBody:
    v: int32                 # = 1
    epoch: int64             # when this revocation takes effect
    revoker: bytes           # 32-byte pubkey of the revoking party
    target_kind: int32       # 0 = identity (revoke all chains containing this id),
                             # 1 = edge (revoke a specific edge by content hash)
    target: bytes            # 32 bytes — pubkey if target_kind=0, edge hash if =1
    reason_code: int32       # registry: compromise, decommission, misissuance, ...

@WireType("aster.revocation.v1")
class Revocation:
    body: bytes              # opaque
    signature: bytes         # 64 bytes, by revoker
```

Domain separator: `b"aster.revocation.v1\0"`.

**Distribution**: iroh-gossip topic per anchor (e.g., `aster.revocation.<anchor_pubkey_hash>`). Verifiers subscribe to the topics for anchors they trust. Append-only by convention; revocations are signed and survive replay.

**Authorization**: a revoker can only revoke identities/edges in its own subtree. The anchor revokes anything below it; an intermediate revokes anything below itself; a node revokes only itself.

**Verifier behavior**: maintains an in-memory revocation set keyed by `(target_kind, target)`; rejects any chain containing a revoked identity or revoked edge content-hash, in addition to all the existing v1 checks.

**Failure mode**: if a verifier hasn't subscribed (or gossip is delayed), revocation simply doesn't apply — the chain is evaluated under epoch-based expiry only, same as today. Revocation strictly *accelerates* the existing security model; it never replaces it.

### Trigger to activate (open question)

When does revocation justify the engineering cost? Concrete metrics:

- A deployment requiring sub-hour response to compromise.
- Multi-tenant environments where one operator's compromise window can't span others' epochs.
- Compliance regimes that mandate revocation infrastructure.

Until one of these surfaces, MVP ships without it.

### What MUST NOT happen at MVP

Resist the temptation to ship a half-built revocation hook (a stub field, an empty topic, a config flag) "just in case." Half-built security infrastructure is worse than none — it creates the impression of revocation without the mechanism, which is exactly the failure mode that has historically broken CRL and OCSP in the wild ("we have CRLs, but nobody actually checks them"). The revocation design lands as a single complete unit when it lands.

---

## Lost or compromised key handling

The recovery mechanisms are deliberately simple. Recovery is rare; the mistakes are expensive; the discipline is **the trusted-anchor-set is the only mutable state in the verifier — update it, and the rest follows.**

### The damage-bounding property of reciprocal signing

Compromise of any single private key in the system does NOT let an attacker impersonate existing identities below it. Each edge requires both the parent and the child to sign; the attacker only has the parent's key. The attacker can:

- **Mint new identities** under the compromised key (generate a new keypair, sign attestations binding it to the compromised parent). These are *fabricated* identities the attacker controls.
- **NOT impersonate existing children** — those identities' private keys are still held by the legitimate parties.

So compromise damage scales with "what new identities can the attacker stand up under this key?" rather than "what existing identities can the attacker take over?". This is the security argument for reciprocal signing: it doesn't prevent compromise, it bounds what compromise enables.

### The recovery lever: the trusted-anchor-set

The verifier's `trusted_anchors: set[bytes]` is the only mutable state in trust derivation. Every recovery action eventually reduces to "modify the trusted-anchor-set."

- **Add new key**: routine rotation (new key starts being trusted).
- **Remove old key**: emergency removal (old key stops being trusted; chains under it fail immediately).
- **Both atomically**: anchor migration or replacement.

Each consumer / verifier / `@aster` service holds its own trusted-anchor-set. Updating them is the operator's job; the protocol's job is to make the update mechanism authenticatable and auditable.

### Per-tier scenarios

Single-tier (root↔node) is the MVP topology; two-tier (anchor↔root↔node) is Step 0.5. Deeper N-tier deployments inherit the same pattern at each tier.

| Scenario | Mechanism | Recovery time |
|---|---|---|
| **Lost deployment root** (single-tier; operator can no longer sign) | Generate new root keypair; re-attest every node under new root; update `@aster` handle registry to bind the handle to the new root pubkey; consumers update their trust sets. | Bounded by re-attestation throughput (CI run) + trust-set propagation. Typically minutes to hours. |
| **Compromised deployment root** (single-tier; attacker has privkey) | Remove old root from all trust sets immediately (the recovery lever); generate new root; re-attest. For deployments using finite `not_after`, old chains additionally expire; for `not_after = 0` deployments, trust-set removal is the only mechanism until revocation lands. | Trust-set propagation determines the floor. `not_after = 0` deployments have no natural backstop; trust-set update must reach every consumer. |
| **Lost deployment root** (two-tier; anchor still held) | Anchor signs a new deployment-root edge; CI re-attests nodes under the new deployment root; nodes get new chains via config push. **Anchor is unchanged. Trusted-anchor-sets are unchanged.** | Bounded by CI throughput. The anchor's offline ceremony is not invoked. |
| **Compromised deployment root** (two-tier) | Same as lost-deployment-root above. The compromised root's chains expire naturally within one epoch; if faster response is needed, that's the deferred revocation design. | Minutes to hours (ceremony for new root) + epoch (for old root chains to expire). |
| **Lost anchor** (catastrophic) | Operator has lost their cryptographic identity at the apex. Generate fresh anchor; re-bootstrap by proving "I am the same org" via out-of-band channels (operator account at `@aster`, vendor security advisory, customer notifications). All consumers must update trust sets to add new anchor. | Days to weeks; this is a security incident, not a routine operation. |
| **Compromised anchor** (catastrophic, adversarial) | Same as lost anchor, but with active adversary. Cross-signing the new anchor against the old does NOT work — the attacker can publish counter-signings too. Recovery requires *exclusively* out-of-band signaling: operator's authenticated channel at `@aster`, public security advisory, direct customer notification. Old anchor must be removed from all consumer trust sets manually. | Days to weeks; same posture as lost anchor, but with adversarial pressure. |

### Why lost-anchor and compromised-anchor have the same recovery posture

In both cases, the cryptographic chain of trust is broken at the apex. There is no key above the anchor to authorise its replacement. The recovery mechanism is *necessarily* out-of-band — the operator proves identity through other means (account at `@aster`, public attestations, business-level verification).

This is why anchors are deliberately rare events:

- Generated infrequently (once per organisation, ideally never re-generated).
- Stored in HSM / cold storage / multi-party custody.
- Compromise is designed to be near-impossible operationally; recovery is designed to be possible but slow and ceremonial.

The recovery is asymmetric on purpose: easy enough that legitimate operators can execute it under duress, slow enough that an attacker who somehow obtained the anchor still can't quickly weaponise it (because they'd also need to compromise the operator's `@aster` account, security-advisory channels, etc.).

### Out-of-band recovery channels (MVP requirements)

For Day-0, the single non-cryptographic dependency is the operator's authenticated account at the `@aster` handle service. This account:

- Is the only channel for emergency trust-set updates at `@aster` for hosted handle resolution.
- Should use multi-factor authentication (webauthn-custodian or equivalent — see related docs).
- Should require additional confirmation (cooldown, multi-channel verification, manual review) for anchor-replacement operations.

Customers / consumers running their own deployments are responsible for their own out-of-band recovery — the protocol doesn't dictate how they update their local trust sets, only that doing so is the recovery lever.

### CLI surface for recovery (MVP)

- `aster trust generate` — new keypair (existing).
- `aster attest node --root <new_root>` — re-attest a node under a new root.
- `aster attest intermediate --anchor <anchor> --root <new_root>` — anchor signs new deployment root (Step 0.5).
- `@aster` web UI / CLI for trust-set updates on the handle registry side (authenticated via operator account).

No protocol-level "revoke this key" command at MVP — that's the deferred revocation section above. Recovery at MVP is "rotate keys + push new chains + update trust sets."

### What this design DOES NOT promise

- **Real-time compromise detection.** MVP has no monitoring. Operators must detect compromise themselves (via logs, observed unauthorised activity, alerts from third-party watchers, etc.). Future transparency-log work makes this auditable by independent parties.
- **Sub-epoch response.** For deployments using finite `not_after`, expiry provides a natural backstop. For `not_after = 0` deployments (long-lived stable systems), there is no time-driven backstop — recovery requires trust-set update propagation to every consumer. Faster response is the deferred-revocation design above; `not_after = 0` deployments have a stronger case for activating it.
- **Recovery without operator action.** There is no auto-recovery. Every recovery flow requires a human at the operator side performing the ceremony.
- **Recovery from total operator loss.** If the operator's `@aster` account is also compromised (or unrecoverable), the trust protocol cannot help — recovery falls to `@aster`'s out-of-band identity verification (support contact, business-level verification), which is outside the trust protocol's scope.

---

## Day-1: Trust publication via signed URL (sketched, not built at Day 0)

Day-0 ships the trusted-anchor-set as operator-edited config files (per §"Lost or compromised key handling"). Day-1 layers on a publication mechanism so operators can host their own canonical trust statement at an HTTPS URL they control. This closes the loop on root/node trust: the root (or anchor) key publishes its own current state at a URL it controls; verifiers fetch and validate the signature; `@aster` becomes a convenience default rather than a required dependency.

The pattern is deliberately modeled on LetsEncrypt / ACME's HTTP-01 challenge: **HTTPS is the publication channel, not the trust authority.** The signature on the published artifact is what makes it trustworthy; web-server compromise or TLS-MITM can DoS the publication but cannot forge it (no privkey). We use the standard `.well-known/` URI convention (RFC 8615) rather than DNS TXT records — well-known paths are the universal pattern for service-level metadata (ACME, WebFinger, OpenID Connect, OAuth Server Metadata all use them).

### Wire format: JWS, not Fory

Trust publications are **boundary artifacts** — published on the open web, fetchable by `curl`, inspectable by `jq`, consumed by Aster verifiers and (occasionally) by third-party audit tools. The constraints invert from internal artifacts:

- One writer (the publisher), many readers verifying signatures.
- Cross-binding determinism is irrelevant.
- Operators expect to inspect with standard web tooling.
- Universal signed-JSON support already exists in every binding language and toolchain.

The right primitive is **JWS** (RFC 7515) Compact Serialization with EdDSA. JWS is the standard for signed JSON on the web, has universal library support, and is DSSE-style by design — the signed bytes are exactly `base64url(protected_header) + "." + base64url(payload)`, so no canonicalisation drift between signer and verifier.

JWS Compact form (three base64url segments separated by dots):

```
eyJhbGciOiJFZERTQSIsInR5cCI6ImFzdGVyLnRydXN0X3B1YmxpY2F0aW9uLnYxIn0
.
eyJ2IjoxLCJlcG9jaCI6MTc0ODk5MDY1NiwibR... (payload JSON, base64url)
.
<base64url-Ed25519-signature-64-bytes>
```

Decoded protected header:

```json
{
  "alg": "EdDSA",
  "typ": "aster.trust_publication.v1"
}
```

The `typ` field is the JWS-native cross-type-replay defense — equivalent to the domain separator on internal artifacts. JWS verifiers MUST check `typ` matches the expected value before trusting signature verification.

Decoded payload:

```json
{
  "v": 1,
  "epoch": 1748990656,
  "not_before": 1748990656,
  "not_after": 1749077056,
  "publisher": "<base64url-32-byte-Ed25519-pubkey>",
  "anchors": [
    "<base64url-32-byte-Ed25519-pubkey>",
    "<base64url-32-byte-Ed25519-pubkey>"
  ],
  "successor": "<base64url-32-byte-Ed25519-pubkey-or-omitted>",
  "successor_attestation": "<base64url-AttestationEdge-bytes-or-omitted>"
}
```

`successor_attestation` carries the only Fory-encoded artifact that crosses the boundary: an `AttestationEdge` (with statements `ANCHOR_AUTHORISED_INTERMEDIATE` ↔ `INTERMEDIATE_AUTHORISED_BY_PARENT` or equivalent) proving the rotation from `publisher` to `successor`. Verifiers decode the base64url, then feed the bytes to the existing internal `decode_edge` / `verify_edge_signatures` machinery (with the `aster.attestation.v1` domain separator). Clean separation: JWS for the JSON envelope, Fory for the embedded internal artifact.

### Configuration

Each Aster verifier (node, `@aster` service, audit tool) holds a publication URL in config. **HTTPS only** — no other URL schemes are supported.

```
# Default: @aster's hosted publication for this handle
ASTER_TRUST_ANCHOR=https://trust.aster0.net/<handle>

# Operator-hosted at the standard well-known path
ASTER_TRUST_ANCHOR=https://acme.com/.well-known/aster

# Operator-hosted at a static file host (GitHub Pages, S3, R2, etc.)
ASTER_TRUST_ANCHOR=https://acme.github.io/aster-trust.jws

# Optional pinning (paranoid mode): reject publications whose publisher pubkey
# doesn't match the pinned value. Without this, the URL is the trust binding.
ASTER_TRUST_ANCHOR_PIN=<base64url-32-byte-Ed25519-pubkey>
```

The default points at `@aster`'s hosted publication (one URL per registered handle). Operators replace with their own HTTPS URL once they've stood up their own publication.

The `.well-known/aster` path follows IETF RFC 8615 conventions for well-known URIs and matches the convention used by ACME, WebFinger, OpenID Connect, and OAuth Server Metadata. Operators who don't want to use the well-known path can host the JWS at any HTTPS URL — the URL is opaque to the protocol; only the signature matters.

Recommend `Content-Type: application/jose` on the HTTPS response.

**Why HTTPS-only and not DNS:** an earlier sketch included a `dns:` scheme that resolved DNS TXT records (either as an HTTPS URL pointer or with an inline JWS). Dropped because (a) inline JWS in TXT is broken at any realistic publication size — even minimal publications exceed the 255-byte single-string limit, (b) the URL-pointer pattern is just an indirection that adds complexity without adding capability, (c) `.well-known/` URL discovery is the universal 2026 convention, and (d) operators without HTTPS access (airgapped, embedded) are served by the Day-0 static-config-file mechanism, not by DNS.

### Verifier algorithm (publication-fetch path)

```python
def fetch_and_verify_publication(
    anchor_uri: str,
    pinned_publisher_pk: Optional[bytes],
) -> set[bytes]:
    """
    Fetch the trust publication from the operator's URL, validate signature,
    return the current trusted-anchor-set. Cache result until `not_after`.
    """
    # 0. Pre-fetch size gate
    jws_bytes = https_get(anchor_uri)                   # HTTPS GET only
    assert len(jws_bytes) <= MAX_PUBLICATION_BYTES      # recommend 8192

    # 1. Parse JWS Compact: header.payload.sig
    header_b64, payload_b64, sig_b64 = jws_bytes.split(b".")
    header = json.loads(base64url_decode(header_b64))
    assert header["alg"] == "EdDSA"
    assert header["typ"] == "aster.trust_publication.v1"

    # 2. Reconstruct signing input — bytes as received, no re-encoding
    sign_input = header_b64 + b"." + payload_b64
    sig = base64url_decode(sig_b64)
    assert len(sig) == 64                               # exact

    # 3. Decode payload, identify publisher
    payload = json.loads(base64url_decode(payload_b64))
    assert payload["v"] == 1
    publisher_pk = base64url_decode(payload["publisher"])
    assert len(publisher_pk) == 32                      # exact

    # 4. Pinning check (if configured)
    if pinned_publisher_pk is not None:
        assert publisher_pk == pinned_publisher_pk

    # 5. Verify signature against claimed publisher
    ed25519_verify(publisher_pk, sign_input, sig)

    # 6. Freshness + monotonic-replay protection
    #    Note: in TRUST PUBLICATIONS, not_after is REQUIRED (must be > 0) — publications
    #    need a finite TTL to force re-fetch. This differs from AttestationBody, where
    #    not_after = 0 means no expiry. Different schemas, different semantics.
    assert payload["not_before"] <= now() <= payload["not_after"]
    assert payload["not_after"] > 0
    assert payload["epoch"] >= cached_publication_epoch_for(anchor_uri)

    # 7. Optional rotation: validate successor attestation as a length-1 chain
    if payload.get("successor"):
        successor_pk = base64url_decode(payload["successor"])
        edge_bytes = base64url_decode(payload["successor_attestation"])
        verify_chain(
            wrap_as_chain(edge_bytes),
            trusted_anchors={publisher_pk},
            expected_node=successor_pk,
        )

    # 8. Return the current trusted-anchor-set
    return {base64url_decode(a) for a in payload["anchors"]}
```

The fetched anchor set is then passed to `verify_chain()` calls as the `trusted_anchors` argument. Cache the result until `not_after`; refresh on cache miss, expiry, or on-demand on operator request.

### Recovery using the publication

| Scenario | What changes in the publication |
|---|---|
| Routine rotation | New publication: `successor` field names new anchor; `successor_attestation` is the signed edge from current publisher to successor. Verifiers fetch on next TTL boundary, validate the rotation, update their cache. |
| Compromised root | Same as rotation, plus operator removes the compromised key from `anchors` list. Verifiers see updated state on next fetch. |
| Lost anchor at the apex | This mechanism doesn't save you — same out-of-band re-bootstrap as documented in §"Lost or compromised key handling." For the deployment-root tier *under* an anchor, publication updates handle it routinely. |
| De-listing from `@aster` | Operator stands up their own publication URL; updates `ASTER_TRUST_ANCHOR` config in consumers; ramps consumers off the `@aster` default. No `@aster` involvement after migration. |

### Architectural rule (boundary discipline)

The two wire formats serve different worlds and the rule should be respected:

- **Inside Aster** (attestations, chains, future revocation entries): Fory + opaque-body + `aster.<artifact>.v1\0` domain separator + Ed25519. Cross-binding determinism, no JOSE library dependency in the verification hot path.
- **At the Aster boundary** (trust publications, future federation broadcasts, anything operators publish on the open web for third-party consumption): JWS Compact + JSON payload + `typ` header for replay protection + Ed25519. Universal tooling, `curl | jq`-inspectable.

JWS appears in exactly one place in the system: at the boundary. JOSE is not in the verification hot path; it's in the trust-publication-fetch path specifically.

### Cautions and failure modes

1. **TTL discipline.** Too long, rotations don't propagate; too short, verifiers hammer the URL. Recommend `not_after - not_before` defaults to 24 hours with operator override.
2. **Fail closed on fetch failure.** A verifier with no cached publication that cannot reach the URL MUST refuse to verify any chain. Soft-fail is the X.509 OCSP mistake. Stale-cache fallback during transient unavailability is acceptable; permanent stale-trust is not.
3. **Replay protection.** `epoch` is monotonic per publisher. Verifiers reject publications with `epoch` ≤ their currently-cached one, even if signature otherwise valid.
4. **Pinning option.** Operators paranoid about URL takeover can pin `ASTER_TRUST_ANCHOR_PIN=<pubkey>`. Verifier rejects publications whose `publisher` differs. Without pinning, the URL is the trust binding (TOFU at the URL layer — same as TOFU on the pubkey layer would be without it).
5. **Bootstrap chicken-and-egg.** A fresh verifier with no cache and no network can't fetch a publication. Mitigation: ship verifiers with the URL in static config (URLs are stable across rotations) plus optionally a pinned bootstrap pubkey.
6. **Size limit.** Enforce `MAX_PUBLICATION_BYTES = 8192` before parsing; rejects allocation-amplification attacks on malicious responses.
7. **`@aster`'s own publication.** `@aster` runs its own publication using exactly this mechanism — eats its own dogfood. There is no special-case bootstrap for `@aster`'s trust state.

### What MUST NOT happen at MVP

- Don't ship a half-built publication-fetch hook (config flag with no consumer, empty default URL, etc.). Same posture as deferred revocation — landing it later is mechanical because the design is settled here.
- Don't introduce JWS into the internal verification hot path (the `verify_chain()` function and its dependencies stay Fory-only). JWS lives at the boundary, not inside.

### Day-0 to Day-1 transition

Non-breaking. Day-0 verifiers consume static config-file trust sets; Day-1 verifiers gain an additional source (the publication URL). The locked `verify_chain(wire_bytes, trusted_anchors, expected_node)` API stays unchanged — Day-1 code populates `trusted_anchors` from the publication instead of from a config file before calling. Operators upgrade at their own pace; the protocol is unchanged below the trust-set-population layer.

---

## Considered and rejected: pure off-the-shelf

### JWS-chains / OpenID Federation 1.0

Recommended in the previous revision. Rejected on second look:

- Spec is optimised for European eIDAS IDPs, not P2P.
- Trust-chain resolution requires HTTP fetches of `.well-known/openid-federation`, JWKS endpoints, and authority-hint walking — operationally heavy and depth-sequential.
- "Metadata policy" merging rules are byzantine; conformance bugs are common.
- Library support is concentrated in EU IDP vendors; binding-language coverage is poor.
- Adoption outside eIDAS 2.0 mandates is essentially zero.

We borrow its valuable concepts (multi-anchor resolution, Trust Marks, self-issued/parent-issued split) without adopting its wire format or its chain-walking resolution protocol.

### COSE_Sign

Considered as the wire envelope. Rejected:

- Carries weight (per-signer `alg`, `kid`, protected/unprotected header split, counter-signature slots) for problems we don't have under the v1 design.
- Brings a second wire-format dependency into every binding when Fory already covers our needs.
- Binding library quality varies (Java in particular is fragmented).
- The interop story (FIDO/passkey/C2PA) doesn't exist for our artifact type; nobody outside Aster will parse these.

A mapping from `AttestationEdge` to `COSE_Sign` is documented as informative reference (see appendix) for the unlikely event that someone wants to consume an Aster edge in COSE-speaking infrastructure.

### Parent-pointer chain walking

The OpenID Federation `authority_hints` model: each artifact names its immediate parent; the verifier walks one hop at a time, fetching the parent's attestation from a directory or cache. Rejected:

- Sequential by nature — no parallelisation of the trust-derivation path.
- Every step has its own failure mode (cache miss, directory unavailable, partial fetch, stale parent).
- Common engineering anti-pattern; bundling beats walking when the producer can assemble the chain at a known point in time.

We bundle at the producer (`AttestationChain`) and verify in parallel.

### X.509

Real reasons for the developer-ergonomics objection: ASN.1/DER encoding is hostile, RFC 5280 path validation is famously easy to get subtly wrong, the X.500 DN naming model is a relic, OCSP/CRL revocation is operationally painful. We document X.509 as a supported integration path for organisations that already have PKI investments and want to issue Aster identity from their existing CA, but it is not the default.

### W3C Verifiable Credentials

Wrong shape: VCs are unidirectional (issuer asserts about subject; subject does not co-sign). Expressing root↔node ownership as VCs requires two cross-referencing credentials, doubling artifact size and losing the reciprocal-signing property.

### Sigstore as a runtime dependency

Right pattern (short-lived signing + transparency log) but the actual stack binds to OIDC IDPs and centralised Rekor infrastructure, contradicting the "the org owns its trust" principle. We borrow the *pattern* (Trust Mark of role `MARK_TRANSPARENCY_LOG`) for high-trust deployments without adopting Fulcio/Rekor as a runtime dependency.

---

## Wire format

**Fory** with `@WireType`-pinned schemas, as defined in the primitive section above. We already use Fory across all four bindings for RPC payloads (cross-binding matrix green since 2026-04-20). Reusing it here means:

- No additional wire-format dependency in any binding (no CBOR, no JOSE, no COSE library).
- Cross-binding determinism is already in production for RPC; the same property carries over to attestation edges and chains.
- Schema versioning discipline (`@WireType` pinning, never-add-defaulted-fields rule) is the existing rule applied to a new artifact type.

Each edge body is signed opaquely (DSSE-style) so the verifier and signer are bit-identical on what's signed — they're literally signing the same bytes that come off the wire, not a re-serialization. The chain wrapper is unsigned (its security comes from each edge's individual signatures).

Domain separator `b"aster.attestation.v1\0"` scopes signatures to this artifact type. Future schema versions get their own separator (`v2`, `v3`, ...). This forecloses any cross-version replay even if the body bytes happened to collide.

**Wire-size budget** (with `not_before` / `not_after` int64 fields adding ~16 bytes per body):
- One edge ≈ 170 bytes (2-of-2 Ed25519, ~98-byte body, ~64-byte sigs each, Fory framing).
- 1-tier chain (root↔node) ≈ 170 bytes.
- 2-tier chain (anchor↔root↔node) ≈ 340 bytes.
- 5-tier worst-case chain ≈ 850 bytes.

For comparison: an X.509 chain is typically 4 KB+; a JWS chain is ~1.5 KB per layer.

`did:key` alias for every Aster identity, regardless of attestation scheme. Cheap, useful, ecosystem-friendly.

---

## Relationship to existing Aster artifacts

- **Admission tokens** stay as-is. They authorise the node to *act* in the network. Ownership attestations are about *provenance display*, not authorisation.
- **Endorsements** stay as-is. Attestations don't replace them; they sit alongside.
- **Anchor delegation** (from the retired inversion doc) becomes one specific shape of `AttestationEdge` (codes `ANCHOR_AUTHORISED_ROOT` / `ROOT_AUTHORISED_BY_ANCHOR`) inside an `AttestationChain`.

Nothing in the current spec needs to change for this to ship. It's purely additive.

---

## Resolved decisions

- **Primary artifact:** `AttestationChain` — bundled, leaf-first, all edges shipped together. Verifier checks locally and in parallel.
- **Wire format:** Fory-serialized, `@WireType`-pinned. Types: `Statement`, `AttestationBody`, `AttestationEdge` (signed), `AttestationChain` (unsigned wrapper).
- **Signing input (per edge):** `b"aster.attestation.v1\0" || body_bytes`. Domain-separated. Bytes signed are exactly the bytes carried in `edge.body` (DSSE-style opaque-body pattern).
- **Canonical orderings (mandatory; out-of-order = invalid):**
  - *Within an edge*: `signatures[i]` is by `statements[i].id`. Slot 0 = parent-role signature; slot 1 = child-role signature; slot 2 = optional Trust Mark signature. Positional binding enforced by `ed25519_verify` per slot.
  - *Within a chain*: `chain.edges` is **leaf-first**. `edges[0]`'s child (slot 1) is the node; `edges[-1]`'s parent (slot 0) is the anchor.
  - Verifier MUST NOT sort, retry, or otherwise recover from misordering. Reject malformed input.
- **Chain wrapper signing:** none. Security comes from each edge's signatures.
- **Verifier API contract:** exactly one public function (`verify_chain`). No "verify just signatures" path, no "structural-only" path, no partial-trust escape hatch. Structural checks MUST run and pass before any signature verification result is trusted. The structural checks (not the signatures) are the load-bearing security boundary against cross-chain edge composition attacks.
- **Size limits (mandatory, enforced before signature verification):** `MAX_CHAIN_BYTES = 4096` (pre-parser gate), `MAX_EDGE_BYTES = 512`, `MAX_BODY_BYTES = 256`. Fixed-length cryptographic blobs use **exact-equality**, not upper bound: `MAX_SIGNATURE_BYTES = 64`, `MAX_STATEMENT_ID_BYTES = 32`. `Statement.code` and `Statement.ext` MUST fit in `u16` (`0..=0xFFFF`).
- **Revocation:** deferred to post-MVP. Day-0 ships epoch-based expiry only. Gossip-topic design sketched in §"Revocation (deferred)" but not built. **No half-built revocation hooks** (no stub fields, no empty topics, no config flags) — landing it later is mechanical because the design is settled.
- **Recovery from key loss / compromise:** the trusted-anchor-set is the *only mutable state in trust derivation*. Every recovery action (lost root, compromised root, lost anchor, compromised anchor) reduces to "modify the trusted-anchor-set." Anchor-tier recovery is deliberately out-of-band and ceremonial; cross-signing a compromised anchor against itself does NOT work and is explicitly forbidden as a recovery path. Full per-tier mechanics in §"Lost or compromised key handling."
- **Trusted-anchor-set must live in mutable storage with auth-controlled write access.** Compiled-in roots are a recoverability anti-pattern and forbidden. Every verifier deployment (node, `@aster` service, embedded client) must support updating its trust set via a documented mechanism (config file + reload, database row + authenticated API, etc.).
- **Architectural rule for wire formats:**
  - *Inside Aster* (attestations, chains, future revocation entries): Fory + opaque-body + `aster.<artifact>.v1\0` domain separator + Ed25519.
  - *At the Aster boundary* (trust publications, anything operators publish on the open web): JWS Compact + JSON payload + `typ` header + Ed25519.
  - JWS appears in exactly one place in the system; JOSE is not in the verification hot path.
- **Trust publication (Day-1, sketched):** operators publish a JWS-signed JSON document at an **HTTPS URL** they control (typically `https://<domain>/.well-known/aster` per RFC 8615; any HTTPS URL works), carrying the current trusted-anchor-set + optional rotation successor. Verifiers fetch + validate signature + cache until `not_after`. **HTTPS-only** — no DNS, no other URL schemes. Default URL is `@aster`'s hosted publication for the operator's handle; operators redirect via `ASTER_TRUST_ANCHOR=` config to take `@aster` out of their trust loop entirely. Full design in §"Day-1: Trust publication." **No half-built hooks at MVP.**
- **Default signature scheme:** Ed25519 (RFC 8032). Same key material as Aster transport keys.
- **Encoding for statements:** two u16 codes per statement (`code` from registry + `ext` for deployment-local nuance, both widened to int32 for Fory friendliness on the wire). Strings are out.
- **Freshness:** `epoch` field per edge, included in signed bytes. Mandatory.
- **Schema evolution rule:** never add a defaulted field to an existing wire version. Bump version, define sibling type, add new domain separator.
- **Trust Mark slot:** stays optional, capped at 1 in v1 (third position in `statements`/`signatures` per edge). Concrete role registry filled in as use cases appear.
- **Trust anchors:** verifiers hold a *set* of trusted anchor public keys, not a single root. Chain valid if its top edge's parent is in the set.
- **No `authority_hints` field:** removed. Bundling makes parent-pointer walking unnecessary; the chain itself names the next-up parent via the next edge's child statement.
- **Max chain depth:** verifier-side configurable cap (default 8); rejects deeper chains as malformed.
- **External identifier alias:** every Aster identity gets a `did:key` form for ecosystem interop.
- **Freshness model:** `epoch` is monotonic-replay-protection only (verifier rejects any chain whose `epoch` is < the cached one for that leaf). `not_before` and `not_after` are optional time bounds (`0` = unbounded). **No fixed protocol expiry**; deployments choose policy. `not_after = 0` is the right setting for long-lived stable systems (months/years uptime); finite `not_after` is for CI-driven re-attestation cadences.
- **Single chain per node, single parent per identity.** No chain-level cross-signing, no alternate chains. Multi-anchor / federation / migration happen at the trust-set layer (multi-anchor resolution) and at the Day-1 publication layer (`successor` rotation), not via chain alternates.
- **Statement code registry is compiled-in** to the core crate and shipped with releases. Verifiers ship with the registry baked in. New codes are additive (new releases only).
- **Text encoding for human-encountered blobs:** `<typetag>:<base64url>` form. `aster.attestation.chain.v1:eyJ...` for chains, `aster.revocation.v1:...` for revocations (Day-2). Trust publications are JWS Compact (already self-described). Identities use the `aster0` bech32 form (separate convention).
- **Public chain-fetch endpoint:** every Aster service exposes `aster_node_chain()` returning its current `AttestationChain`. Read-only, unauthenticated; security comes from chain signatures, not access control.
- **Verification points:** `verify_chain()` is called at connection establishment (primary), mesh admission, discovery registration, handle resolution, on trust-set change (cache invalidation), and audit. Never skipped on the trust path.

---

## Open questions

1. **Distribution beyond node config.** MVP serves chains from node config and `@aster` handle service via the public chain-fetch endpoint. Directory cache and broader distribution is post-MVP.
2. **Revocation activation trigger.** Concrete trigger conditions for activating the deferred gossip-topic revocation design: sub-hour response requirement? Multi-tenant operational need? Compliance mandate? More urgent for `not_after = 0` deployments (no natural expiry) than for those with finite `not_after`.
3. **Sub-key vs key migration for BLS.** If/when v2 lands, is BLS a separate sub-key alongside Ed25519, or do we expect deployments to migrate? Lean: separate sub-key.
4. **Registry governance** for `code` allocations — who allocates new codes, on what cadence.
5. **First Trust Mark role** to wire up — likely `MARK_BUILD_PROVENANCE` once CI signing exists.
6. **Cross-deployment replay binding.** Should the signed body include a deployment-id field to scope an edge to a specific deployment? Not blocking MVP.
7. **Upper-edge re-distribution after rotation.** When an upper edge changes (anchor rotation, intermediate replacement), the operator must push updated chains to all leaves below. Mechanism is operator tooling, not protocol — but worth specifying the expected flow.
8. **BLS introduction trigger.** What concrete metric (chain depth, bulk-verify throughput, customer ask) flips the switch on a v2 schema?
9. **Default `not_after` policy in the CLI.** When `aster attest node` is called without explicit time bounds, what does it issue? Lean: configurable per deployment, with a deployment-level default (probably `now + 90 days` for CI-driven, `0` for stable embedded) set in the operator config. CLI exposes both modes via flags.

---

## Next concrete steps

1. **Implement Fory schemas** (`Statement`, `AttestationBody`, `AttestationEdge`, `AttestationChain`) in core. Wire `attest_edge()` and `verify_chain()` functions. Cross-binding matrix runs the round-trip (every binding's signer + every binding's verifier) to confirm Fory determinism for these schemas, same way RPC payload compatibility is gated.
2. **`aster attest node` CLI subcommand.** Inputs: root keypair (file or KMS reference), node identity. Output: `AttestationChain` of length 1 appended to node config under a stable key.
3. **Aster runtime: load chain at node start**, expose through identity surface. No protocol change to admission flow.
4. **`@aster` service: chain parser + verifier + deployment anchor-set config.** Handle resolution for `node.handle.aster0.net` checks chain present, chain verifies via `verify_chain()`, and the top edge's parent identity is in the deployment's anchor set.
5. **`aster attest intermediate` CLI subcommand** (Step 0.5). Produces an upper edge under the parent identity; subsequent `aster attest node` runs assemble multi-edge chains.
6. **`did:key` alias for every Aster identity.** Cheap, useful, ecosystem-friendly. Do alongside.
7. **Don't market Aster as an identity platform.** Identity supports the P2P RPC story; that's where the differentiation and willingness-to-pay live.

---

## Appendix: Mapping to COSE_Sign (informative, not normative)

For the unlikely event that someone wants to consume an Aster attestation edge in COSE-speaking infrastructure (FIDO, C2PA, etc.). This is reference material — Aster does not produce or consume COSE on the wire.

The mapping is per-edge; an `AttestationChain` would map to a sequence of COSE_Sign objects with no native COSE chain wrapper.

| `AttestationEdge` field | COSE_Sign equivalent |
|--------------------------|----------------------|
| `body` (opaque Fory bytes) | `payload` (the signed bytes) |
| `signatures[i]` | `signatures[i].signature` with `sign_protected = {alg: -8 EdDSA}` and `sign_unprotected = {kid: body.statements[i].id}` |
| Domain separator `b"aster.attestation.v1\0"` | `body_protected = {content type: "application/aster-attestation+fory.v1"}` plus the COSE built-in `Sig_structure` context string `"Signature"` |
| `body.epoch`, `body.statements` | Inside `payload`; opaque to COSE |
| Chain ordering (leaf-first) | No native equivalent; would live as an array of COSE_Sign objects in an outer wrapper |

Future Trust Marks attached to an edge would map to COSE counter-signatures in `body_unprotected` — but again, this mapping is reference material; Aster's native slot 3 inside the edge's `AttestationBody` is sufficient.
