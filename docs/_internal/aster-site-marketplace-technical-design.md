# aster.site — Technical Design (Service Infrastructure)

**Status:** Pre-design  
**Date:** 2026-04-07  
**Companion docs:** [aster-site-marketplace.md](aster-site-marketplace.md), [aster-trust-architecture.md](aster-trust-architecture.md)

---

## Scope

This document covers the **aster.site service infrastructure** — everything that lives *outside* the Aster RPC framework repo. The framework repo handles only: (1) delegated admission check (second trusted pubkey) and (2) CLI commands that talk to aster.site's API.

Everything below is the aster.site service's responsibility.

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────┐
│                    aster.site                         │
│                                                       │
│  ┌─────────────┐  ┌──────────────┐  ┌─────────────┐ │
│  │ REST API    │  │ FROST Quorum │  │ Transparency │ │
│  │ (accounts,  │  │ (threshold   │  │ Log (Merkle  │ │
│  │  publish,   │  │  signing,    │  │  tree, audit │ │
│  │  discover,  │  │  epoch keys) │  │  proofs)     │ │
│  │  access)    │  │              │  │              │ │
│  └──────┬──────┘  └──────┬───────┘  └──────┬──────┘ │
│         │                │                  │        │
│  ┌──────┴──────────────────┴────────────────┴──────┐ │
│  │              Credential Issuance                 │ │
│  │  (ephemeral key signs consumer credentials)      │ │
│  └──────────────────────────────────────────────────┘ │
│                                                       │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────┐  │
│  │ Web UI       │  │ Account /    │  │ Endpoint   │  │
│  │ (browse,     │  │ Handle       │  │ Health     │  │
│  │  manage      │  │ System       │  │ Prober     │  │
│  │  access)     │  │ (OAuth)      │  │            │  │
│  └──────────────┘  └──────────────┘  └────────────┘  │
└─────────────────────────────────────────────────────┘
        ▲                                    │
        │  HTTP (publish, discover,          │ ed25519 credential
        │  access grant, enrollment)         │ (standard — FROST output
        │                                    │  is indistinguishable)
        │                                    ▼
┌───────┴────────────────────────────────────────────┐
│  Aster Framework (this repo)                        │
│  - Admission handler: verify against delegation_    │
│    pubkey if configured                             │
│  - CLI: aster publish / discover / access           │
└─────────────────────────────────────────────────────┘
```

---

## Component 1: FROST Threshold Signing Quorum

### Design Decisions Needed

- **Library choice:** ZF FROST (Zcash Foundation) is the most mature ed25519-compatible FROST implementation. Alternatively: frost-ed25519 from IETF draft implementors.
- **Quorum size:** 3-of-5 recommended for initial deployment. Geographic + jurisdictional distribution.
- **HSM backing:** Key shares should be stored in HSMs (AWS CloudHSM, GCP Cloud HSM, or YubiHSM) for production. Software keys acceptable for dev/staging.
- **Language:** Rust (for performance and memory safety in crypto code).

### Epoch-Based Delegated Signing

The FROST quorum signs once per epoch (~1 hour), producing an ephemeral ed25519 keypair. Regional signing nodes use the ephemeral key for fast (<5ms) credential issuance.

```
FROST quorum (3-of-5, ~800ms, once/hour)
    │
    │  signs ephemeral keypair + epoch metadata
    ▼
Ephemeral signing key distributed to regional nodes
    │
    │  signs credentials instantly (<5ms)
    ▼
Consumer credentials (standard ed25519 signature)
```

**Key properties:**
- FROST output is a standard ed25519 signature — verifiers (the Aster framework) don't need to know it's threshold-signed
- Ephemeral key destroyed at epoch end — compromise of one node gives at most 1 epoch of blast radius
- Each regional node can independently refuse to sign (local policy enforcement)

### Epoch Key Lifecycle

1. FROST quorum generates new ephemeral keypair (~800ms across nodes)
2. Ephemeral pubkey + FROST signature + epoch metadata appended to transparency log
3. Ephemeral private key distributed to regional signing nodes (encrypted channel)
4. Regional nodes sign credentials for the duration of the epoch
5. At epoch end, ephemeral key zeroed; new epoch begins
6. **Grace period:** Both old and new epoch keys valid for ~5 minutes during transition (handles clock skew)

### Latency Budget

| Operation | Latency | Frequency |
|-----------|---------|-----------|
| FROST epoch key signing | 500ms-1s | Once per hour |
| Credential issuance (ephemeral sign) | 1-5ms | Per consumer enrollment |
| Credential verification (service side) | <1ms | Per connection |
| Log root gossip | Background | Continuous |

---

## Component 2: Transparency Log

### Design

Append-only Merkle tree (BLAKE3), modeled on Certificate Transparency (RFC 6962).

Every credential issuance and every epoch key rotation is logged. The log is the mechanism by which:
- Service owners detect unauthorized credential issuance
- Consumers verify credentials are properly logged (reject unlogged/rogue credentials)
- Independent monitors watch for anomalies
- Peers detect split-view attacks via gossip

### Log Entry Types

```
EpochKeyEntry {
    epoch: u64,
    ephemeral_pubkey: [u8; 32],
    frost_signature: [u8; 64],     // FROST quorum over ephemeral_pubkey
    timestamp: u64,
}

