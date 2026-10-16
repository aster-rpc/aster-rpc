# Durable audit events from Go: sender, receiver and acknowledgement log

The sender records audit events (here, daily usage reports) in a local durable
outbox and Aster delivers them, oldest first and at least once, to an HTTP
receiver you operate. The receiver stores each event before acknowledging it.
The sender's acknowledgement log then shows exactly which events the receiver
accepted, and when.

- **Sender** (`main.go`, `send`): `aster.OpenAuditOutbox` with a directory and
  the receiver endpoint, `Submit` per event (returns once the event is durable
  locally), `Flush` (returns once the receiver acknowledged everything
  submitted before it), then `Acknowledgements(ctx, after, limit)` to list what
  was acknowledged. `WatchAcknowledgements(ctx, after)` follows the log live.
- **Receiver** (`receiver.go`): a plain `http.Handler` with no Aster
  dependency that implements the receiver rules in
  [`docs/audit-outbox.md`](../../../../docs/audit-outbox.md):
  - It validates the batch and every event, and checks that each envelope's
    SHA-256, outbox ID and sequence match its full event ID.
  - It deduplicates on the **whole** event ID and stores the exact envelope
    bytes.
  - It commits every event durably before replying
    `{"version":1,"batch_id":…,"durable":true}`.
  - An identical retry is acknowledged again.
  - The same ID with different bytes gets 409
    `{"version":1,"error":"integrity_conflict"}`.

  Storage is one fsynced file per event. Replace it with your database, but
  keep the commit-before-acknowledgement order.

The example lives in the binding's module because it needs nothing beyond the
standard library.

## Run

You need the native library (see the binding README, "CGO_CFLAGS/CGO_LDFLAGS").

```sh
cd bindings/go/examples/audit_outbox
export CGO_CFLAGS="-I$(pwd)/../../../../ffi"
export CGO_LDFLAGS="-L$(pwd)/../../../../target/debug -laster_transport_ffi -Wl,-rpath,$(pwd)/../../../../target/debug"

go run .            # demo: receiver + sender in one process
go test -v .        # the demo plus sender/receiver tests

# or as two processes
go run . receiver -listen 127.0.0.1:8787 -dir ./state/receiver
go run . sender -endpoint http://127.0.0.1:8787/audit -dir ./state/outbox -count 3
go run . sender -endpoint http://127.0.0.1:8787/audit -dir ./state/outbox -count 1 -after 3
```

The demo also shows the two failure cases a receiver must handle:

- **Lost acknowledgement.** The receiver commits the first batch and then
  answers 503. The outbox retries the identical batch, the receiver finds the
  same bytes already stored and acknowledges again, and each event is
  acknowledged and logged once.
- **Conflicting bytes.** A forged retry of a stored ID with different envelope
  bytes gets 409 `integrity_conflict`.

Output from a run (IDs shortened):

```
receiver listening on http://127.0.0.1:52655/audit
submitted aster-audit-v1:a5c1b3a6e978f5c18d158273e554fe64:1:414cc922ff...
submitted aster-audit-v1:a5c1b3a6e978f5c18d158273e554fe64:2:636e17b855...
submitted aster-audit-v1:a5c1b3a6e978f5c18d158273e554fe64:3:c7a1900e91...
watch: receiver acknowledged aster-audit-v1:a5c1b3a6e978f5c18d158273e554fe64:1:414cc922ff... (cursor 1)
watch: receiver acknowledged aster-audit-v1:a5c1b3a6e978f5c18d158273e554fe64:2:636e17b855... (cursor 2)
watch: receiver acknowledged aster-audit-v1:a5c1b3a6e978f5c18d158273e554fe64:3:c7a1900e91... (cursor 3)
acknowledgement log (3 entries):
  cursor=1 seq=1 id=aster-audit-v1:a5c1b3a6e978f5c18d158273e554fe64:1:414cc922ff... batch=48140e2178e2 at=2026-10-16T03:22:00.927Z
  cursor=2 seq=2 id=aster-audit-v1:a5c1b3a6e978f5c18d158273e554fe64:2:636e17b855... batch=48140e2178e2 at=2026-10-16T03:22:00.927Z
  cursor=3 seq=3 id=aster-audit-v1:a5c1b3a6e978f5c18d158273e554fe64:3:c7a1900e91... batch=48140e2178e2 at=2026-10-16T03:22:00.927Z
lost acknowledgement: receiver saw 2 attempts, stored each event once
identical retry: 200 {"batch_id":"afa7158560...","durable":true,"version":1}
same ID, different bytes: 409 {"error":"integrity_conflict","version":1}
```

## Deployment notes

- Put the outbox directory on a persistent volume, not `emptyDir`.
  Unacknowledged events must survive a restart. One process holds the
  directory lock, which suits a StatefulSet.
- A 4xx from the receiver other than 401/403/408/425/429 stops delivery at
  that event until an operator repairs the receiver or quarantines the event.
  Answer 422 only for requests that are really malformed. Answer 5xx for
  temporary storage trouble; the sender retries 5xx.
- The acknowledgement log keeps the last 100,000 entries or 7 days by default
  (`AcknowledgementLogMaxEntries`, `AcknowledgementLogMaxAgeMS`). Record the
  last `Cursor` you processed and pass it as `after` to resume.
