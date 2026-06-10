# Trust + Identity + Discoverability — Thinking Session

> **RETIRED 2026-06-10.** Superseded by
> [../trust-directory.md](../trust-directory.md) — which is, in retrospect,
> this session's core instinct ("signed facts on a gossip substrate, not
> authority chains with revocation lists") carried to its conclusion via the
> portal-sync control plane. The "@aster is never in the trust chain"
> principle and the handle-service framing survive in
> [../aster-trust-architecture.md](../aster-trust-architecture.md); the
> P2–P5 forward/reverse lookup problems are answered by directory records
> plus the boundary attestation. Kept for genealogy; do not design from it.

**Status:** Retired working document (was: ideas in flight, not normative)
**Date:** 2026-04-15 (retired 2026-06-10)
**Participants:** Emrul + Claude
**Companion docs (existing):**
- [aster-trust-architecture.md](../aster-trust-architecture.md) — earlier aspirational design (FROST, transparency log, epoch keys)
- [../../../ffi_spec/Aster-trust-spec.md](../../../ffi_spec/Aster-trust-spec.md) — what is actually implemented today (v0.7.2)
- [aster-site-marketplace.md](../aster-site-marketplace.md) — @aster platform concept

---

## Purpose of this session

Step back from implementation details and re-examine the trust model, *starting from the primitives we have rather than from a pre-existing design*. Goal: avoid building a frankenstein that bolts FROST, CT logs, and epoch keys onto the current root-key model just because they sound good.

Explicitly out of scope (separate conversation):
- Gate 3 / capability layer (what a caller can do once connected). Rcans may appear there too but with different issuer/audience/lifecycle.
- Root key lifecycle across dev→QA→prod environments (parked as P1).

In scope:
- Trust + identity + discoverability, treated as one coherent problem. You can't have discovery without trust (a found service is useless if unverifiable) and you can't have trust without discovery (you can't verify what you can't find).

---

## Primitives we have (and why they lean in a direction)

These aren't just good building blocks — they lean toward a particular architecture. Fighting that lean produces complexity.

- **Cryptographic identity = network identity.** Iroh endpoints are ed25519 keypairs. On connect, you already know *which key* you're talking to. No CA chain, no DNS indirection. Strictly stronger than TLS, not just equivalent.
- **Content-addressed state.** Blobs are BLAKE3 hashes. The hash IS the name. Anything we put in a blob is self-verifying.
- **Free gossip.** Multicast pub/sub without a broker. Any fact you want a mesh to agree on, you publish.
- **Pkarr discovery.** Public-key-addressable DNS records via Mainline DHT. A node (or a root key) can publish signed records under its own key, discoverable by anyone with the key.
- **Root key as offline anchor.** Cold key signs enrollments, never touches running mesh.

These primitives invite: **"signed facts on a gossip substrate"** as the base construct, not "authority chains with revocation lists."

---

## Design principle (non-negotiable)

> **@aster is never in the trust chain.**
>
> It is a directory, a CDN, a convenience. If it disappeared tomorrow, every Aster deployment would keep working.

Rules out: anything where @aster signs on behalf of operators, or where @aster's outage breaks verification, or where self-hosting changes the verification algorithm.

Rationale: the platform's role is to make things *easier to find*, not to be *the source of truth*. Trust always terminates at operator root keys, never at @aster's keys.

---

## Problem enumeration

### P1 — Root key lifecycle (parked)

Solo devs / small teams: root key as a secret in CI is fine. As the operation grows (QA/staging/prod, larger orgs), the root key needs a stronger custody story — HSM, split custody, quorum signing, etc. The *model* doesn't necessarily change; the *key storage* does. Parked for a separate session.

### P2 — Node ID → human operator

An endpoint key is 32 bytes of randomness. Nothing intrinsic says "this belongs to Maersk." Today the binding only surfaces via an enrollment credential on an admission stream — invisible to third parties browsing discovery metadata.

**Answer:** `@aster handle service`. Operator proves possession of root key, claims a handle (`maersk`). Handle resolves to root key. `@aster` is structurally a **name service**, not a CA — asserts name↔key bindings, never issues service certs.

**Open thread (parked, not solved):** whatever trust we put in @aster for the handle↔key binding is load-bearing for everything downstream. Eventually the handle registry itself needs to be verifiable without "just trust @aster's word" — this is where the arch doc's transparency-log idea arrives naturally from below. Not solving now.

