# @aster Day 0 — Technical Design

**Status:** Draft — needs agreement before building  
**Date:** 2026-04-08  
**Companion docs:** [aster-identity-and-join.md](aster-identity-and-join.md), [aster-publish-design.md](aster-publish-design.md), [aster-trust-architecture.md](aster-trust-architecture.md)

---

## Scope

This doc covers the *how* — the technical decisions needed before we start building. Three areas:

1. **Security model** — how `@aster` signs enrollment tokens, how services verify them (the big one)
2. **`@aster` backend** — where it lives, how it stores data, how it runs across multiple nodes
3. **Framework changes** — what changes in aster-rpc to support delegated enrollment

---

## 1. Security Model: 2-Layer Signing

### The Problem

When `@aster` issues an enrollment token for @alice to access @emrul/TaskManager, the service needs to verify that token is legitimate. How?

### Why Not Just One Key?

A single `@aster` signing key would work but has problems:
- Compromise = game over. Every token ever issued is suspect.
- Rotation requires updating every published service's delegation config.
- The key is hot (used for every enrollment) — exposure risk is high.

### Why Not the Full FROST/Epoch Architecture?

The trust architecture doc describes FROST threshold signing, epoch-based ephemeral keys, and transparency logs. That's the right end state but wrong for Day 0:
- FROST requires 3-of-5 distributed nodes with HSM-backed key shares
- Epoch rotation requires coordinated key distribution
- Transparency log requires Merkle tree infrastructure

We need something that **achieves the same security properties at small scale** and upgrades to the full architecture without changing the verification protocol.

### The Day 0 Design: Root Key + Signing Keys

Two layers. That's it. No chain beyond two levels.

```
Layer 1: @aster root key (long-lived, offline)
    │
    │ signs attestation: "this signing key is valid from T1 to T2"
    │
    ▼
Layer 2: @aster signing key (short-lived, online)
    │
    │ signs enrollment tokens
    │
    ▼
Enrollment token → consumer presents to service → service verifies
```

**Layer 1: Root key**

| Property | Value |
|----------|-------|
| Type | ed25519 keypair |
| Lifetime | Years. Rotated only if compromised. |
| Storage | Offline. Not on any running node. Brought online only to sign new signing key attestations. |
| What it signs | Signing key attestations, DNS TXT records |
| Hardcoded where | CLI (pubkey only) — used for DNS record verification and as trust anchor |
| Compromise impact | Catastrophic. Requires CLI update to rotate. Same as any root of trust. |
| Scope | Global for one `@aster` deployment. The DNS TXT record and signing key attestations are both anchored to this one key. If we run multiple independent `@aster` deployments (e.g., staging vs production), each has its own root key. |

**Layer 2: Signing key**

| Property | Value |
|----------|-------|
| Type | ed25519 keypair |
| Lifetime | Short. Day 0: rotate weekly. Later: daily or hourly with FROST. |
| Storage | In-memory on running `@aster` nodes. Loaded at startup. |
| What it signs | Enrollment tokens for consumers |
| Attested by | Root key signature over `(signing_pubkey, valid_from, valid_until, key_id)` |
| Compromise impact | Bounded. Attacker can sign tokens only until the key expires (max 1 week Day 0). Detection: unexpected tokens in audit log. Recovery: rotate to new signing key. |

### Why This Isn't the CA Problem

| CA intermediates | @aster signing keys |
|-----------------|---------------------|
| Live for years | Live for days/hours |
| Chains can be N levels deep | Always exactly 2 levels |
| Revocation (CRL/OCSP) is broken | Short TTL — key expires naturally |
| Many CAs, many intermediates | One root, one active signing key |
| Compromise hidden for months | Short validity window bounds exposure |

The key insight: **we don't need revocation because we use short TTLs.** A compromised signing key expires before anyone needs to check a revocation list. This is the same principle behind JWTs with short expiry — you don't revoke them, you let them die.

### Signing Key Attestation

The root key signs an attestation for each signing key:

