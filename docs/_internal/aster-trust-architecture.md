# Aster Trust Architecture — Identity, Enrollment, and Transparency

**Status:** Design draft  
**Date:** 2026-04-07  
**Companion docs:** [aster-security-hardening.md](aster-security-hardening.md), [aster-site-marketplace.md](aster-site-marketplace.md)  
**Spec references:** [Aster-trust-spec.md](../../ffi_spec/Aster-trust-spec.md), [Aster-ContractIdentity.md](../../ffi_spec/Aster-ContractIdentity.md)

---

## The Problem: Discovery-Trust Paradox

The biggest friction point in P2P: **you can have the most efficient protocol in the world, but if a consumer can't find a service or verify who owns it, they won't use it.**

Today's answers are unsatisfying:

- **TLS/CAs:** You know the connection is encrypted, but you don't know *who* you're talking to. A Let's Encrypt cert proves domain ownership, not identity. mTLS exists but requires a shared CA — fine inside a company, painful across organizations.
- **PGP Web of Trust:** Theoretically beautiful. Practically dead. Nobody maintains keyrings, the UX is hostile, and the trust graph is sparse.
- **TOFU (SSH model):** Simple and practical for first contact, but no verification before first contact and no revocation after compromise.
- **OAuth/OIDC:** Centralized. Every call routes through or depends on an identity provider. The IdP is a single point of failure and a surveillance chokepoint.

Aster needs something better: **decentralized identity with centralized convenience, where the centralized part is auditable and optional.**

---

## Comparison: Why Existing Models Fall Short

### The Identity Problem

| | TLS + CAs | mTLS | PGP Web of Trust | OAuth/OIDC | Aster + aster.site |
|---|---|---|---|---|---|
| **Who is the other end?** | Domain owner (not person/org) | Certificate holder | Key holder (if you trust the chain) | Account holder at IdP | Endpoint keypair owner |
| **How is identity bound?** | CA issues cert for domain | Shared CA issues client certs | Other people sign your key | IdP vouches via token | Transparency log binds key → owner |
| **Identity verification before first contact?** | Yes (CA chain) | Yes (CA chain) | Only if you find a trust path | Yes (IdP) | Yes (transparency log) |
| **Works across organizations?** | Yes | Painful (shared CA or cross-signing) | In theory, rarely in practice | Requires federation (SAML/OIDC) | Yes (aster.site is the bridge) |

### The Compromise Problem

| | TLS + CAs | mTLS | PGP | OAuth/OIDC | Aster + aster.site |
|---|---|---|---|---|---|
| **What if the authority is compromised?** | Attacker issues certs valid for years | Same | No central authority to compromise | Attacker issues tokens until detected | Attacker gets at most 1 epoch (~1h) of ephemeral key |
| **How is compromise detected?** | Certificate Transparency logs (bolted on after the fact) | CT logs or manual audit | Key revocation (if anyone checks) | IdP audit logs (internal only) | Transparency log (public, gossip-verified) |
| **Blast radius of compromise** | All domains the CA serves, for cert lifetime (years) | All clients the CA serves | Depends on key's trust position | All users of the IdP | Credentials issued in current epoch only (hours) |
| **Revocation mechanism** | CRL/OCSP (broken — clients skip checks) | CRL/OCSP | Key revocation certs (rarely checked) | Token expiry + revocation lists | Short TTL (credentials expire naturally) + log monitoring |
| **Does revocation actually work?** | Poorly — CRL is ignored, OCSP is slow, browsers soft-fail | Same | No — revocation certs rarely propagate | Yes, if IdP is responsive | Yes — short TTL means revocation is rarely needed |

### The Architecture Problem