### P3 — Contract ID → operator

Contract ID is the interface shape (BLAKE3 of the manifest). Good — it's operator-agnostic so anyone can reimplement `ShippingAPI`. But a consumer searching for "ShippingAPI" gets many candidates with no built-in signal for which one is Maersk vs a random dev who forked the contract.

**Answer:** a signed statement from the operator — "`maersk` says endpoint E serves contract C on my behalf, expiring T." The consumer resolves handle → root key, verifies the statement.

**Falls out of the framing:** the distinction between "operator runs this endpoint directly" and "operator authorizes someone else (partner, region, CDN) to run it" **dissolves**. Both are the same shape: a signed statement from the operator's key pointing at an endpoint + contract. The verifier doesn't care whether the operator also holds the endpoint's private key.

### P4 / P5 — Forward and reverse lookup

- **Forward (node → operator):** given an endpoint ID from discovery, who operates it? Today requires a handshake to surface the enrollment credential.
- **Reverse (operator → nodes):** given a root key or handle, enumerate what they run. Today doesn't exist at all.

**Answer:** both are forward and reverse queries on the same graph edge:

```
(operator) ──authorizes──▶ (endpoint, contract)
```

Same signed statement, indexed two ways. Pkarr, iroh-gossip, and @aster are three distribution channels for the same facts; they're not mutually exclusive.

### Collapsed framing

After enumeration, P2–P5 collapse to two queries on one primitive:

- **Discovery**: given an operator (root key or handle), what services do they run or authorize?
- **Attestation**: given a producer, is it actually operated/authorized by who it claims?

Both answered by the same published signed statement, indexed differently.

---

## The primitive that unifies trust + identity + discoverability

**A signed statement from an operator key, saying "endpoint E serves contract C on my behalf until time T," published to a discoverable substrate.**

This is already *almost* what today's `EnrollmentCredential` is, just missing the contract binding and the "publishable" property. The existing primitive can be promoted from "local admission artifact" to "publishable attestation" with additive changes.

---

## Chains: structural vs artifact

Critical distinction surfaced during the session:

- **Delegation chain (structural).** Real org hierarchy: Maersk → AWS → EU-West → producer. Can't be dissolved — that's just how Maersk operates.
- **Credential chain (artifact).** x509's bad habit: producer carries N signed certs, verifier walks them at connect time. Bloat, complexity, fragility.

**You can have (1) without (2).** This is where the elegance lives.

### Two candidate models for how to do (1) without (2)

Both were considered. Key realization: **they converge.**

**Model A — Graph-walk (decentralized publishing).**
Each key publishes its own delegation facts under its own Pkarr zone / gossip / index. Verifier walks the graph on first contact; caches the walk. Fact chain exists as public infrastructure.

**Model B — Registry (centralized collapse).**
Operator (or @aster) collects the internal chain at producer registration time, collapses it into one signed endorsement. Verifier checks a single signature.

**These are the same shape viewed from different ends.** The client-side verification is one signature in both cases (cached in A, pre-collapsed in B). The difference is *when* the collapse happens: at first contact, or at registration.

**Practical decision:** lean toward **Model B** as the default hot path (fast, simple). Keep **Model A's raw facts** published underneath so that:
- Monitors can audit ("did @aster's registry ever publish an endorsement not authorized by the root?")
- @aster can go offline without breaking verification
- Self-hosters can skip @aster entirely

### Who signs the endorsement token?

Three options with very different security properties:

- **(a) Operator root signs the endorsement directly.** @aster is pure directory. Clients trust the operator, never @aster. **Chosen.**
- **(b) @aster signs on the operator's behalf.** @aster becomes a CA with operators as tenants. Rebuilt x509 with extra steps. **Rejected — violates the non-negotiable principle.**
- **(c) @aster holds an operator-delegated key and signs.** Same as (a) in practice but @aster operates hot keys per customer. Complicated. Not needed.

(a) is chosen because it **makes @aster self-hostable without changing the trust model.** Any operator who wants to skip @aster and run their own directory just publishes their endorsements somewhere else (Pkarr, git, static HTTP, whatever). Clients verify the same way.

### Depth-agnostic chains

The protocol does not mandate chain depth. The operator picks their internal structure:

