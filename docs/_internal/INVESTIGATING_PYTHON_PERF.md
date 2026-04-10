# Investigating Python Performance

Notes from the 2026-04-10 perf pass on the Aster Python binding.

## Setup

Mission Control example, in-process loopback (server and client on the
same host), JSON serialization mode (TS uses JSON-only; we matched for
fairness). 1000-call unary loop after a 20-call warmup.

## Baseline measurements

| Stack | Unary req/s | Unary p50 | Notes |
|-------|-------------|-----------|-------|
| Python -> Python (proxy) | 537 | 1.85ms | stock asyncio |
| Python -> Python (typed) | 538 | 1.85ms | typed and proxy identical |
| TS -> TS (proxy) | 2238 | 0.41ms | bun + libuv |
| TS -> TS (typed) | 2439 | 0.38ms | typed and proxy identical |

The 4.5x gap (TS:Python = 2400:540) was the puzzle.

## Hypotheses tested

### H1: The Python proxy's __getattr__ + dynamic dispatch is slow

**Wrong.** The typed client (no proxy) performs identically to the proxy
in both languages. Allocating a `_ProxyMethod` per call is measurable but
small (~10us) compared to the 1.85ms total. We did add a method-name
cache to ProxyClient to avoid the per-call allocation, but the gain was
under 1%.

### H2: Dynamic imports inside the hot path cost real time

**Marginal.** `import dataclasses; import collections.abc` inside
`_ProxyMethod.__call__` were caching after the first call but the dict
lookup overhead was ~5us per call. Hoisting them gained nothing
measurable.

### H3: `uuid.uuid4()` per call (call_id generation) is expensive

**Confirmed but small.** `str(uuid.uuid4())` is ~3-5us. The callId for
shared streams is just for tracing (the stream itself is the
correlation), so we now send empty string and let the server generate
its own. ~1% gain.

### H4: `asyncio.wait_for()` per frame read costs ~50us each

**Wrong (or hidden).** `read_frame` was calling `asyncio.wait_for` with
a 30s default timeout on every read. Removing it gave zero measurable
improvement on the benchmark. Possible explanation: `wait_for` is fast
when the awaitable resolves quickly because the timeout handler is
cancelled in the same tick. The cost is amortized across the kqueue
wait that dominates.

### H5: Per-call frame writes cost a yield each

**Confirmed, small.** Combining `write(header) + write(request) +
finish()` (3 yields) into one `write_all + finish()` (2 yields) gave
~6% (537 -> 571 req/s, p50 1.85 -> 1.73). Real but modest.

### H6: PyO3 vs NAPI FFI overhead is the bottleneck

**Wrong.** Both bridge to the same Rust core (iroh). PyO3 per-call
overhead is ~100-500ns, same as NAPI. With ~9 PyO3 calls per RPC, that's
1-5us total -- nowhere near the 1.45ms gap.

### H7: asyncio scheduling is the dominant cost

**Confirmed by profile.** cProfile of 500 unary calls showed:

```
ncalls  cumtime  percall  function
9000    1.040s   0.000    asyncio.base_events._run_once
9000    0.751s   0.000    selectors.select / kqueue.control
5000    0.150s   0.000    iroh.unary  (our code)
1000    0.066s   0.000    json_codec.decode
2000    0.022s   0.000    IrohRecvStream.read_exact
1000    0.012s   0.000    IrohSendStream.write_all
1000    0.010s   0.000    IrohSendStream.finish
```

**75% of the time is `kqueue.control`** -- the event loop waiting on I/O
between yields. Only 15% is in our code. The Rust/PyO3 layer is ~3% of
total time.

The dominant cost is **per-yield asyncio overhead**: each `await` on a
PyO3 async function causes asyncio to schedule the task, suspend it,
hand control to kqueue, wake on the I/O completion event, schedule a
callback, resume the task. Each of these context switches costs
~100-150us in stock asyncio.

Per RPC call we have ~9 awaits (open_bi + 3 writes/finish + 4-6 reads).
9 * 150us = 1.35ms of pure scheduling overhead, which lines up almost
exactly with the gap we observe.

### H8: uvloop will help

**Yes, ~20% gain.**

