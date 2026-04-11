# Reactor Architecture Design

## Status: DRAFT — ready for review before implementation

## What we learned (v0.3 experiments)

### Performance measurements

| Metric | Value | Source |
|--------|-------|--------|
| PyO3 FFI crossing cost | 48μs | GIL release + tokio spawn + call_soon_threadsafe |
| NAPI (TS) FFI crossing cost | ~5μs | No GIL |
| Java FFM crossing cost | ~5μs (est) | Virtual thread + downcall |
| Go CGo crossing cost | ~0.1μs (est) | Stack switch only |
| Fory XLANG encode (small obj) | 0.7μs | Python pyfory |
| Fory XLANG decode (small obj) | 0.5μs | Python pyfory |
| JSON encode (small obj) | 1.1μs | Python json |
| QUIC loopback RTT | ~200μs | iroh relay disabled |
| Crossing collapse result | 500 → 1,097 RPS | 8 crossings → 1 |
| Reactor result | 1,097 → 1,489 RPS | Server crossings 3 → 1 |
| Python-to-Java JSON mode | Working | End-to-end proven |

### Key findings

1. **Crossing count dominates for Python.** Reducing from 8 to 1
   crossing gave 2.2x. The reactor gave another 24%. Data format
   (Fory vs JSON vs struct) saves <2μs per call — noise against
   48μs crossing.

2. **For Java/Go, data format and allocation matter more.** The FFI
   crossing is cheap (5μs / 0.1μs), so per-call allocation, copy
   cost, and memory pressure under load become the bottleneck.

3. **Fory IDL is essential for cross-language schemas.** Hand-written
   types caused schema hash mismatches (Python `int` → int64, Java
   `int` → int32). The IDL compiler generates deterministic type IDs
   and correct field types. This likely caused the TS Fory failures
   that led to the JSON fallback.

4. **Fory serialization is wrong for in-process IPC.** Serializing
   ReactorCallData to pass it across a function boundary within the
   same process adds allocation and copy overhead for no benefit.
   Fory is for the wire (client ↔ server). IPC needs direct memory
   access.

5. **Zero-copy is about memory under load, not just throughput.**
   Per-call Vec<u8> clones compound: at 10K concurrent calls, that's
   10K allocations in flight, GC pressure in managed languages, and
   cache pollution from copying payloads. Bounded pre-allocated
   buffers give predictable latency under load.

## Design principles (informed by Aeron)

### 1. No allocation on the hot path

Pre-allocate all buffers at startup. The accept → read → route →
deliver → respond → write cycle must not allocate. This means:

- Ring buffer for call delivery (reactor → binding) — fixed slots
- Ring buffer or pre-allocated pool for response delivery
- QUIC recv buffers stay pinned; binding reads from them directly
- String fields (service name, method name) are offsets into the
  recv buffer, not owned copies

### 2. Bounded memory

The system's memory footprint is determined at startup by:
- Ring buffer capacity (number of in-flight calls)
- Max payload size per slot
- Number of connections

Under overload, new calls are rejected (backpressure) rather than
queued unboundedly. This is Aeron's "flow control at every layer"
principle applied to the reactor.

### 3. Single-writer principle

Each ring buffer has exactly one writer and one reader. No locks,
no CAS on the hot path. Coordination is via memory fences and
sequence counters:

- Rust reactor is the single writer to the call ring
- Language binding is the single reader
- Language binding is the single writer to the response ring
- Rust reactor is the single reader

### 4. Batch-friendly

The binding can drain multiple calls per wake-up. This amortizes
the wake-up cost (event loop notification for Python, poll return
for Java) across N calls. Critical for throughput under load.

### 5. Cache-line aligned

Ring buffer slots are aligned to cache line boundaries (64 bytes).
Metadata fields are packed into the first cache line. Payload data
follows. No false sharing between adjacent slots.

## Protocol field review

The StreamHeader should be minimal for constrained networks.

### Current fields

| Field | Type | Size | Question |
|-------|------|------|----------|
| service | string | variable | Required |
| method | string | variable | Required |
| version | int32 | 4 bytes | Required |
| callId | string | variable | Optional — only for tracing |
| deadlineEpochMs | int64 | 8 bytes | Oversized. See below |
| serializationMode | int32 | 4 bytes | Could be uint8 (4 modes) |
| metadataKeys | list[string] | variable | Optional — often empty |
| metadataValues | list[string] | variable | Optional — often empty |

### Proposed changes

**deadline**: Change from epoch milliseconds (int64) to relative
duration. Options:

| Encoding | Type | Max value | Precision | Wire size |
|----------|------|-----------|-----------|-----------|
| Relative seconds | uint16 | 18h 12m | 1s | 2 bytes |
| Relative deciseconds | uint16 | 1h 49m | 100ms | 2 bytes |
| Relative centiseconds | uint16 | 5m 27s | 10ms | 2 bytes |
| Fory varint seconds | int32 | 68 years | 1s | 1-4 bytes |