- **Depth 1 (solo dev):** root key IS the endorsement key. Zero ceremony.
- **Depth 2 (small team):** root → hot endorsement key. Rotate hot key quarterly.
- **Depth N (enterprise):** root → corporate → regional → per-AZ.

Client verifier loop is the same whatever depth:

```
while signer != trust_anchor:
    verify_signature(fact, signer)
    signer = lookup_who_authorized(signer)
verify trust_anchor matches the handle we expected
```

In Model B (registry), this loop terminates in one hop most of the time because the registry publishes a collapsed endorsement. In Model A (graph-walk), depth matters on first contact but not after caching. Same ceiling of complexity.

### Onboarding ceremony (sketch, ACME-like)

1. Producer boots with whatever internal delegation chain operator's infra provided.
2. Producer presents chain to the operator's endorsement service (hosted at @aster for convenience, or self-hosted).
3. Endorsement service validates chain, issues a root-signed (or root-delegated-key-signed) fact: "endpoint E serves contract C until T."
4. Fact is published to the registry (@aster, Pkarr under root, or both).
5. Producer goes live. Consumers verify by querying the registry or receiving the fact at connection time.

---

## Distribution layers — DNS and Pkarr

Two complementary channels for publishing the same signed facts, with different trust/latency/availability profiles.

### DNS at aster.site (default, fast, centralized distribution)

Proposed name layout:

```
<handle>.id.aster0.net             TXT  "v=aster1; k=ed25519; p=<base64 root pubkey>; ..."
<node_id>.<handle>.id.aster0.net   TXT  "v=aster1; c=<contract_id>; e=<expiry>; s=<base64 sig>; ..."
_nodes.<handle>.id.aster0.net      TXT  aggregation record (list of node IDs, or pointer to blob)
```

Records are **signed by the operator's root (or endorsement) key**. aster.site's DNS zone is a *distribution layer*, not a trust root. A compromised zone could serve wrong records, but clients verify the signature, so forgery fails.

DKIM / DANE / SSHFP / OPENPGPKEY are the precedents for this pattern.

Known gaps:
- DNS doesn't enumerate children natively. Need an explicit aggregation record, or an aster.site HTTP API for listings (with per-node signed records still as the source of truth).
- DNS TTLs bound revocation latency. Shorter TTL = more queries, faster revocation.

### Pkarr (decentralized, self-hostable, cryptographically rooted)

**What Pkarr is:** a protocol for publishing signed DNS-format records to BitTorrent's Mainline DHT, keyed by ed25519 public keys. The global DHT (~10M+ nodes, ~20 years running) is the substrate. Not a service anyone owns.

**Core property:** the DHT entry key IS the public key. Records must be signed by the corresponding private key; DHT nodes verify before storing. This means **nobody can publish records under someone else's key**.

**Spam / garbage resistance** (falls out of the design):

| Attack | Why it fails |
|---|---|
| Forge Maersk records | No private key → signature fails → DHT drops it |
| Flood DHT with junk under random keys | Junk goes under attacker's own keys; nobody queries them |
| Overwrite with older records | Records carry sequence numbers; peers prefer newest |
| Replay old records | Sequence number check |
| Steal a storage slot | Slot is cryptographically bound to the key |

Worst case: you publish infinite junk under your own keys, and nobody reads it. Same "attack" as running a million web servers with random content.

**Practical limitations (not spam-related):**

- **1000-byte limit per record** (BEP-44). A single endorsement fits easily. Long aggregation lists need an iroh-blob pointer or multiple salted records.
- **Best-effort DHT storage.** Publisher must republish every ~hour to keep records alive.
- **Cold lookup latency ~hundreds of ms to seconds.** Cache aggressively on the client side. Fine for initial resolution, not for per-call.
- **No wildcards or zone transfers.** Enumeration needs an explicit index record.

### Two ways to interact with Pkarr

**1. Direct DHT (UDP).** Your process uses the `pkarr` Rust crate (or JS equivalent), speaks UDP to the DHT. Works for servers, CLIs, desktop apps. iroh already uses this internally for endpoint address discovery.

**2. Pkarr relay (HTTPS).** A small HTTP server that bridges HTTP ↔ DHT for clients that can't speak UDP (browsers, restricted networks):

```
PUT  https://relay.example/<pubkey>   body: <signed record>   → relay pushes to DHT
GET  https://relay.example/<pubkey>                          → relay queries DHT
```

