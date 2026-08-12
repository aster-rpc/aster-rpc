# aster-leases — Single-Owner Leases On A P2P QUIC Mesh

**Status:** Design draft — revised 2026-07-04 after two review rounds (fence
binding, release/revoke as fence transitions, expiry ownership, Tier-2
correction, serializer state machine; then restart semantics, the
resource-side `FencedResource` contract, and the directory-row audit
downgrade). The state machine, fence rule, timing rules, and test matrix below
are intended as **normative once implemented**; defaults are chosen-and-tunable.
Shared primitive for portal-sync and the orchestrator.
User guide: `docs/aster-leases-getstarted.md`.

> **Core primitive shipped 2026-07-05 (Rust).** `core/src/lease.rs` +
> `aster::lease`: the normative state machine as a pure `transition()`
> function, fence rule (`check_fence`), `LeaseSerializer` trait with
> `MemorySerializer` (reference co-located backend + restart rule +
> authorizer hook + `mutate_fenced`), `FencedResource` contract, and the
> `LeaseHandle` engine (renew loop, send-time self-limit, fenced/lost/released
> watch). Test matrix rows 1–6, 8, 10 pass under a paused clock
> (`aster/tests/lease.rs`).
>
> **Grantor over RPC shipped 2026-07-05 (Rust).** `aster::rpc::lease`: the
> `aster.lease.Grantor` service (hand-dispatched so every op binds to the
> authenticated QUIC peer — the wire cannot impersonate a holder; `revoke`
> Gate-3 gated, `operator` role by default) + `GrantorSerializer`, a
> `LeaseSerializer` backend so `LeaseHandle` works unchanged against a
> remote grantor. Wire types `aster/Lease*` (Fory, one runtime per payload
> root); predicate denies travel as data, not RPC errors. Tested end-to-end
> incl. impersonated-renew refusal and the renew loop over the wire
> (`aster/tests/rpc_lease.rs`). **Remaining:** portal-store row + S3
> catalog backends, the advisory directory row + serializer-authored audit
> (rows 7, 9 of the matrix), and `aster.ops` metrics for fencing
> rejections.

**Related:**
- [trust-directory.md](trust-directory.md) — the advisory grant + audit substrate; lease *authority withdrawal* reuses its tombstone mechanism (but see Revocation: the fence is a separate, second step)
- [working_ideas/aster-orchestrator.md](working_ideas/aster-orchestrator.md) — "CP for leases, AP for everything else" stance; singleton placement and stateful failover consume this
- [aster-network-topology.md](aster-network-topology.md) — bridge election is eventually-agreed, explicitly *not* exclusive; a consumer that needs an exclusive bridge needs this primitive on top (its open question 7 lands here)
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
enforced at the resource**: a monotonic epoch the holder stamps on every
action, which the resource rejects unless it matches the current grant. A
paused or partitioned ex-holder that wakes and acts late has its stale writes
*rejected by the thing it writes to*. The lock service being wrong becomes
survivable. This doc makes that the universal model.

## Three tiers — pick by what the resource loses under overlap

Most "leases" do not need true mutual exclusion. Sort by the cost of transient
double-ownership:

| Tier | Use when overlap causes… | Mechanism | Consensus? |
| --- | --- | --- | --- |
| **0 — advisory ownership** | a *detectable conflict*, not corruption | LWW owner record + conflict detection/preservation | none |
| **1 — fenced lease** | *corruption*, and the resource has (or can be given) a single serialization point | advisory grant + TTL + **fence enforced at the resource** (state machine below) | only at the serialization point, per-resource |
| **2 — interposed quorum** | corruption *and* no resource-side enforcement point can exist | small per-resource quorum that **sits in the write path** (writes proxy through it — the quorum *becomes* the enforcement point) | yes, per-resource, rare |

Tier 0 is the majority — portal occupancy (P5T-007), the conflict-sibling model,
and most orchestrator placement ("prefer one replica per node" is a preference,
not a safety property). It needs no fencing and no consensus: LWW picks an
owner, collisions are detected and surfaced. **Do not pay for consensus where a
conflict is survivable.**

Tier 1 is the workhorse for everything that genuinely corrupts. Tier 2 is the
escape hatch, paid per-resource, never as a standing global service — the
opposite of k8s routing everything through one etcd.

