# Durable audit events

The audit outbox provides explicit, locally durable submission and at-least-once
delivery to a receiver selected by the SDK consumer. It is separate from Aster's
ordinary logs and OpenTelemetry exporters. Sending a diagnostic log, including
`aster_log_emit`, does not place it in the durable outbox.

The outbox is available in the Aster 0.3.893 preview across Rust, Python,
TypeScript, Go, Java and .NET. Its delivery guarantee depends on local storage
surviving and the receiver implementing the acknowledgement protocol below.

## Submission and delivery

Await `submit` before allowing a mandatory audited operation to proceed. Success
means the exact envelope is committed locally; it does not mean the receiver has
accepted it. Aster retains and retries the record until the receiver acknowledges
durable acceptance. Await `flush` to confirm delivery of the captured prefix.

A durable intent is not an atomic commit with application data. If needed, submit
a separate correlated outcome after the business operation. Cancelling an await
can leave an event committed without the caller receiving its ID. Resubmitting may
create a duplicate intent; exactly-once business operations are not promised.

Use a persistent, private storage directory. One process holds its lifetime lock.
On Windows, provision a private NTFS directory whose inheritable ACL grants access
to the application account and trusted administrators/SYSTEM only. Aster relies on
that inherited ACL; it does not tighten or reject broad Windows ACLs automatically.
Cloned handles share workers, queues and limits. Closing stops the shared outbox,
releases its storage and preserves pending records; it does not require a working
receiver. Reopen the same directory to resume delivery.

Defaults are 256 MiB retained data and 64 KiB per immutable envelope. The maximum
configurable envelope is 1 MiB, including its metadata. Retained data includes
pending envelopes, quarantined envelopes and disposition receipts. Limits describe
logical retained data, not an exact physical database file size. No unacknowledged
records are evicted. When admission fails, an application requiring mandatory
auditing must refuse its associated operation.

Storage surviving is essential. Local durability cannot protect against disk loss,
operator deletion or records missing from a restored backup. Content-bound IDs
prevent a restored sequence from silently colliding with different earlier content;
they cannot reconstruct events absent from that backup.

## Language entrypoints

These APIs call the same Rust outbox. Import Python types from `aster`,
TypeScript types from `@aster-rpc/aster`, and Java types from `site.aster.audit`.
Rust consumers enable the `audit-outbox` feature; .NET uses `Aster.Audit`.

| Language | Open | Event and integer fields | Flush and shutdown |
| --- | --- | --- | --- |
| Rust | `aster::audit_outbox::AuditOutbox::open(OutboxConfig)` | `AuditEvent`; unsigned 64-bit timestamps and counters | Async `flush(std::time::Duration)` and `close()` |
| Python | `await AuditOutbox.open(AuditOutboxConfig(directory, AuditDestination(endpoint)))` | `AuditEvent(timestamp_ms, kind, payload)`; exact Python `int` | `await outbox.flush(timeout_seconds)`; `await outbox.close()` or async context management |
| TypeScript | `await AuditOutbox.open({ directory, destination: { endpoint } })` | `{ timestampMs, kind, payload }`; `bigint` timestamps, retained budget and status counters | `await outbox.flush(timeoutMs)`; `await outbox.close()` or async disposal |
| Java | `AuditOutbox.open(runtime, new AuditOutbox.Config(directory, new AuditOutbox.Destination(endpoint)))` | `AuditOutbox.Event`; `BigInteger` represents unsigned 64-bit fields, and payload is a Jackson `JsonNode` | `outbox.flush(Duration)` and `outbox.closeAsync()` return `CompletableFuture`; `close()` enqueues shutdown and returns immediately |
| Go | `OpenAuditOutbox(ctx, runtime, AuditOutboxConfig)` | `AuditEvent`; `uint64` timestamps and counters | `Flush(ctx, time.Duration)`; `Close(ctx)` |
| .NET | `AuditOutbox.OpenAsync(runtime, AuditOutboxConfig, ct)` | `AuditEvent`; `ulong` timestamps and counters | `FlushAsync(TimeSpan, ct)`; `CloseAsync(ct)` or async disposal |

Rust `Clone`, Python and TypeScript `clone()`, Java `copy()`, and Go/.NET
`Clone()` share workers, storage and capacity. Explicitly closing any clone stops
the shared outbox. Await shutdown
before reopening its directory; Java callers should compose `closeAsync()` when
they need to wait for the lock to be released. Cancelling a Java future only
cancels that wait; native completion and handle cleanup continue.

```python
from aster import AuditDestination, AuditEvent, AuditOutbox, AuditOutboxConfig

async with await AuditOutbox.open(
    AuditOutboxConfig("./state/audit", AuditDestination(endpoint))
) as outbox:
    event_id = await outbox.submit(AuditEvent(timestamp_ms, "operation.intent", {"operation": "update"}))
    await outbox.flush(10)  # seconds
```

```typescript
import { AuditOutbox } from '@aster-rpc/aster';

const outbox = await AuditOutbox.open({
  directory: './state/audit', destination: { endpoint },
});
try {
  const eventId = await outbox.submit({
    timestampMs: BigInt(Date.now()), kind: 'operation.intent',
    payload: { operation: 'update' },
  });
  await outbox.flush(10_000); // milliseconds
} finally {
  await outbox.close();
}
```

