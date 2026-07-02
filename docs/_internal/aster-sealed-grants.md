# Sealed Grants — Per-Recipient Capability Distribution over Public Docs

> **Status: core mechanism SHIPPED (2026-07-02)** as `aster::grants` — `GrantContext` + AAD discipline, `seal_grant`/`open_grant` (identity-derived + standalone-key variants), typed namespace-capability helpers with role consistency, `PolicyDoc` pinned-author reads, and the `grant_key` convention. See `aster/src/grants.rs`, tests in `aster/tests/grants.rs`, end-user doc [../aster-sealed-grants-getstarted.md](../aster-sealed-grants-getstarted.md). Still open: the reconcile-loop helper + local cap cache (piece 3), portal-sync's migration onto this API, and namespace rotation.

**Companion docs:**
- [working_ideas/aster-network-topology.md](working_ideas/aster-network-topology.md) — first internal consumer: the topology namespace secret distributes via this mechanism
- [aster-baseline-services.md](aster-baseline-services.md) — the trust-side surface (`aster.trust.*`) is the natural home if any of this becomes an RPC
- Origin: portal-sync (`/Users/emrul/dev/emrul/portal-sync`), `crates/portal-cas/src/policy/` — the hand-rolled implementation this generalizes

---

## The pattern

A root node needs to hand a secret (typically an iroh-docs namespace capability) to *individual* nodes, using only replicated storage — no pairwise RPC, no requirement that both ends are online together. portal-sync's answer, proven in production use:

- A **publicly-replicated, root-authored policy doc**. Anyone admitted can sync it; entries are plaintext records *except* the grants.
- Each grant is the capability **HPKE-sealed to one recipient**, written at a deterministic key (`…/grants/<node_id>`). The recipient finds its grant by direct key lookup (its own node id is the path segment) and opens it with a key derived from its identity secret.
- **No shared content key, no envelope-per-N-recipients**: the "content" being protected *is* the capability (e.g. a 32-byte namespace secret), so it's simply sealed once per recipient. O(recipients) records, O(1) reader lookup.

Note what this is **not**: it is not an "encrypted doc" — doc content stays public. It's *capability distribution* riding a public doc. The doc that the granted capability unlocks is a separate, ordinary namespace.

## Layer inventory — what's where today

**Already in Aster (crypto layer, done):**
- `hpke_seal` / `hpke_open` — HPKE Base, DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305 (`core/src/hpke_envelope.rs`, suite + `HPKE_INFO` pinned, alg string `HPKE-Base-X25519-HKDF-SHA256-ChaCha20Poly1305`).
- Fory envelope record `aster.hpke.Envelope` — `{v, alg, encapped_key, ciphertext}`.
- **Identity-derived recipient keys** (`aster/src/crypto.rs`): X25519 keypair derived from the Ed25519 node identity (libsodium-compatible ed25519→curve25519 maps). The root can seal to a node knowing only its `aster1` node id from a ticket; the node opens with its identity secret. Nothing extra to publish or distribute.
- `NamespaceCapability::{Read(id) | Write(secret)}` (`core/src/namespace.rs`) — the usual grant payload.

**Hand-rolled in portal-sync (the layer to generalize):**

| Piece | portal-sync location | Generalization | Status |
|---|---|---|---|
| AAD construction / domain separation | `policy/grant.rs` (`grant_aad`) | `GrantContext` (`aster/src/grants.rs`, AAD domain `aster/sealed-grant/v1`) | ✅ shipped |
| Pinned-author policy reads | `policy/namespace.rs` (`get_exact(root_author, …)`) | `PolicyDoc` (`aster/src/grants.rs`, `docs` feature) | ✅ shipped |
| Grant watch/reconcile loop + local 0600 cap cache | `portal-control/src/grants.rs`, `policy/store.rs` (`CapStore`) | `aster::grants` reconcile helper | ⏳ future — building blocks shipped (`PolicyDoc::subscribe` + `is_policy_write` + `fetch_grant`); the loop + cap cache remain |
| Deterministic key scheme | `policy/paths.rs` (`…/grants/<node_id>`) | `grant_key()` | ✅ shipped |

**Stays in portal-sync (domain payload):** `TreeGrant`/`NodePolicy` record shapes, tree ids, portal-sync's role semantics, the `/portal/v1/…` path root. Same mechanism/policy split as the NodeService → NodeIdentity discussion in [aster-baseline-services.md](aster-baseline-services.md): Aster ships the mechanism; consumers keep their schemas.

## The four generalizable pieces

### 1. AAD discipline → `GrantContext`

