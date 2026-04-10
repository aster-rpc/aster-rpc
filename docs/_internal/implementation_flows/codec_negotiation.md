# Codec Negotiation

**Status:** Stub -- to be filled after chaos tests confirm invariants.

**Reference:** `bindings/python/aster/server.py` lines 358-370 (sniff) + `bindings/python/aster/json_codec.py`

## What this flow covers

How server and client agree on the serialization format (Fory XLANG vs
JSON) for a given stream. Critical for cross-language interop since the
TypeScript binding only speaks JSON (Fory JS not yet XLANG-compliant).

## Sections to write

### 1. SerializationMode enum
- XLANG = 0 (Fory cross-language binary, default for Python)
- NATIVE = 1 (language-native, only for LocalTransport)
- ROW = 2 (row-oriented random access)
- JSON = 3 (UTF-8 JSON, the TypeScript binding's only option)

### 2. Server-side sniffing
- Server reads first frame (StreamHeader), checks first byte of payload
- `0x7B` ('{') -> JSON. Anything else -> binary (Fory XLANG)
- If server only supports JSON (TS) and receives binary: write INVALID_ARGUMENT trailer encoded as JSON (so the client CAN read it) and return
- Python server: accepts both. Uses `header.serializationMode` to select codec for subsequent frames on this stream
- TS server: accepts only JSON. Rejects binary with clear error

### 3. Client-side mode selection
- IrohTransport sets `serializationMode` in StreamHeader based on codec type
- JsonProxyCodec -> mode 3. ForyCodec -> mode 0
- Python typed client (`AsterClient.client()`) checks ServiceSummary.serialization_modes
- If server only offers `['json']`, auto-picks JsonProxyCodec
- Generated clients (`from_connection()`) do the same check

### 4. Per-stream codec -- all frames on one stream use the same codec
- StreamHeader, request payloads, response payloads, and trailers all use the same codec
- Exception: error trailers on early-return paths MUST also use the client's requested codec
- **Known gap fixed:** Python server's early-return `_write_error_trailer` was defaulting to Fory even when the client sent JSON

### 5. Session streams
- The codec for the session is determined by the StreamHeader's `serializationMode`
- All CALL frames, request/response payloads, and per-call trailers use this codec
- Python server: if mode=3, creates `JsonProxyCodec` and passes to `SessionServer`

### 6. Protocol frames vs payload frames
- The framing layer (length + flags + payload) is codec-agnostic
- HEADER, TRAILER, CALL, CANCEL flags are in the 1-byte flags field, not in the payload
- The codec only encodes/decodes the payload bytes
- A JSON trailer looks like: `[len][0x02 TRAILER][{"code":0,"message":""}]`
- A Fory trailer looks like: `[len][0x02 TRAILER][<fory binary bytes>]`

## Invariants for new implementations

_(To be confirmed by chaos tests, then documented here)_

## Bugs this flow exposed

- TS server was hardcoded to JsonCodec but published `serializationModes: ['xlang']` -- lie
- Python typed client sent Fory to a JSON-only server, got "Expected RpcStatus, got NoneType"
- Python server's error trailers on scope mismatch / auth denial used default codec, not the client's
- Generated clients hardcoded ForyCodec, ignoring the server's advertised modes