**Tier 2 honesty (pinned).** A quorum that merely *grants* leases does not
protect a resource that cannot check a fence — a paused ex-holder still writes
late, which is this doc's own thesis turned against its own escape hatch. If
no enforcement point exists, the only correct moves are (a) interpose the
quorum on the data path so it *is* the enforcement point, or (b) accept the
residual pause-window risk explicitly, with conservative timing, as a
documented gamble. A "quorum grant + unfenced resource" hybrid is not a tier;
it must be rejected in review. A single **designated grantor** (resource
"home" node, or root for simple deployments) is a legitimate *serialization
point* for Tier 1 — but it gets no automatic failover: failing it over without
consensus reintroduces split-brain of the grantor itself. A dead grantor means
no new grants *and no renewals* — holders run to their self-limit and stop;
and if the data path itself flows through the grantor (rather than the
resource checking the fence), writes stop with it immediately. Availability
loss either way, corruption never — until an operator (or the root) moves the
grantor.

## Ruled out

- **A standing global lock service.** Reintroduces the grant/write gap
  Kleppmann identified, plus a SPOF the mesh doesn't otherwise have.
- **Pure-LWW epoch key.** Two racing candidates both pick `N+1`; LWW cannot
  break the tie (the *same-epoch collision*). Rejected in review, always.
- **Bare-integer bearer fence.** The epoch is replicated to every admitted
  reader via the directory — an unbound integer is trivially stampable by any
  member. The fence must bind the holder (see fence rule).
- **Wall-clock expiry as a safety input.** TTLs are liveness only; expiry
  decisions belong to the serializer's clock, and fencing backstops every
  timing violation (see timing rules).

## Tier 1 in detail

### Two records, one authority

There are **two** lease records with different jobs. Conflating them is the
root error this design exists to prevent:

1. **The serializer row** — lives at the resource's serialization point
   (SQLite row, S3 catalog object, grantor's store). **Authoritative.** Every
   state transition below is a CAS on this row. It is the fence.
2. **The directory record** — an iroh-docs row. **Best-effort observability
   only.** Written by the holder *after* a successful CAS, replicated for
   free, consumed for routing hints and dashboards. Readers may use it to find
   the *believed* holder; nothing may gate a write on it — and nothing may
   treat it as audit either: a holder can crash before writing it, and a lying
   holder can write anything. The authoritative audit trail is
   serializer-authored (see Observability).

```text
SerializerRow {                       # authoritative — the fence
  resource_id:  bytes
  epoch:        u64                   # monotonic, never reused; the incarnation id
  holder:       bytes(32) | none      # Ed25519 NodeId of current holder
  state:        Held | Free
  granted_at:   expiry anchor         # VOLATILE timing state — see restart rule
  ttl_ms:       u32
}
# Durable fields: resource_id, epoch, holder, state, ttl_ms. granted_at is
# volatile — monotonic instants must never be persisted or compared across a
# boot (restart rule under timing).

Lease {                               # advisory — directory row, hint only
  resource_id:  bytes
  holder:       bytes(32)
  epoch:        u64
  granted_by:   bytes(32)             # which serializer issued it
  expires_at:   int64                 # wall-clock, display/audit ONLY
  state:        Held | Released | Revoked
}
```

Divergence between the two is normal (AP directory) and must be harmless by
construction: the test matrix requires that a stale or LWW-raced directory row
can never confer write access.

### The fence rule (normative)

Every protected action carries `(resource_id, epoch)`; the transport
authenticates the writer (the QUIC handshake already proves NodeId — same
trust class as topology's "verified" tier). The resource accepts an action
**iff**:

```text
epoch == row.epoch  ∧  writer == row.holder  ∧  row.state == Held
```

Three rejections, all load-bearing:

- `epoch < row.epoch` — stale holder (paused, partitioned, pre-release). The
  classic fence.
- `epoch == row.epoch ∧ writer ≠ row.holder` — replay/impersonation. The
  epoch is visible to every admitted directory reader, so a bare integer is a
  bearer token; **binding the authenticated writer to the holder is what makes
  it not one.**
- `row.state ≠ Held` — the equal-epoch-after-release case (see release).

`epoch > row.epoch` is impossible under a single serialization point; seeing
it is a serializer-integrity alarm, never something to accept.

**Fence durability (normative).** The fence check and the protected write
must be atomic-or-ordered: co-located serializers do both in one transaction
(the portal-store SQLite writer); remote serializers embed the fence *inside*
the single mutation point (the epoch lives in the S3 catalog object and moves
only via the same conditional PUT). A resource restart must never regress the
fence — if the fence state could be lost while a write survived, the fence is
not a fence.

**Single-mutation-point rule (normative).** Fencing is per enforcement point.
A resource spanning many objects can end in mixed-epoch state: holder A (N)
writes object 1, is fenced at object 2 where B (N+1) already landed — no lost
update, but partial application. Therefore a Tier-1 resource has **exactly one
fenced mutation point**; every other write is immutable / content-addressed
and becomes visible only via the fenced pointer swap. The snapshot design
already has this shape (content-addressed blobs + catalog CAS) — the rule
makes it a requirement, not a coincidence.

### The crux: co-locate issuance with the data it protects

Two nodes racing to take over both pick `epoch = N+1`; epoch alone cannot
break the tie. Safe increment requires a serialization point, and the rule is:

> **Do not build a separate lock service. Let the resource serialize itself
> via its own compare-and-swap, and co-locate epoch issuance with the data
> writes it protects.**

If the authority that issues the epoch is the same authority that accepts the
writes, there is no window between "I hold the lease" and "my write lands."
Separate services reintroduce the gap; the same authority eliminates it. This
is the single most important implementation decision in this doc.

Concrete serialization points already on the substrate:

| Resource | Serialization point (the CAS) | Enforcement honesty |
| --- | --- | --- |
| Portal Tree / volume | lease row in `portal-store` (the per-tree SQLite writer is already a single serializer) | Full: CAS + fence check + data write share one transaction; writer NodeId authenticated by the Aster stream. Any path that bypasses the portal writer (direct file access) is unfenced by definition. |
| Object storage (snapshots) | S3 conditional PUT (`If-Match` on the catalog object; epoch embedded in it) | **Cooperative only.** The bucket checks ETags, not identities — anyone holding the bucket credentials can write. Fencing here protects against *races and stale holders among cooperating nodes*, not against a malicious credential holder; IAM is the actual trust boundary. |
| Singleton with no resource CAS | designated grantor (resource "home" node / root), or Tier 2 interposed quorum | Grantor: full, if writes flow through it or the resource checks the fence; no auto-failover (see Tier 2 honesty). |

### Serializer state machine (normative)

Every transition is **one CAS** on `SerializerRow`; the predicate is evaluated
against the row the CAS read (`expired(row)` = serializer-local time since
`granted_at` exceeds `ttl_ms` — see timing rules for who holds the clock on
dumb-CAS backends):

```text
ACQUIRE(candidate):
  allowed  iff  row.state == Free  ∨  expired(row)
  effect        epoch += 1; holder = candidate; state = Held; granted_at = now
  # takeover-after-expiry IS acquire — there is no separate "steal" verb

RENEW(caller, epoch):
  allowed  iff  row.state == Held  ∧  epoch == row.epoch
                ∧  caller == row.holder  ∧  ¬expired(row)
  effect        granted_at = now
  # renewal goes to the SERIALIZER — refreshing the directory row renews nothing

RELEASE(caller, epoch):
  allowed  iff  epoch == row.epoch  ∧  caller == row.holder
  effect        epoch += 1; holder = none; state = Free
  # the epoch bump is the point: without it, the releasing holder's still-in-
  # flight writes at epoch N pass an "reject lower" check (N < N is false).
  # Advancing the fence at release is what makes release safe, not the
  # directory write.

REVOKE(authority):
  allowed  iff  caller holds revoke authority for resource_id
  effect        epoch += 1; holder = none; state = Free
  # two steps, both required, in this order:
  #   1. serializer CAS (above)      — advances the FENCE; the old holder is
  #                                    rejected at the resource from here on
  #   2. directory tombstone + row   — withdraws AUTHORITY and records audit,
  #                                    trust-directory mechanism, sync-speed
  # A directory tombstone alone revokes nothing: the resource never reads the
  # directory.
```

A failed CAS is information, not an error to retry blindly: the row that came
back says who holds the lease and until when. Candidates back off accordingly
(acquisition policy below).

### Acquisition policy — liveness, separated from safety

The state machine makes overlap *safe*; policy makes ownership *stable*.
Without it two eager candidates ping-pong ACQUIREs forever — 100% safe, 0%
available. Normative policy:

- **No live-steal.** `ACQUIRE` is only legal against `Free ∨ expired` — the
  predicate enforces this at the serializer; candidates must additionally not
  *attempt* while they can see the holder alive (an open QUIC connection to
  it, or a fresh directory heartbeat) — attempts against a live holder are
  policy violations even though they lose the CAS anyway.
- **Backoff on lost CAS.** Jittered exponential (`acquire_backoff`), reset on
  a successful acquire. Two candidates converge on one winner in one round in
  the common case.
- **Authorization.** The serializer accepts `ACQUIRE`/`RENEW` only from nodes
  holding the lease role for the resource (a trust-directory grant — sealed
  grants distribute it like any capability), and `REVOKE` only from the
  granting authority. Without this, any admitted node can burn epochs — no
  corruption, but a trivial availability attack (perpetual fencing of the real
  holder). On the S3 path, IAM credentials *are* this boundary.

### Timing rules (normative)

`expires_at` in a replicated record is display only. Expiry that gates a
takeover is decided as follows:

- **Live serializer (portal-store, grantor): the serializer's local clock
  owns expiry.** It granted the lease; it measures the interval; monotonic
  instants that never leave the machine. No cross-node clock comparison
  exists in this path at all.
- **Holder self-limit.** The holder computes
  `valid_until = monotonic_at(request_send) + ttl − holder_margin` after each
  successful ACQUIRE/RENEW and **stops protected actions past it** — the
  Chubby discipline. Counting from *send* (not receive) makes the estimate
  conservative regardless of one-way delay; `holder_margin` absorbs RTT and a
  processing allowance. RTT is a *margin input*, not a skew bound — smoothed
  RTT (free on the live QUIC connection) does not bound clock skew and is
  never used as if it did.
- **Dumb-CAS serializer (S3): the candidate evaluates expiry**, wall-clock
  against a `granted_at_unix_ms` stored in the catalog object, and may only
  take over after `ttl + steal_grace` — the grace absorbs skew (nodes are
  NTP-synced per the topology stance; `max_clock_skew` is shared config). This
  path is honestly worse: a fast clock steals early. The fence converts that
  from corruption into an availability hit on the fenced holder — which is the
  entire design trade — and `steal_grace` bounds how often it happens.
- **Renewal cadence**: renew every `renew_interval` (≈ ttl/3), over the
  already-open connection to the serializer. Multiple missed renewals fit
  inside one TTL, so a single dropped packet never costs the lease.
- **Serializer restart (pinned).** Monotonic instants do not survive a boot
  and must never be persisted or compared across one. On restart the
  serializer recovers the durable fence fields (`epoch`, `holder`, `state`,
  `ttl_ms`) and **re-arms expiry with a full fresh ttl** (`granted_at = now`).
  This is conservative toward the holder: a live holder simply renews on its
  next cycle; a dead holder's takeover is delayed by at most one extra ttl;
  and the holder's self-limit bounds it independently throughout, so the
  serializer forgetting elapsed time costs availability, never safety. The
  fence is **never advanced on restart** — bumping would fence a live holder
  for no reason — and the recovered epoch/holder/state must reject pre-crash
  stale writes exactly as before (test 6). The dumb-CAS path is unaffected:
  its `granted_at_unix_ms` is wall-clock inside the catalog object and has no
  process to restart.

### What the QUIC/iroh substrate provides

- **The holder↔serializer connection is the heartbeat that matters.** Its
  drop is fast transport-level failure detection, far quicker than a gossip
  timeout, and renewal piggybacks on it. Connections to *other* peers are
  hints only: P2P visibility is not transitive — under partial partition,
  candidates can disagree about holder liveness, and the fence (not their
  agreement) is what keeps that safe. On S3 there is no connection to the
  serializer; TTL is the only liveness signal, as the timing rules say.
- **Authenticated writers for free.** The fence rule's holder binding costs
  nothing on Aster streams — the handshake already proves the NodeId.
- **The directory carries the advisory record and the serializer-authored
  audit for free**, and lease *authority* withdrawal reuses the
  trust-directory tombstone — one conceptual model for "withdraw authority"
  across leases, grants, and roles. The fence advance is always the separate,
  resource-side step.
- **Graceful vs forced handoff falls out** (roaming already specifies it):
  graceful = RELEASE (clean fence advance, full continuity); forced = expiry
  takeover + crash-consistent activation (run the app's normal crash
  recovery — Git lock cleanup, DB WAL replay).

### Observability (normative)

**A fencing rejection is never routine.** Every one means a race, a liveness
misconfiguration, or an attempted replay — silently retrying would hide all
three behind the safety net until an outage surfaces them. Requirements:

- fencing rejections emit a loud audit event + metric (surfaced via
  `aster.ops`), tagged with resource, epochs (carried vs current), and writer;
- **the authoritative audit trail is serializer-authored**: the serializer
  records every transition (acquire/renew/release/revoke/expiry-takeover,
  plus fencing rejections) at its own store — a transitions table next to the
  lease row in portal-store, the grantor's log — and, where the serializer is
  a node, republishes them to the directory under its *own* author key. The
  holder-written `Lease` row is a hint that may lag, lie, or be absent (a
  holder can crash between the CAS and the directory write); nothing audits
  from it;
- **the S3 path has no serializer author** — its audit is best-effort,
  participant-written, and honestly labelled as such; the catalog object's
  own version history is the closest thing to ground truth there;
- an `epoch > row.epoch` observation is a serializer-integrity alarm, page-worthy.

## Defaults (chosen-and-tunable, provisional until an implementation pass)

| Constant | Default | Meaning |
| --- | --- | --- |
| `lease_ttl` | 30 s | serializer-side expiry interval |
| `renew_interval` | 10 s | holder renew cadence (ttl/3 → 2 missed renews survivable) |
| `holder_margin` | max(2 × sRTT, 1 s) | subtracted from ttl for the holder's self-limit |
| `steal_grace` | 30 s | extra wait beyond ttl before dumb-CAS (S3) takeover |
| `acquire_backoff` | 1–30 s jittered exp | after a lost ACQUIRE CAS |
| `max_clock_skew` | 30 s | shared with topology; NTP assumed, violators self-exclude |

## Lifecycle (revised)

```text
acquire:  candidate CASes ACQUIRE at the serializer (legal only on Free ∨
          expired) → epoch N+1 bound to its NodeId; writes the advisory
          Lease row (best-effort); computes its self-limit; keeps the
          serializer connection open where one exists.

hold:     every protected action stamped (resource_id, N+1) over an
          authenticated stream; resource enforces the fence rule. RENEW at
          the serializer every renew_interval; stop acting at the
          self-limit if renewal fails.

release:  RELEASE CAS — advances the fence to N+2 with no holder, so the
          releaser's own in-flight epoch-N+1 writes are rejected; then the
          advisory row flips to Released. Next candidate acquires
          immediately.

expire:   no renewal within ttl (serializer clock) — next ACQUIRE succeeds
          at N+2. The old holder, if it returns, fails the fence rule and
          must re-acquire. Activation on the new holder is
          crash-consistent.

revoke:   authority CASes REVOKE at the serializer (fence advances — the
          actual safety), then writes the directory tombstone (authority
          withdrawal + audit — the trust-directory mechanism).
```

## Surface sketch

Core (Rust) owns the primitive; bindings follow the usual wave:

```text
aster::lease
  trait Serializer            # load(resource) -> Row; cas(expected, next) -> CasResult
    PortalStoreSerializer     # SQLite row, one txn with the data write
    S3CatalogSerializer       # If-Match conditional PUT, epoch in the object
    GrantorSerializer         # designated-node authority (Aster RPC)
  LeaseHandle                 # renew loop, self-limit, fence(), watch for fenced/released
  Fence { resource_id, epoch, holder }   # stamp on every protected action

  trait FencedResource        # the resource-side half — MANDATORY for Tier 1
    mutate(fence, op) -> Result<_, Fenced>
    # evaluates the fence rule INSIDE the resource's own serialization point
    # (same SQLite transaction in portal-store; the same conditional PUT on S3)
```

**The library grants; only resources enforce (normative).** `aster::lease`
issues, renews, and watches leases — by itself it makes nothing safe, and
shipping the grant half without the resource half quietly recreates the
separate lock service this doc rules out. A resource qualifies as Tier 1 only
when its mutation API accepts a `Fence` and evaluates the fence rule inside
its own serialization point (`FencedResource`, or the equivalent transaction
hook). A consumer holding a `LeaseHandle` but writing through an unfenced API
is Tier 0 with extra steps, and review must treat it as such.

## Consumers

- **portal-sync** — the roaming exclusive-migrating lease is Tier 1 with the
  portal-store CAS as the serializer; per-tree single runtime owner (P5T-007)
  is Tier 0 (advisory). Snapshot capture/prune coordination uses the S3
  catalog CAS (Tier 1, cooperative-enforcement caveat noted above).
- **orchestrator** — singleton workloads and stateful-failover primaries are
  Tier 1 (volume = portal Tree, so the portal-store CAS serializes); ordinary
  "one replica per node" placement is Tier 0. The fence is what makes VM/DB
  failover safe over a NAT-spanning mesh without a SAN — with the explicit
  caveat that "the resource" is the portal write path; a bypass path is
  unfenced.
- **topology bridge exclusivity** — a consumer that needs the elected bridge
  to be *exclusive* (single cross-cluster writer) keys a Tier 1 lease on the
  cluster resource; the eventually-agreed election picks the *candidate*, the
  lease makes it *exclusive*. Idempotent bridge work (replication sync)
  correctly stays Tier 0/none.
- **exec / shell** — long-running interactive sessions that must not
  double-run can use a Tier 1 lease keyed on the session resource; most exec
  jobs need none.

## Test matrix (normative when this is built)

| # | Scenario | Expected |
| --- | --- | --- |
| 1 | Same-epoch collision: two candidates race ACQUIRE | exactly one CAS wins; loser sees the new row, backs off |
| 2 | Equal-epoch after release: releaser's in-flight write lands post-RELEASE | rejected — fence advanced at release |
| 3 | Equal-epoch replay: admitted non-holder stamps the current epoch | rejected — holder binding (writer ≠ holder) |
| 4 | Paused-holder resurrection: holder pauses past ttl, successor acquires, holder wakes and writes | rejected; holder must re-acquire |
| 5 | Renew/takeover race at expiry boundary | CASes serialize; exactly one outcome; loser fenced or backed off |
| 6 | Serializer/resource crash + recovery | durable fence fields survive; expiry re-armed to a full ttl (never advanced); stale old-epoch write still rejected after restart, current holder's same-epoch write still accepted |
| 7 | Directory divergence: stale/LWW-raced advisory row disagrees with serializer | advisory row confers nothing; serializer wins; consumers using it as a hint mis-route at worst |
| 8 | Ping-pong: two maximally eager candidates | backoff bounds churn; one stable holder emerges |
| 9 | Fast clock on dumb-CAS path: candidate steals at ttl + steal_grace with skewed clock | live holder fenced (availability hit, loud metric); zero corruption |
| 10 | Unauthorized ACQUIRE (no lease role) | refused at the serializer; audit event |

Plus the single non-negotiable data-loss rule (from roaming): a holder that
vanishes with un-replicated local changes must return as an explicit
**conflict to resolve**, never a silent loss and never a silent clobber of the
now-active holder. Loud, always.

## Open questions

1. **Tier 2 interposed-quorum shape.** Designed only when a real resource
   needs it; the pinned constraint is that the quorum must sit in the write
   path (Tier 2 honesty above), which likely makes it a proxy, not a grantor.
2. **Lease-role naming and granularity** — per-resource role vs per-resource-
   class, and whether the sealed-grant `role` field carries it directly.
3. **Directory heartbeat cadence** — whether renewals also refresh the
   advisory row (nice for dashboards, more doc churn) or only transitions do.
4. **Defaults pass** — the table is rules-of-thumb; validate ttl/backoff
   against real portal-store and S3 latencies before declaring final.

### Decided (was open, now pinned)

- Fence = `(resource_id, epoch, holder)` with the writer authenticated at the
  transport; epoch is never reused, so it doubles as the incarnation id — no
  separate `lease_id`.
- Release and revoke advance the fence at the serializer; a directory
  tombstone alone revokes nothing.
- Expiry ownership: live serializer's local clock; holder self-limits from
  send-time; candidate-evaluated wall-clock (+ `steal_grace`) only on
  dumb-CAS backends.
- One fenced mutation point per resource; all other writes immutable.
- Takeover is ACQUIRE under the expired predicate — no separate steal verb.
- Serializer restart: durable fence fields recover, expiry re-arms to a full
  ttl, the fence never advances on restart; monotonic instants are never
  persisted or compared across boots.
- The library grants; only resources enforce — Tier 1 requires the resource's
  own fenced mutation API (`FencedResource`); a grant API alone is Tier 0.
- Directory lease rows are best-effort hints; authoritative audit is
  serializer-authored (participant-written best-effort on the S3 path).

## The one-line version

> Grant cheaply (advisory directory record + TTL for liveness), enforce safely
> (a fence the resource checks: current epoch **and** authenticated holder
> **and** Held), serialize at the resource's own CAS (never a global lock
> service) — and advance that fence on every release and revoke, not just on
> takeover. Detect-don't-prevent wherever a conflict is survivable (Tier 0);
> interpose a quorum only where nothing can check a fence (Tier 2). The
> holder↔serializer connection is the heartbeat, the serializer's clock is
> the expiry authority, and fencing is the check that makes an imperfect
> grant correct.
