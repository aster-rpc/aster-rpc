# Aster Monetisation Strategy

## Core positioning

The library is free. The infrastructure that makes it production-grade is paid.
Iroh owns the transport layer (relays, NAT traversal). Aster owns the identity
and resilience layer on top.

---

## Revenue streams

### 1. Managed Replica Nodes (primary)

Private service nodes that replicate a producer's blobs and CRDT docs as warm
standby. The producer's business logic stays in their environment; Aster
strengthens the foundations.

**How it works:**
- Producer admits an Aster-managed node to their mesh (one CLI command)
- Blobs and docs replicate to the Aster node automatically (iroh sync)
- If the producer goes offline, consumers read from the replica transparently
- Producer comes back, syncs catch up — content-addressing and CRDTs make this
  idempotent by design
- Trust flows from the producer's root key, not Aster-the-company — the producer
  controls who replicates their data

**Key properties:**
- Invisible to consumers — they connect to the producer's address, failover is
  transparent. No client-side failover logic needed.
- Producer opts in — Aster node is admitted to the mesh via the existing trust
  model. One `aster enroll` command.
- Read-only replica — the Aster node serves reads but doesn't mutate state.
  Business logic stays with the producer.

**Analogy:** CDN for state. You don't move your app, you make the foundation
more resilient.

### 2. Identity & Trust Management (high value)

Hosted credential lifecycle: issuance, rotation, revocation, audit logs.

- `aster trust keygen` and `aster enroll` are self-hosted today
- Hosted version adds: credential dashboard, expiry alerts, revocation lists,
  audit trail, SSO integration
- Enterprises don't want to manage PKI — they want to declare policy and have
  it enforced

### 3. Fleet Observability

Dashboard showing mesh topology, admitted peers, RPC call patterns, credential
expiry, health status.

- Built on data already flowing through the admission and RPC layers
- Complements the CLI (`aster shell`) with a visual overview
- Upsell from replica nodes — "you're already paying for availability, see
  what's happening"

---

## Pricing tiers

| Tier | What you get | Price |
|------|-------------|-------|
| **Free** | Library + public relays (iroh) + CLI tools | $0 |
| **Pro** | 1 managed replica node, blob backup, doc sync, 99.9% SLA | $X/mo |
| **Team** | Multiple replicas, private relay, credential dashboard | $Y/mo |
| **Enterprise** | Dedicated nodes, SSO, audit logs, SLA negotiation | Custom |

---

## Why this works

1. **Trust model is the moat** — the Aster node must be admitted by the
   producer's root key. This isn't "give us your data" — it's "strengthen
   your mesh with a managed peer." Much easier enterprise sell.

2. **Zero client changes** — consumers don't know or care that a replica
   exists. Failover is a property of the mesh, not the client.

3. **Iroh does the heavy lifting** — content-addressed blobs and CRDT docs
   already sync automatically. We're selling operational convenience, not
   building a new replication layer.

4. **Natural upsell from free** — developer uses the library, hits production,
   needs availability guarantees, adds a replica node. No migration, no
   architecture change.

5. **Identity compounds** — every credential issued, every peer enrolled, every
   role defined increases switching cost. The trust graph is the stickiest
   part of the product.

---

## Competitive positioning

| Competitor | What they sell | Our angle |
|-----------|---------------|-----------|
| gRPC + Envoy | Transport + service mesh | No infrastructure needed |
| Tailscale | Network layer VPN | Application-level RPC + auth |
| Cloudflare Workers | Edge compute | P2P, no central cloud |
| HashiCorp Vault | Secrets management | Identity built into transport |
| Nutanix | Hyperconverged infra | Same model: open source → managed |

---

## Open questions

- Pricing benchmarks: what do teams pay for similar infra? (Tailscale $5-18/user,
  Cloudflare $25/mo pro, managed Redis $15-100/mo)
- Should replica nodes also relay RPC calls, or only serve blobs/docs?
- Multi-region: should Aster offer geo-distributed replicas?
- Marketplace: producers publish services, consumers discover them via Aster
  directory (see aster-site-marketplace.md)