```python
@wire_type
@dataclass
class SigningKeyAttestation:
    signing_pubkey: str     # hex-encoded ed25519 public key
    key_id: str             # unique identifier (e.g., "sk-2026-04-08-001")
    valid_from: int         # epoch seconds
    valid_until: int        # epoch seconds
    root_signature: str     # root key signature over canonical JSON of above fields
```

Attestations are public — anyone can fetch the current attestation from `@aster` and verify it against the hardcoded root pubkey.

### Enrollment Token Format

```python
@wire_type
@dataclass
class EnrollmentToken:
    consumer_handle: str        # "@alice"
    consumer_pubkey: str        # alice's root pubkey (hex)
    target_handle: str          # "@emrul"
    target_service: str         # "TaskManager"
    target_contract_id: str     # BLAKE3 hash of the service contract
    roles: list[str]            # ["reader"]
    issued_at: int              # epoch seconds
    expires_at: int             # epoch seconds (default: issued_at + service's token_ttl)
    signing_key_id: str         # which signing key issued this
    signature: str              # signing key's signature over canonical JSON of above fields
```

### Verification Chain (Service Side)

When a consumer presents an enrollment token to a published service, the service verifies:

```
1. Is the token signed by a valid signing key?
   → ed25519_verify(token.signature, canonical(token_fields), signing_pubkey)

2. Is the signing key attested by @aster's root key?
   → ed25519_verify(attestation.root_signature, canonical(attestation_fields), aster_root_pubkey)

3. Is the signing key currently valid?
   → attestation.valid_from <= now <= attestation.valid_until

4. Is the token not expired?
   → token.issued_at <= now <= token.expires_at

5. Does the token target this specific service?
   → token.target_contract_id == my_contract_id
   → token.target_handle == my_published_handle
   → token.target_service == my_service_name
   (All three. Contract ID alone isn't enough — two services could share the same
   contract shape. Binding to handle + name + contract prevents cross-service portability.)

6. Does the consumer prove possession of the key in the token?
   → consumer signs a channel-bound challenge: nonce || target_handle || target_service || alpn
   → service verifies against token.consumer_pubkey
   (Channel-binding prevents replaying the proof across services or ALPNs.)

7. What roles does the consumer have?
   → token.roles → applied to CallContext.attributes for Gate 2
```

Checks 1-5 are pure crypto + timestamp checks — no network calls, no `@aster` dependency. Check 6 is a live challenge-response during the `aster.admission` handshake. The service only needs:
- The `@aster` root pubkey (received at publish time, cached forever)
- The current signing key attestation (cached by `key_id`, refreshed when expired or when consumer presents a newer one)

### Attestation Caching Policy

- Services cache attestations by `key_id`
- Accept any attestation whose root signature is valid and whose validity window covers `now`
- Multiple attestations may be valid simultaneously during rotation overlap periods
- Consumer presents the attestation with the token — service can use it directly if valid, whether or not it already has it cached
- This makes admission robust during signing key rotation: the consumer got the new attestation from `@aster`, the service hasn't seen it yet, but it can verify the root signature on the spot

### Day 0 Trust Assumption

**The service trusts `@aster` to honor the delegation constraints.** The service verifies the *token* (cryptographically), but it does not independently verify that `@aster` only issued tokens within the bounds of the DelegationStatement (correct roles, correct mode, correct rate limits). That enforcement lives on `@aster`'s side. The audit log is how the publisher verifies `@aster` did its job.

This is acceptable for Day 0 because:
- The token TTL is short (minutes) — damage from a policy violation is bounded
- The audit log provides after-the-fact verification
- The publisher can revoke delegation (unpublish) at any time

**Later:** services can verify stronger proofs — e.g., inclusion in a transparency log that proves the token was issued according to the stated policy. The verification chain doesn't change; it gains an optional additional check.

### Signing Key Rotation

```
Week 1: signing key SK1 (valid Mon 00:00 → Sun 23:59)
         Root key signs attestation for SK1

Week 2: signing key SK2 (valid Mon 00:00 → Sun 23:59)
         Root key signs attestation for SK2
         SK1 attestation still valid for overlap period (24h grace)
         → services cache both attestations briefly
```

Day 0: manual rotation. Operator generates new signing key, signs attestation with root key offline, deploys to `@aster` nodes.

