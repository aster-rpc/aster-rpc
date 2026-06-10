# aster-leases — Single-Owner Leases On A P2P QUIC Mesh

**Status:** Working idea / shared primitive (to implement and use across
portal-sync and the orchestrator)
**Date:** 2026-06-10
**Related:**
- [../trust-directory.md](../trust-directory.md) — the advisory grant + audit substrate; revocation = epoch bump + tombstone (same mechanism)
- [aster-orchestrator.md](aster-orchestrator.md) — "CP for leases, AP for everything else" stance; singleton placement and stateful failover consume this
- portal-sync: `docs/roaming-workspace.md` (exclusive migrating lease + fencing), `docs/phase5-triage-queue.md` P5T-007 (single-owner occupancy)

---

## The problem, and the one reframe that solves it

"Obtain a single lease/lock on a resource" recurs everywhere: the roaming
active node, the orchestrator's singleton workload, a primary database, a
single-writer volume, portal's per-tree runtime owner. The mesh is P2P and the
directory is **AP** (iroh-docs is last-write-wins), so the instinct is to ask
the directory who holds the lease. That instinct is a corruption bug.

> **The grant and the enforcement are different problems and must not share a
> mechanism.** LWW gossip converges to one winner *eventually*, but during the
> convergence window two nodes can both believe they won. For a conflict you
> can sibling away, that is fine. For a volume or a database, that window is
> corruption.

