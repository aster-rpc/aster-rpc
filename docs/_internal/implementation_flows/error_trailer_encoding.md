# Error Trailer Encoding

**Status:** Stub -- to be filled after chaos tests confirm invariants.

**Reference:** `bindings/python/aster/server.py` _write_error_trailer() + `bindings/typescript/packages/aster/src/server.ts` writeErrorTrailer()

## What this flow covers

How error trailers are encoded so the client can always decode them,
regardless of codec mismatch. This was the single most common source of
"Expected RpcStatus, got NoneType" failures in cross-language testing.

## Sections to write

### 1. The invariant
- An error trailer MUST be decodable by the client
- The client's codec is determined by the StreamHeader's `serializationMode`
- Therefore ALL error trailers on a stream must use the SAME codec the client requested
- This includes early-return paths BEFORE the server has validated the serialization mode

### 2. Early-return error paths
- These fire after the StreamHeader is decoded but before normal dispatch:
  - Missing service name -> INVALID_ARGUMENT
  - Service not found -> NOT_FOUND
  - Scope mismatch -> FAILED_PRECONDITION
  - Session service class not found -> INTERNAL
  - Method not found -> UNIMPLEMENTED
  - Serialization mode not supported -> INVALID_ARGUMENT
  - Handler method not found -> INTERNAL
  - Auth interceptor denial -> PERMISSION_DENIED
- Every single one must pass `serialization_mode=header.serializationMode`

### 3. Python implementation
- `_write_error_trailer(send, code, message, serialization_mode=0)` -- default is Fory
- If `serialization_mode == SerializationMode.JSON.value`: encode as JSON dict
- Otherwise: encode via ForyCodec
- **The pattern:** extract `ser_mode = header.serializationMode` once after header decode, pass it to every subsequent `_write_error_trailer` call

### 4. TypeScript implementation
- `writeErrorTrailer(send, code, message)` always uses `this.codec` (JsonCodec)
- Since TS server only speaks JSON, all trailers are JSON -- naturally compatible
- If TS server ever gains Fory support, this will need the same per-stream codec logic

### 5. Binary-first server receiving JSON client (Python)
- Python server's default is Fory (binary)
- JSON client sends StreamHeader with first byte `{` -> server sniffs and switches
- But error paths between the sniff and the actual dispatch used to use the default codec
- **Fix applied:** all early-return error trailers now pass `ser_mode`

### 6. JSON-only server receiving binary client (TS)
- TS server sniffs first byte, finds it's not `{`
- Writes INVALID_ARGUMENT trailer as JSON (which the binary client may or may not decode)
- This is a best-effort -- the client chose binary but the trailer is JSON
- Most binary clients (Python ForyCodec) have a JSON fallback sniff in their decode path

## Invariants for new implementations

_(To be confirmed by chaos tests, then documented here)_

## Bugs this flow exposed

- Python server's scope mismatch trailer was Fory-encoded, unreadable by JSON clients
- Python server's auth denial trailer was Fory-encoded, same issue
- TS client got "JSON Parse error: Unrecognized token ''" on every denied call to a Python server
- After fix: all 8 denial paths in Python server pass serialization_mode correctly