Later: automated. FROST quorum generates epoch keys. Same verification chain — services don't care whether the signing key was generated by one node or a threshold quorum.

### What the Service Stores (Post-Publish)

```toml
# Written by aster publish to local config
[published_services.TaskManager]
aster_root_pubkey = "abc123..."     # @aster's root pubkey — the trust anchor
delegation_enabled = true
```

The signing key attestation is fetched at runtime and cached. It's not stored permanently — it changes on rotation.

### Upgrade Path to Full Architecture

| Day 0 | Later |
|-------|-------|
| Single signing key, rotated weekly | FROST-generated epoch keys, rotated hourly |
| Manual rotation (offline root key) | Automated FROST ceremony |
| No transparency log | Append-only Merkle log of all issuance |
| Attestation fetched from `@aster` | Attestation in transparency log + gossiped |
| Services trust `@aster` to be honest | Services verify via inclusion proofs |

**The verification chain is identical.** Services verify: token → signing key → root key. Whether the signing key was generated by one node or a 3-of-5 FROST quorum doesn't change the verification protocol. This is the key property — Day 0 is a strict subset of the full architecture.

---

## 2. `@aster` Backend

### Where It Lives

New directory in this repo: `services/aster/`. It's an aster-rpc service — uses `@service`, `@unary`, `@wire_type` decorators. Dogfooding.

```
services/aster/
├── __init__.py
├── service.py          # AsterService class with all @unary methods
├── types.py            # wire types (payloads, results, tokens)
├── storage.py          # storage abstraction over iroh primitives
├── signing.py          # 2-layer signing (attestation verification, token issuance)
├── validation.py       # handle validation, reserved words
├── email.py            # verification code delivery
└── server.py           # entry point: stand up an Iroh node + AsterServer
```

### Running It

```bash
# Start the @aster service
python -m services.aster.server \
  --signing-key ./signing.key \
  --signing-attestation ./attestation.json \
  --data-dir /var/aster/data
```

The server:
1. Creates a persistent `IrohNode` (stable node ID across restarts)
2. Loads the signing key + attestation
3. Registers `AsterService` with `AsterServer`
4. Listens for RPC connections

For multiple nodes across regions: each node runs the same service, shares the same iroh-docs for replicated state, uses the same signing key for the current period.

### Storage: Iroh Primitives, Not SQLite

The `@aster` backend should eat its own dogfood. Iroh gives us:

**iroh-docs (CRDT documents)** for replicated state:

```
Doc: "aster-handles"
  key: b"handle:emrul"           → JSON: {pubkey, email_hash, status, registered_at}
  key: b"handle:alice-dev"       → JSON: {pubkey, email_hash, status, registered_at}
  key: b"email:sha256(emrul@…)" → JSON: {handle: "emrul"}  (reverse lookup, for uniqueness)

Doc: "aster-services"
  key: b"svc:emrul:TaskManager"       → JSON: {version, contract_id, visibility, token_ttl, published_at}
  key: b"svc:emrul:InvoiceService"    → JSON: {…}
  key: b"ep:emrul:TaskManager:<nid>"  → JSON: {relay, ttl, registered_at}

Doc: "aster-access"
  key: b"grant:emrul:TaskManager:alice-dev" → JSON: {role, granted_at, granted_by}
  key: b"grant:emrul:TaskManager:bob"       → JSON: {role, granted_at, granted_by}
```

**iroh-blobs** for contract manifests:

```
When someone publishes TaskManager:
  1. Manifest JSON → blobs.add_bytes(manifest) → hash
  2. Tag: blobs.tag_set("manifest:emrul:TaskManager", hash, "raw")

When someone fetches the manifest:
  1. Look up tag → hash
  2. blobs.read_to_bytes(hash) → manifest JSON
```

Manifests are content-addressed — same contract = same hash. Natural dedup.

**iroh-gossip** for cross-node notifications:

```
Topic: b"aster-events" (32-byte hash of this string)
  → publish events: handle_registered, service_published, access_granted, signing_key_rotated
  → all @aster nodes subscribe and react (e.g., update local search index)
```

### Why Not SQLite?