Python failures expose `AuditOutboxError.kind`, TypeScript failures expose
`AuditOutboxError.kind`, and Java failures expose `AuditOutboxException.kind()`.
Rust uses `AuditError.kind()`, Go uses `*AuditOutboxError` with its `Kind` field,
and .NET uses `AuditOutboxException.Kind`.
Status includes `storage_recovered` (Python) or `storageRecovered` (TypeScript and
Java): the database performed a physical repair during this open and subsequently
passed the outbox's logical validation. Credentials are excluded from status.

## Receiver responsibility

The SDK consumer chooses and operates the HTTP(S) receiver. It must implement the
protocol below, durably store the original envelope bytes, and deduplicate by the
**entire** event ID. A normal HTTP 200 or OTLP export response is insufficient.

Aster tests its SDK against a persistent reference receiver with injected faults.
That demonstrates SDK behaviour under this contract. Consumers must separately
verify the durability and acknowledgement behaviour of their selected receiver,
including any proxy and storage service in front of it.

The endpoint, optional stable receiver identity and local directory are explicit
configuration. Authorization is configured separately; rotating it does not change
the destination identity. Changing the destination while pending or quarantined
records exist is refused, including after restart. Automatic HTTP redirects are
not followed. When no receiver identity is supplied, the deployment must ensure
that the same URL continues to refer to the same durable receiver.

## HTTP protocol v1

Aster POSTs JSON with content type `application/json`:

```json
{
  "version": 1,
  "batch_id": "<lowercase SHA-256 hex>",
  "events": [
    {
      "event_id": "aster-audit-v1:<outbox identity>:<sequence>:<envelope SHA-256>",
      "envelope_base64": "<standard padded base64>"
    }
  ]
}
```

There are 1–64 events and at most 1 MiB of decoded envelope bytes in a batch.
The encoded request is bounded at 1,420,000 bytes. Each envelope is a versioned
JSON object containing `version`, `outbox_id`, `sequence`, `timestamp_ms`, `kind`
and `payload` (a JSON object). Event kinds are 1–128 printable UTF-8 bytes.
Preserve its decoded bytes exactly; do not reconstruct them from
parsed JSON. Whitespace and key ordering in the outer batch may change, but changes
to envelope bytes change their digest.

The full event ID consists of `aster-audit-v1`, a 32-character lowercase hexadecimal
random outbox identity, a decimal unsigned 64-bit sequence and a 64-character
lowercase hexadecimal SHA-256 digest, separated by colons. Hash the exact decoded
envelope bytes. Check that its identity and sequence also match the ID.

Calculate the batch digest using SHA-256 over this concatenation:

1. The UTF-8 bytes of `aster-audit-batch-v1` followed by one zero byte.
2. For each ordered full event ID: its UTF-8 byte length as an unsigned 32-bit
   big-endian integer, followed by its UTF-8 bytes.

The receiver validates bounds and IDs, durably commits **all** events, then returns
a 2xx response with this complete acknowledgement:

```json
{"version":1,"batch_id":"<the exact batch digest>","durable":true}
```

Responses are bounded at 4,096 bytes. Unknown or malformed acknowledgement fields,
a missing durable flag, a mismatched batch digest or a lost reply cannot authorize
local deletion. Partial acknowledgements are unsupported. If an identical batch
arrives again, verify duplicates and acknowledge it again. A previously stored
full ID paired with different bytes must never be silently treated as a duplicate.

On digest or ID/content conflict, return HTTP 409 with:

```json
{"version":1,"error":"integrity_conflict"}
```

The fixture at `tests/audit_outbox/receiver.py` illustrates exact-byte storage and
commit-before-ACK using SQLite with full synchronous commits. It binds loopback
and is a test fixture, not a deployable production service.

## Failures and operator actions

| Result | Outbox behaviour |
| --- | --- |
| Valid full-batch durable ACK | Delete the batch in a durable local transaction. |
| Transport failure or missing/malformed 2xx ACK | Keep the records and retry. |
| 401/403 | Retry and expose an authorization error; rotate credentials to recover. |
| 408/425/429/5xx | Retry with bounded backoff, respecting bounded `Retry-After`. |
| 413/422 | Isolate the rejected batch into ordered singletons; stall at a rejected singleton. |
| Explicit 409 integrity conflict | Report an integrity stall and isolate the offending record. |
| Other 4xx, unrecognized 409 or 3xx | Report a configuration/protocol stall; repair and explicitly retry. |

Delivery stays oldest-first. A rejected record is not skipped automatically.
`flush` fails immediately for a blocking singleton rejection, integrity conflict,
configuration/protocol stall or disposition in its prefix. Authentication,
transport, overload and server failures remain retryable; flush waits for recovery
or timeout. Timeout does not delete records.

Prefer repairing the receiver and retrying. If a record must be bypassed, explicit
`quarantine` of the rejected oldest singleton atomically retains its envelope and rejection with an operator reason
and reference in local dead-letter storage. It is a recorded delivery exception,
not delivery success. `export` reads that evidence. After receiver repair, `replay`
moves the original ID back to pending, preserving the disposition trail.

Quarantine and export do not free capacity. There is no manual or automatic purge
in v1. Disposition receipts remain after replay, so enough exception history can
exhaust the retained-data budget indefinitely. Repeated actions also have a bounded
per-event receipt trail; an action that would exceed its limit fails before mutation.
Raise the configured budget only when physical storage permits. Do not delete the
database as a recovery procedure if retained evidence is required.

`status` separates pending, dead-letter and receipt bytes, readiness to accept,
last delivery error, worker health and lifecycle state. The application is
responsible for including required audit readiness in its admission policy.
Online compaction and maintenance operations are not part of v1.