CredentialEntry {
    consumer_pubkey: [u8; 32],
    service_contract_id: [u8; 32],
    owner_root_pubkey: [u8; 32],
    roles: Vec<String>,
    epoch: u64,
    issued_at: u64,
    expires_at: u64,
}
```

### Merkle Proof Verification

Services and consumers verify inclusion proofs: O(log n) proof size, standard Merkle audit paths.

```
verify_inclusion(leaf_hash, proof_path, root_hash, leaf_index, tree_size) -> bool
```

This verification could be offered as an optional client library. For MVP, credential signature verification alone is sufficient — the transparency log is defense-in-depth.

### Storage

**Option A:** Store the log in iroh-blobs (content-addressed, replicated, peers can cache).
**Option B:** Dedicated append-only store (Postgres + Merkle overlay, or purpose-built like Trillian).

Recommendation: Option B for the authoritative log, with Option A for distribution/caching.

### Compaction

The log grows forever. Old epochs can be archived while preserving verifiability via Merkle mountain ranges. Not needed for MVP — log growth is proportional to credential issuance, not RPC call volume.

---

## Component 3: Gossip-Based Consistency

Iroh's gossip layer provides free split-view detection. Peers subscribe to a well-known topic (`aster-log-roots`) and share log root hashes.

```
Peer A sees log root: abc123... for epoch 1047
Peer B sees log root: abc123... for epoch 1047  ← consistent
Peer C sees log root: xyz789... for epoch 1047  ← SPLIT VIEW DETECTED
```

If aster.site shows different logs to different parties, peers detect it through gossip. Same principle as CONIKS and Google Key Transparency.

**Implementation:** This is a thin layer on top of the existing gossip infrastructure. Each peer periodically broadcasts the most recent log root it has seen. Disagreement triggers an alert.

---

## Component 4: REST API

### Endpoints (Draft)

**Authentication:** OAuth 2.0 (GitHub, Google, email+password). Bearer token for API calls.

```
POST   /api/v1/auth/login          # OAuth flow initiation
POST   /api/v1/auth/link-key       # bind root pubkey to account

POST   /api/v1/services            # publish a service (contract manifest + endpoints)
GET    /api/v1/services             # search/discover services
GET    /api/v1/services/:handle/:name  # service detail
DELETE /api/v1/services/:handle/:name  # unpublish
PATCH  /api/v1/services/:handle/:name  # update visibility, metadata

POST   /api/v1/services/:handle/:name/endpoints    # register endpoint (node ID + TTL)
DELETE /api/v1/services/:handle/:name/endpoints/:id # deregister endpoint

POST   /api/v1/services/:handle/:name/access        # grant access
DELETE /api/v1/services/:handle/:name/access/:pubkey # revoke access
GET    /api/v1/services/:handle/:name/access         # list grants

POST   /api/v1/enroll              # consumer requests enrollment token
GET    /api/v1/audit/:handle/:name # credential issuance audit trail