| SQLite | Iroh primitives |
|--------|----------------|
| Single-node. Replication requires custom solution. | iroh-docs auto-replicate via CRDT sync. |
| Need backup strategy. | Data distributed across nodes by construction. |
| Familiar, fast, well-understood. | Less familiar, but we're dogfooding. |
| No content-addressing. | Manifests are content-addressed blobs. |
| No pub-sub. | Gossip for cross-node events. |

**The trade-off:** iroh-docs are eventually consistent. For handle uniqueness (must be strongly consistent), we need a serialization point.

### Handle Uniqueness: Primary Writer Model

Handle claiming must be serialized — two people can't claim "emrul" simultaneously. 

Approach: **single primary writer per doc**.

- One designated `@aster` node is the "primary" for the handles doc.
- All write requests (join, publish, access grant) route to the primary.
- The primary writes to the iroh-doc. CRDT sync replicates to other nodes.
- Read requests (discover, resolve, get_manifest) can be served by any node.

This is analogous to a leader-based replication model. The primary is a single point of write availability (not a single point of failure — reads still work from any node, and primary can failover).

Day 0: single node = trivially the primary. Multi-node: designate one as primary, others are read replicas that sync via iroh-docs.

### Ephemeral State (Not in Iroh)

Some state is ephemeral and doesn't need replication:

- **Verification codes** — short-lived (15 min), per-node. In-memory dict with TTL eviction. If the node restarts, codes are lost — user just resends.
- **Rate limit counters** — per-node. In-memory. If node restarts, counters reset (acceptable).
- **Nonce dedup** — per-node, 10-minute sliding window. In-memory.

No need to replicate this. Each node manages its own ephemeral state.

### Multi-Node Deployment

```
Node 1 (eu-west, Hetzner)     ─── primary writer
  └── persistent IrohNode
      ├── aster-handles doc (write + read)
      ├── aster-services doc (write + read)
      ├── aster-access doc (write + read)
      ├── blobs (manifests)
      └── AsterService (RPC)

Node 2 (us-east, DigitalOcean) ─── read replica
  └── persistent IrohNode
      ├── aster-handles doc (read, synced from Node 1)
      ├── aster-services doc (read, synced from Node 1)
      ├── aster-access doc (read, synced from Node 1)
      ├── blobs (manifests, synced on demand)
      └── AsterService (RPC — reads local, writes proxy to Node 1)

Node 3 (ap-southeast, DigitalOcean) ─── read replica
  └── (same as Node 2)
```

Sync: each replica calls `doc.start_sync([primary_node_id])` on startup. CRDT reconciliation handles the rest.

Write proxying: when a replica receives a write request (join, publish, grant), it forwards to the primary via an internal RPC call (which is just... another Aster RPC call between the nodes).

### DNS Bootstrap Points to Nearest Node

The signed DNS TXT record can contain multiple nodes:

```
_aster-registry.aster.site TXT "v=aster1 nodes=<nid1>,<nid2>,<nid3> relay=<url> ts=<epoch> sig=<hex>"
```

CLI tries each node, connects to the first that responds. Natural geographic affinity if relays are region-aware.

---

## 3. Framework Changes (aster-rpc)

### 3a. New ALPN: `aster.admission`

Clean separation from the existing self-issued credential paths:

```
aster.producer_admission   → operator's domain. Self-issued producer credentials. Unchanged.
aster.consumer_admission   → operator's domain. Self-issued consumer credentials. Unchanged.
aster.admission            → @aster's domain. Delegated tokens. NEW.
                             Only enabled on published services.
                             Handles both consumer and producer tokens (Day 0: consumer only).
```

The `aster.admission` ALPN:
- **Only exists on published services.** Non-published services don't expose it. Zero new attack surface.
- **Designed from scratch.** No legacy constraints. We can make it elegant.
- **Coexists with existing ALPNs.** Publishing doesn't break self-issued credential workflows. Operators can still issue credentials directly. Both paths work simultaneously.

#### `aster.admission` Protocol