HPKE Base gives confidentiality only; the AAD is what stops a sealed grant being replayed at a different node, resource, role, or doc path. portal-sync binds (length-prefixed): app label, root node id, recipient node id, resource id, exact doc path, role. **This is the piece every future consumer would get subtly wrong on their own** — so the Aster API takes a structured context and builds the AAD itself, never a raw `&[u8]`. Shipped as:

```rust
pub struct GrantContext<'a> {
    pub app: &'a str,          // versioned label, e.g. "portal-sync/tree-grant/v1"
    pub granter: &'a NodeId,   // sealing authority (root)
    pub recipient: &'a NodeId,
    pub resource: &'a [u8],    // what the capability unlocks (e.g. tree/namespace id)
    pub path: &'a str,         // exact doc key the grant is written at
    pub role: &'a str,         // "read" | "write" | consumer-defined
}

// root side (raw payload, or seal_namespace_grant for the typed case)
let sealed = grants::seal_grant(&ctx, payload)?;                    // → Fory envelope bytes
// node side (identity-derived key by default)
let cap = grants::open_grant(&identity_secret, &ctx, &sealed)?;     // ctx mismatch ⇒ open fails
```

The AAD is length-prefixed (u32 BE) over a versioned domain label (`aster/sealed-grant/v1`) plus the six context fields. Role/plaintext consistency (a `Write` capability under a `read` role, etc.) is checked at both seal and open in the typed helpers, as portal-sync does today. AAD-binding behavior is pinned by `aster/tests/grants.rs::seal_open_roundtrip_and_aad_binding`.

### 2. Pinned-author policy reads → `PolicyDoc`

HPKE Base has no sender authenticity. portal-sync gets authenticity from *where the record sits*: reads go through `get_exact(root_author, key)` in a namespace where only the root author's records are trusted. That "root-authored policy doc, pinned author, read-only import" wrapper is now wanted by at least three consumers — portal-sync policy, the topology namespace ([working_ideas/aster-network-topology.md](working_ideas/aster-network-topology.md) §Shared topology doc), and admission-record distribution. Shipped:

```rust
let policy = PolicyDoc::import_read_only(&docs, namespace_id, root_author).await?;
policy.get(key).await?;           // get_exact(root_author, key) — other authors invisible
policy.list_prefix(prefix).await?; // pinned-author-filtered listing
policy.subscribe().await?;         // raw events; filter with policy.is_policy_write(&ev)
policy.fetch_grant(prefix, &me).await?; // the convention-key lookup
```

### 3. Grant reconcile loop → `aster::grants`

The node-side lifecycle is generic modulo the record type: watch `…/grants/<my_node_id>` under the pinned author → open with my identity secret → cache the capability in a `0600` local store → **drop the capability and tear down sessions when the grant is revoked, absent, or unopenable**. portal-sync's `reconcile_grants`/`watch_grants` + `CapStore` is the reference implementation; the generalized helper takes the prefix, the `GrantContext` template, and a callback for apply/revoke.

### 4. Key scheme

`<consumer prefix>/grants/<recipient_node_id>` — recipient's O(1) self-lookup, no scanning, and enumeration-by-prefix for the granter's bookkeeping. A convention the API encodes rather than a rule anyone has to remember.

## Recipient-key schemes — the decision the API must make unmixable

Two schemes exist and **must not be silently mixable** (portal-sync currently mixes them — see *Field notes* below):

| Scheme | Pros | Cons |
|---|---|---|
| **Identity-derived** (default) | Zero key distribution — the `aster1` ticket is the address; nothing to publish | Encryption bound to identity: rotating the recipient key = rotating the node identity (`aster/src/crypto.rs` security note) |
| **Published standalone** X25519 key | Recipient-key rotation without identity rotation | Must be published + pinned in policy (a record the granter trusts), one more thing to get wrong |

Default identity-derived; allow standalone as the rotation escape hatch. **Shipped as separate, explicitly-named functions**: `seal_grant`/`open_grant` (identity-derived, the default) vs `seal_grant_to_key`/`open_grant_with_key` (standalone) — the caller cannot mix schemes without noticing. Whatever record eventually *publishes* a standalone key must still tag the scheme explicitly (e.g. `hpke_key_scheme: identity | standalone`); that record shape rides the admission/trust spec (open question 3). The failure mode otherwise is exactly portal-sync's: grants sealed to a key the daemon never uses, failing only at open time.

## Group grants — one seal, many openers

Per-recipient sealing is O(members) per grant. When the same capability goes to a set of nodes, amortize with a **group identity** — and this needs **no new API**, because a "recipient" is just an Ed25519 keypair:

