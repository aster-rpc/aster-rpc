# Aster — High Availability, Load Balancing & Routing Design

**Status:** Design draft  
**Date:** 2026-04-07  
**Companion docs:** [Aster-trust-spec.md](../../ffi_spec/Aster-trust-spec.md), [aster-site-marketplace.md](aster-site-marketplace.md), [aster-security-hardening.md](aster-security-hardening.md)  
**Spec references:** [Aster-SPEC.md](../../ffi_spec/Aster-SPEC.md) (RPC wire protocol), [Aster-ContractIdentity.md](../../ffi_spec/Aster-ContractIdentity.md) (contract identity)

---

## The Problem

The Aster spec defines how producers form meshes and serve contracts, but does not specify how consumers route requests across multiple producers serving the same contract. Without routing:

- Consumers pick producers randomly or sequentially — no load awareness
- Overloaded producers reject requests, forcing a slow try→fail→retry loop
- There is no mechanism for graceful scale-up or scale-down
- No failover — if a consumer's chosen producer dies, the consumer has no guidance on where to go next

This document defines a **client-side routing architecture** that provides load balancing, failover, and elastic scaling — all without a central load balancer component.

---

## Design Principles

1. **No central load balancer.** Every consumer is its own load balancer. The load balancing work is distributed across all consumers, not concentrated in a single component. No extra hop, no single point of failure.
2. **Happy path: right producer first time.** Consumers should almost never hit an overloaded producer. Rejection is the safety net, not the mechanism.
3. **Information-gated by trust level.** Consumers (untrusted) see coarse routing hints. Producers (trusted, salt-gated gossip) see exact metrics. Defense in depth.
4. **Scale-up/down with zero config changes.** Add a producer → it self-announces → consumers discover it → load redistributes. Remove a producer → it drains → consumers stop routing to it. No DNS updates, no LB reconfiguration.

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│              Producer Gossip Mesh (salt-gated)           │
│                                                         │
│  ┌───────────┐  ┌───────────┐       ┌───────────┐      │
│  │Producer-01│──│Producer-02│──...──│Producer-N │      │
│  │           │  │           │       │           │      │
│  │ Writes    │  │ Writes    │       │ Writes    │      │
│  │ own lease │  │ own lease │       │ own lease │      │
│  └─────┬─────┘  └─────┬─────┘       └─────┬─────┘      │
│        │              │                    │            │
│        ▼              ▼                    ▼            │
│  ┌─────────────────────────────────────────────────┐    │
│  │         Registry Doc (iroh-docs, CRDT)          │    │
│  │                                                 │    │
│  │  lease/node-01 → {health: healthy, load: low}   │    │
│  │  lease/node-02 → {health: healthy, load: medium} │    │
│  │  lease/node-N  → {health: draining, load: high} │    │
│  └─────────────────────┬───────────────────────────┘    │
│                        │                                │
└────────────────────────┼────────────────────────────────┘
                         │ CRDT sync via K existing connections
                         │ (consumers are read-only leaves)
          ┌──────────────┼──────────────┐
          │              │              │
     Consumer-A     Consumer-B     Consumer-C  ... millions more
     (reads leases,  (reads leases,
      scores,         scores,
      connects to     connects to
      top K=3)        top K=3)
```

### Information Boundary

The producer gossip channel is gated by the mesh salt (Aster-trust-spec.md §2.3). Consumers do not have the salt and cannot subscribe to producer gossip. This creates a natural information boundary:

| Channel | Access | Contents |
|---------|--------|----------|
| **Producer gossip** (salt-gated) | Producers only | Exact metrics: capacity ratio, session counts, max sessions, CPU pressure, memory |
| **Registry doc** (read-only ticket) | Producers + admitted consumers | Coarse routing hints: health status enum, load bucket, region, timestamp |
| **Response trailers** (per-connection) | The connected consumer only | Per-RPC load hint for that specific producer |

---

## Consumer Routing Table

Each consumer maintains a scored routing table per contract_id:

```
contract_id: blake3(TaskManager)
┌────────────────┬──────────┬──────┬──────────┬───────────┬───────┐
│ endpoint       │ latency  │ load │ health   │ in-flight │ score │
├────────────────┼──────────┼──────┼──────────┼───────────┼───────┤
│ node-eu-1      │ 12ms     │ low  │ healthy  │ 2         │ 0.91  │
│ node-us-east-1 │ 85ms     │ low  │ healthy  │ 0         │ 0.78  │
│ node-eu-2      │ 15ms     │ high │ healthy  │ 7         │ 0.45  │
│ node-ap-1      │ 210ms    │ low  │ draining │ 0         │ 0.00  │
└────────────────┴──────────┴──────┴──────────┴───────────┴───────┘
```

### Score Computation

```
score = w_health * health_score
      + w_load   * load_score
      + w_latency * latency_score
      + w_inflight * inflight_score