```
Consumer → Service (on aster.admission ALPN):

1. Consumer sends: EnrollmentToken + SigningKeyAttestation
   (both obtained from @aster during enrollment)

2. Service verifies:
   a. Attestation signed by @aster root key? (cached root pubkey from publish)
   b. Signing key in attestation still valid? (timestamp check)
   c. Token signed by attested signing key? (ed25519 verify)
   d. Token targets this service?
      → token.target_contract_id == my contract_id
      → token.target_handle == my published handle
      → token.target_service == my service name
      (all three must match — prevents cross-service token portability)
   e. Token not expired? (timestamp check)
   f. Consumer proves possession of root key in token?
      → service sends challenge: 32-byte nonce + service_identity + alpn
      → consumer signs: nonce || target_handle || target_service || alpn
        (channel-bound — prevents replay across services or ALPNs)
      → service verifies signature against token.consumer_pubkey

3. Service admits with: {handle: token.consumer_handle, roles: token.roles}
   → Gate 2 (capability interceptors) sees these attributes as normal
```

Step (f) is what prevents token theft. The token is bound to @alice's root pubkey. Even if someone intercepts the token, they can't prove possession of @alice's private key.

### 3b. The Delegation Statement

When `aster publish` runs, the publisher signs a delegation:

```python
@wire_type
@dataclass
class DelegationStatement:
    action: str                 # "delegate_enrollment"
    handle: str                 # "@emrul"
    service_name: str           # "TaskManager"
    contract_id: str            # BLAKE3 hash
    aster_root_pubkey: str      # which @aster instance is trusted
    authority: str              # "consumer" (Day 0). Future: "producer" | "both"
    mode: str                   # "open" | "closed"
    token_ttl: int              # seconds (default: 300 = 5 min)
    rate_limit: str | None      # e.g., "1/60m" per consumer, or None (no limit)
    roles: list[str]            # roles @aster can grant. Day 0: all roles from contract.
    timestamp: int
    nonce: str
    # Signed by publisher's root key
```

**What this proves:** @emrul explicitly authorized `@aster` (identified by `aster_root_pubkey`) to issue enrollment tokens for TaskManager, with these constraints. The signature is the consent. `@aster` stores this and won't exceed the stated authority.

**Updating the delegation:**

```bash
# Change from open to closed
aster access close --service TaskManager
# → signs new DelegationStatement with mode="closed", replaces previous

# Change token TTL
aster publish TaskManager --token-ttl 10m
# → signs new DelegationStatement with updated token_ttl
```

Each update is a new signed statement that replaces the previous one on `@aster`.

**Does the service verify the delegation?** No. The service verifies the *token* (which is bounded by the delegation). The delegation is between the publisher and `@aster` — it's `@aster`'s responsibility to honor the constraints. The audit log is how the publisher verifies `@aster` did its job.

**Future-proofing for producer tokens:**

The `authority` field is `"consumer"` on Day 0. When we support `"producer"`, `@aster` can mint producer enrollment credentials too. The delegation statement already has the field. The `aster.admission` ALPN already handles both types — the token carries a `type` field ("consumer" or "producer"). The service routes accordingly.

### 3c. Open vs Closed Services

**Open (default):**
- Any verified `@aster` handle can request an enrollment token
- `@aster` checks: is the handle verified? Is the service open? → issue token
- Rate limiting optional (per-consumer-per-service, set by publisher)
- Token TTL controlled by publisher (default: 5 min)

**Closed:**
- Only handles explicitly granted access can get tokens
- `aster access grant @alice --service TaskManager --role reader`
- `@aster` checks: does @alice have a grant for this service? → issue token with granted role
- Grants are at **handle scope** (Day 0) — any node @alice operates can use the token
- Future: node scope (`--node <nid>`) for tighter control

```bash
# Open service with rate limiting
aster publish TaskManager --open --token-ttl 5m --rate-limit "1/60m"

# Closed service
aster publish TaskManager --closed
aster access grant @alice --service TaskManager --role reader
aster access grant @bob --service TaskManager --role admin

# Switch modes after publish
aster access close --service TaskManager
aster access open --service TaskManager
```