Recommendation: relative seconds as uint16. Covers all practical
RPC deadlines. 0 = no deadline. Saves 6 bytes per call on the wire.
The binding converts to absolute time on receipt.

**serializationMode**: uint8, not int32. There are 4 modes (XLANG,
NATIVE, ROW, JSON). Saves 3 bytes.

**callId**: Consider making this a uint32 sequence number instead of
a string UUID. 4 bytes vs 36 bytes. The binding can maintain a
mapping to trace IDs if needed.

**metadata**: Consider a single bytes field with a compact key-value
encoding instead of two parallel string lists. Or make metadata a
separate optional frame (most calls don't use it).

### Minimal header (common case)

For a simple unary call with no metadata, no deadline, no callId:

```
Current: ~60 bytes (Fory XLANG encoded StreamHeader with empty lists)
Proposed minimal: service(var) + method(var) + version(4) + mode(1) = ~20 bytes
```

## Proposed architecture

```
                    ┌─────────────────────────────┐
                    │         Rust Core            │
                    │                              │
  QUIC recv ───────►│  accept_loop                 │
                    │    ├─ read frames             │
                    │    │  (into pinned recv buf)  │
                    │    ├─ parse StreamHeader       │
                    │    │  (JSON or Fory XLANG)    │
                    │    ├─ registry lookup          │
                    │    ├─ REJECT → error trailer   │
                    │    │  (zero FFI crossings)    │
                    │    ├─ deadline check           │
                    │    ├─ rate limit check         │
                    │    └─ ACCEPT:                  │
                    │         write to call ring     │
                    │         (slot = metadata +     │
                    │          payload ptr/len)      │
                    │                              │
  response ring ───►│  response_handler             │
                    │    ├─ read from response slot  │
                    │    ├─ write to QUIC send       │
                    │    └─ advance sequence         │
                    └──────────────┬───────────────┘
                                   │ call ring
                    ┌──────────────▼───────────────┐
                    │      Language Binding         │
                    │                              │
                    │  poll / await (batch drain)   │
                    │    ├─ read N slots             │
                    │    ├─ for each slot:           │
                    │    │   ├─ read metadata        │
                    │    │   │  (fixed offsets)      │
                    │    │   ├─ read payload         │
                    │    │   │  (ptr into recv buf)  │
                    │    │   ├─ decode with Fory     │
                    │    │   ├─ run interceptors     │
                    │    │   ├─ invoke handler       │
                    │    │   ├─ encode response      │
                    │    │   └─ write to resp ring   │
                    │    └─ signal Rust              │
                    └──────────────────────────────┘
```

### Call ring slot layout

```
Cache line 0 (64 bytes) — metadata:
  [0]   u64  sequence         (for coordination)
  [8]   u32  call_id          (reactor-assigned)
  [12]  u32  flags            (is_routed:1, is_session:1, req_flags:8, ser_mode:4, pattern:3)
  [16]  u16  svc_offset       (offset into recv buffer for service name)
  [18]  u16  svc_len
  [20]  u16  method_offset
  [22]  u16  method_len
  [24]  i32  version
  [28]  u16  deadline_secs    (relative, 0 = none)
  [30]  u16  peer_id_offset
  [32]  u16  peer_id_len
  [34]  u16  metadata_len     (0 if no metadata)
  [36]  u32  header_buf_id    (BufferRegistry id, for release)
  [40]  u32  request_buf_id
  [44]  u32  reserved

Cache line 1 (64 bytes) — payload pointers:
  [64]  u64  header_ptr       (pointer into recv buffer)
  [72]  u32  header_len
  [76]  u32  _pad
  [80]  u64  request_ptr      (pointer into recv buffer)
  [88]  u32  request_len
  [92]  u32  _pad
  [96]  u64  response_sender  (handle for submit)
  [104] 24 bytes reserved
```

Total: 128 bytes per slot, 2 cache lines, fully aligned.
A 256-slot ring = 32KB — fits in L1 cache.

### Response ring slot layout

```
  [0]   u64  sequence
  [8]   u32  call_id          (matches the call)
  [12]  u32  response_len
  [16]  u64  response_ptr     (pointer to response frame bytes)
  [24]  u32  trailer_len
  [28]  u32  _pad
  [32]  u64  trailer_ptr
  [40]  u32  buf_id           (for release after write)
  [44]  20 bytes reserved
```

Total: 64 bytes, 1 cache line per response.

## Fory's role (clarified)

| Purpose | Use Fory? | Notes |
|---------|-----------|-------|
| Wire format (client ↔ server payloads) | Yes | StreamHeader, request, response, trailer |
| IDL (type definitions) | Yes | Single source of truth for all languages |
| In-process IPC (reactor → binding) | No | Use ring buffer with fixed layout |
| Protocol type registration | Yes | IDL-generated type IDs ensure cross-lang compat |

## C FFI API (refined)

```c
// Setup
iroh_registry_create(runtime, out_handle) → status
iroh_registry_add_service(runtime, registry, ...) → status

// Reactor lifecycle
iroh_reactor_create(runtime, node, registry, config, out_handle) → status
iroh_reactor_destroy(runtime, reactor) → status

// Call ring (language binding reads)
iroh_reactor_poll(runtime, reactor, slots[], max_count, timeout_ms) → count
// Returns filled ring slots directly — no event system, no allocation

// Response submission
iroh_reactor_submit(runtime, call_id, response_ptr, response_len,
                    trailer_ptr, trailer_len) → status

// Buffer management
iroh_buffer_release(runtime, buf_id) → status
```

Key change: `iroh_reactor_poll` replaces the event-based `next_call`.
It returns a batch of call descriptors in a pre-allocated array,
similar to `io_uring`'s completion queue drain. The language binding
provides the array; Rust fills it.

## Implementation phases

### Phase 1: Ring buffer core (Rust only, no FFI)
- Implement SPSC ring buffer with sequence coordination
- Call ring: Rust writes, test reader
- Response ring: test writer, Rust reads
- Benchmark: throughput and p99 latency vs tokio mpsc

### Phase 2: FFI integration
- `iroh_reactor_poll` batch drain API
- Buffer pinning (recv buffers stay alive until released)
- Zero-copy payload pointers in ring slots

### Phase 3: Python binding
- PyO3 wrapper for reactor_poll
- Batch processing in asyncio event loop
- Compare RPS and memory usage under load vs current reactor

### Phase 4: Java binding
- FFM wrapper for reactor_poll
- Kotlin coroutine dispatcher reading from ring
- Benchmark vs current CQ-based approach

### Phase 5: Protocol optimization
- Review and slim StreamHeader per the field analysis above
- Fory IDL update with optimized types
- Regenerate all language bindings

## Technical gotchas (from this session)

### Fory cross-language schema hash mismatch
Python `int` maps to Fory `int64`. Java `int` maps to `int32`. The
schema hash includes field types, so Python-serialized StreamHeader
can't be deserialized by Java. **Fix**: Use Fory IDL (`foryc`) to
generate types with explicit `int32`/`int64`. Field IDs (not names)
determine cross-language matching, so each language can use its own
naming convention.

### Fory IDL reserved words
`service`, `method`, and `message` are reserved in the IDL grammar.
Use alternatives: `svc`, `rpcMethod`, `msg`. The wire format uses
field IDs so the name difference doesn't affect interop.

### fory-core Rust crate panic on Vec<u8>
The `fory` Rust crate (0.16.0) panics in type meta encoding when
serializing structs containing `Vec<u8>` with arbitrary bytes:
`"Invalid character value for LOWER_SPECIAL decoding: 30"`. This is
a bug in the crate's LOWER_SPECIAL string encoding applied to byte
array metadata. **Workaround**: Don't use Fory for in-process IPC
with raw byte payloads — use direct memory access instead.

### Java FFM stale .so on macOS
After `maturin develop`, the `.abi3.so` in the Python source tree
can be stale. `scripts/build.sh` builds a wheel but doesn't always
install it over the editable install. Running `uv run maturin develop`
directly is more reliable. For Java, the dylib at
`target/release/libaster_transport_ffi.dylib` is loaded by
`IrohLibrary` — rebuild with `cargo build --manifest-path ffi/Cargo.toml --release`.

### Java FFM argument type precision
C function `i32` parameters need `ValueLayout.JAVA_INT` in the
FunctionDescriptor, not `JAVA_LONG`. Getting this wrong causes
silent argument corruption. Match every parameter type exactly to
the C signature.

### Java Fory package rename
The Maven artifact moved from `org.apache.fury:fury-core` to
`org.apache.fory:fory-core`. The Java package is `org.apache.fory`,
class is `Fory` (not `Fury`). The version 0.16.0 matches the Rust
crate version.

### IrohNode accessors are package-private
`IrohNode.nodeHandle()` and `runtime()` were package-private
(`com.aster.node`). The reactor server in `com.aster.server` needs
them — make them public.

### Python pyfory.dataclass vs @dataclass
`@pyfory.dataclass` replaces both `@dataclass` and `@wire_type`.
It uses `pyfory.field(id=N)` instead of `dataclasses.field()`.
The `__wire_type__` attribute must be set manually after class
definition for backward compatibility with the JSON codec sniffing.

### Segfault on relay reconnect after server crash
When the Java server crashes (e.g., Fory deserialization panic) and
the Python client tries to reuse the relay connection, the Python
process segfaults (exit 139). Root cause unclear — likely corrupted
QUIC connection state. Needs investigation.

### iroh_registry_add_service version parameter
The C function takes `version: i32` as a separate parameter between
`name_len` and `scoped_ptr`. The Java downcall must pass it as
`JAVA_INT` in the correct position. Easy to get wrong because
`usize` (name_len) and `i32` (version) are different widths.

## What NOT to build

- Custom transport layer (we have QUIC/iroh)
- Custom congestion control (QUIC handles this)
- Userspace networking / kernel bypass (overkill for our use case)
- Multi-producer ring buffer (single reactor writer is sufficient)
