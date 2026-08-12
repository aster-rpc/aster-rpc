# Leases: single-owner coordination with `aster::lease`

Sometimes exactly one node may act: one writer for a database, one owner of
a volume, one live instance of a singleton workload. On a P2P mesh there is
no central lock server to ask — and Aster deliberately doesn't add one.
Instead, `aster::lease` gives you **fenced leases**: a cheap, imperfect
*grant* (who currently holds the lease) backed by a *fence* the resource
itself checks on every write. Even when the grant goes wrong — a paused
process, a partition, a clock hiccup — the stale holder's writes are
**rejected by the thing it writes to**. Grant errors cost availability,
never corruption.

> **Status: core primitive + RPC grantor shipped (Rust, 2026-07-05).** The
> state machine, fence rule, holder engine (§3–§4), and the
> `aster.lease.Grantor` service (§5) are implemented in the Rust `aster`
> crate; other bindings follow the FFI wave. Still to come
> ([design doc](_internal/aster-leases.md)): portal-store and S3-catalog
> serializer backends, the advisory directory row (lease visibility in the
> shared doc), and `aster.ops` metrics for fencing rejections.

---

## 1. What you get

Three pieces, and you need the first two — granting without enforcing is
not a lease, it's a suggestion:

1. **A lease handle** — [`LeaseHandle::acquire`] takes the lease, renews it
   in the background over a live connection, and hands out the current
   **fence** until the moment the holder must stop. One winner per
   resource, guaranteed by a compare-and-swap at a single serialization
   point.

2. **The fence** — a `(resource, epoch, holder)` token stamped on every
   protected action. Your resource checks it *inside its own write path*:
   stale epoch → rejected; right epoch but wrong (authenticated) writer →
   rejected; released or revoked → rejected. That integer-and-identity
   check is the actual safety.

3. **A grantor service** — `aster.lease.Grantor`, for resources that don't
   have their own CAS to serialize on. Run it on one designated node; every
   other node acquires through it with `GrantorSerializer`, and all lease
   operations are bound to the caller's **authenticated QUIC identity** —
   the wire cannot impersonate a holder.

## 2. The mental model

First question, always: **what breaks if two nodes briefly both act?**

| If overlap causes… | Use | Cost |
|---|---|---|
| a conflict you can detect and resolve later | no lease — detect, don't prevent | none |
| corruption (double-writer on a DB, volume, catalog) | a **fenced lease** (this guide) | one CAS per grant/renew |
| corruption, and nothing can check a fence | don't ship it — see the design doc's Tier 2 | consensus, per-resource |

Most "locks" are the first row. Portal's "one runtime owner per tree
preference", replica placement, dashboards — a transient collision there is
survivable, so pay nothing. Reach for a lease only when overlap corrupts.

Two rules worth internalising:

- **The grant is a hint; the fence is the safety.** Nothing may gate a
  write on "I believe I hold the lease" — beliefs go stale. Writes are
  gated by the resource checking the fence at write time.
- **The library grants; only *your resource* enforces.** A `LeaseHandle`
  over an unfenced write API provides no protection at all. Every
  protected mutation path must check the fence inside its own
  serialization point (§4).

## 3. Acquiring and holding a lease

The serializer is where grants linearize. For a resource that lives on the
same node (or in tests), the in-memory reference serializer is enough;
remote grantors arrive in §5:

```rust
use std::sync::Arc;
use aster::lease::{AcquireOutcome, LeaseHandle, LeaseOptions, MemorySerializer};

let serializer = Arc::new(MemorySerializer::new());
let me = node.id().to_string();

match LeaseHandle::acquire(serializer.clone(), "tree/42", &me,
    LeaseOptions::default()).await?
{
    AcquireOutcome::Granted(lease) => {
        // Hold: stamp the fence on every protected action, AT SEND TIME.
        while let Some(fence) = lease.fence() {
            do_protected_work(&fence).await?;
        }
        // fence() returned None: we must stop. Either we released, we were
        // fenced (someone else holds a newer epoch), or renewals failed and
        // the self-limit passed. lease.status() says which.
    }
    AcquireOutcome::Denied(reason, snapshot) => {
        // Someone else holds it. snapshot.remaining hints when to retry;
        // back off (jittered exponential) — never hammer the serializer.
    }
}
```

