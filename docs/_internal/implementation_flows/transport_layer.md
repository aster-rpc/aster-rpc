# Transport Layer

How to go from an iroh Endpoint to framed RPC streams. This is the glue
between the iroh QUIC transport and the Aster protocol layer -- the part
that isn't covered by the other flow docs.

**Spec:** Aster-SPEC.md §6, §8

## Overview

Aster uses iroh's QUIC transport. Each RPC call (or session) is a single
bidirectional QUIC stream. The transport layer's job:

1. Open/accept QUIC connections with the correct ALPN
2. Open/accept bidirectional streams
3. Write the first frame (HEADER + StreamHeader)
4. Read/write subsequent frames using the framing protocol
5. Close streams cleanly

## ALPNs

Aster uses multiple ALPNs to separate concerns:

| ALPN | Purpose | Who opens |
|------|---------|-----------|
| `aster/1` | RPC streams | Client (consumer) |
| `aster-consumer-admission/1` | Consumer admission handshake | Client |
| `aster-producer-admission/1` | Producer admission | Producer |

The server registers all needed ALPNs at endpoint creation time.

## Connection lifecycle

### Server side

```python
# 1. Create endpoint with ALPNs
endpoint = IrohNode.memory_with_alpns(["aster/1", "aster-consumer-admission/1"])

# 2. Accept connections in a loop
while serving:
    connection = await endpoint.accept()
    alpn = connection.alpn()
    
    if alpn == "aster-consumer-admission/1":
        # Route to admission handler
        handle_admission(connection)
    elif alpn == "aster/1":
        # Route to RPC handler
        handle_rpc_connection(connection)

# 3. For each RPC connection, accept streams
async def handle_rpc_connection(conn):
    while True:
        send, recv = await conn.accept_bi()
        # Each stream is one RPC call (or one session)
        handle_stream(send, recv)
```

**Python:** `server.py` `Server._accept_loop()` and `Server._connection_loop()`.
**TypeScript:** `server.ts` `RpcServer.serve()` and `RpcServer.handleConnection()`.

### Client side

```python
# 1. Connect to server
connection = await endpoint.connect(node_addr, "aster/1")

# 2. Open a bidi stream for each RPC call
send, recv = await connection.open_bi()

# 3. Write HEADER frame with StreamHeader
header = StreamHeader(service="Echo", method="echo", version=1, ...)
payload = codec.encode(header)
await write_frame(send, payload, flags=HEADER)

# 4. Write request frame
request_payload = codec.encode(request)
await write_frame(send, request_payload, flags=0)

# 5. Read response frame
frame = await read_frame(recv)
payload, flags = frame
response = codec.decode(payload)

# 6. Read trailer frame
frame = await read_frame(recv)
payload, flags = frame
# flags & TRAILER should be true
status = codec.decode(payload)
```

**Python:** `transport/iroh.py` `IrohTransport.call_unary()`.
**TypeScript:** `transport/iroh.ts`.

## Framing protocol

Every frame on the wire:

```
[4 bytes: payload_length + 1, little-endian u32]
[1 byte:  flags]
[N bytes: payload]
```

The length field includes the flags byte. So `length = len(payload) + 1`.

See `conformance/vectors/framing.json` for encode/decode/error vectors.

### write_frame

```python
async def write_frame(send, payload: bytes, flags: int):
    frame_len = len(payload) + 1  # +1 for flags byte
    send.write_all(frame_len.to_bytes(4, 'little'))
    send.write_all(bytes([flags]))
    send.write_all(payload)
```

### read_frame

```python
async def read_frame(recv) -> tuple[bytes, int] | None:
    length_bytes = await recv.read_exact(4)
    frame_len = int.from_bytes(length_bytes, 'little')
    if frame_len == 0:
        raise FramingError("zero-length frame")
    if frame_len > MAX_FRAME_SIZE:
        raise FramingError("frame too large")
    flags_byte = await recv.read_exact(1)
    flags = flags_byte[0]
    payload = await recv.read_exact(frame_len - 1)
    return (payload, flags)
```

**Python:** `framing.py` `write_frame()` and `read_frame()`.
**TypeScript:** `framing.ts` `writeFrame()` and `readFrame()`.

## First frame: HEADER

The first frame on every stream has the HEADER flag (0x04) set. Its
payload is a serialized StreamHeader. The server uses this to route
the stream to the correct service and method.

```
[length][HEADER flag][StreamHeader payload]
```

See `conformance/vectors/protocol-payloads.json` for StreamHeader vectors
in both JSON and XLANG encoding.

## Stream patterns

### Shared (stream-per-call)

One QUIC bidi stream per RPC call:

```
Client                          Server
  |-- HEADER(StreamHeader) -->    |
  |-- request data frame -->      |
  |<-- response data frame --     |
  |<-- TRAILER(RpcStatus) --      |
  |-- send.finish() -->           |
```

### Session (stream-per-instance)

One QUIC bidi stream per session. Multiple calls multiplexed:

```
Client                              Server
  |-- HEADER(StreamHeader, method="") -->  |
  |-- CALL(CallHeader) -->                 |
  |-- request frame -->                    |
  |<-- response frame --                   |  (unary: no trailer)
  |-- CALL(CallHeader) -->                 |
  |-- request frame -->                    |
  |<-- response frames --                  |
  |<-- TRAILER(OK) --                      |  (streaming: trailer)
  |-- send.finish() -->                    |  (session close)
```

## Gate 0: Connection-level filtering

Before any streams are opened, the server can reject connections at the
QUIC handshake layer using Gate 0 (the admitted-set hook). Only peers
that have been admitted through consumer admission can open RPC streams.

**Python:** `MeshEndpointHook.run_hook_loop()` in `high_level.py`.
**TypeScript:** Gate 0 hook loop in `high-level.ts`.

## Performance notes

- Reuse connections across multiple RPC calls. Don't connect per call.
- For session-scoped services, one stream handles all calls. Don't open
  new streams for each call.
- Hoist module-level imports. Dynamic imports in the stream handler hot
  path (e.g. `from aster.codec import ...`) add measurable latency on
  every call.
- The iroh endpoint manages connection pooling internally.

## Implementation checklist for new bindings

- [ ] Register correct ALPNs at endpoint creation
- [ ] Route connections by ALPN (admission vs RPC)
- [ ] Accept bidi streams in a loop per connection
- [ ] Client: open bidi stream, write HEADER frame first
- [ ] Implement write_frame / read_frame per framing spec
- [ ] Validate against `conformance/vectors/framing.json`
- [ ] Validate protocol payloads against `conformance/vectors/protocol-payloads.json`
- [ ] Handle stream close (`send.finish()`) on both sides
- [ ] Respect `MAX_FRAME_SIZE` (16 MiB) on read
- [ ] Apply `DEFAULT_FRAME_READ_TIMEOUT_S` (30s) on read_frame
- [ ] Gate 0 hook loop for connection-level filtering (server)

## Key files

| Binding | File | Entry point |
|---------|------|-------------|
| Python | `framing.py` | `write_frame()`, `read_frame()` |
| Python | `transport/iroh.py` | `IrohTransport` |
| Python | `server.py` | `Server._accept_loop()` |
| TS | `framing.ts` | `writeFrame()`, `readFrame()` |
| TS | `transport/iroh.ts` | IrohTransport |
| TS | `server.ts` | `RpcServer.serve()` |
| Conformance | `conformance/vectors/framing.json` | Frame encode/decode vectors |
| Conformance | `conformance/vectors/protocol-payloads.json` | StreamHeader/CallHeader/RpcStatus vectors |