Correctness never comes from a perfect grant (you cannot build one over an
async network — Kleppmann's argument against Redlock). It comes from **fencing
tokens enforced at the resource**: a monotonic epoch the holder stamps on every
action, which the resource rejects if lower than the highest it has seen. A
paused or partitioned ex-holder that wakes and acts late has its stale writes
*rejected by the thing it writes to*. The lock service being wrong becomes
survivable. This doc makes that the universal model.

## Three tiers — pick by what the resource loses under overlap

Most "leases" do not need true mutual exclusion. Sort by the cost of transient
double-ownership:

| Tier | Use when overlap causes… | Mechanism | Consensus? |
| --- | --- | --- | --- |
| **0 — advisory ownership** | a *detectable conflict*, not corruption | LWW owner record + conflict detection/preservation | none |
| **1 — fenced lease** | *corruption* (DB writer, volume, VM disk, roaming active node) | advisory grant + TTL + **monotonic fencing token enforced at the resource** | only at the serialization point, per-resource |
| **2 — quorum lease** | corruption *and* no resource-side enforcement point exists | small per-resource Raft group (3–5 nodes) | yes, per-resource, rare |

Tier 0 is the majority — portal occupancy (P5T-007), the conflict-sibling model,
and most orchestrator placement ("prefer one replica per node" is a preference,
not a safety property). It needs no fencing and no consensus: LWW picks an
owner, collisions are detected and surfaced. **Do not pay for consensus where a
conflict is survivable.**

Tier 1 is the workhorse for everything that genuinely corrupts. Tier 2 is the
escape hatch, paid per-resource, never as a standing global service — the
opposite of k8s routing everything through one etcd.

## Tier 1 in detail — the fenced lease

### The lease record (advisory, in the directory)

```text
Lease {
  resource_id:  bytes,        // what is being held
  holder:       node_id,      // current believed holder
  epoch:        u64,          // the fencing token — monotonic per resource
  granted_by:   node_id,      // the serialization authority (see below)
  expires_at:   u64,          // TTL deadline (liveness, not safety)
  state:        Held | Released | Revoked,
}
```

This is just a directory row: replicated for free, auditable for free, and its
revocation (`epoch++` + tombstone) is the *same* mechanism as trust-directory
role revocation. The record is **advisory** — it says who is *believed* to
hold the lease. It is never the safety boundary.

### The crux: where the monotonic counter is incremented

Two nodes racing to take over both pick `epoch = N+1`; epoch alone cannot break
the tie (the **same-epoch collision** — and why a pure-LWW epoch key is unsafe).
Safe increment requires a **serialization point**. The rule:

> **Do not build a separate lock service. Let the resource serialize itself via
> its own compare-and-swap, and co-locate epoch issuance with the data writes
> it protects.**

Concrete serialization points already on the substrate:

| Resource | Serialization point (the CAS) |
| --- | --- |
| Object storage (snapshots) | S3 conditional PUT / ETag — already used for the snapshot catalog |
| Portal Tree / volume | atomic compare-and-increment row in `portal-store` (the per-tree SQLite writer is already a single serializer) |
| Orchestrator singleton with no resource CAS | Tier 2 small quorum, or a deterministic designated grantor (resource "home" node, or root for simple deployments) |

**Co-locating lease serialization with data serialization closes the Kleppmann
gap.** If the authority that issues the epoch is the same authority that accepts
the writes, there is no window between "I hold the lease" and "my write lands."
Separate services reintroduce the gap; the same authority eliminates it. This is
the single most important implementation decision in this doc.

### Enforcement (the actual safety)

Every protected action carries the holder's epoch. The resource persists
`highest_epoch_seen` and **rejects any action with a lower epoch**. That integer
compare — not the grant — is what makes the system correct. It holds even if:

- the grant raced and two nodes briefly believed they held the lease (the lower
  epoch is rejected at first write);
- a partitioned ex-holder wakes and acts late (fenced);
- a clock was wrong and a TTL expired early or late (fenced).

Fencing converts every grant/timing imperfection from a *correctness* failure
into an *availability* failure (a wrongly-fenced node must re-acquire). That is
the trade you want.

## What the QUIC/iroh substrate provides (the "efficiently")

The P2P transport helps here rather than hinders:

- **A held QUIC connection is the heartbeat.** No separate liveness-gossip
  protocol — connection drop / path failure is fast, transport-level failure
  detection, far quicker than a gossip timeout. Lease renewal piggybacks on a
  connection that already exists.
- **Smoothed RTT bounds the clock-skew envelope.** TTL leases are unsafe under
  unbounded drift; iroh's per-path RTT gives a real delay bound. Use **monotonic
  clocks + measured RTT, never wall-clock**; fencing is the backstop if timing
  is violated anyway.
- **The directory carries the advisory grant + audit for free**, and lease
  revocation reuses the trust-directory revocation primitive (`epoch++` +
  tombstone) — one conceptual model for "withdraw authority" across leases,
  grants, and roles.
- **Graceful vs forced handoff falls out** (roaming already specifies it):
  graceful = holder releases, clean epoch bump, full continuity; forced = TTL
  expiry + fence + crash-consistent activation (run the app's normal crash
  recovery — Git lock cleanup, DB WAL replay).

## Lifecycle

```text
acquire:  candidate asks the serialization authority to issue epoch N+1
          (CAS: succeeds only if it advances the counter);
          writes the Lease record (holder=self, epoch=N+1) to the directory;
          opens/keeps a QUIC connection as its heartbeat.

hold:     every protected action is stamped epoch=N+1; resource enforces
          monotonicity. Renew TTL over the live connection before expires_at.

release:  graceful — write state=Released, bump epoch so no stale action
          from this holder is accepted; new candidate may acquire immediately.

expire:   connection lost / TTL passed with no renew — peers stop seeing the
          holder; a new candidate acquires epoch N+2 via the CAS. The old
          holder, if it returns, is fenced (its epoch N+1 < N+2 at the resource)
          and must re-acquire. Activation on the new holder is crash-consistent.

revoke:   authority writes a Revoked tombstone + bumps epoch — identical to
          trust-directory revocation; the old holder is fenced at the resource.
```

## Consumers

- **portal-sync** — the roaming exclusive-migrating lease is Tier 1 with the
  portal-store CAS as the serializer; per-tree single runtime owner (P5T-007)
  is Tier 0 (advisory). Snapshot capture/prune coordination uses the S3-ETag
  CAS (Tier 1 against the bucket).
- **orchestrator** — singleton workloads and stateful-failover primaries are
  Tier 1 (volume = portal Tree, so the portal-store CAS serializes); ordinary
  "one replica per node" placement is Tier 0. The fencing token is what makes
  fenced VM/DB failover safe over a NAT-spanning mesh without a SAN.
- **exec / shell** — long-running interactive sessions that must not double-run
  can use a Tier 1 lease keyed on the session resource; most exec jobs need
  none.

## Design work when this is built

- **Same-epoch collision** is the load-bearing correctness case — the CAS
  serialization point is non-negotiable for Tier 1; a pure-LWW epoch key is
  unsafe and must be rejected in review.
- **Clock discipline** — monotonic + measured-RTT bounds, fencing as the safety
  net so a clock violation costs availability, never integrity.
- **The single non-negotiable data-loss rule** (from roaming): a holder that
  vanishes with un-replicated local changes must return as an explicit
  **conflict to resolve**, never a silent loss and never a silent clobber of the
  now-active holder. Loud, always.
- **Tier 2 authority shape** — designated-primary vs small-majority for the
  per-resource quorum; only designed if a real resource needs it.

## The one-line version

> Grant cheaply (LWW directory record + TTL for liveness), enforce safely
> (monotonic fencing token the resource checks), serialize at the resource's
> own CAS (never a global lock service), and detect-don't-prevent wherever a
> conflict is survivable (Tier 0). The QUIC connection is the heartbeat, RTT is
> the timing bound, and fencing is the integer compare that makes an imperfect
> grant correct.
