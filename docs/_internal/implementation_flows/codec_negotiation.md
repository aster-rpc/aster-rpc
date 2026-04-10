# Codec Negotiation

How server and client agree on serialization format for a given stream.
Critical for cross-language interop since different bindings may support
different codecs.

**Spec:** Aster-SPEC.md §5.5

## SerializationMode enum

| Value | Name | Description |
|-------|------|-------------|
| 0 | XLANG | Fory cross-language binary. Default for Python. |
| 1 | NATIVE | Language-native (only for LocalTransport). |
| 2 | ROW | Row-oriented random access. |
| 3 | JSON | UTF-8 JSON. The TypeScript binding's only option. |

The value is carried in `StreamHeader.serializationMode` and applies to
the entire stream: StreamHeader, request payloads, response payloads,
and trailers.

## Server-side sniffing

The server reads the first frame (StreamHeader) and checks the **first
byte of the payload**:

- `0x7B` (`{`) → JSON. The payload is a JSON-encoded StreamHeader.
- Anything else → binary (Fory XLANG). The payload is Fory-encoded.

This sniff is the primary codec detection mechanism. The
`serializationMode` field in the StreamHeader is confirmatory.

**Python:** `server.py:365-371`. Sniffs `payload[0:1] == b'{'`.
**TypeScript:** `server.ts:173`. Sniffs `payload[0] !== 0x7b`.

### If the server doesn't support the client's codec

- Python server: supports both JSON and XLANG. Always succeeds.
- TS server: supports only JSON. If binary is received, writes
  INVALID_ARGUMENT trailer encoded as JSON and closes the stream.

## Client-side mode selection

The client sets `serializationMode` in the StreamHeader based on its
codec:

| Codec | Mode | When used |
|-------|------|-----------|
| `ForyCodec` | 0 (XLANG) | Python default |
| `JsonProxyCodec` | 3 (JSON) | When server advertises JSON-only |

### How the client knows which codec to use

During consumer admission, the server returns `ServiceSummary` objects
with a `serialization_modes` array:

```json
{"serialization_modes": ["xlang"]}        // Python server
{"serialization_modes": ["json"]}          // TS server
{"serialization_modes": ["xlang", "json"]} // future: both
```

**Python auto-detection:** `AsterClient.client()` in `high_level.py`
checks `serialization_modes`. If server only offers `['json']`, creates
`JsonProxyCodec` instead of `ForyCodec`.

**TypeScript auto-detection:** `proxy()` in `high_level.ts` checks
`summary.pattern` for session vs shared, and always uses `JsonCodec`.

**Generated clients (codegen):** `from_connection()` in `codegen.py`
performs the same `serialization_modes` check.

## Per-stream codec invariant

**All frames on one stream use the same codec.** This includes:
- StreamHeader (first frame)
- Request payloads
- Response payloads
- Error trailers
- OK trailers

There is no codec switching mid-stream.

### The error trailer trap

Error trailers on early-return paths (before normal dispatch) must also
use the client's requested codec. If the server sniffs JSON but then
writes a Fory-encoded error trailer, the JSON client cannot decode it.

See `error_trailer_encoding.md` for the full list of early-return paths.

## Session streams

The codec for a session is determined by the first StreamHeader. All
subsequent CALL frames, request/response payloads, and per-call trailers
use this codec.

**Python:** If `serializationMode == JSON`, creates `JsonProxyCodec` and
passes it to `SessionServer` (server.py session dispatch path).

## Compression

Compression is orthogonal to codec choice. Any payload can be
zstd-compressed (COMPRESSED flag = 0x01). The codec decodes after
decompression.

**Decompression bomb protection:** Both codecs enforce
`MAX_DECOMPRESSED_SIZE` (16 MiB). The Python `ForyCodec._safe_decompress()`
uses streaming decompression and rejects payloads exceeding the limit
without trusting the content-size header.

**Python JSON codec:** `safe_decompress()` in `json_codec.py:18`.
**Python Fory codec:** `_safe_decompress()` in `codec.py:576`.
**TypeScript:** `zstdDecompress()` in `codec.ts:70` with `maxOutputLength`.

## Protocol frames vs payload frames

The framing layer (length prefix + flags + payload) is codec-agnostic:

```
[4 bytes: payload length, little-endian u32]
[1 byte: flags]
[N bytes: payload]
```

HEADER, TRAILER, CALL, CANCEL flags are in the flags byte, not in the
payload. The codec only encodes/decodes the payload bytes.

A JSON trailer: `[len][0x02][{"code":0,"message":"","detailKeys":[],"detailValues":[]}]`
A Fory trailer: `[len][0x02][<fory binary bytes>]`

## wire_type registration

For Fory XLANG mode, every dataclass exchanged over the wire must be
registered with a `@wire_type("namespace/TypeName")` decorator. The
wire type string is part of the contract identity hash and must be
identical across all bindings.

```python
@wire_type("mission_control/StatusRequest")
@dataclass
class StatusRequest:
    subsystem: str = ""
```

JSON mode does not use wire types — it relies on field names.

## Naming conventions (wire compatibility)

| Category | Convention | Example |
|----------|-----------|---------|
| Wire type strings | `namespace/TypeName` | `mission_control/StatusRequest` |
| StreamHeader fields | camelCase | `serializationMode`, `callId` |
| RpcStatus fields | camelCase | `detailKeys`, `detailValues` |
| Admission JSON | snake_case | `root_pubkey`, `credential_type` |
| Fory field names | Match the class field names | Language-specific |
| JSON field names | Match the class field names | Language-specific |

The StreamHeader/CallHeader/RpcStatus field names are camelCase on the
wire because the Python protocol.py dataclasses use camelCase field names
(not Pythonic, but necessary for wire compat).

## Performance notes

- Hoist codec imports to module level. Dynamic `from aster.codec import ...`
  inside hot-path functions adds measurable latency.
- ForyCodec maintains internal state (type registry, compressor context).
  Create one per server, not one per call.
- JSON codec is stateless — safe to create per-stream if needed.

## Invariants confirmed by chaos tests

- Decompression bomb rejected (`test_g5_decompression_bomb_rejected`)
- Corrupt payload produces error trailer (`test_g12_corrupt_payload_produces_error_trailer`)
- 58/58 cross-language matrix passes (Python↔TS, all codec combos)

## Implementation checklist for new bindings

- [ ] Implement at least one of: Fory XLANG codec, JSON codec
- [ ] Set `serializationMode` in StreamHeader based on codec choice
- [ ] Server: sniff first byte of StreamHeader payload for codec detection
- [ ] Server: reject unsupported codecs with INVALID_ARGUMENT trailer
- [ ] All error trailers use the client's requested codec
- [ ] Decompression bomb protection with `MAX_DECOMPRESSED_SIZE` (16 MiB)
- [ ] `wire_type` registration for all exchanged dataclasses (XLANG only)
- [ ] Advertise `serialization_modes` in ServiceSummary at admission
- [ ] Client: auto-select codec based on server's advertised modes
- [ ] Compression orthogonal to codec (COMPRESSED flag)

## Key files

| Binding | File | Entry point |
|---------|------|-------------|
| Python | `codec.py:319` | `ForyCodec` class |
| Python | `json_codec.py:40` | `JsonProxyCodec` class |
| Python | `server.py:365` | First-byte sniff |
| Python | `codec.py:576` | `_safe_decompress()` |
| TS | `codec.ts:83` | `JsonCodec` class |
| TS | `codec.ts:70` | `zstdDecompress()` |
| TS | `server.ts:173` | First-byte sniff |