What the handle does for you:

- **Renews in the background** (default: every 10s against a 30s TTL, so
  two lost renewals cost nothing).
- **Stops you in time.** `fence()` returns `None` once the *self-limit*
  passes — a conservative deadline computed from when the last renewal was
  *sent*, minus a margin. If the serializer becomes unreachable, you stop
  acting **before** anyone else can be granted the lease. There is no
  instant where two nodes can both get a fence for the same epoch window.
- **Tells you why it ended.** `lease.watch()` is a watch channel over
  `Held / Released / Fenced / Lost`. `Fenced` means stop everything and
  re-acquire; `Lost` means the serializer was unreachable past the
  self-limit.

Release when you're done — it's not optional politeness:

```rust
lease.release().await?;
```

A graceful release **advances the fence**, so even your own still-in-flight
writes from the old epoch are rejected, and the next candidate can acquire
immediately instead of waiting out your TTL. Dropping the handle without
releasing just stops renewals: the lease lapses at the serializer after one
TTL.

Two things to *never* do with a fence:

- **Don't cache it across awaits.** Take `lease.fence()` immediately before
  each protected action. A fence fetched earlier may belong to a lease you
  since lost.
- **Don't ship it to another node to act for you.** The fence is bound to
  the holder's authenticated identity at enforcement; a forwarded fence is
  rejected as an impersonation.

## 4. Enforcing at your resource

This is the half that makes it safe. Your resource keeps the current lease
row and rejects any action that isn't *current epoch, from the current
holder, while held* — evaluated **inside** the same lock/transaction as the
write:

```rust
use aster::lease::{check_fence, Fence, FenceRejection, RowState};

// Inside your resource's own serialization point (its mutex, its SQLite
// transaction, its conditional PUT) — never against a snapshot:
fn apply_write(row: &RowState, fence: &Fence, writer: &str, w: Write) -> Result<(), FenceRejection> {
    check_fence(row, fence, writer)?;   // writer = authenticated peer id
    perform(w);                          // same critical section
    Ok(())
}
```

For in-process resources, `MemorySerializer::mutate_fenced` is exactly this
pattern, pre-built:

```rust
serializer.mutate_fenced(&fence, &authenticated_writer, || {
    // runs only if the fence rule passed, under the same lock
    state.apply(write)
})?;
```

Structure your resource so there is **one fenced mutation point** — a
single pointer/catalog/row that moves under the fence — and make every
other write immutable (content-addressed data the pointer swap makes
visible). Fencing is per enforcement point; scattering mutable writes
across many objects lets a fenced-out holder leave partial state behind.

And treat every `FenceRejection` as an event, not an error to retry:
a rejection means a race was survived, a liveness setting is wrong, or
someone attempted a replay. Log it loudly; a silently-retried rejection
hides real problems behind the safety net.

## 5. A designated grantor over RPC

When the resource has no natural CAS of its own, pick one node — the
resource's home node, or your root — and run the grantor there:

```rust
use std::sync::Arc;
use aster::lease::MemorySerializer;
use aster::rpc::lease::LeaseGrantor;

let store = MemorySerializer::new();          // the grantor's lease rows
let server = aster::rpc::AsterServer::builder()
    .config(config)
    .service(LeaseGrantor::new(Arc::new(store.clone())))
    .start()
    .await?;
// Keep `store` if this node also hosts the protected resource:
// store.mutate_fenced(...) is the co-located enforcement point.
```

Any node then acquires through it — the same `LeaseHandle`, a different
serializer:

