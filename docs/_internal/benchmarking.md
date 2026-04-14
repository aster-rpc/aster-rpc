# Benchmarking Guide

How to run the Aster benchmarks and what the numbers mean. For *why* Python is slower than TS and what to do about it, see [`INVESTIGATING_PYTHON_PERF.md`](INVESTIGATING_PYTHON_PERF.md) — that doc is authoritative on the bottleneck and the options. This one is just operational.

## Quick start

Start a server in one terminal, run the benchmark in another. Mix languages to test cross-language performance.

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

## Current baseline (2026-04-10, Apple M2, local loopback)

```
Python → Python:
  Unary (getStatus)         ~640 req/s   p50=1.56ms  (with reactor + uvloop)
  Client stream (10,000)    ~6,400 msg/s
  Concurrent (100)          ~1,950 req/s
  Memory: start=39.8MB  end=95.9MB

TS → TS (bun):
  Unary (getStatus)         ~2,400 req/s  p50=0.41ms
```

Gap is ~3.75× on unary throughput, ~3.8× on p50 latency. The gap is structural and the diagnosis is in [`INVESTIGATING_PYTHON_PERF.md`](INVESTIGATING_PYTHON_PERF.md) — short version: it is *not* in the FFI layer (the reactor already extracted that win), it is in the single asyncio event loop dispatching every handler under one Python thread.

These numbers predate the reactor going live by two days and should be re-measured the next time someone runs a focused perf pass. The reactor reduced FFI crossings but did not move the dispatch fan-out, so headline req/s sits in the same neighbourhood.

## Why client streaming is ~10× faster than unary

Client streaming sends N messages over one already-open stream. The per-message cost is just encode + write (one yield per message), versus unary which pays for `open_bi` + multiple writes + multiple reads + decode (~9 yields per call). Most of the unary cost is the dispatch overhead amortized once per call rather than once per stream. Streaming amortizes it over N.

This is also why "concurrent 100" beats "sequential 1000" by ~3×: 100 parallel streams give the tokio multi-thread runtime something to chew on instead of waiting on one Python task at a time.

## Environment notes

- Always benchmark on the same machine (results vary 2-3× between M1/M2/Intel).
- Close other CPU-intensive processes.
- Run at least 3 times and take the median.
- Local loopback eliminates network variance but also hides latency improvements that matter in production (relay, NAT traversal). At ~1ms of real network RTT the Python/TS gap shrinks substantially because asyncio dispatch overhead stops dominating.
- The Rust side runs a multi-threaded tokio runtime (`new_multi_thread`, `num_cpus` workers — see `bindings/python/rust/src/lib.rs:50`). The Python side runs one asyncio event loop in one thread. The fan-out asymmetry is the headline.
- uvloop is auto-installed at `import aster` time on Linux/macOS. Opt out with `ASTER_NO_UVLOOP=1`. Contributes ~20% on top of stock asyncio; not the dominant lever.

## See also

- [`INVESTIGATING_PYTHON_PERF.md`](INVESTIGATING_PYTHON_PERF.md) — current state of the Python perf investigation, the actual remaining bottleneck, and the options for closing the gap.
- [`reactor-ffi-guide.md`](reactor-ffi-guide.md) — C ABI contract for the reactor; relevant if benchmarking a new language binding.
- [`aster-java-fory-threading.md`](aster-java-fory-threading.md) — how Java handles dispatch fan-out with virtual threads + a thread-pool codec. The reference design for the Python service-to-thread story.
