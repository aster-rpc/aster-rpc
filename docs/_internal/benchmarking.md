# Benchmarking Guide

## Quick start

Start a server in one terminal, run the benchmark in another. Mix languages
to test cross-language performance.

### Python server

```bash
cd /path/to/iroh-python

# Option A: run as module
uv run python -m examples.python.mission_control.server

# Option B: inline (if module imports fail)
uv run python -c "
import asyncio
from aster import AsterServer
from examples.python.mission_control.services import MissionControl, AgentSession

async def main():
    srv = AsterServer(services=[MissionControl(), AgentSession()])
    await srv.start()
    print(srv.address, flush=True)
    await srv.serve()

asyncio.run(main())
"
```

### TypeScript server

```bash
cd bindings/typescript
bun run ../../examples/typescript/missionControl/server.ts
```

### Run benchmarks

```bash
# Python client
uv run python examples/python/mission_control/benchmark.py <address>

# TypeScript client
cd bindings/typescript
bun run ../../examples/typescript/missionControl/benchmark.ts <address>
```

## Cross-language matrix

Run all four combinations to identify language-specific bottlenecks:

| Server | Client | What it tests |
|--------|--------|---------------|
| Python | Python | Same-language baseline |
| Python | TypeScript | TS client + Py server interop |
| TypeScript | Python | Py client + TS server interop |
| TypeScript | TypeScript | Same-language baseline |

## What gets measured

| Metric | How | Notes |
|--------|-----|-------|
| **Unary throughput** | 1000 sequential getStatus calls | Single-stream, measures per-call overhead |
| **Unary latency** | p50, p90, p99 per call | Includes serialization + QUIC round-trip |
| **submitLog throughput** | 1000 sequential calls | Same as getStatus but with larger payload |
| **Client streaming** | 100 / 1K / 10K metrics | Amortizes per-call overhead over stream |
| **Concurrent unary** | 10 / 50 / 100 parallel calls | Tests multiplexing + server concurrency |
| **Memory** | RSS at start and end | Measures client-side memory growth |

## Baseline results (2026-04-10, Apple M2, local loopback)

### Python server + Python client

```
  Unary (getStatus)         555 req/s   p50=1.77ms  p90=1.99ms  p99=2.46ms
  Unary (submitLog)         562 req/s   p50=1.76ms  p90=1.98ms  p99=2.24ms
  Client stream (  100)     4,721 msg/s   21.2ms total
  Client stream (1,000)     5,497 msg/s   181.9ms total
  Client stream (10,000)    5,554 msg/s   1800.7ms total
  Concurrent ( 10)         1,314 req/s
  Concurrent ( 50)         1,514 req/s
  Concurrent (100)         1,635 req/s
  Memory: start=39.8MB  end=95.9MB  delta=+56.1MB
```

## Analysis: why 550 req/s unary is low

550 req/s unary with p50=1.77ms on localhost is significantly below what
you'd expect from gRPC (10-50K req/s) or even HTTP/2 (5-20K req/s) on
the same machine. The bottleneck is likely one or more of:

### Suspected bottlenecks (investigate in order)

**Note:** The proxy client reuses the QUIC connection — it does NOT open
a new connection per call. `_rpc_conn_for()` caches the connection, and
each call opens a new QUIC **stream** (`open_bi()`), which is cheap.

The 1.8ms per unary call breaks down across 7 sequential async operations:

1. `open_bi()` — open a new QUIC bidi stream (~0.1ms)
2. Encode + write StreamHeader frame (JSON + framing, ~0.1ms)
3. Encode + write request payload frame (~0.1ms)
4. `send.finish()` — signal end of send (~0.05ms)
5. Read response frame from QUIC (~0.3ms, includes server processing)
6. Read trailer frame (~0.2ms)
7. Decode response + trailer (~0.1ms)

Each step is an `await` that yields to the asyncio event loop, adding
scheduling overhead (~10-20µs per yield). With 7 yields per call, that's
~100µs of pure scheduling per call, or ~5% of the total.

**Why client streaming is 10x faster:** It amortizes steps 1-4 over N
messages. After the first frame, each additional metric is just
encode + write (~0.2ms amortized).

**Why concurrent unary is 3x faster:** It parallelises the 7-step
pipeline across multiple streams. QUIC multiplexes well — the server
processes streams concurrently on the Tokio runtime.

### Other factors

- **JSON serialization.** `JSON.stringify` → encode → write, then
  read → decode → `JSON.parse` on every message. For small payloads
  (~100 bytes) this is ~5µs but adds up at 1000 sequential calls.

- **QUIC encryption.** Every packet is TLS-encrypted (ChaCha20-Poly1305
  on ARM). On localhost this is pure overhead — ~2-3µs per packet.

- **UUID generation.** Each call generates a UUID for `callId`. On
  Python this uses `uuid.uuid4()` which reads `/dev/urandom`. Could
  be replaced with a counter for non-distributed use.

### What "good" looks like

| Pattern | Target (localhost) | Why |
|---------|-------------------|-----|
| Unary | 5,000-10,000 req/s | Stream reuse or connection pooling |
| Client streaming | 50,000+ msg/s | Already amortized, mostly I/O bound |
| Concurrent unary | 10,000+ req/s | Multiplexed QUIC streams |

### Next steps for optimization

- [ ] **Profile with py-spy** to identify hot paths in the Python client
- [ ] **Benchmark typed client vs proxy** to isolate proxy overhead
- [ ] **Benchmark with Fory native codec** vs JSON to isolate serialization
- [ ] **Test stream reuse** — can multiple unary calls share one bidi stream?
- [ ] **Test connection pooling** — does reusing connections help?
- [ ] **Compare with raw QUIC** — benchmark `open_bi` + write + read without
      Aster framing to establish the QUIC floor

## Environment notes

- Always benchmark on the same machine (results vary 2-3x between M1/M2/Intel)
- Close other CPU-intensive processes
- Run at least 3 times and take the median
- Local loopback eliminates network variance but also hides latency
  improvements that matter in production (relay, NAT traversal)
- The Python server runs on a single Tokio runtime thread; the Python
  client runs on asyncio's event loop — both are single-threaded
