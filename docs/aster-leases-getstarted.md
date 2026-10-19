# Leases: single-owner coordination with `aster::lease`

Use a fenced lease when a resource needs one current writer, such as a database,
volume or singleton workload. `aster::lease` combines a grant identifying the
holder with a fence checked inside the resource's write path. A stale holder's
writes are rejected only when every protected mutation enforces that fence.

The lease state machine, holder engine and `aster.lease.Grantor` RPC service
ship in the Rust crate. Python, TypeScript and C-ABI bindings expose lease
primitives and foreign serializer hooks. For a replicated grantor, use the
[coordination client](aster-coordination-getstarted.md).

---

## 1. What you get

The API has three pieces. A protected resource needs both the handle and
fence enforcement:

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

## 2. Choose when to use a lease

Choose according to the effect of two nodes acting at once.

| If overlap causes… | Use | Cost |
|---|---|---|
| a conflict you can detect and resolve later | detect and resolve the conflict | none |
| corruption (double-writer on a DB, volume, catalog) | a **fenced lease** (this guide) | one CAS per grant/renew |
| corruption, and nothing can check a fence | redesign the write path or use resource-specific consensus | consensus, per-resource |

Advisory placement and dashboards can often tolerate transient overlap. Use a
fenced lease when overlapping actions would corrupt protected state.

Enforce both of these rules:

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

Release the lease when the protected work is complete:

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
  refused server-side (`NotHolder`); the RPC tests cover this identity binding.
- **`revoke` is gated.** By default it requires the `operator` role
  (Gate 3); override with `LeaseGrantor::revoke_requires`, and gate the
  whole service with `.requires(...)` if any admitted caller shouldn't be
  able to contend for leases (epoch-burning is an availability nuisance
  even though it can't corrupt).
- **A dead grantor stops grants and renewals — nothing else.** Holders run
  to their self-limit and stop; nobody new can acquire until you move the
  grantor. Automatic grantor failover requires consensus to preserve a single
  serialization point; use coordination for replicated grants.

The Go binding has the same pair over the C ABI: a server serves the grantor
over a serializer written in Go, and clients acquire through it
(`bindings/go/examples/lease_postgres` runs a Postgres serializer this way):

```go
srv, err := aster.StartServer(ctx, aster.ServerOptions{
    Config:       cfg,
    LeaseGrantor: &aster.LeaseGrantor{Serializer: &PGSerializer{DB: db}}, // nil = in memory
})

leases, err := client.LeaseGrantor(ctx, grantorAddr)
me, _ := client.ID()
lease, err := leases.Acquire(ctx, "db/primary", me, aster.LeaseOptions{})
```

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

- **Snapshots or directory rows as permission.** `snapshot()` may already be stale
  when it returns. Use it for dashboards, routing hints and backoff; check the
  fence inside each write.
- **TTLs as safety.** Expiry timing decides *availability* (when a takeover
  may happen). Safety is only the fence check. If a clock is wrong, the
  worst case is a live holder gets fenced and must re-acquire.
- **Revoking via the trust directory alone.** Withdrawing authority
  (tombstone) does not stop in-flight writes — only the grantor/serializer
  revoke advances the fence. Do both, fence first.
- **A lease making an unfenced API safe.** If any write path skips the
  fence check, that path is unprotected.

## 7. Tuning

These per-acquire options control renewal and takeover timing:

| Knob | Default | When to change |
|---|---|---|
| `ttl` | 30 s | Faster takeover after a crash (lower) vs tolerance for slow networks/GC pauses (raise) |
| `renew_interval` | 10 s | Keep at ~ttl/3 so multiple lost renewals fit inside one TTL |
| `holder_margin` | 1 s | Raise on high-RTT paths — it's the gap between "stop acting" and "takeover possible" |

All three are per-acquire (`LeaseOptions`), so different resources can run
different tempos against the same grantor.

## 8. Sizing leases for production

Fencing keeps a stalled holder safe: whatever the cause of the stall, its
stale epoch is rejected at the resource. This section is about availability,
meaning how often a healthy holder loses a lease it still wants.

**Set `ttl` to at least 3× the worst stall you expect.** A stall is any
period where the holder cannot run its renew loop: CPU throttling, a GC
pause, node pressure, a frozen or migrating VM. Renewal runs every
`renew_interval`, and `fence()` returns `None` once the self-limit
(`ttl - holder_margin`, counted from when the last renewal was sent) passes.
A stall that outlasts it costs you the lease, and the next renewal is
refused. With the defaults (30 s `ttl`, 1 s `holder_margin`) that tolerates
stalls of a few seconds; if your worst case is 20 s, use at least 60 s:

```rust
LeaseOptions {
    ttl: Duration::from_secs(60),
    renew_interval: Duration::from_secs(20), // keep at ~ttl/3
    holder_margin: Duration::from_secs(1),
}
```

A longer `ttl` delays takeover after a real crash by the same amount, so
size it against both numbers. A graceful `release()` skips the wait.

**On Kubernetes, give lease-holding pods CPU requests and no CPU limit.**
A CPU limit is enforced by CFS throttling, which can freeze every thread of
the pod for most of each accounting period once the limit is hit, including the
renew loop. Set requests and leave the limit unset, or set requests equal to
limits for memory and CPU so the pod gets Guaranteed QoS (and run it on a node
with CPU manager `static` if you need exclusive cores). Also keep the node
from running hot: lease holders should not share a saturated node with batch
work.

**Treat `NotHeld` and `EpochMismatch` as "re-acquire, then retry or step
down".** A renewal or release refused with `DenyReason::NotHeld` or
`DenyReason::EpochMismatch` (and a resource refusing a write with
`FenceRejection::NotHeld` or `StaleEpoch`) means someone else holds a newer
epoch, or nobody holds the row. It is not a transient error to retry in
place:

1. Stop protected work. `lease.watch()` reports `Fenced` or `Lost`, and
   `fence()` returns `None`.
2. Call `LeaseHandle::acquire` again, with jittered backoff on `Denied`.
3. If you win, retry the interrupted action with the new fence. If you are
   `Denied`, step down and treat the other node as the holder.

Never replay the old fence. Count these events: a holder that is fenced
repeatedly is undersized, and the fix is a larger `ttl` or less CPU
contention, not a retry loop.

**Watch the early-warning metrics.** `aster.lease.renew_late` (ms) shows how
late renewals are sent; a tail approaching `ttl - holder_margin - renew_interval`
means stalls are eroding the lease before it costs you one. `aster.lease.lost`
counts leases lost, by `reason` (`fenced`, `expired`, `self_limit`). Both are in
the [observability guide](aster-observability-getstarted.md) and need the `otel`
feature (every binding has it).

## 9. Next steps

Use the [coordination guide](aster-coordination-getstarted.md) for replicated
fenced registers and leases. The [Python guide](aster-python-getstarted.md)
shows foreign-language lease serializers and fenced resources.
