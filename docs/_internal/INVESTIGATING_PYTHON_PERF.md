# Python Performance — Current State

**Last updated:** 2026-04-14
**Status:** Current. Supersedes the 2026-04-13 pre-fast-path numbers.

## TL;DR

Two waves of fixes have landed:

1. **Server reactor** (2026-04-12) — `core/src/reactor.rs` + `bindings/python/aster/server.py`. Moved the server accept-loop and frame reader off the Python thread and onto the multi-threaded tokio runtime. Each inbound call now arrives at Python as a fully-formed call descriptor in **one** PyO3 crossing instead of ~9.
2. **Client unary fast path** (2026-04-14, commit `c147a0c`) — `AsterCall.unary_fast_path` in `bindings/python/rust/src/call.rs`. Collapses the client unary path from four PyO3 crossings (send_header + send_request + recv_frame(response) + recv_frame(trailer)) to **one**. Mirrors the Java FFI `aster_call_unary` fast path.

**After both waves**, Python unary is **~1,500 req/s** on the MC benchmark, **p50 ~0.66 ms**. The remaining gap to Java (~3.2k req/s) is no longer in the call pipeline. It is in the **dispatch fan-out**: every inbound call goes through `asyncio.create_task(...)` onto a single asyncio event loop running in one Python thread. The Rust side is already multi-threaded (`tokio::runtime::Builder::new_multi_thread()` in `bindings/python/rust/src/lib.rs:50`, defaulting to `num_cpus` workers) and has cores to spare — Python is feeding it through a one-lane funnel.

Closing the remaining gap requires service-to-thread dispatch, ideally under free-threaded Python (3.13t+). See *Options* below.

## Current numbers

Mission Control example, in-process loopback, JSON codec, 1000-call unary loop after a 20-call warmup. Apple M2. Measured 2026-04-14 on commit `c147a0c`.

### Sequential unary (getStatus)

| Stack | req/s | p50 | vs 2026-04-13 |
|---|---:|---:|---|
| Python → Python | **1,507** | 0.66 ms | +72% (was 877) |
| Python → TS      | **1,447** | 0.67 ms | +32% (was 1,100) |
| TS → Python      | **1,750** | 0.56 ms | +77% (was 990) |
| TS → TS          | **1,717** | 0.57 ms | +58% (was 1,085) |
| **Java → Java (reference)** | **3,233** | 0.29 ms | unchanged |

### Concurrent unary (Promise.all / asyncio.gather)

| Stack | conc-10 | conc-50 | conc-100 |
|---|---:|---:|---:|
| Python → Python | 4,013 r/s | 5,299 r/s | 5,942 r/s |
| TS → TS (dynamic proxy) | 3,666 | 5,706 | 6,164 |
| TS → TS (typed) | 5,652 | 5,567 | 6,222 |
| **Java → Java** | 1,969 | **11,728** | **14,291** |

Gap to Java, sequential: **~2.1×**. Gap to Java, concurrent-100: **~2.3×** (dynamic) / **~2.3×** (typed).

### How we got here (2026-04-14)