where:
  health_score:   healthy=1.0, degraded=0.5, draining=0.0
  load_score:     low=1.0, medium=0.6, high=0.2
  latency_score:  1.0 - clamp(rtt_ms / max_acceptable_rtt, 0, 1)
  inflight_score: 1.0 - clamp(in_flight / max_concurrent, 0, 1)
```

Weights are tunable per deployment. Default emphasis: health > load > latency > in-flight.

### Three Information Sources

**1. Registry doc leases (background, coarse-grained)**

Producers write their own lease entry to the registry doc every 10–15 seconds. The doc syncs to consumers via CRDT through their existing producer connections.

Consumer-visible lease entry:

```json
{
  "service": "TaskManager",
  "health": "healthy",
  "load": "low",
  "region": "eu-west",
  "updated_at": 1743933600
}
```

Three load buckets only: `low`, `medium`, `high`. No exact session counts, no capacity numbers. See §Security below for rationale.

**2. Response trailers (per-RPC, real-time)**

Each RPC response includes a lightweight load hint in trailer metadata:

```
x-aster-load: medium
```

One field, one of three values. This is per-connection real-time feedback — "I just served your request and here's my current state." The consumer updates the routing table score for that endpoint after every RPC.

**3. Measured latency (passive, per-connection)**

QUIC measures RTT natively during handshakes and with every ACK. The consumer already knows the latency to every endpoint it has an active connection to. For endpoints without an active connection, latency can be estimated from region hints or measured with a probe (1-RTT QUIC handshake).

---

## Endpoint Selection: Power of Two Choices

When routing an RPC, the consumer does not simply pick the top-scoring endpoint. It uses the **power of two choices** algorithm:

1. Pick the top 2 endpoints by score from the active set
2. Choose the one with fewer in-flight requests

This is a well-studied result from load balancing theory (Mitzenmacher, 2001). It achieves near-optimal load distribution with O(1) decision cost and no global state. It avoids the thundering herd problem where all consumers converge on the same "best" endpoint.

```python
def pick_endpoint(table, contract_id):
    candidates = table.top_n(contract_id, n=2)
    return min(candidates, key=lambda e: e.in_flight)
```

---

## Subset Connection Management

Consumers do NOT connect to all producers. Each consumer maintains connections to a small subset of K producers:

| Producer count | K (connections per consumer) | Notes |
|---------------|---------------------------|-------|
| 1–5 | All | Small mesh, no subsetting needed |
| 5–30 | 3–5 | Subset, power-of-two selection within subset |
| 30–100 | 3–5 | Same — K does not grow with N |
| 100+ | 5–10 | Slightly larger subset for better distribution |

K stays small regardless of producer count. The consumer's connection count is O(1), not O(N).

### Consumer Lifecycle

```
1. Admission
   └── Receive registry doc ticket + full endpoint list

2. Initial sync
   └── Sync registry doc → have all lease entries locally

3. Initial subset selection
   └── Score all endpoints from leases
   └── Connect to top K by score (latency estimate + health + load)

4. Steady state routing
   └── Route RPCs using power-of-two from active K
   └── Update scores from response trailers after each RPC

5. Periodic refresh (every 30s)
   └── Re-read leases from local doc replica (auto-synced via CRDT)
   └── Re-score all endpoints
   └── If a significantly better endpoint exists outside current K:
       └── Connect to it, drop the worst performer from K

6. On endpoint failure
   └── Mark failed endpoint score = 0
   └── Pick next-best from full endpoint list
   └── Connect (QUIC 1-RTT handshake — fast)
   └── Resume routing
```

### Latency Estimation Without Connecting

For endpoints not in the active K (no RTT measurement available):

- **Region hints:** Lease entries include `region`. Consumer knows its own region. Same-region endpoints get a low estimated latency; cross-region gets higher.
- **Relay proximity:** iroh tracks relay servers. Endpoints on the same relay are likely nearby.
- **Probe on demand:** When swapping an endpoint into the active set, the consumer performs a QUIC handshake (1-RTT) to measure actual latency before committing.

---

## Registry Doc Scaling

### Why It Works at Scale

The registry doc is small (~100 lease entries × ~100 bytes = ~10KB) and changes infrequently (each producer updates its own entry every 10–15 seconds). Consumers are read-only — they never write to the doc.

The sync path:

```
Producer-42 updates its lease entry
    ↓ (producer gossip mesh, ~7 hops)
All producer doc replicas updated
    ↓ (CRDT sync through existing QUIC connections)
