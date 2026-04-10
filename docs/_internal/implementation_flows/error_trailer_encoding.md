# Error Trailer Encoding

How error trailers are encoded so the client can always decode them,
regardless of codec mismatch. This was the single most common source of
cross-language failures during the integration matrix.

**Spec:** Aster-SPEC.md §5.7

## The invariant

> An error trailer MUST be encoded using the same codec the client
> requested in its StreamHeader.

The client's codec is determined by the `serializationMode` field in the
StreamHeader (confirmed by first-byte sniffing). Every error trailer on
that stream — including early-return paths before normal dispatch — must
use this codec.

## Why this matters

If a JSON client connects to a Python server (default: Fory binary) and
the server writes a Fory-encoded error trailer, the JSON client gets
`"JSON Parse error: Unrecognized token"` instead of the actual error
message. The client knows something went wrong but has no idea what.

## The pattern

Extract `ser_mode` once after decoding the StreamHeader. Pass it to every
subsequent error trailer write:

```python
# Python server.py, after decoding StreamHeader:
ser_mode = header.serializationMode

# Every error path:
await self._write_error_trailer(send, StatusCode.NOT_FOUND,
    f"service {name} not found",
    serialization_mode=ser_mode)  # ← must be explicit
```

## Early-return error paths

These fire after the StreamHeader is decoded but before normal dispatch.
Every one must pass `serialization_mode`:

| Error | Status code | When |
|-------|-------------|------|
| Empty service name | INVALID_ARGUMENT | StreamHeader.service is empty |
| Service not found | NOT_FOUND | Service not in registry |
| Method not found | UNIMPLEMENTED | Method not on service |
| Scope mismatch | FAILED_PRECONDITION | Session call to shared service or vice versa |
| Handler missing | INTERNAL | Method registered but no handler on instance |
| Auth interceptor denial | PERMISSION_DENIED | CapabilityInterceptor rejects |
| Metadata too large | RESOURCE_EXHAUSTED | CallHeader metadata exceeds limits |
| Codec not supported | INVALID_ARGUMENT | Server can't handle client's codec |
| Deadline already expired | DEADLINE_EXCEEDED | DeadlineInterceptor rejects on receipt |

## Python implementation

`_write_error_trailer()` at `server.py:1011`:

```python
async def _write_error_trailer(self, send, code, message, serialization_mode=0):
    if serialization_mode == SerializationMode.JSON.value:
        payload = json_encode({
            "code": code.value,
            "message": message,
            "detailKeys": [],
            "detailValues": [],
        })
    else:
        status = RpcStatus(code=code, message=message)
        payload = self._codec.encode(status)
    await write_frame(send, payload, flags=TRAILER)
```

The default `serialization_mode=0` is Fory. If a caller forgets to pass
the mode, the trailer is Fory-encoded — which is correct for Fory clients
but wrong for JSON clients.

### Session server

`_write_trailer()` at `session.py:121` always uses the session's codec
(determined at session open time), so it doesn't have this problem.

## TypeScript implementation

`writeErrorTrailer()` at `server.ts:550` always uses `this.codec` which
is `JsonCodec` by default. Since the TS server only speaks JSON, all
trailers are JSON — naturally compatible with JSON clients.

If the TS server ever gains Fory support, it will need the same per-stream
codec logic as Python.

## RpcStatus wire format

The trailer payload is a serialized `RpcStatus`:

```json
{
  "code": 13,
  "message": "service MissionControl not found",
  "detailKeys": [],
  "detailValues": []
}
```

| Field | Type | Wire name | Notes |
|-------|------|-----------|-------|
| Status code | int | `code` | StatusCode enum value |
| Message | string | `message` | Truncated to MAX_STATUS_MESSAGE_LEN (4096) |
| Detail keys | string[] | `detailKeys` | camelCase on wire |
| Detail values | string[] | `detailValues` | camelCase on wire |

### StatusCode values

| Code | Name | Typical use |
|------|------|-------------|
| 0 | OK | Success trailer |
| 1 | CANCELLED | After CANCEL frame |
| 3 | INVALID_ARGUMENT | Bad StreamHeader |
| 4 | DEADLINE_EXCEEDED | Handler timeout |
| 5 | NOT_FOUND | Service not found |
| 7 | PERMISSION_DENIED | Auth failure |
| 8 | RESOURCE_EXHAUSTED | Rate limit, metadata too large |
| 9 | FAILED_PRECONDITION | Scope mismatch |
| 12 | UNIMPLEMENTED | Method not found |
| 13 | INTERNAL | Handler crash, decode error |
| 14 | UNAVAILABLE | Stream ended |

## Binary-first server receiving JSON client (Python)

1. Python server's default codec is Fory (binary)
2. JSON client sends StreamHeader with first byte `{`
3. Server sniffs and sets `ser_mode = JSON`
4. All subsequent error trailers on this stream use JSON encoding

The fix was ensuring `ser_mode` is passed to every `_write_error_trailer`
call, including early-return paths.

## JSON-only server receiving binary client (TS)

1. TS server sniffs first byte, finds it's not `{`
2. Writes INVALID_ARGUMENT trailer as JSON
3. The binary client may not be able to decode this JSON trailer

This is best-effort. Most binary clients (Python ForyCodec) have a JSON
fallback sniff in their decode path, so they can usually read it.

## Naming conventions (wire compatibility)

RpcStatus field names are **camelCase** on the wire:

| Wire name | Notes |
|-----------|-------|
| `code` | Integer |
| `message` | String |
| `detailKeys` | camelCase, not `detail_keys` |
| `detailValues` | camelCase, not `detail_values` |

These names must be identical across all bindings because they are encoded
by the codec and decoded by a potentially different binding.

## Performance notes

- Error trailers are not hot-path (they represent failures). Don't
  optimise for speed; optimise for correctness.
- `validate_status_message()` truncates messages to 4096 bytes. Apply it
  before encoding to avoid oversized trailers.

## Invariants confirmed by chaos tests

- Corrupt payload produces INTERNAL error trailer (`test_g12`)
- Non-OK EoI trailer rejected with INTERNAL (`test_g4`)
- Deadline exceeded returns DEADLINE_EXCEEDED trailer (`test_g8`)
- 58/58 cross-language matrix passes (error trailers readable both ways)

## Implementation checklist for new bindings

- [ ] Error trailer uses the client's requested codec (not server default)
- [ ] Extract `serializationMode` once after StreamHeader decode
- [ ] Pass it to every error trailer write, including early-return paths
- [ ] RpcStatus fields: `code` (int), `message` (string), `detailKeys`, `detailValues`
- [ ] Truncate message to MAX_STATUS_MESSAGE_LEN (4096)
- [ ] StatusCode enum values must match across bindings
- [ ] Test: JSON client against binary server must receive readable error trailers

## Key files

| Binding | File | Entry point |
|---------|------|-------------|
| Python | `server.py:1011` | `_write_error_trailer()` |
| Python | `session.py:121` | `_write_trailer()` |
| Python | `status.py` | `StatusCode` enum |
| Python | `protocol.py:58` | `RpcStatus` dataclass |
| TS | `server.ts:550` | `writeErrorTrailer()` |
| TS | `session.ts:361` | `writeOkTrailer()` |
| TS | `status.ts` | `StatusCode` enum |
| TS | `protocol.ts:45` | `RpcStatus` class |