- **`AsterCall.unary_fast_path` (commit `c147a0c`, fix #2 in the 04-14 perf pass).** One PyO3 crossing for a full unary. Previously four: `send_header`, `send_request`, `recv_frame(response)`, `recv_frame(trailer)`. Each crossing bounced the asyncio loop into Rust and back. Eliminating three of them was measured as the main win.
- **`typing.get_type_hints` caching in `_dict_to_dataclass` (fix #4).** Profile showed `get_type_hints` at ~90 µs per call on a small dataclass, re-evaluated on every JSON response decode because forward-ref resolution isn't memoized by the stdlib. Cached per-class. Minor on its own but it compounds with the fast path.
- **v1 Python `IrohTransport.unary` rewritten** to build the header+request frame pair in Python and hand it to the native fast path in a single `await`. The `_CallDriver` path is kept for server-stream / client-stream / bidi where per-frame awaits are still the right primitive.

The same four fixes were landed for TypeScript on the same commit. TS sees bigger absolute gains because v1 TS had a catastrophic concurrent collapse (303 r/s at conc-50) that the fast path fully resolved.

## Where the time actually goes

cProfile of 500 unary calls, captured on 2026-04-14 against the Python MC server **after** the fast-path landed. Self time, top 10 entries (trimmed):

```
  tottime  percall  function
    5.909    0.001  {method 'control' of 'select.kqueue' objects}
    0.021    0.000  asyncio/base_events.py:1962(_run_once)
    0.017    0.000  {method 'run' of '_contextvars.Context' objects}
    0.011    0.000  {method 'send_frame' of 'builtins.AsterCall'}  (streaming path only)
    0.011    0.000  {method 'recv_frame' of 'builtins.AsterCall'}  (streaming path only)
    0.009    0.000  asyncio/selectors.py:540(select)
    0.009    0.000  {method 'recv' of '_socket.socket'}
```

**95% of wall time is in `kqueue.control`** — the asyncio event loop parking between suspensions. Only ~5% is in our code. FFI crossings (the fast path's single `unary_fast_path` hop and the streaming path's `send_frame`/`recv_frame` methods) are **sub-1%** each. The reactor extracted the server-side crossings; the fast path extracted the client-side ones; what's left is structural asyncio scheduling cost.

Per-call math: ~0.66 ms measured p50 / ~1,500 req/s. Of that:
- **~0.4-0.5 ms** is asyncio scheduling: `_run_once` + `kqueue.control` + context-var work between the `await` on `unary_fast_path` and the response landing. Each `await` costs ~100-150 µs in stock asyncio, ~60-80 µs under uvloop. The fast path brings us down to ONE `await` per unary call, so this is already the minimum we can hit without changing the event-loop model.
- **~0.05-0.1 ms** is Fory/JSON codec work: `encode_compressed` (request), `decode_compressed` (response), `_dict_to_dataclass` walk. Caching type hints (fix #4 on 2026-04-14) shaved ~50 µs off this budget.
- **~0.05-0.1 ms** is the actual Rust work: pool acquire (LIFO, cheap), `write_all`, read loop until trailer, pool release.

The remaining lever is **not the call path** — it's **how many concurrent calls can run in parallel on distinct Python threads**. With GIL-bound CPython that number is 1; with free-threaded CPython it scales to N cores. See *Options* below.

## What was previously believed and why it was wrong

Two earlier framings need to be retired:

1. **"FFI crossings are the bottleneck."** Correct for the pre-reactor architecture (April 10-11), and the analysis that produced this conclusion is exactly what motivated building the reactor. The mistake was carrying the conclusion forward after 2026-04-12 when the reactor shipped — at that point the FFI cost was no longer the dominant term, but assumed-knowledge memory and stale doc text continued to cite it. The reactor took the win the older docs predicted; the bottleneck moved.
2. **"Scale Python by forking N workers behind a virtual address" (the ASGI/uvicorn analogy).** This does not apply to iroh. Each iroh endpoint has its own NodeId, and the cryptographic identity *is* the socket — there is no SO_REUSEPORT equivalent. Forking creates N distinct producers from the network's point of view, not N instances of the same one. The scaling axis is not "more endpoints," it is "more threads servicing one endpoint." Anything that assumes the web-framework worker model applies to Aster is wrong by construction.

The "ring buffer / Python bottleneck is QUIC" entry in the old design notes is also wrong for the same reason — it predates both the reactor and the cProfile pass that pinned the cost on asyncio scheduling, not transport.

## What's already shipped

- **Multi-threaded tokio runtime.** `bindings/python/rust/src/lib.rs:50`. One shared multi-thread runtime per Python process, sized to `num_cpus`. I/O, accepts, frame reads/writes already use every core.
- **uvloop auto-install** on Linux/macOS at `import aster` time. ~20% gain. Opt-out with `ASTER_NO_UVLOOP=1`.
- **Write batching.** `bindings/python/aster/transport/iroh.py` — `write_all + finish` instead of `write + write + finish`. ~6%.
- **`_ProxyMethod` cache + empty `callId`** on shared streams. ~2% combined.
- **Server reactor.** `core/src/reactor.rs` + `bindings/python/aster/server.py:266-312`. Server-side accept loop, per-connection tasks, frame reading, and call delivery all happen in Rust on the tokio pool. Python only sees fully-formed call descriptors arriving via `aster_reactor_poll`. Sibling bindings (Java, TS) consume the same C ABI.
- **Single-call PyO3 dispatch (server).** Each call from the reactor is delivered to Python in one FFI crossing rather than the ~9 of the pre-reactor stream-orchestration path.
- **Unary fast path (client).** `AsterCall.unary_fast_path` in `bindings/python/rust/src/call.rs` — collapses acquire + send_header + send_request + recv_frame×2 + release into one PyO3 crossing. The Rust side does pool acquire → single `write_all` → read loop until FLAG_TRAILER → pool release, all inside one `future_into_py`. Landed 2026-04-14 in commit `c147a0c`. Measured +72% on `py→py` unary getStatus (877 → 1,507 req/s).
- **`get_type_hints` caching.** `bindings/python/aster/json_codec.py` — memoizes `typing.get_type_hints(cls)` and `dataclasses.fields(cls)` per class. `get_type_hints` re-evaluates string forward refs every call without this cache, costing ~90 µs on a small dataclass. Landed in `c147a0c`.

## What's not yet shipped — the actual remaining bottleneck

`serve_reactor()` does this on every inbound call:

```python
asyncio.create_task(self._dispatch_reactor_call(...))
```

One asyncio loop. One Python thread. Under standard CPython the GIL serializes bytecode across that thread regardless of how many tokio workers are feeding it. 100 concurrent calls = 100 tasks on one loop = serialized through one scheduler.

`docs/_internal/aster-java-fory-threading.md` documents the same problem solved cleanly on the Java side: `Executors.newVirtualThreadPerTaskExecutor()` with a `ThreadPoolFory` codec sized at `4 × Runtime.availableProcessors()` to avoid contention on the shared serializer. Java has this option because it has no GIL. Python doesn't, *yet*.

## Options for closing the gap

### Option α — Service-to-thread under standard CPython

Each `AsterService` registered on the server runs on its own OS thread with its own asyncio event loop. The reactor's pump task dispatches each inbound call to the right service's loop based on the routing it already does (`contract_id` + service version → service handle).

**What you gain:** scheduler isolation. A slow handler in service A no longer starves service B's loop. Cleaner mental model — each service is a self-contained worker.
**What you don't gain:** real multi-core parallelism. The GIL still serializes Python bytecode across those threads. CPU-bound handlers don't get faster. I/O-bound handlers see modest wins from reduced contention on the single loop.
**Realistic improvement:** 1.5-2× on mixed workloads, less on pure unary throughput.
**Cost:** real architectural change to `server.py` and the reactor pump's dispatch — 3-5 days of focused work plus tests.

### Option β — Service-to-thread under free-threaded Python (3.13t+)

Same code as α, built and tested on the no-GIL interpreter. Per-service threads now run actually-in-parallel on different cores.

**What you gain:** N services ≈ N× throughput on the same endpoint, bounded by how many cores Python can saturate. This is the Java story in Python, with no fork required.
**Cost:** build wheels for free-threaded Python (separate ABI tag `cp313t`), confirm PyO3 + `pyo3-async-runtimes` work cleanly under no-GIL (mostly they do as of late 2025, but module-init has sharp edges), and accept that users who want full perf need a specific interpreter build.
**Strategic angle:** in 2026, free-threaded Python is no longer experimental but uptake is still early. Being the first serious RPC framework built around it is a real positioning story for a launch — *"Aster is the first RPC framework designed for free-threaded Python."*

### Option γ — Bypass asyncio in the dispatch path entirely

Run handlers from a synchronous worker pool reading directly off the reactor queue, in the same shape Java does. This collapses the per-call asyncio scheduling overhead — there is no event loop in the dispatch path anymore, just threads pulling work from a queue and calling handlers.

**What you gain:** removes the 75% kqueue dominance for the dispatch path. Mixes well with α and β.
**What you lose:** handlers can no longer freely `await` arbitrary asyncio things — they're running on a worker thread, not an event loop. Either we provide a way to hand a coroutine back to an event loop, or we split the API into "fast unary handlers" (sync, run on worker pool) and "general async handlers" (async, run on the legacy serve path).
**Cost:** ergonomic regression on handler authoring. Probably a non-starter unless the API split is acceptable.

## Recommendation

α and β are the same code shipped under two interpreters. Build service-to-thread dispatch once. Validate correctness under standard CPython and measure the (modest) gain. Then test under free-threaded 3.13t and measure the (larger) gain. If the free-threaded number is what we hope, that becomes the documented production interpreter for Aster Python.

This is **not near-free.** Realistic estimate: 5-8 days of focused work including tests under both interpreters, plus changes to how services are registered with the reactor. The "near-free" framing applies to *forking N processes behind a load balancer*, which doesn't work here. There is no near-free path to multi-core Python under one iroh endpoint.

Before greenlighting this, weigh it against the positioning work — at the time of writing, Aster's bigger problem is "people don't know what it's for," not "Python is 4× slower than TS on loopback." Day-zero use cases (operator CLI, agent control plane, mesh admin) are not bound by the current Python throughput. The perf gap matters for *workloads we don't yet have demand for*.

## What's explicitly not on the table

- **Multi-process producer fan-out** — wrong scaling axis. NodeId is per-endpoint. Forking creates distinct producers, not load-balanced workers.
- **Replacing asyncio with a different event loop** — uvloop is already on. The remaining cost is structural to the single-loop dispatch model, not to the loop implementation.
- **More PyO3-level optimization** — the reactor extracted that win.

## Reproducing

```bash
# Start servers (one per terminal)
PYTHONUNBUFFERED=1 uv run python -m examples.python.mission_control.server &
cd examples/typescript/missionControl && bun run server.ts &

# Benchmark
uv run python -m examples.python.mission_control.benchmark <python-addr>
bun run examples/typescript/missionControl/benchmark.ts <ts-addr>

# Profile (Python)
uv run python -c "
import cProfile, pstats, asyncio
from aster import AsterClient
async def main():
    c = AsterClient(address='aster1...')
    await c.connect()
    mc = c.proxy('MissionControl')
    for _ in range(20): await mc.getStatus({'agent_id': 'warmup'})
    pr = cProfile.Profile(); pr.enable()
    for i in range(500): await mc.getStatus({'agent_id': f'b{i}'})
    pr.disable()
    pstats.Stats(pr).sort_stats('cumulative').print_stats(20)
    await c.close()
asyncio.run(main())
"
```

## See also

- [`reactor-ffi-guide.md`](reactor-ffi-guide.md) — the C ABI contract for the reactor. Authoritative for any new binding.
- [`aster-java-fory-threading.md`](aster-java-fory-threading.md) — how Java solves the same dispatch-fan-out problem with virtual threads + a thread-pool codec. Reference design for the Python service-to-thread story.
- [`benchmarking.md`](benchmarking.md) — how to run the benchmarks. (Some analysis sections are pre-reactor and need a similar refresh — flag for follow-up.)
- [`v0.3-perf/PERF_FFI_CROSSINGS.md`](v0.3-perf/PERF_FFI_CROSSINGS.md) — the historical workspace that produced the reactor architecture. Useful as background, not as current state.