Consumer replicas updated (each consumer syncs through its K=3 producers)
```

Consumers sync through the producers they're already connected to. They are **read-only leaf replicas**, not mesh participants. They never sync with each other.

### Connection math at scale

| Consumers | Producers | K | Connections per producer | Doc delta broadcast per producer |
|-----------|-----------|---|------------------------|----------------------------------|
| 1,000 | 10 | 3 | ~300 | ~30KB/s |
| 10,000 | 50 | 3 | ~600 | ~60KB/s |
| 100,000 | 100 | 3 | ~3,000 | ~300KB/s |
| 1,000,000 | 100 | 3 | ~30,000 | ~3MB/s |

At 1M consumers and 100 producers: each producer handles ~30K consumer connections and broadcasts ~100-byte deltas to each. This is well within modern server capacity. QUIC connections are lightweight (no TCP state machine per connection), and the sync traffic is minimal.

---

## Producer-Side Load Reporting

### To producer gossip (full metrics, producers only)

Producers broadcast detailed load information on the salt-gated gossip channel every 10 seconds:

```
LeaseUpdate (gossip envelope) {
    service: "TaskManager",
    health: HealthStatus,
    capacity: 0.73,           // ratio: 0.0 = full, 1.0 = idle
    active_sessions: 73,
    max_sessions: 100,
    queued_requests: 12,
    cpu_utilization: 0.45,
    memory_used_mb: 3200,
    updated_at: epoch_ms
}
```

This is used for producer-to-producer coordination: rebalancing, alerting, capacity planning. Consumers never see it.

### To registry doc (coarse buckets, consumers can read)

Producers write a simplified lease to the registry doc:

```json
{
  "service": "TaskManager",
  "health": "healthy",
  "load": "low",
  "region": "eu-west",
  "updated_at": 1743933600
}
```

**Health enum:** `healthy` | `degraded` | `draining`  
**Load buckets:** `low` (< 40% capacity) | `medium` (40–75%) | `high` (> 75%)

Thresholds for bucket boundaries are configurable per deployment.

### To response trailers (per-RPC, real-time)

Each RPC response trailer includes:

```
x-aster-load: medium
```

One field. Allows the consumer to fine-tune its score for this specific endpoint based on the most recent interaction.

---

## Rejection and Fast Recovery

When a producer is genuinely overloaded and receives a request it cannot serve:

1. Producer returns `StatusCode.UNAVAILABLE` with optional `retry-after-ms` in trailer metadata
2. Consumer marks that endpoint with score = 0
3. Consumer immediately retries on the next-best endpoint from its active set (no sleep)
4. The existing `RetryInterceptor` and `ExponentialBackoff` handle retry policy
5. The rejected endpoint's score recovers gradually after the retry-after window expires

**This is the slow path.** With gossip-fed load buckets and trailer feedback, consumers almost never hit overloaded producers. Rejection is the safety net for burst scenarios where information is temporarily stale.

---

## Session Affinity

For session-scoped services (Aster-session-scoped-services.md):

- The routing decision happens at **session creation time**
- Once a session is bound to an endpoint, all RPCs in that session go to that endpoint
- No re-routing mid-session — sessions are stateful and tied to a specific producer
- If the endpoint dies mid-session, the session fails; the consumer creates a new session on a different endpoint
- Session creation uses the same scored routing table as unary RPCs

---

## Scale-Up and Scale-Down

### Adding a producer

1. New producer joins the mesh (admission handshake, §2.4)
2. Producer writes its lease to the registry doc: `{health: healthy, load: low}`
3. Registry doc syncs to consumers via CRDT
4. Consumers see the new endpoint in their next refresh cycle (≤30s)
5. New producer is scored high (healthy + low load) → consumers start routing to it
6. Load redistributes organically — no manual intervention

### Removing a producer (graceful)

1. Producer sets its lease to `{health: draining, load: high}`
2. Consumers see the update → score drops to 0 → stop routing new requests
3. Producer finishes in-flight RPCs and active sessions
4. Producer shuts down cleanly
5. Lease entry in the doc becomes stale (no more updates) → consumers ignore it after TTL

### Removing a producer (crash)

1. Producer stops updating its lease entry
2. After the lease TTL expires (e.g., 60s with no update), consumers treat it as unhealthy
3. Consumers with active connections to the crashed producer detect the QUIC connection drop immediately
4. Those consumers mark the endpoint as failed, swap in the next-best from the full list
5. QUIC connection failure → failover is near-instant (new stream on existing warm connection to another producer)

---

## Security: Why Consumers See Coarse Metrics Only

Consumers are untrusted. Giving them exact load metrics enables targeted attacks:

| Attack | Requires | Coarse buckets prevent? |
|--------|----------|----------------------|
| **Targeted DoS** — hammer the most-loaded producer to tip it over | Exact capacity numbers | Yes — attacker can't identify which producer is closest to failing |
| **Capacity reconnaissance** — sum max_sessions to calculate total mesh capacity | Exact max_sessions per producer | Yes — no max values exposed |
| **Timing attacks** — monitor load patterns to find maintenance windows | Fine-grained load over time | Partially — buckets change less frequently, harder to correlate |
| **Selective pressure** — always target least-loaded producer to prevent distribution | Exact capacity ratio | Partially — attacker knows "low" but not which of several "low" endpoints is lowest |

**What coarse buckets DON'T prevent:**

- A consumer can probe endpoints (connect, measure RTT, observe response latency) to infer approximate load. This is slow, noisy, and detectable.
- A consumer can connect to all endpoints and observe rejection patterns. This is also detectable via anomaly monitoring.

**Layered defense:**

1. **Information hiding:** Coarse buckets in registry doc. Exact metrics in producer-only gossip.
2. **Producer-side enforcement:** Per-consumer rate limits, session caps, `UNAVAILABLE` rejection.
3. **Anomaly detection:** Consumer opening connections to all producers? Consumer consistently targeting high-load endpoints? Flag and potentially revoke.
4. **Admission revocation:** Misbehaving consumer's credential has a short TTL. Don't renew it.

---

## Implementation Plan

### What to build (this repo)

| Component | Location | Description |
|-----------|----------|-------------|
| **Lease entry schema** | `aster/registry/models.py`, TS `registry/models.ts` | Extend lease with `health`, `load` (bucket), `region` |
| **Lease writer** | `aster/registry/publisher.py`, TS `registry/publisher.ts` | Producer writes own lease on timer (10–15s) |
| **Load bucket computation** | `aster/routing/load.py` (new) | Map actual capacity ratio → low/medium/high bucket |
| **Endpoint scorer** | `aster/routing/scorer.py` (new) | Score = f(health, load, latency, in-flight) |
| **Subset router** | `aster/routing/router.py` (new) | Manages active K connections, subset swap logic, power-of-two selection |
| **Trailer load hints** | `aster/server.py` response path | Producer includes `x-aster-load` in response trailers |
| **Trailer hint reader** | `aster/client.py` response path | Consumer reads `x-aster-load`, updates scorer |
| **Connection pool** | `aster/transport/pool.py` (new) | Maintain warm QUIC connections to K endpoints |
| **TypeScript parity** | `packages/aster/src/routing/` (new) | Mirror Python routing in TypeScript |

### What stays in Rust core

| Component | Location | Description |
|-----------|----------|-------------|
| **RTT exposure** | `core/src/lib.rs` | Expose QUIC connection RTT to binding layer |
| **Connection pool (optional)** | `core/src/lib.rs` | Manage warm connections to multiple node IDs |

The routing intelligence lives in the binding layer because it integrates with the RPC framework (trailers, interceptors, client stubs) which is implemented in Python/TypeScript. The Rust core provides transport primitives (connections, RTT).

### What's NOT in this repo

- Producer gossip content for detailed metrics — already defined in Aster-trust-spec.md §2.6 (LeaseUpdate message type). Implementation follows the existing gossip framework.
- aster.site endpoint list API — the initial endpoint list for discovery. Ongoing routing uses the registry doc.

---

## Open Questions

- **Bucket thresholds:** Should low/medium/high boundaries be fixed (40%/75%) or adaptive (based on mesh-wide distribution)?
- **Subset swap frequency:** How often should a consumer consider swapping an endpoint in its active set? Too frequent = connection churn. Too infrequent = slow adaptation.
- **Warm connection cost:** QUIC connections are cheap, but K=5 warm connections still consume memory and keepalive bandwidth. What's the right K for different deployment sizes?
- **Multi-contract routing:** If a producer serves multiple contracts, should the consumer maintain separate routing tables per contract or one unified table?
- **Priority/QoS:** Should some consumers get priority routing to low-load endpoints? (e.g., premium tier via aster.site)
- **Geographic awareness:** Should the scorer heavily penalize cross-region endpoints, or let the latency measurement handle it naturally?

---

## References

- Mitzenmacher, M. (2001). "The Power of Two Choices in Randomized Load Balancing." IEEE Transactions on Parallel and Distributed Systems.
- [Aster-trust-spec.md](../../ffi_spec/Aster-trust-spec.md) — Producer gossip, LeaseUpdate message type (§2.6)
- [Aster-SPEC.md](../../ffi_spec/Aster-SPEC.md) — RPC wire protocol, trailers, metadata
- [Aster-session-scoped-services.md](../../ffi_spec/Aster-session-scoped-services.md) — Session affinity model
- [aster-security-hardening.md](aster-security-hardening.md) — Threat model, size limits, defense layers
- [aster-site-marketplace.md](aster-site-marketplace.md) — Initial endpoint list from directory