The relay **cannot forge records** — signatures are end-to-end. It's a dumb proxy. Public relays exist (Pubky project); you can run your own with ~200 lines of Rust using the reference `pkarr-relay` crate.

### Moving parts for Aster

| Component | Who runs it | Purpose |
|---|---|---|
| Operator publisher | Operator infra (cron, daemon, CLI) | Signs and republishes records hourly |
| Client resolver | Embedded in aster-cli / consumer libraries | Looks up records, verifies signatures |
| Pkarr relay (optional) | aster.site (or any host) | HTTPS bridge for browser / UDP-blocked clients |
| Iroh's built-in Pkarr | Already in iroh | Endpoint address discovery (extend to trust records) |

### Where @aster actually lives in this picture

Crucially, @aster is **not** the Pkarr authority. Its roles reduce to:

1. **Handle registry** (its core job) — the `<handle>.id.aster0.net` DNS zone that binds handles to root keys.
2. **Convenience directory / index** — HTTP APIs that aggregate and serve operator records for quick discovery.
3. **Optional Pkarr relay at `pkarr.aster0.net`** — for browser clients; tiny service; could also defer to public Pubky relays day one.
4. **Its own published records** — @aster is just another operator; can publish its own state (e.g., current registry root hash) via Pkarr. Eats own dogfood.

### DNS + Pkarr side-by-side

| Property | DNS at aster.site | Pkarr |
|---|---|---|
| Latency (cold) | 10–50 ms | 100 ms – few seconds |
| Latency (cached) | <1 ms | <1 ms |
| Trust root | aster.site zone + operator signatures | Operator signatures only |
| If aster.site disappears | Records vanish (for that operator) | Unaffected |
| Works in browsers | Yes (DoH) | Via HTTPS relay |
| Enumeration | Via aggregation record / API | Via aggregation record |
| Familiar to ops | Extremely | Barely |
| Republish cost | Once per record TTL | Every ~hour |

**Default client strategy:** try DNS first (fast, familiar). Fall back to Pkarr (decentralized, self-hosted-friendly). Same signed records, same verification.

---

## @aster's own bootstrap (the "who watches the watcher" problem)

@aster provides handle resolution for others but cannot use it for itself — circular. How does a client know a given iroh node is a legitimate @aster node?

### Framing

The non-negotiable principle (@aster not in the trust chain) applies to *other operators*. @aster's own identity is a separate problem — @aster is the entity that ships code, owns a domain, and operates binaries, so it can legitimately use vendor-trust mechanisms the way Sigstore, Signal, Docker Hub, and every OS vendor do.

### The shape

1. **Embed @aster's root public key in every distribution.** aster-cli source, every SDK, every binary release. Documented in README for out-of-band verification. Rotated via binary updates. This is the anchor — same tradeoff every software distribution makes.

2. **@aster publishes its own node list, signed by that root.** Multiple channels, same signed blob:
   - DNS: `_aster-nodes.id.aster0.net TXT "v=aster1; n=<list>; s=<sig>"`
   - HTTPS well-known: `https://aster0.net/.well-known/aster-nodes.json`
   - Pkarr (v2): under @aster's own root key in the DHT

3. **Per-node endorsements — eat your own dogfood.** Each @aster node has its own iroh endpoint keypair. @aster's root signs "endpoint X is an @aster node, serving contracts [handle_registry, directory_api, producer_registration], expiring T." Same primitive every other operator uses.

4. **Bootstrap flow from the client's view:**
   1. Embedded @aster root key present at startup
   2. Fetch current node list (DNS → HTTPS → Pkarr fallback)
   3. Verify signature against embedded root key
   4. Pick a node, connect via iroh (transport verifies endpoint key)
   5. Cross-check: endpoint key is on the signed list
   6. Talking to a verified @aster node

### Rotation

- **Binary updates.** New clients get new root. Old clients stuck. Standard.
- **Transitional signing.** New root signed by both old and new during transition window.
- **Transparency log.** Every root-change logged publicly. Sigstore's approach. v2+.

### Self-hosted @aster variants

Clients configured with a hostname→trust-root mapping:
- Default: `aster.site` → `<public @aster root key>`
- Config: `aster.mycompany.internal` → `<company's @aster root key>`

Same protocol, different trust roots. Self-hosters don't compromise public trust; public users don't see internal instances.

---

## Pointer records for large content

Pkarr records are capped at 1000 bytes (BEP-44). Aggregation records and large registries need a **signed pointer to off-record content**.