| Configuration | req/s | p50 | client streams (10k) | concurrent 100 |
|---|---|---|---|---|
| stock asyncio | 537 | 1.85ms | 5230 msg/s | 1561 req/s |
| + write batching | 571 | 1.73ms | 5103 msg/s | 1525 req/s |
| + uvloop (client only) | 606 | 1.63ms | 5312 msg/s | 1609 req/s |
| + uvloop (both sides) | **638** | **1.56ms** | **6436 msg/s** | **1956 req/s** |

uvloop is now auto-installed at `import aster` time on Linux/macOS
(opt out with `ASTER_NO_UVLOOP=1`). It's a hard dependency on those
platforms; Windows users skip it cleanly.

## The remaining gap

After all optimizations: Python ~640 req/s, TS ~2400 req/s. Still 3.75x.

The kqueue dominance (75%) is the smoking gun: it's not our code,
it's libc-level select() costs amortized over 9 yields per call. TS
gets the same number of yields but each one is a V8 microtask (~5us)
not a full asyncio task scheduling cycle (~100-150us).

### Things we have NOT tried (would need a focused perf pass)

1. **Push the entire RPC into one PyO3 call.** A single
   `conn.unary_call(service, method, payload) -> response_bytes` Rust
   function would eliminate 8 of the 9 awaits per call, leaving just
   the one await on the result. This is the biggest possible win --
   maybe 4-5x -- but it requires duplicating the framing/codec logic
   in Rust or accepting that the Python layer becomes a thin wrapper.

2. **Pre-encode the StreamHeader bytes per (service, method).** For a
   given proxy method, the StreamHeader is identical every call. We
   could cache the 50-100 bytes of encoded header on the `_ProxyMethod`
   instance and skip the `codec.encode(StreamHeader(...))` step. Saves
   ~50us per call.

3. **Connection pooling for unary calls.** Currently every unary opens
   a new bidi stream. A "fire-and-forget on a long-lived stream"
   protocol could amortize the open_bi cost (which is one of the
   yields). Would require protocol changes.

4. **Vectored writes.** `write_all([buf1, buf2, buf3])` with one syscall.
   Doesn't matter much in our case because we already pack frames into
   one buffer, but if we split the read side it could help.

5. **Replace Python json with orjson.** json_decode shows up at 66ms /
   500 = 132us per call. orjson is 2-3x faster. Saves ~80us per call.
   Easy win, but adds a dependency.

## Why this matters less than it looks

The 1.85ms (now 1.56ms) Python latency is a worst case: in-process
loopback, where network latency is ~0. In a real deployment with even
1ms of network RTT, the difference between Python and TS shrinks to
20-30%. The asyncio overhead becomes a fixed cost on top of network,
not the dominant term.

For our day-zero target (operator CLI, agent control plane, mesh
admin), Python at 640 req/s is more than sufficient. The benchmark
exists to catch regressions, not to drive Python toward TS parity.

## Recommendations

1. **Done:** uvloop is now a default dependency on Linux/macOS.
2. **Done:** Write batching + ProxyMethod caching landed.
3. **Defer:** "Push RPC into one PyO3 call" is the biggest available
   win but it's an architecture change. Schedule a focused perf pass
   when there's a real-world workload that needs it.
4. **Defer:** orjson swap is an easy win but adds a dep. Worth doing
   if we ever publish numbers and want to look better.
5. **Document:** This file. So future-us doesn't re-walk the same
   investigation.

## Reproducing

```bash
# Start servers
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

## Files touched in this pass

- `bindings/python/aster/__init__.py` — uvloop auto-install
- `bindings/python/aster/high_level.py` — `_ProxyMethod` cache + fast path
- `bindings/python/aster/transport/iroh.py` — empty callId, batched writes
- `bindings/python/aster/framing.py` — hoisted imports, conditional wait_for
- `bindings/python/aster/contract/manifest.py` — (unrelated, dataclasses import fix)
- `pyproject.toml` — uvloop dep
- `examples/python/mission_control/benchmark.py` — typed client + ASTER_USE_UVLOOP support
- `examples/typescript/missionControl/benchmark.ts` — typed client section