1. Root mints a group keypair (`SecretKey::generate()`); its public key is the group's "node id" — purely an encryption address, no node runs under it.
2. Bootstrap (once per membership change): each member receives the **group secret** via an ordinary per-member sealed grant (`app` label like `…/group-membership/v1`, `resource` = the group id).
3. Steady state: every grant to the group is sealed **once**, `recipient = group_id`; any member opens it with the group secret (it's just a `SecretKey` to `open_grant`).
4. Join = 1 seal (the group secret; the new member immediately opens all existing group grants). Leave = rotate the group key + re-seal — which piggybacks on the honesty limit below: a hostile leave already forces rotation of the underlying namespaces, so group rotation adds little.

| | Per-recipient (default) | Group identity |
|---|---|---|
| New grant to N members | N seals, N records | 1 seal, 1 record |
| Member joins | re-seal each resource for them | 1 seal |
| Member leaves | delete their records | group-key rotation + re-seal |
| AAD binds | the individual | the group (per-member attribution lost) |
| Compromise blast radius | one node's grants | every group grant |

**When to use which:** individual grants where attribution/audit or differing capability sets matter; group grants for uniform fleet-wide capabilities where membership changes are rarer than grants. Don't mix a member's personal grants into the group context — the AAD `recipient` field keeps them distinct by construction.

True broadcast encryption (one ciphertext, N independent identity keys, no shared material) is mKEM/MLS territory — out of scope. A shared-DEK-wrapped-per-recipient record (age/PGP style) only pays off for large payloads; capabilities are 32 bytes, so it never does here.

Proven in `aster/tests/grants.rs::group_identity_grant_one_seal_many_openers`. If the pattern gets real consumers, a thin `GroupIdentity` newtype (mint/id/secret + the membership-grant convention) would make it harder to misuse — candidate for the same increment as the reconcile helper.

## Revocation — honesty limit (inherited by every consumer)

Property of the pattern, not a portal-sync bug: revoking a grant (tombstone record → reconcile loop drops the cached capability) only stops an **honest** node. A malicious node that already learned a namespace secret keeps it; the only true revocation is **rotating the underlying namespace** and re-granting. The generalized API should carry this warning in its docs, and namespace-rotation mechanics are a shared open question with the topology doc (its Q6) — one rotation design should serve both.

## Where it landed

- Mechanism (GrantContext/seal/open, typed helpers, PolicyDoc, grant_key) → `aster/src/grants.rs` (`aster::grants`; PolicyDoc behind the `docs` feature). Pure library, no new service. Tests: `aster/tests/grants.rs`. End-user doc: [../aster-sealed-grants-getstarted.md](../aster-sealed-grants-getstarted.md).
- If a runtime surface is ever wanted ("what grants do I hold", "re-check grants now"), it belongs in the `aster.trust.*` baseline catalog — not needed for v1.
- **portal-sync migration is pending** — its records/paths/roles stay its own; the migration should also fix the enrollment-path bug below.

## Field notes from the portal-sync read (2026-07-02)

- **Likely live bug — divergent enrollment paths:** `portalctl init node` mints a *random* standalone HPKE keypair and publishes its public half in `NodePolicy` (`portalctl/src/main.rs:724, 788`; secret kept in `NodeBundle.hpke_secret`), while `portalctl add node` publishes the *identity-derived* public key (`portal-control/src/root.rs:113`). The daemon opens grants with the **identity-derived secret only** (`portal-syncd/src/main.rs:496,512`) — so a node enrolled via `init node`'s self-publish path receives grants it can never open. `NodeBundle.hpke_secret` appears vestigial. Fix in portal-sync regardless of this generalization.
- Nonces need no app management: each seal runs a fresh HPKE `setup_sender` with a fresh ephemeral encapped key.
- Root-side secrets (`TreeSecretStore`) never touch the doc; node-side opened capabilities cache in `0600` files (`CapStore`).

## Open questions

1. ~~**Module shape.**~~ Resolved: one `aster::grants` module.
2. ~~**Generic payloads.**~~ Resolved: generic bytes (`seal_grant`/`open_grant`) with a typed convenience layer (`seal_namespace_grant`/`open_namespace_grant`).
3. **Standalone-key publication record.** Where a published recipient key lives and who signs it — probably a pinned-author policy record, but the shape should ride the admission/trust spec. (The seal/open functions for standalone keys exist; only the publication record is undesigned.)
4. **Namespace rotation mechanics.** Shared with topology Q6; needs its own session.
5. **Multi-granter.** Everything above assumes one root author. Delegated granters (an anchor grants on the root's behalf) intersect with the attestation-chain work — out of scope until a consumer needs it.
6. **Reconcile-loop helper + cap cache** (piece 3): watch → open → cache (`0600`) → revoke-on-absence, generic over the record type. Do it alongside the portal-sync migration so the helper is shaped by a real consumer.