### The pattern

```
v=aster1
u=https://aster0.net/.well-known/aster-nodes.json
h=<BLAKE3 content hash>
ts=...
sig=<signature over url + hash + ts>
```

- Client fetches URL
- Verifies `BLAKE3(fetched) == h`
- Verifies Pkarr record signature against expected root key
- Content is cryptographically trustworthy regardless of delivery channel

### HTTPS URL vs iroh blob hash

**HTTPS URL** is the recommended pointer target for publicly-discoverable content:
- Reachable from browsers, curl, CI, any client
- No peer discovery needed (cold-start works)
- Operator-controlled hosting (they pick the URL)

**Iroh blob hash** was considered but has a cold-start bootstrap problem: content-addressed retrieval doesn't help you find peers if you don't know any. iroh blobs remain useful for *in-mesh* content sharing (producers distributing manifests among themselves) but are **not suitable** for first-contact discovery.

### When pointers are actually needed

| Case | Direct Pkarr or pointer? |
|---|---|
| @aster's own node list (5–20 nodes) | Direct — ~640 bytes, fits |
| Operator root key + metadata | Direct |
| Operator with ~10 producers | Direct, maybe tight |
| @aster's full handle registry | Pointer to URL+hash |
| Operator with hundreds of producers | Pointer to URL+hash |
| Per-node endorsement (single record) | Direct — ~200 bytes |

Pointers are mostly for aggregation / index records. Per-item records typically fit directly.

### Nice property

Because the Pkarr record is signed *and* commits to the content hash, the content inherits the signature indirectly:

```
Operator key ─signs→ Pkarr record ─commits to→ content hash ─identifies→ content
```

No separate signature on the content itself needed. Blob stays pure data; all crypto lives in the signed pointer.

---

## Proposed v1 registration flow

This is the implementation cut that emerged from the session. It reuses what's already in the spec, adds minimal surface area, and preserves all the invariants.

### Core idea

Extend the existing `EnrollmentCredential` (producer admission token, already root-signed) to serve as **the registration evidence producers present to @aster**. @aster verifies, resolves handle, and publishes to DNS. @aster is a publisher and verifier, never a signer of operator-scoped records.

### Critical nuance: who publishes where

| Layer | @aster publishes? | Operator publishes? |
|---|---|---|
| DNS under aster0.net zone (`<handle>.id.aster0.net`, `<node_id>.<handle>.id.aster0.net`) | Yes — it's @aster's own zone | No — can't write to aster0.net |
| Pkarr under operator's root key | No — would require operator's private key | Yes — only the operator can sign records under their own key |
| Pkarr under @aster's own key (for @aster's own identity) | Yes | No |

So the registration server writes to DNS. Pkarr-under-operator-key is the operator's own job if they want the self-sovereign path. @aster MAY optionally host a Pkarr *relay* (HTTPS → DHT bridge) as a convenience — @aster just forwards UDP on the operator's behalf, no signing involved.

### Registration server responsibilities

Inputs from the producer:
- `endpoint_id`
- `admission_token` (operator-root-signed)
- Declared `contract_ids` (requires P3 token extension for signed contract binding)
- Metadata (region, version hints)

Core logic:
1. Verify admission token signature → extract `root_pubkey`, expiry, attributes
2. Look up `root_pubkey` in handle registry
   - Handle found → register under that handle
   - No handle → either reject, or publish under raw pubkey (policy call)
3. Construct DNS record embedding the raw admission token + metadata
4. Publish via DNS backend (Route53 / Cloudflare / CoreDNS / etc.)
5. Update aggregation record (`_nodes.<handle>.id.aster0.net`) and HTTPS listing
6. Persist registration in local DB (audit, dedup, update handling)
7. Return re-registration deadline to the producer

Lifecycle:
- Re-registration as token nears expiry or contracts change
- Deregistration via signed shutdown request (never anonymous)
- Automatic cleanup of expired records
- Per-operator abuse limits (quotas, dedup)

### Shape as an Aster service

```
service aster.ProducerRegistration {
  register(RegisterRequest) -> RegisterResponse
  deregister(DeregisterRequest) -> DeregisterResponse
  heartbeat(HeartbeatRequest) -> HeartbeatResponse  // optional liveness
}
```

Dogfooded: called via Aster RPC using an `aster-auth` producer credential. @aster's own trust model gates admission to its own registration service.