```rust
use aster::lease::{AcquireOutcome, LeaseHandle, LeaseOptions};
use aster::rpc::lease::GrantorSerializer;

let conn = node.rpc_connect(&grantor_id).await?;
let me = node.id().to_string();
let serializer = Arc::new(GrantorSerializer::new(conn, me.clone()));

let outcome = LeaseHandle::acquire(serializer, "db/primary", &me,
    LeaseOptions::default()).await?;
```

Properties worth knowing:

- **Identity is bound by the transport.** The grantor takes the holder for
  every acquire/renew/release from the QUIC handshake, not from the
  request. Another node replaying your epoch, or claiming to be you, is
  refused server-side (`NotHolder`) — this is tested, not aspirational.
- **`revoke` is gated.** By default it requires the `operator` role
  (Gate 3); override with `LeaseGrantor::revoke_requires`, and gate the
  whole service with `.requires(...)` if any admitted caller shouldn't be
  able to contend for leases (epoch-burning is an availability nuisance
  even though it can't corrupt).
- **A dead grantor stops grants and renewals — nothing else.** Holders run
  to their self-limit and stop; nobody new can acquire until you move the
  grantor. That's deliberate: automatic grantor failover without consensus
  would split-brain the grantor itself. Availability loss, never
  corruption.

## 6. What to rely on — and what not to

**Rely on:**

- **Exactly one winner per epoch.** Concurrent acquires race a single CAS;
  the loser gets `Denied` with the row (holder, remaining TTL) for backoff.
- **The fence surviving everything.** Release, revoke, takeover, serializer
  restart — in every case the stale epoch is rejected at the resource, and
  the current holder's fence keeps working across a serializer restart.
- **The self-limit.** A holder that can't reach the serializer stops acting
  before any successor can be granted the lease.

**Do not rely on:**

- **Snapshots or directory rows as permission.** `snapshot()` (and the
  future directory record) may be stale the moment you read them. They're
  for dashboards, routing hints, and backoff — never for gating a write.
- **TTLs as safety.** Expiry timing decides *availability* (when a takeover
  may happen). Safety is only the fence check. If a clock is wrong, the
  worst case is a live holder gets fenced and must re-acquire.
- **Revoking via the trust directory alone.** Withdrawing authority
  (tombstone) does not stop in-flight writes — only the grantor/serializer
  revoke advances the fence. Do both, fence first.
- **A lease making an unfenced API safe.** If any write path skips the
  fence check, that path is unprotected, full stop.

## 7. Tuning

Defaults suit "seconds-scale failover, negligible traffic". The knobs:

| Knob | Default | When to change |
|---|---|---|
| `ttl` | 30 s | Faster takeover after a crash (lower) vs tolerance for slow networks/GC pauses (raise) |
| `renew_interval` | 10 s | Keep at ~ttl/3 so multiple lost renewals fit inside one TTL |
| `holder_margin` | 1 s | Raise on high-RTT paths — it's the gap between "stop acting" and "takeover possible" |

All three are per-acquire (`LeaseOptions`), so different resources can run
different tempos against the same grantor.

## 8. Roadmap

- **Core primitive + RPC grantor — shipped (Rust).** Normative state
  machine, fence rule, `MemorySerializer` (restart-safe, authorizer hook),
  `LeaseHandle` engine, `aster.lease.Grantor` + `GrantorSerializer` with
  transport-bound identity. Design-doc test matrix rows 1–6, 8, 10 plus
  wire tests pass.
- **Next:** portal-store (SQLite row) and S3-catalog serializer backends,
  the advisory directory row + serializer-authored audit trail, fencing-
  rejection metrics in `aster.ops`. Python/TS/Java surfaces follow the FFI
  wave.

Design rationale, the full state machine, timing rules, and the test
matrix: [`docs/_internal/aster-leases.md`](_internal/aster-leases.md).