**Rate limiting details:**
- Format: `"<count>/<period>"` — e.g., `"1/60m"`, `"5/1h"`, `"10/24h"`
- Enforced per-consumer-per-service on `@aster`'s side
- Optional. Default: no rate limit (but `@aster` has a global limit to protect itself — e.g., 100 tokens/min per consumer across all services)
- The publisher doesn't have to think about this if they don't want to
- **Multi-node: best-effort.** Rate limiting is in-memory per `@aster` node. In multi-node mode, the configured limit is approximate — a consumer could get tokens from different nodes in parallel. This is acceptable because token TTLs are short (minutes), so the practical blast radius is bounded regardless. Strictly enforced rate limiting would require cross-node coordination and is not worth the complexity for Day 0.

### 3d. Access Grants (Closed Services)

```python
@wire_type
@dataclass
class AccessGrant:
    handle: str                 # service owner
    service_name: str
    consumer_handle: str        # who gets access
    role: str                   # granted role
    scope: str                  # "handle" (Day 0). Future: "node"
    scope_node_id: str | None   # only when scope="node" (Day 2)
    granted_at: int
    granted_by: str             # pubkey of granter (for audit)
```

Stored on `@aster` in the access doc:
```
key: b"grant:emrul:TaskManager:alice-dev" → JSON(AccessGrant)
```

When @alice requests enrollment:
1. `@aster` looks up grant for @alice on @emrul/TaskManager
2. Grant exists → issue token with granted role
3. No grant → reject with "access denied" (not "not found" — no enumeration)

### 3e. Audit Log (Gossip + History)

Every token `@aster` issues is logged two ways:

**1. Real-time: gossip broadcast**

```
Topic: blake3(b"aster-audit:<handle>:<service>")

Events:
  {type: "token_issued", consumer: "@alice", role: "reader", at: <ts>}
  {type: "token_issued", consumer: "@bob", role: "admin", at: <ts>}
  {type: "access_granted", consumer: "@alice", role: "reader", by: "<pubkey>", at: <ts>}
  {type: "access_revoked", consumer: "@alice", at: <ts>}
```

The operator can tail this:
```bash
aster audit tail --service TaskManager
# live stream of all token issuance + access changes
```

**2. Historical: iroh-doc entries**

```
Doc: "aster-audit"
  key: b"audit:emrul:TaskManager:1744123800:token:alice-dev" → JSON(audit event)
  key: b"audit:emrul:TaskManager:1744123900:grant:bob"       → JSON(audit event)
```

The operator can query history:
```bash
aster audit log --service TaskManager --last 24h
```

### 3f. Consumer-Side: Auto-Enrollment

When `AsterClient.connect("@handle/Service")` is called:

```python
async def connect(cls, target: str) -> AsterClient:
    handle, service = parse_handle_target(target)  # "@emrul/TaskManager"

    # 1. Resolve endpoints from @aster
    aster = await get_aster_client()
    resolve_result = await aster.resolve(handle, service)

    # 2. If we have a verified handle, get enrollment token
    token = None
    config = load_local_config()
    if config.handle_status == "verified":
        enroll_result = await aster.enroll(
            consumer_handle=config.handle,
            target_handle=handle,
            target_service=service,
        )
        token = enroll_result.token
        attestation = enroll_result.attestation

    # 3. Connect P2P on aster.admission ALPN, present token
    endpoint = pick_endpoint(resolve_result.endpoints)
    client = await cls._connect_p2p(
        node_id=endpoint.node_id,
        relay=endpoint.relay,
        alpn="aster.admission",         # new ALPN
        credential=token,               # enrollment token
        attestation=attestation,         # signing key attestation (for service to verify)
        root_key_signer=config.signer,  # to prove possession during admission
    )
    return client
```

For **open public services** where the consumer doesn't have a handle: skip step 2, connect on `aster.consumer_admission` with no credential (if the service allows unauthenticated consumers), or fail with a clear message.

### 3g. `aster publish` Auto-Configuration

When `aster publish TaskManager` succeeds:

1. Publisher signs `DelegationStatement` → sent to `@aster`
2. `@aster` returns `PublishResult` containing `aster_root_pubkey`
3. CLI writes to local config:
   ```toml
   [published_services.TaskManager]
   aster_root_pubkey = "abc123..."
   delegation_enabled = true
   contract_id = "def456..."
   ```