### Optional v2 add-on: Pkarr relay service

```
service aster.PkarrRelay {
  publish(SignedRecord) -> PublishResponse   // operator-signed payload, relayed to DHT
  resolve(PublicKey) -> ResolveResponse      // DHT query, returns signed record
}
```

@aster has no signing authority — pure UDP broker. Lets operators publish Pkarr records without running their own daemon.

### Minimum v1 build scope

1. `ProducerRegistration` Aster service (as above)
2. DNS publishing backend — pick one (Route53 easiest for managed; CoreDNS if self-hostable matters at v1)
3. Handle registry lookup (already exists at @aster — just wire it up)
4. Aggregation records / HTTPS listing endpoint
5. Admission token verification (reuse `cli/aster_cli/enroll.py` and `bindings/python/aster/trust/`)
6. **(Defer to v2)** Pkarr relay at `pkarr.aster0.net`
7. **(Defer to v2)** Operator-run Pkarr publisher tooling
8. **(Defer to v2)** @aster publishing its own records to Pkarr

### Self-hosting parity

The same `ProducerRegistration` binary runs for anyone who wants a private handle registry — just point it at a different DNS zone and use a different trust root. Operators who skip registration entirely get no public directory but the mesh still works.

### Invariants preserved

- @aster verifies and publishes; never signs operator-scoped records
- Clients verify against the operator's root key, not aster.site's zone
- Self-hosting is the same binary with different config, not a parallel codepath

---

## Freshness and revocation — the lifecycle separation

Pragmatic reality check: in 2026, a producer node typically starts up and runs for days or weeks (until crash, update, or operational reason forces restart). Short-TTL admission tokens that require continuous re-issuance are operationally impractical — they'd need the root key online, or complex delegation with high-cadence re-signing.

This earlier framing ("short TTL solves revocation") conflated two things that actually have different lifecycles.

### Two artifacts, two lifecycles

**Admission token** (long-lived, signed once)
- Operator issues when authorizing a node
- Signed by root (offline, rare ceremony)
- Validity: **days to weeks** — matches actual deployment reality
- Used only at initial mesh admission
- Operator never re-signs during normal operation