| | TLS + CAs | mTLS | PGP | OAuth/OIDC | Aster + aster.site |
|---|---|---|---|---|---|
| **Traffic routing** | Direct (good) | Direct (good) | N/A | Token-based (IdP not in data path) | Direct P2P (platform never sees traffic) |
| **Single point of failure** | CA (but many CAs exist) | Your CA | None | IdP | None — aster.site is optional convenience |
| **Can you self-host?** | No (you can't run a public CA) | Yes (internal CA) | Yes | Yes (run your own IdP) | Yes (self-issue credentials, skip aster.site) |
| **Vendor lock-in** | CA market is competitive | Locked to your CA infra | None | Locked to IdP | None — protocol is open source, aster.site is optional |
| **Setup complexity** | Low (ACME/LE) | High (CA infra, client cert distribution) | Very high (keyring management) | Medium (IdP config, client registration) | Low (aster.site handles it) or Medium (self-hosted) |

### The Trust Model Problem

| | TLS + CAs | mTLS | PGP | OAuth/OIDC | Aster + aster.site |
|---|---|---|---|---|---|
| **Trust basis** | "Trust these ~150 root CAs" | "Trust this one CA" | "Trust people who trust people" | "Trust this IdP" | "Verify the public log" |
| **Auditability** | CT logs (public, but only for certificates) | Internal audit only | Key servers (public but incomplete) | IdP logs (internal only) | Full transparency log (public, gossip-verified) |
| **Can a third party verify?** | Yes (CT monitors) | No | Partially (key servers) | No | Yes (anyone can run a log monitor) |
| **Granularity** | Domain-level | Certificate-level | Key-level | Scope/claim-level | Role-level per service per consumer |
| **Authorization model** | None (TLS is authn only) | None (mTLS is authn only) | None | Scopes/claims (coarse) | Three-gate: connection → credential → capability/role |

---

## Aster's Trust Architecture

### What Iroh gives us (transport layer)

Every iroh QUIC connection is authenticated by ed25519 endpoint keypair. You **always know which endpoint** you're talking to. This is strictly stronger than TLS — there's no CA in the loop, no certificate chain to validate, no domain-to-IP indirection. The cryptographic identity IS the network identity.

But endpoint identity alone doesn't tell you: who owns this endpoint, what are they authorized to do, and should I trust them?

### Three layers of trust

```
┌─────────────────────────────────────────────────────────┐
│ Gate 0: Transport Identity                              │
│ QUIC + ed25519 endpoint keys (iroh built-in)            │
│ "I know WHICH endpoint I'm connected to"                │
├─────────────────────────────────────────────────────────┤
│ Gate 1: Enrollment & Credential                         │
│ Signed credentials bind endpoint → owner → service      │
│ "I know WHO owns this endpoint and that they're         │
│  authorized to serve/consume this contract"             │
├─────────────────────────────────────────────────────────┤
│ Gate 2: Capability & Role                               │
│ Method-level access control via roles in credential     │
│ "I know WHAT this consumer is allowed to do"            │
└─────────────────────────────────────────────────────────┘
```

Gate 0 is solved by iroh. Gates 1 and 2 are solved by Aster's trust model. The question is: **who issues the credentials for Gate 1?**

### Credential issuance: three modes

**Mode 1: Self-issued (fully decentralized)**

The service operator generates credentials directly using their root key. No platform involved. Full sovereignty.

```
Service owner (root key)
    │
    │ signs credential directly
    ▼
Consumer credential → presented at Gate 1
```

Best for: internal services, high-security environments, operators who want full control.

**Mode 2: aster.site delegated (convenient, auditable)**

The service owner publishes to aster.site and delegates enrollment authority. aster.site issues credentials on behalf of the owner, based on the owner's access control settings.

```
Service owner ──delegates──→ aster.site ──issues──→ Consumer credential
                              (logged in transparency log)
```

Best for: public services, team collaboration, discovery-oriented use cases.

**Mode 3: Hybrid**

Service accepts both self-issued and aster.site-issued credentials. Owner manages high-trust consumers directly, delegates routine access to aster.site.

```
Service owner ──direct──→ Admin credentials (self-issued)
              ──delegates──→ aster.site ──→ Reader credentials (platform-issued)
```

Best for: production services with mixed access patterns.

---

## aster.site Trust Infrastructure

### Component 1: Threshold Signing (FROST)

aster.site's signing authority isn't held by one server. It's split via **FROST** (Flexible Round-Optimized Schnorr Threshold signatures) across multiple geographically distributed nodes.

```
Signing quorum: 3-of-5 nodes must agree

Node 1 (us-east)     ──┐
Node 2 (eu-west)     ──┤
Node 3 (ap-southeast) ─┼──→ threshold signature (ed25519-compatible)
Node 4 (us-west)     ──┤
Node 5 (eu-central)  ──┘
```

**Properties:**
- Compromise of 1-2 nodes doesn't compromise the signing key
- Resulting signature is standard ed25519 — verifiers don't need to know it's threshold-signed
- Each node can independently refuse to sign (policy enforcement at each node)

**Why FROST:** Works with ed25519 (which iroh already uses), requires only 2 rounds, output is indistinguishable from a normal ed25519 signature.

### Component 2: Epoch-based Delegated Signing

FROST's 2-round protocol adds ~500ms-1s latency across globally distributed nodes. Acceptable for rare events, not for high-frequency credential issuance.

**Solution: ephemeral signing keys, rotated per epoch.**

```
FROST quorum (slow, secure, infrequent)
    │
    │  signs once per epoch (e.g., every hour)
    ▼
Ephemeral signing key (fast, single-node, short-lived)
    │
    │  signs credentials instantly (<5ms, no round trips)
    ▼
Consumer credentials
```

**Epoch lifecycle:**

1. Every epoch (e.g., 1 hour), the FROST quorum signs a new ephemeral keypair (~800ms)
2. The ephemeral public key + threshold signature + epoch metadata is appended to the transparency log
3. The ephemeral private key is distributed to regional signing nodes
4. For the duration of the epoch, any regional node signs credentials instantly using the ephemeral key
5. At epoch end, the ephemeral key is destroyed; a new one is generated

**Regional deployment:**

```
FROST quorum signs epoch key (once/hour, ~800ms)
         │
         ├──→ us-east  (serves Americas, <5ms credential signing)
         ├──→ eu-west  (serves Europe/Africa, <5ms credential signing)
         └──→ ap-south (serves Asia-Pacific, <5ms credential signing)
```

**Latency budget:**

| Operation | Latency | Frequency |
|-----------|---------|-----------|
| FROST epoch key signing | 500ms-1s | Once per hour |
| Credential issuance (ephemeral sign) | 1-5ms | Per consumer enrollment |
| Credential verification (service side) | <1ms | Per connection |
| Log root gossip | Background | Continuous |

### Component 3: Transparency Log

Every credential issuance and every ephemeral key rotation is logged in an **append-only Merkle tree** — a verifiable log modeled on Certificate Transparency (RFC 6962).

```
Merkle tree (append-only):

Root hash: abc123... (published every epoch via gossip)
├── [epoch 1047] ephemeral_pubkey: a1b2c3..., signed by FROST quorum
├── [credential] consumer A → service X, roles [admin], epoch 1047
├── [credential] consumer B → service X, roles [reader], epoch 1047
├── [credential] consumer C → service Y, roles [writer], epoch 1047
├── [epoch 1048] ephemeral_pubkey: d4e5f6..., signed by FROST quorum
├── ...
```

**What the log enables:**

| Actor | What they can do |
|-------|-----------------|
| **Service owner** | "Show me every credential issued for my service" — detect unauthorized issuance |
| **Consumer** | "Prove this credential is in the log" — reject unlogged (rogue) credentials |
| **Independent monitor** | Watch the full log for anomalies — anyone can run one |
| **Peers (via gossip)** | Compare log root hashes — detect split-view attacks |

**Credential format with log binding:**

```json
{
  "consumer": "<consumer_pubkey>",
  "service": "<contract_id>",
  "owner": "<owner_root_pubkey>",
  "roles": ["reader"],
  "issued_at": 1743933600,
  "expires_at": 1743940800,
  "epoch": 1047,
  "log_index": 47291,
  "log_root": "abc123...",
  "signature": "<ephemeral key signature>"
}
```

**Verification chain (consumer or service side):**

```
1. Credential is signed by ephemeral key for epoch 1047   ← ed25519 verify
2. Epoch 1047's ephemeral key is signed by FROST quorum   ← ed25519 verify
3. Epoch 1047's ephemeral key is in the transparency log   ← merkle inclusion proof
4. Credential is in the transparency log                   ← merkle inclusion proof
5. Ephemeral key hasn't expired                            ← timestamp check
6. Credential hasn't expired                               ← timestamp check
```

Two signature checks, two inclusion proofs, two timestamp checks. Fast. And the verifier **doesn't need to talk to aster.site** — they just need a recent log root (distributed via gossip).

### Component 4: Gossip-based Consistency

Iroh's gossip layer provides **free split-view detection**. Peers subscribe to a well-known topic (e.g., `aster-log-roots`) and share the log root hashes they've seen.

```
Peer A sees log root: abc123... for epoch 1047
Peer B sees log root: abc123... for epoch 1047  ← consistent ✓
Peer C sees log root: xyz789... for epoch 1047  ← INCONSISTENT ✗ (split-view attack!)
```

If aster.site shows different logs to different people (a split-view attack), peers detect it through gossip and raise an alert. This is the same principle behind CONIKS and Google's Key Transparency.

---

## Compromise Scenarios

### Scenario 1: Single aster.site node compromised

**Impact:** Attacker gets the current epoch's ephemeral private key for that region.  
**Blast radius:** Can sign rogue credentials for the remainder of this epoch (at most ~1 hour).  
**Detection:** Rogue credentials appear in the transparency log (or fail inclusion verification if not logged).  
**Recovery:** Epoch rotates. New ephemeral key generated. Compromised node excluded from quorum.  
**Compare to CA compromise:** CA key valid for years; attacker can issue certs for any domain.

### Scenario 2: Threshold quorum compromised (3-of-5 nodes)

**Impact:** Attacker can sign arbitrary ephemeral keys and credentials.  
**Blast radius:** Severe — can issue credentials for any service until detected.  
**Detection:** Transparency log shows unexpected ephemeral keys. Service owners see unauthorized credentials. Gossip consistency detects split views.  
**Recovery:** Emergency key rotation — publish new FROST quorum public key. All services must update their delegation trust anchor.  
**Mitigation:** Geographic + jurisdictional distribution of nodes (e.g., different cloud providers, different countries). HSM-backed key shares.  
**Compare to CA compromise:** Similar severity, but transparency log makes detection faster.

### Scenario 3: Transparency log tampered with

**Impact:** Attacker tries to hide rogue credentials by modifying the log.  
**Detection:** Merkle tree structure makes tampering evident — any modification changes the root hash. Peers holding prior roots detect the inconsistency.  
**Mitigation:** Log roots are gossiped and cached by many peers. Tampering requires controlling all peers who've seen the original root.  
**Compare to CT:** Same properties as Certificate Transparency — append-only Merkle trees are tamper-evident by construction.

### Scenario 4: aster.site goes offline

**Impact:** No new credentials can be issued.  
**Blast radius:** Existing credentials keep working until TTL expiry. Self-issued credentials (Mode 1) unaffected.  
**Recovery:** Service resumes. Consumers re-enroll.  
**Mitigation:** Longer credential TTLs provide more buffer. Services can fall back to Mode 1 (self-issued) during outages.  
**Compare to CA going offline:** Similar — existing certs work, new issuance stops.

---

## Tunable Security Posture

Service owners choose their comfort level:

| Posture | Credential TTL | Epoch duration | Trade-off |
|---------|---------------|----------------|-----------|
| **Relaxed** | 24h | 6h | Fewer renewals, larger compromise window |
| **Standard** | 2h | 1h | Good balance for most services |
| **Strict** | 15min | 15min | Tight security, more frequent FROST rounds |
| **Paranoid** | 5min | 5min | Very tight, requires reliable aster.site connectivity |
| **Sovereign** | N/A (self-issued) | N/A | No platform dependency; full operator control |

---

## Crypto Choices

| Component | Algorithm | Rationale |
|-----------|-----------|-----------|
| Endpoint identity | ed25519 | Iroh built-in; fast; well-studied |
| Threshold signing | FROST (ed25519-compatible) | 2-round; output is standard ed25519; no verifier changes needed |
| Transparency log | Merkle tree (BLAKE3) | Proven by Certificate Transparency at Google-scale; BLAKE3 for consistency with iroh |
| Credential format | CBOR + ed25519 signature | Compact; no ASN.1; deterministic encoding |
| Inclusion proofs | Standard Merkle audit paths | O(log n) proof size; well-understood |
| Ephemeral key generation | CSPRNG (OS-provided) | Standard practice |

---

## What Iroh Gives Us for Free

| Iroh feature | How we use it |
|-------------|---------------|
| **QUIC endpoint identity** | Gate 0 — transport-level authentication, no CA needed |
| **Content-addressed blobs** | Transparency log stored as iroh blobs (Merkle tree is naturally content-addressed) |
| **Gossip pub-sub** | Log root consistency checking; split-view detection |
| **CRDT docs** | Replicated log state for resilience and offline caching |
| **NAT traversal** | P2P connections work without infrastructure — no traffic through aster.site |

---

## Integration with aster.site Access Control

When a service is published to aster.site with delegation enabled:

1. **Service owner** sets visibility (public/private) and grants access via web UI or CLI
2. **aster.site** stores the access policy: `{consumer_pubkey → roles}`
3. **Consumer** requests enrollment → aster.site checks the policy → signs credential with ephemeral key → logs to transparency log → returns credential + inclusion proof
4. **Consumer** connects P2P to the service, presents credential
5. **Service admission handler** verifies:
   - Credential signature (ephemeral key for this epoch)
   - Ephemeral key signature (FROST quorum)
   - Inclusion proof (credential is in the log)
   - TTL (not expired)
   - Roles (Gate 2 capability check)
6. **Service owner** can audit at any time: `aster audit --service TaskManager`

The service never talks to aster.site during the connection. The consumer obtained the credential beforehand. All verification is local (signatures + inclusion proofs + cached log root).

---

## Open Questions

- **Log storage:** Should the transparency log be stored in iroh-blobs (eating our own dogfood)? Pros: content-addressed, replicated, peers can cache. Cons: adds dependency on iroh availability for log verification.
- **Epoch overlap:** During epoch transitions, should both the old and new ephemeral keys be valid? (Probably yes — grace period of a few minutes to handle clock skew.)
- **Offline credential caching:** How long should a consumer cache a credential before re-enrolling? Should services accept slightly-expired credentials with a grace period?
- **Cross-service credentials:** Can one credential grant access to multiple services from the same owner? Or always one credential per service?
- **Log compaction:** The transparency log grows forever. Can old epochs be compacted/archived while preserving verifiability? (Merkle mountain ranges may help.)
- **FROST implementation:** Use an existing FROST library (e.g., ZF FROST from Zcash Foundation) or implement from spec?
- **HSM integration:** Should FROST key shares be stored in HSMs for the production deployment?
- **Regulatory:** Does the transparency log's public nature create GDPR concerns? (Consumer pubkeys are pseudonymous, not PII, but worth confirming.)

---

## References

- [RFC 6962 — Certificate Transparency](https://datatracker.ietf.org/doc/html/rfc6962)
- [FROST: Flexible Round-Optimized Schnorr Threshold Signatures](https://eprint.iacr.org/2020/852)
- [CONIKS: Bringing Key Transparency to End Users](https://coniks.cs.princeton.edu/)
- [Google Key Transparency](https://github.com/google/keytransparency)
- [Aster-trust-spec.md](../../ffi_spec/Aster-trust-spec.md) — Three-gate trust model
- [Aster-ContractIdentity.md](../../ffi_spec/Aster-ContractIdentity.md) — Contract identity and hashing
- [aster-site-marketplace.md](aster-site-marketplace.md) — Platform concept and business model
- [aster-security-hardening.md](aster-security-hardening.md) — Current security posture and defenses