4. CLI enables the `aster.admission` ALPN on the service. This can be:
   - A config flag the framework reads at startup
   - Or: `aster publish` restarts/signals the running service to enable the ALPN

Next time the service starts, the framework:
- Reads the published_services config
- Enables `aster.admission` ALPN for published services
- Registers the admission handler that verifies `@aster`-issued tokens

### 3h. What We Don't Change

- **Gate 0** (QUIC endpoint identity) — unchanged
- **Gate 2** (capability/role check via interceptors) — unchanged. Receives roles from the `@aster` token just like it would from a self-issued credential. The interceptor doesn't know or care where the roles came from.
- **`aster.producer_admission`** — unchanged. Self-issued producer credentials work exactly as before.
- **`aster.consumer_admission`** — unchanged. Self-issued consumer credentials work exactly as before.
- **LocalTransport** — unchanged. No Gates 0/1 for in-process calls.
- **Self-issued credentials** — always available. Publishing adds a *new* path, doesn't replace the existing one.

---

## 4. Agreed Decisions

| # | Decision | Choice | Rationale |
|---|----------|--------|-----------|
| D1 | Signing key rotation | Weekly (Day 0) | Manual rotation. Tighten later with FROST. |
| D2 | Token TTL default | 5 min, configurable | Short default. Publisher sets via `--token-ttl`. |
| D3 | Handle uniqueness | Primary writer | Day 0 likely one node. Multi-node: writes proxy to primary. |
| D4 | Storage | Iroh primitives | Dogfooding. Docs for state, blobs for manifests, gossip for events. |
| D5 | Service name | `AsterService` | Matches `@aster`. |
| D6 | Token presentation | New `aster.admission` ALPN | Clean separation from self-issued credential ALPNs. |
| D7 | Consumer identity binding | Root pubkey + possession proof | Stable across nodes. Consumer signs challenge nonce with root key. |
| D8 | Open/closed model | Publisher choice, changeable after publish | Open = any handle. Closed = explicit grants. Rate limiting optional. |
| D9 | Access grant scope | Handle-scope (Day 0) | Node-scope is Day 2. Data model has the field. |
| D10 | Delegation authority | Consumer only (Day 0) | Producer delegation is Day 2. Field exists in DelegationStatement. |
| D11 | Audit | Gossip (real-time) + iroh-doc (history) | `aster audit tail` and `aster audit log`. |

---

## 5. Open Issues

1. **`aster.admission` wire format.** The ALPN stream format: consumer sends token + attestation → service verifies crypto (steps 1-5) → service sends 32-byte nonce → consumer signs `nonce || target_handle || target_service || "aster.admission"` with root key → service verifies possession (step 6) → service sends admit/reject. Need to define the exact frame encoding (length-prefixed JSON? length-prefixed canonical CBOR? raw bytes with type tags?).

2. **Attestation bundling.** The signing key attestation is bundled with the enrollment token when the consumer gets it from `@aster`. The consumer forwards both to the service. The service caches attestations by `key_id` so it doesn't re-verify the root signature on every connection.

3. **Multi-node signing key distribution.** All `@aster` nodes need the current signing key. Day 0: all nodes load from the same file. Later: FROST ceremony distributes epoch keys.

4. **Search index.** iroh-docs don't have full-text search. Day 0 `discover` does prefix match on keys. For richer search later: in-memory index built from doc on startup, updated on doc events.

5. **Blob replication for manifests.** When a read-replica receives `get_manifest` for an unsynced manifest, it fetches from the primary via blob download between nodes.

6. **Rate limit format and enforcement.** The `"1/60m"` format needs parsing. Enforcement is per-consumer-per-service in-memory on each `@aster` node. Since tokens have short TTLs, even if two nodes issue tokens for the same consumer (split-brain), the blast radius is bounded by TTL.

7. **Open service without a handle.** ~~Can a consumer connect to an open published service without having an `@aster` handle?~~ **Decided: no.** All `@aster`-mediated connections require a handle. A handle is free and gives us identity for rate limiting and audit. The alternative is direct P2P — producer gives the consumer endpoint details out-of-band, consumer connects on `aster.consumer_admission`. Two clean paths, no middle ground.