**Discovery record** (short-lived, continuously refreshed)
- Published at `<node_id>.<handle>.id.aster0.net` (and/or Pkarr)
- Signed by @aster's zone or the operator's *delegated publishing key*
- Validity: **5–60 minutes** — matches DNS TTL / Pkarr republish cadence
- Kept fresh by a live daemon (registration server heartbeat, or operator's own publisher)
- Disappears within minutes of the live process stopping

These answer different questions:

| Artifact | Question it answers | Needs live refresh? |
|---|---|---|
| Admission token | "Did the root key *ever* authorize this node?" | No — static fact |
| Discovery record | "Is the operator *currently* vouching for this node?" | Yes — live fact |

### Why this matches operational reality

Operator never re-signs admission tokens. Root stays offline. Week-long tokens match week-long deployments.

What stays online is cheap: a single daemon with a **delegated publishing key** (not root) that refreshes discovery records every 5–60 minutes. That daemon just needs one root-signed delegation cert saying "this publishing key speaks for me." Root signs it once, maybe per quarter; the daemon runs indefinitely.

### Revocation in three tiers

1. **Fastest — discovery record pulled.** Operator sends signed deregistration to @aster. @aster drops the DNS record. Within DNS TTL (5–15 min), consumers can't find the peer. Mesh re-verification detects absence, evicts. Common case: planned decommission.

2. **Medium — discovery record decays naturally.** Publisher daemon stops (crash, pulled plug, operator walks away). Within republish cadence (~1 hr), all that operator's discovery records fade. Useful for "operator went offline entirely."

3. **Hard — salt rotation** (existing spec §2.8). Catastrophic compromise; rotate the mesh salt. Compromised peers lose their participation route. Stays as is.

### The small new behavior the mesh needs

Mesh members must **periodically re-verify discovery records for connected peers**:

- Every ~hour, re-fetch `<peer_node_id>.<peer_handle>.id.aster0.net`
- Record exists + signature still valid → peer is fine
- Record missing → operator has effectively deauthorized → evict

Not on the hot path — background task with cached results. This is what makes revocation real even with long-lived admission tokens.

### Heartbeat cadence (tunable)

Example numbers:
- Producer heartbeats to @aster every **5 min**
- @aster publishes DNS record with **10 min TTL**
- Mesh re-verifies peers every **5–10 min**
- Discovery record disappears within **~15 min** of producer/operator stopping

All policy — operators can tighten for stricter postures or loosen for relaxed ones.

### The mental model

- **"Authorization" is a long-lived static fact** bound to root. Changes rarely. Signed once, cached forever until expiry.
- **"Presence" is a short-lived live fact** bound to a publisher daemon. Changes constantly. Republished every few minutes while the producer is healthy.
- **Revocation works by stopping the live fact**, not by expiring the static one.

This is how the design actually holds up against real-world deployment lifecycles. Short TTLs still matter — just not on the admission token.

### What this changes about the registration server design

Minor refinement, not overhaul. The `heartbeat` method in `ProducerRegistration` now has a specific semantic role:

- Producer heartbeats = "I'm still alive" signal
- @aster keeps refreshing the DNS record **only while heartbeats arrive**
- Producer dies / network partitions / operator pulls the plug → heartbeats stop → @aster stops refreshing → DNS record expires → mesh re-verification catches it → peer evicted

The heartbeat TTL, DNS TTL, and mesh re-verify cadence are all policy knobs operators can tune.

---

## Open threads (not yet discussed)

Two remaining before we reach a final design (#3 closed by the freshness/revocation section above):

### 1. Consumer-side trust bootstrap

When a consumer first wants to call Maersk, how do they get Maersk's root key?

- Default: @aster handle lookup (`maersk` → root key).
- Exit ramps needed for: @aster down, @aster distrusted, self-hosted deployments, air-gapped environments.
- Options to consider: TOFU pin on first contact, out-of-band key exchange, Pkarr lookup under the handle itself, signed "operator cards" distributed out-of-band.

### 2. Cross-operator discovery

Maersk's producer wants to call a CMA CGM producer. Totally foreign operator — how do they find and verify each other?

- Probably the same mechanism (handle lookup at @aster or federated directories).
- Worth naming explicitly because it stresses "no shared trust anchor" assumption harder than same-org calls do.

### Resolved this session

- ~~**Freshness / revocation semantics**~~ — closed by the "Freshness and revocation" section above. The lifecycle separation between long-lived admission tokens and short-lived discovery records, plus periodic mesh re-verification, gives revocation that actually works under realistic deployment lifecycles.

### Other parked items

- **P1: Root key lifecycle.** Scaling custody from "solo dev secret" to "enterprise HSM/quorum." Separate session.
- **Gate 3 / capabilities.** What a caller can do once connected. Separate session.
- **Handle registry transparency.** Eventually @aster's handle↔key bindings need to be verifiable by third parties (transparency log-ish). Parked.

---

## Tentative elevator pitch (draft)

Not finalized, but the shape that's emerging:

> **Aster trust is verifiable, not trusted.**
>
> Every authorization is a portable, signed artifact, published to a substrate that anyone can independently query. Trust always terminates at an operator's root key, never at a platform. aster.site is a convenience directory that helps people find each other — when you walk away, your mesh keeps working. The platform is never in the data path, never holds traffic, and never holds a key that isn't already delegatable or rotatable.

Three-audience test:

| Audience | What sells them |
|---|---|
| Security pros | Publicly verifiable facts; no opaque CA; compromise is detectable; tamper-evident by construction |
| Devs | Works like SSH + ACME: handle registration, no cert chains, no IdP config, one CLI command |
| Ops | Self-hostable, no vendor lock-in, exit ramp is "publish your own directory" |

---

## Next steps

1. Finish enumerating the three open threads above (consumer bootstrap, cross-operator, revocation semantics).
2. Decide on the P3 token extension: add `contract_ids` to `EnrollmentCredential` (proper end-to-end signed binding) vs. accept producer self-declaration for v1 (weaker but ships faster).
3. Pick the DNS publishing backend for the registration server (managed vs self-hostable trade-off).
4. Compare the proposed v1 registration flow against today's `EnrollmentCredential` (Aster-trust-spec.md §2.2–2.4) — identify additive vs breaking changes.
5. Revisit `aster-trust-architecture.md` and decide which pieces (FROST, transparency log, epoch keys) are subsumed by the new primitive vs still needed.
6. Produce an action plan: what gets built for v1, what defers to v2, what gets deprecated.