GET    /api/v1/log/root            # current transparency log root
GET    /api/v1/log/proof/:index    # Merkle inclusion proof for entry
```

**Key response from `POST /api/v1/services`:**
```json
{
  "service_id": "...",
  "handle": "myhandle",
  "name": "TaskManager",
  "delegation_pubkey": "abc123...",  // ← this is what `aster publish` writes to local config
  "created_at": "..."
}
```

### Endpoint Liveness

Registered endpoints include a TTL. aster.site can optionally probe endpoints (QUIC connect to the node ID) to verify liveness. Stale endpoints (past TTL, failing probes) are marked unhealthy in the directory but not removed — the owner may want to see them.

Endpoints should heartbeat via `POST /api/v1/services/:handle/:name/endpoints` periodically (e.g., every 5 minutes). The CLI or framework could automate this.

---

## Component 5: Web UI

Browse published services, view contract details (methods, types, live endpoints), manage access grants, handle access requests, audit credential issuance.

Not specified further here — standard web application, not architecturally novel.

---

## Component 6: Account / Handle System

- Handles are unique, immutable, lowercase alphanumeric + hyphens
- One root pubkey per handle (can be rotated with proof of old key)
- Org handles with team membership and role-based admin
- Handle reservation/dispute process TBD

---

## Security Concerns for This Service

### Compromise Scenarios

| Scenario | Blast radius | Detection | Recovery |
|----------|-------------|-----------|----------|
| Single regional node compromised | Rogue credentials for remainder of epoch (max ~1 hour) | Transparency log shows unexpected issuance | Epoch rotates, node excluded |
| FROST quorum compromised (3-of-5) | Can sign arbitrary credentials until detected | Log shows unexpected epoch keys; gossip detects inconsistency | Emergency key rotation; all services update delegation_pubkey |
| Transparency log tampered | Hidden rogue credentials | Merkle root inconsistency detected by peers caching prior roots | Rebuild from peer caches; alert all service owners |
| aster.site goes offline | No new credentials issued; existing credentials work until TTL | Obvious | Resume service; consumers re-enroll; services can fall back to self-issued |

### Tunable Security Posture (per service)

| Posture | Credential TTL | Trade-off |
|---------|---------------|-----------|
| Relaxed | 24h | Fewer renewals, larger compromise window |
| Standard | 2h | Good balance |
| Strict | 15min | Tight security, requires reliable aster.site |
| Paranoid | 5min | Very tight, high aster.site dependency |

### Consumer Key Bootstrapping

When a consumer requests enrollment from aster.site:
1. Consumer generates an ed25519 keypair locally (or already has one from running an Iroh endpoint)
2. Consumer sends their pubkey + proof-of-possession (sign a challenge) to aster.site
3. aster.site verifies the consumer controls the key
4. aster.site issues a credential binding consumer_pubkey → service → roles
5. Consumer presents credential to service; service verifies signature against delegation_pubkey

### Revocation Between TTL Expiry

When an owner revokes a consumer via the web UI:
- **Immediate:** aster.site stops issuing new credentials for that consumer
- **Window:** Already-issued credentials remain valid until TTL expiry
- **Optional active revocation:** aster.site could push a revocation event via gossip to the service's producer mesh. This is best-effort — the service processes it if online. Not a hard guarantee.
- **Recommendation:** Keep TTLs short enough that the window is acceptable. For most services, 2-hour TTL means revocation takes effect within 2 hours without active push.

### Abuse Protection

- Rate limiting on enrollment token issuance (per consumer, per service)
- Service registration rate limiting (per account)
- Handle squatting: reservation period, trademark dispute process
- Spam services: flag/review mechanism, terms of service

---

## Crypto Choices

| Component | Algorithm | Rationale |
|-----------|-----------|-----------|
| Endpoint identity | ed25519 | Iroh built-in |
| Threshold signing | FROST (ed25519-compatible) | 2-round; output is standard ed25519 |
| Transparency log | Merkle tree (BLAKE3) | Consistent with iroh; proven at scale by CT |
| Credential format | JSON + ed25519 signature | Simple; matches existing framework credential format |
| Inclusion proofs | Standard Merkle audit paths | O(log n); well-understood |

---

## Open Questions

- **Log storage backend:** Trillian (Google's verifiable log) vs. custom? Trillian is battle-tested but adds operational complexity.
- **Multi-region deployment:** How many regions for epoch signing nodes? Cost vs. latency trade-off.
- **Cross-service credentials:** Can one credential grant access to multiple services from the same owner? Simpler UX but wider blast radius on compromise.
- **GDPR:** Consumer pubkeys in the public transparency log are pseudonymous (not PII), but worth legal review.
- **Pricing:** What constitutes a "private service" for billing? Number of access grants? Number of endpoints?
- **Federation:** Could multiple aster.site instances interoperate? (Probably not needed for v1.)

---

## References

- [RFC 6962 — Certificate Transparency](https://datatracker.ietf.org/doc/html/rfc6962)
- [FROST: Flexible Round-Optimized Schnorr Threshold Signatures](https://eprint.iacr.org/2020/852)
- [ZF FROST (Zcash Foundation)](https://github.com/ZcashFoundation/frost)
- [Google Trillian](https://github.com/google/trillian)
- [CONIKS: Bringing Key Transparency to End Users](https://coniks.cs.princeton.edu/)
- [aster-site-marketplace.md](aster-site-marketplace.md) — Platform concept and business model
- [aster-trust-architecture.md](aster-trust-architecture.md) — Trust architecture overview
- [aster-security-hardening.md](aster-security-hardening.md) — Current security posture
