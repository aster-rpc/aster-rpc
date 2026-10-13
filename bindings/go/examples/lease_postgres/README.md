# Postgres lease serializer behind a lease grantor, and fenced writes

A controller that pauses (GC pause, `SIGSTOP`, VM live migration, a long
syscall) and is replaced while it is paused must not overwrite the work its
successor did. A lease alone cannot prevent that: the paused controller
does not know its lease expired. Fencing prevents it. Every protected write
carries the fence `(resource, epoch, holder)`, and the store accepts the
write only if that fence is still current.

The example runs the high-level path (`StartServer` / `StartClient`, Rust
`aster::rpc::lease`):

- **The grantor node** is an `AsterServer` serving `aster.lease.Grantor`
  (`ServerOptions.LeaseGrantor`) over **`PGSerializer`** (`store.go`), a
  Postgres implementation of `aster.LeaseSerializer`. It is the one
  serialization point for the resource's leases. It also serves the
  protected store, **`Work.Write`** (`main.go`), which applies a write with
  **`FencedWrite`** (`store.go`) only while the fence is current.
- **The controllers** are `AsterClient`s with their own node keys. Each
  acquires through the grantor (`AsterClient.LeaseGrantor`); the Rust holder
  engine renews in the background through it. The grantor binds every
  lease operation to the caller's **authenticated** node id, never a request
  field, and `Work.Write` uses that same id as both the fence holder and the
  writer: a controller can neither take nor use a lease as anyone else.

Configuration comes from `aster.ConfigFromEnv()` (defaulting to a relay-free
loopback node for the demo), so the same program runs with Enrollment
(`ASTER_ENROLLMENT_*`): the grantor then admits only enrolled controllers,
and `LeaseGrantor.Requires` can gate the methods by role.

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
go test -v .      # serializer, fence and grantor tests (skipped when PG_DSN is unset)
```

The demo creates `leases` and `work` if they do not exist. It uses the
resource `kasm/session-controller`. Epochs carry over between runs and are
never reused. Output from a real run against a fresh Postgres 17 (Rust log
lines on stderr omitted):

```
controller-a is node 39b4eeb2cc4f, controller-b is node a89417916e3b
start: kasm/session-controller epoch=0 held=false (epochs persist across runs; never reused)

1. controller-a acquired epoch 1
  controller-a  write "a: scale pool to 3"         epoch=1: APPLIED
   controller-b acquire while a is live: denied (held_by_other, retry in ~1995ms)

2. controller-a paused (no renewals), sleeping 2.5s past its TTL of 2s

3. controller-b acquired epoch 2 (a's lease expired)
  controller-b  write "b: scale pool to 5"         epoch=2: APPLIED

4. controller-a wakes up and sends its delayed write
  controller-a  write "a: scale pool to 3 (stale)" epoch=1: REFUSED (stale_epoch, carried=1 current=2)

5. controller-a replays the current epoch 2 (read from a snapshot)
  controller-a  write "a: replayed current epoch"  epoch=2: REFUSED (not_holder)
   a naive "WHERE epoch = 2" guard would have accepted that write: true

6. controller-b released (fence advances)
  controller-b  write "b: late write after release" epoch=2: REFUSED (stale_epoch, carried=2 current=3)

final work row: "b: scale pool to 5" written by controller-b at epoch 2
final lease:    epoch=3 held=false
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
  the QUIC handshake proves: `Work.Write` passes `CallInfo.PeerID`, the
  same id the grantor bound the lease to. The request carries only the
  resource, the epoch and the payload.
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
- Authorization: the grantor's `revoke` requires the `operator` role by
  default (`LeaseGrantor.RevokeRequires`); `LeaseGrantor.Requires` gates
  every method by role (for example from enrollment credentials). A
  serializer can also refuse with a `Denied(unauthorized)` result from
  `Transition`.
- A dead grantor stops grants and renewals; holders run to their
  self-limit and report `Lost`. It never splits: there is no automatic
  failover.
