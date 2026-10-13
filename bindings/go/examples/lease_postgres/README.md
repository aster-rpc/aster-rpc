# Postgres lease serializer and fenced writes

A controller that pauses (GC pause, `SIGSTOP`, VM live migration, a long
syscall) and is replaced while it is paused must not overwrite the work its
successor did. A lease alone cannot prevent that: the paused controller
does not know its lease expired. Fencing prevents it. Every protected write
carries the fence `(resource, epoch, holder)`, and the store accepts the
write only if that fence is still current.

This example implements both halves on Postgres:

- **`PGSerializer`** (`store.go`) is the lease serialization point. It
  implements `aster.LeaseSerializer` and is served with
  `Upcalls.ServeLeaseSerializer`. The Rust holder engine (acquire,
  background renewals, the holder's self-limit) runs over it through
  `aster.NewForeignLeaseSerializer`.
- **`FencedWrite` / `PGFencedResource`** (`store.go`) are the resource side.
  They update a `work` row only when the writer holds the current epoch.
  They are served with `Upcalls.ServeFencedResource` and called through
  `aster.FencedResourceHandle.Mutate`. You can also call `FencedWrite`
  directly from your own code.

The example is a separate Go module, so the `pgx` driver stays out of the
binding's `go.mod`. A `replace` points it at the binding (`../..`).

## Run

You need a Postgres you can create tables in, and the native library (see
the binding README, "CGO_CFLAGS/CGO_LDFLAGS").

```sh
cd bindings/go/examples/lease_postgres
export CGO_CFLAGS="-I$(pwd)/../../../../ffi"
export CGO_LDFLAGS="-L$(pwd)/../../../../target/debug -laster_transport_ffi -Wl,-rpath,$(pwd)/../../../../target/debug"
export PG_DSN="postgres://postgres@127.0.0.1:5432/postgres?sslmode=disable"

go run .          # the demo
go test -v .      # serializer and fence tests (skipped when PG_DSN is unset)
```

The demo creates `leases` and `work` if they do not exist. It uses the
resource `kasm/session-controller`. Epochs carry over between runs and are
never reused. Output from a real run against Postgres 17:

```
start: kasm/session-controller epoch=3 held=false (epochs persist across runs; never reused)

1. controller-a acquired epoch 4
  controller-a  write "a: scale pool to 3"         fence(epoch=4, holder=controller-a): APPLIED
   controller-b acquire while a is live: denied (held_by_other, retry in ~1999ms)

2. controller-a paused (no renewals), sleeping 2.5s past its TTL of 2s

3. controller-b acquired epoch 5 (a's lease expired)
  controller-b  write "b: scale pool to 5"         fence(epoch=5, holder=controller-b): APPLIED

4. controller-a wakes up and sends its delayed write
  controller-a  write "a: scale pool to 3 (stale)" fence(epoch=4, holder=controller-a): REFUSED (stale_epoch, carried=4 current=5)

5. controller-a replays the current epoch 5 (read from a snapshot)
  controller-a  write "a: replayed current epoch"  fence(epoch=5, holder=controller-a): REFUSED (not_holder)
   a naive "WHERE epoch = 5" guard would have accepted that write: true

6. controller-b released (fence advances)
  controller-b  write "b: late write after release" fence(epoch=5, holder=controller-b): REFUSED (stale_epoch, carried=5 current=6)

final work row: "b: scale pool to 5" written by controller-b at epoch 5
final lease:    epoch=6 held=false
OK: the deposed controller did not overwrite its successor's work
```

## The serializer

There is one row per resource in `leases`, with the columns `epoch`,
`holder`, `held`, `ttl_ms` and `granted_at`. Each `Transition` is one
transaction:

1. `INSERT ... ON CONFLICT DO NOTHING` creates the initial row (epoch 0,
   free), so there is always a row to lock.
2. `SELECT ... FOR UPDATE` reads and locks the row. In the same statement,
   Postgres evaluates expiry on its own clock (`clock_timestamp()`), never
   on the caller's clock.
3. The state machine of `core/src/lease.rs` (`transition`) is applied. The
   predicates and their order are the same:

   | op | denied when (in order) | effect |
   |----|-----------------------|--------|
   | ACQUIRE | held and not expired: `held_by_other` | epoch+1, holder=candidate, new TTL, anchor reset |
   | RENEW | not held: `not_held`; epoch differs: `epoch_mismatch`; other holder: `not_holder`; expired: `expired` | anchor reset |
   | RELEASE | epoch differs: `epoch_mismatch`; other holder: `not_holder` (no expiry check) | epoch+1, free |
   | REVOKE | never (authorization comes before the state machine) | epoch+1, free |

4. A single `UPDATE ... WHERE resource=$1 AND epoch=$old` writes the new
   row. If it does not affect exactly one row, that is a backend error.

The epoch only ever goes up. A `CHECK` constraint keeps it non-negative, and
a trigger refuses `DELETE` on `leases`: a deleted row would restart at epoch
0 and reissue fences that were already handed out.

**Errors are never denials.** A failed query, a lost connection or a
cancelled context is returned as an `error`. The binding reports it to the
Rust holder engine as a backend error. A renewal that hits a database outage
therefore does not fence the holder. The holder keeps its fence until its
self-limit (TTL minus margin) passes, and then reports `Lost`. A denial
(`TransitionDenied`) is information from a healthy serializer.
`TestPGSerializer_DBFaultIsAnError` checks this. On the write path, a
database fault likewise surfaces as `ErrMutationOutcomeUnknown`, never as
"rejected".

The database clock is the only clock. If it jumps (for example a manual time
change on the database host), leases expire early or late. Monotonic time is
not available across Postgres sessions, so keep the database host on NTP.

## The fenced write

```sql
-- in one transaction:
SELECT epoch, held, holder FROM leases WHERE resource = $1 FOR SHARE;
-- aster.CheckFence(epoch, held, holder, fence, writer) in Go, for the reason
UPDATE work SET payload = $4, lease_epoch = $2, lease_holder = $3, updated_at = now()
WHERE resource = $1
  AND EXISTS (SELECT 1 FROM leases l
              WHERE l.resource = $1 AND l.epoch = $2 AND l.held AND l.holder = $3);
```

- `FOR SHARE` conflicts with the serializer's `FOR UPDATE`. No acquire,
  release or revoke can commit between the check and the write. A plain
  read under READ COMMITTED would allow that race.
- The `UPDATE` repeats the whole fence rule (epoch **and** held **and**
  holder). The statement is safe on its own, even if someone later removes
  the `CheckFence` call. `CheckFence` exists only to report *why* a write
  was refused (`stale_epoch`, `not_holder`, `not_held`, `future_epoch`).
- `$3` (`writer`) must be the **authenticated** identity of the caller,
  never a value from the request. On Aster, that is the peer node id that
  the QUIC handshake proves (`CallInfo.PeerID` in a handler). In this
  single-process demo, the writer names are plain strings.
- If your business table can store the lease epoch, you can fence without
  a join. Add `WHERE work.lease_epoch <= $2` and also keep the holder check
  against the lease row. A monotonic per-row epoch on its own still does
  not prove who is writing.

### Why `WHERE epoch = $current` alone is insufficient

The epoch is not a secret. Every snapshot reader can see it
(`Snapshot`, `LeaseDeniedError.Snapshot`, directory listings, logs). A
guard that checks only the epoch accepts a write from **anyone** who
presents the current number:

- A deposed controller that wakes, reads a fresh snapshot (or retries with
  "the latest epoch") and writes. That is step 5 of the demo: the
  epoch-only predicate would have accepted it.
- A buggy or compromised peer that copies the epoch out of a log line.
- Equal-epoch confusion after a release: the row is no longer held, but an
  epoch-only guard cannot tell. This is why the rule also checks `held`.

The holder check binds the epoch to the identity it was issued to. The
fence rule is `epoch == current AND held AND writer == holder`, evaluated
atomically with the write. That is `aster::lease::check_fence`, which is
`aster.CheckFence` in Go.

## Notes

- The holder must read the fence at send time (`Lease.Fence()`) and must
  not cache it. `ok == false` means the holder must stop acting. The demo
  deliberately keeps a fence across a pause to show that the store, not
  the holder, is the last line of defence.
- `Lease.Close()` stops renewals without releasing, as a frozen process
  would. `Release` hands the lease off at once and advances the epoch.
  After a release, the holder's own in-flight writes are refused.
- Authorization (who may acquire or revoke) is not part of this example.
  Gate it before the state machine, either in your RPC layer or with a
  `Denied(unauthorized)` result from `Transition`.
