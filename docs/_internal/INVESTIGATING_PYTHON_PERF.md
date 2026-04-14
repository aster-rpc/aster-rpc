# Python Performance — Current State

**Last updated:** 2026-04-14
**Status:** Current. Supersedes all earlier revisions.

## TL;DR

Python unary RPC on the Mission Control benchmark, Apple M2, in-process loopback, post-fix (commit `31a80b4`):

| Stack | Seq dyn JSON | Seq typed Fory | Conc-100 typed |
|---|---:|---:|---:|
| Python → Python | ~1,500 r/s | **~1,600 r/s** | **~6,050 r/s** |
| Java → Java (reference) | — | ~3,230 r/s | ~14,290 r/s |
| Gap | — | **~2.0×** | **~2.4×** |

Typed Fory p50: Python 0.60 ms vs Java 0.29 ms — **~330 µs delta per call**.

The sequential gap to Java is **CPU-bound work distributed across the Python interpreter, PyO3 bindings, pyfory codec, and tokio-for-Python glue**. It is NOT wait time, NOT a single hot function, and NOT something a targeted optimization can close cheaply. Samply profiling (2026-04-14) accounted for ~95% of the per-call round-trip latency as CPU time in one of the two processes (server ~340 µs, client ~249 µs, combined ~589 µs out of ~620 µs p50) and showed no single component above ~30% of active samples.

The most recent optimization landed the same day: `ForyCodec._coerce_enum_fields` was calling `typing.get_type_hints` uncached on every decoded dataclass, re-parsing forward refs via compile+eval. Adding a per-class cache plus an early-return for types with no enum fields recovered **+38% typed Fory sequential throughput and +42% concurrent-100**. This was the single largest Python-side cost on the hot path before the fix, and it's now gone. There is no equivalent single lever left.

**Concurrent scaling options are now well-understood.** Three forward paths were investigated on 2026-04-14:
- **Pattern A (N independent processes, distinct NodeIds, multi-endpoint discovery)** — works on stock CPython today, zero framework changes, recommended default.
- **Pattern B (1 Rust supervisor + N Python worker processes, single NodeId)** — viable, spike confirmed ~11 µs IPC tax over unix sockets (~1.8% of today's p50), ~1.5-2 weeks to build. The architectural stepping stone for β.
- **β (free-threaded Python 3.13t worker threads)** — **blocked upstream**: pyfory 0.16.0 ships no `cp313t` wheel across any of its 20 published wheels, and our current abi3 binding cannot load on cp313t either. When Apache Fory ships free-threaded wheels, Pattern B's worker side can upgrade from multi-process to multi-thread without changing the supervisor.

## Where the 330 µs gap lives

Samply native profile of both processes during the benchmark (commit `31a80b4`). Per-call CPU time derived from active samples over the benchmark window:

| Process | Active CPU / call | Python thread | Rust tokio workers |
|---|---:|---:|---:|
| Server (30s window, ~9,400 calls) | ~340 µs | ~136 µs | ~205 µs |
| Client (20s tight loop, ~29,500 calls) | ~249 µs | ~83 µs | ~166 µs |
| **Total both processes** | **~589 µs** | **~220 µs** | **~370 µs** |

Benchmark median p50 ≈ 620 µs. **~95% of the per-call round-trip latency is CPU time somewhere** — when one process is blocked in a kqueue wait, the other is doing work. Net wire/kernel overhead is ~30 µs per call. There is very little idle slack to reclaim.

### Crate-level distribution (active samples combined across all threads)

| Category | % of active samples | ~µs per call (both procs) |
|---|---:|---:|
| CPython bytecode (`python-interp`) | ~23% | ~135 |
| Rust generic stdlib (`core`, `alloc`, `std`, impl methods) | ~43% | ~250 |
| `tokio` runtime + scheduler | ~12% | ~70 |
| `noq_proto` (iroh's QUIC) | ~4-5% | ~27 |
| `pyo3` + `pyo3_async_runtimes` binding glue | ~3% | ~18 |
| `pyfory` Cython codec | ~2-3% | ~18 |
| `hashbrown`, `parking_lot`, `ring` (crypto) | ~2-3% | ~18 |
| Python C extensions (uvloop, `_asyncio`, `serialization.so`) | ~3% | ~18 |
| `aster_transport_core` (our reactor) | ~0.6% | ~4 |
| `aster` (our PyO3 binding code) | ~0.2% | ~1 |

**No single category is above ~30% of active CPU.** Our own reactor and binding code combined are <1%. The cost is distributed across dozens of small contributors — the "death of a thousand cuts" shape.

### What Python pays that Java wouldn't

Comparing to a hypothetical Java binding on the same iroh library (same `noq_proto`, same `tokio`, same `ring` crypto, same kernel network stack):

- **`python-interp`** (~135 µs/call): CPython bytecode interpreter executing Python dispatch, codec wrappers, handlers. Java's JIT compiles equivalent work down to native code at several-times higher throughput.
- **`pyo3` + `pyo3_async_runtimes`** (~18 µs/call): GIL acquire/release on every crossing, `future_into_py` wrapping, `TaskLocals` propagation. Java's JNI has its own overhead but a structurally different cost model.
- **`pyfory`** (~18 µs/call): client dominance is 2× decode (response + trailer), server is 1× decode. Already Cython-accelerated. Already smart-skipped on enum-free types (the 2026-04-14 fix). Further reductions would require detailed per-op auditing.
- **Tokio-for-Python fraction**: not cleanly separable, but a meaningful chunk of the ~70 µs tokio budget is spent waking Python callbacks and propagating task locals for code that Java wouldn't need.

**Ballpark Python-attributable overhead: ~220 µs direct (`python-interp` + `pyo3` + `pyfory` + Python C extensions) + ~50-80 µs indirect (tokio/alloc/core driven by Python-aware machinery) ≈ 270-300 µs per call.** Matches the observed ~330 µs gap to Java closely.

**Conclusion:** the sequential gap to Java is a fundamental CPython-vs-JIT trade-off, not a fixable bug. No single Python-layer change closes more than a few tens of microseconds. Closing the whole gap would require either a different Python runtime (JIT or no-GIL with a different cost model) or moving substantially more of the dispatch path into Rust so Python only touches user-owned handlers.

## What we just shipped (commit `31a80b4`, 2026-04-14)

- **`ForyCodec._coerce_enum_fields` cache + smart skip** (`bindings/python/aster/codec.py`). Cached `get_type_hints` plus a precomputed `_needs_coerce_cache[cls]` bool that early-returns for types with no enum fields anywhere in their graph. Measured impact on the MC getStatus benchmark (plain Fory, async dispatch, median of 3 rounds):
  - Typed Fory sequential: 1,193 → 1,641 r/s (**+38%**, p50 −230 µs)
  - Typed Fory conc-100: 4,258 → 6,046 r/s (**+42%**)
  - Dynamic JSON: unchanged (different decode path)
- **`test_pyfory_cython_acceleration_is_enabled`** (`tests/python/test_aster_codec.py`). Regression canary that asserts (a) `pyfory.ENABLE_FORY_CYTHON_SERIALIZATION` is True and (b) `ForyCodec` instantiated the Cython fast-path class, not the pure-Python fallback. If a broken wheel ever slips through CI, codec throughput would silently drop several-fold; this test catches it.

## Earlier perf wins (pre-`31a80b4`)

Commit `c147a0c` (2026-04-14 morning) landed four related Python-side optimizations that produced the prior baseline:

- **`AsterCall.unary_fast_path`** (`bindings/python/rust/src/call.rs`) — collapses the client unary path from 4 PyO3 crossings to 1 (acquire + send_header + send_request + recv_frame ×2 → one `future_into_py`). Mirrors the Java FFI equivalent.
- **`typing.get_type_hints` cache in `_dict_to_dataclass`** (`bindings/python/aster/json_codec.py`) — same root-cause shape as the later Fory-side fix, for the JSON decode path.
- **`IrohTransport.unary`** rewritten to build frames in one step.
- Matching TypeScript-side fixes on the same commit.

Earlier still, the server reactor (`core/src/reactor.rs` + `bindings/python/aster/server.py:serve_reactor`) moved the server accept-loop, per-connection tasks, and frame reading entirely into Rust/tokio. Python only sees fully-formed call descriptors arriving through the reactor's C ABI, shared by all bindings (Python, TypeScript, Java).

## What's next — what's actually worth building

The remaining gap is distributed. Options ranked by effort vs payoff:

### A. Small Python-side wins (1-2 days each, 10-30 µs per call each)

- **Audit `IrohTransport.unary` and `_run_call_with_interceptors`** — every extra Python attribute lookup, method call, or context-var push on the hot path is ~1-2 µs. The cProfile shows ~60 µs/call of total client Python code outside PyO3 and codec; some is necessary, some may not be.
- **Empty-interceptor fast path** — if a call has no interceptors registered (common in dev), the `await apply_*_interceptors(...)` chain should skip the async hop entirely instead of entering an empty coroutine.
- **Lazy call context** — contextvar propagation costs ~4 µs per call on the client. For calls that don't observe deadline / metadata / peer attributes, materialize the `CallContext` lazily.
- **pyfory-wrapper audit** — pyfory serialize/deserialize Cython is already fast. The ForyCodec wrapper around it (type checks, buffer resets, lookup paths) may have a few µs to shave.

Cumulative best case: 50-100 µs per call. Would bring typed Fory sequential from ~1,600 r/s to ~1,900 r/s. Incremental, not transformative.

### B. Move the codec decode into Rust (in-process structural work, 3-5 days each)

- **Server-side codec pre-decode in Rust** — the reactor delivers raw bytes to Python which then decodes them. If the request frame were decoded in Rust before crossing into Python (via pyfory-rs if it exists, or a small custom decoder for the common shapes), Python would receive structured data and skip the decode call. Est. 20-40 µs per call.
- **Client-side trailer handling in Rust** — `unary_fast_path` returns raw bytes; Python decodes both the response and the trailer. If `unary_fast_path` inspected the trailer in Rust and returned `(response_bytes, status_code, error_message)`, Python would skip one decode. Est. 10-20 µs per call.

These are real engineering tasks with downstream cost (more Rust complexity, Fory dependency on the Rust side, schema distribution). Only worth doing if the sequential headline matters.

### C. Concurrent throughput — Pattern A (documentation + verification, ~1 day)

**N independent Aster nodes with distinct NodeIds advertising the same logical service.** Clients discover multiple endpoints through the discovery layer (pkarr / mesh routing) and pick one per call or per connection. Standard Kubernetes/systemd scale-out. Works on stock CPython. Zero framework changes.

This is the right production answer for throughput-bound workloads. Today's concurrent-100 typed at ~6,050 r/s per process × N processes scales linearly until something else becomes the bottleneck. For any real deployment, aggregate throughput is what matters, and Pattern A dominates any single-process optimization.

Prerequisite: verify the discovery layer's multi-endpoint advertisement story is production-ready. Then document it.

### D. Pattern B — Rust supervisor + N Python worker processes (spike confirmed, ~1.5-2 weeks to build)

One Rust binary owns the iroh endpoint, the reactor, and a pool of Python worker processes spawned as children. Incoming calls are dispatched over unix-socket IPC to workers; each worker is an independent CPython interpreter with its own GIL, so there is no concurrent-threading footgun. Presents as one NodeId to the network (no multi-endpoint discovery needed), works on stock CPython today.

**Architecture:**

```
[Client] ──QUIC──► [Rust supervisor: 1 iroh endpoint, 1 NodeId, the reactor]
                         │
                         ├──unix socket──► [Python worker 1]
                         ├──unix socket──► [Python worker 2]
                         └──unix socket──► [Python worker N]
```

The supervisor consumes reactor events from the existing C ABI (the same interface the PyO3 binding uses today, but called from a Rust dispatcher instead of delivering into Python). Each call is routed to one worker based on a sticky-per-stream strategy — all frames of a QUIC stream go to the same worker for the stream's lifetime. Session-scoped services use sticky-per-session routing on top.

**Spike results (2026-04-14).** Raw unix-socket frame-exchange cost measured between a minimal Rust binary and a Python script ping-ponging length-prefixed frames in a tight loop. 50,000 RTTs per measurement, 4 rounds median for the default payload:

| Payload | Per RTT | Throughput |
|---:|---:|---:|
| 64 B | 10.8 µs | 92,800 RTT/s |
| 128 B | ~11.0 µs | ~92,000 RTT/s (median of 4 rounds) |
| 256 B | 10.7 µs | 93,100 RTT/s |
| 512 B | 11.5 µs | 86,800 RTT/s |
| 1 KB | 11.6 µs | 86,200 RTT/s |
| 4 KB | 12.4 µs | 81,000 RTT/s |

Stock CPython 3.13 and free-threaded cp313t give identical IPC cost (~11 µs) — the cost is kernel-syscall-dominated, not interpreter-dominated.

**Interpretation.** Inserting Pattern B's supervisor→worker IPC into today's unary path adds one round-trip per call ≈ **~11 µs on top of today's ~620 µs p50 = ~1.8% latency tax**. Per-worker-socket throughput ceiling is ~90,000 RTT/sec; today's full Aster pipeline runs at ~1,600 r/s per process, so the IPC layer has ~56× headroom before becoming a bottleneck. An earlier revision of this doc guessed "10-50 µs IPC tax"; the measured number is at the cheap end of that range and leaves plenty of budget for doing real work inside the supervisor before the IPC itself becomes a problem.

**What still has to be built** (the spike only proved raw IPC is cheap; the system around it is real engineering):

- **`aster-runtime` Rust binary** that owns the iroh endpoint, consumes reactor events Rust-side, spawns N worker processes as children, and dispatches over unix sockets. ~300-500 lines building on the existing reactor. (2-3 days)
- **IPC protocol** — length-prefixed frames with a correlation ID, worker-hello handshake, shutdown sentinel. The frame shape can reuse the existing QUIC wire format; what needs defining is the supervisor↔worker envelope. (1-2 days)
- **Python worker module** (`aster.worker` or similar) that connects to the supervisor's socket, registers services, runs a minimal asyncio loop, reads calls, invokes handlers, writes responses. (2 days)
- **Dispatch strategy** — sticky-per-stream at stream-open time, session-scoped service affinity, round-robin across workers for new streams. Aligns with the earlier "stream is the right dispatch unit in a QUIC+P2P world" finding. (2 days)
- **Worker lifecycle** — child supervision, graceful shutdown, crash recovery (failed worker's in-flight calls get error-responded, supervisor optionally restarts). (1-2 days)
- **Streaming support** — bidi and server-stream with backpressure across IPC. The hardest piece, but maps cleanly to "correlation ID + multi-frame exchange on one socket." (2-3 days)
- **Integration tests + benchmarks** — concurrent-100 actually scales with N, session routing works, crash recovery works. (2-3 days)

**Total: ~1.5-2 weeks of focused work.** Earlier doc revisions called this "speculative, hardest" — the spike upgrades it to "tested, viable, well-scoped."

**Stepping stone to β.** Pattern B is the architecture you want *anyway*, even if you eventually ship free-threaded Python. When pyfory ships cp313t wheels, the worker side can change from "N OS processes, each with its own GIL" to "1 OS process with N threads on cp313t, each with its own asyncio loop" — the supervisor, IPC protocol, and dispatch logic stay the same. Building Pattern B now is not wasted work toward β; it's the prerequisite shape for it. The only part that would become removable under β is the unix-socket round-trip (saving the ~11 µs IPC tax), and everything else — supervisor ownership of iroh, dispatcher loop, worker-registry, crash handling — is load-bearing in both worlds.

**Comparison to Pattern A and β:**

| | Pattern A (N procs, distinct NodeIds) | **Pattern B (1 NodeId, N worker procs)** | β (free-threaded threads) |
|---|---|---|---|
| Engineering cost | ~1 day (docs + discovery verify) | **~1.5-2 weeks** | Blocked upstream |
| Works on stock CPython | Yes | **Yes** | No (cp313t only) |
| pyfory dependency | Today's 0.16.0 | **Today's 0.16.0** | Needs cp313t wheel (unavailable) |
| NodeId topology | N distinct (client discovers N) | **1 (client sees 1 endpoint)** | 1 |
| Shared state across workers | No (process-isolated) | **No (process-isolated)** | Yes (thread-shared) |
| Per-call IPC tax | 0 | **~11 µs (1.8%)** | 0 |
| Handler thread-safety required | No | **No** | Yes |
| Concurrent throughput ceiling | N × single-process | **N × single-process, one NodeId** | N cores within one process |

Pattern A is strictly cheaper and is the right answer if multi-endpoint discovery fits the user's topology. Pattern B is the right answer if the "single NodeId" property matters — long-lived stream identity pinning, operational simplicity of one address to monitor/restart, or contexts where multi-endpoint discovery is awkward for the client.

**When to build it.** Trigger: a concrete need for single-NodeId multi-core throughput that Pattern A cannot satisfy. In the absence of that trigger, Pattern A is cheaper and sufficient. The spike confirms the path exists and measures its cost — it doesn't mandate building it now.

### E. Free-threaded Python 3.13t (β) — blocked on ecosystem

A plausible concurrent-scaling path is "multi-threaded dispatch under free-threaded CPython 3.13t+": N worker threads each running their own asyncio loop, actually-in-parallel on different cores, single NodeId. A 2026-04-14 spike against `explore/gamma-spike` specifically tested whether this path is viable today.

**Spike results (~20 minutes to definitive answer):**

1. **Free-threaded Python 3.13.12 works natively.** `python3.13t` installs via `uv python install 3.13t`, `sys._is_gil_enabled()` returns `False` out of the box, the interpreter itself is fine.
2. **Our current `_aster.abi3.so` does not load under cp313t.** Error: `SystemError: init function of _aster returned uninitialized object`. Our binding is built with PyO3's `abi3-py39` feature, and the abi3 stable ABI is incompatible with free-threaded Python. Fixable in principle by dropping `abi3-py39`, adding `#[pymodule(gil_used = false)]` on the Rust side, and `maturin develop`ing for cp313t. Roughly a day of work including a PyO3 Sync audit. **Fixable.**
3. **`pyfory` 0.16.0 has no cp313t wheel.** Apache Fory publishes 20 wheels for the current release — cp39 / cp310 / cp311 / cp312 / cp313 — and all 20 are GIL-enabled (none have the `cp313t-cp313t` ABI tag). No sdist on PyPI. A source-build would require Apache Fory's full monorepo build toolchain locally, which includes Java + Cython + a codegen step; not a one-afternoon task. **Blocked upstream.**

**Conclusion.** The blocker for β is *pyfory cp313t wheels*, not our code. Aster's entire typed-Fory path depends on pyfory, so until Apache Fory ships free-threaded wheels, β cannot run regardless of how much engineering we put in on the PyO3 side. The right posture is: watch Apache Fory's release cadence, and revisit β when cp313t wheels exist. No Aster-side work needed in the interim.

**What to watch for.** When pyfory ships a `cp313t` wheel, the remaining work to validate β shape is:
- Drop `abi3-py39` and rebuild `_aster` for cp313t with `#[pymodule(gil_used = false)]`.
- PyO3 `Sync` audit on every `#[pyclass]` type we expose (AsterCall, AsterReactor, reactor event types, pool handles). Each needs either interior synchronisation or a `Py<T>`-safe design.
- Minimal smoke test: two Python threads each calling `unary_fast_path` concurrently against the same server, verify no segfault and that `sys._is_gil_enabled()` stays `False` after module import.
- If smoke test passes, then the larger architectural change (multi-worker reactor dispatch, shared service state semantics, per-worker asyncio loops) becomes worth building.

**Why other options are better until then.** Pattern A (N independent processes, multi-endpoint discovery) delivers concurrent scaling today on stock CPython with zero framework changes. Pattern B (Rust supervisor + N Python worker processes) delivers the same single-NodeId property β offers, works today on stock CPython, and — per the spike — would serve as the prerequisite architecture for β anyway. Pattern B's worker processes can be upgraded to worker threads on cp313t when upstream catches up; the supervisor/dispatcher shape stays the same. There is no scenario where waiting for β is better than building Pattern B first if single-NodeId throughput is the goal.

### F. Don't build — declare ~2× acceptable

Day-zero use cases (operator CLI, agent control plane, mesh admin) are not bound by sub-millisecond loopback latency. 620 µs p50 is fine for every production workload that isn't in a tight benchmark loop. The 2× gap to Java is:

- A rounding-error difference in any deployment where network RTT dominates
- A fundamental Python-vs-JIT trade-off, not a bug
- Amortized cleanly by concurrent workloads (conc-100 typed is already >6,000 r/s — higher than single-thread Java sequential)

This is the honest answer for most situations. The perf doc should stop framing "sequential 1-thread parity with Java" as the headline goal — it's the wrong metric for a P2P RPC framework used in mesh deployments where throughput and horizontal scaling are the real levers.

## Recommendation

1. **`_coerce_enum_fields` fix is shipped** (commit `31a80b4`). No more action needed.
2. **Document Pattern A as the horizontal scaling story** (~1 day). Confirm multi-endpoint discovery works, write the docs page, point users at "run more processes" when they need throughput. Pattern A is the right default recommendation for any user who can deploy more than one process.
3. **Pattern B stays on the shelf until there is demand.** The spike confirmed it is viable (~11 µs IPC tax, no ecosystem blockers) and well-scoped (~1.5-2 weeks to build), but it is not worth the engineering investment in the absence of a concrete use case that Pattern A cannot satisfy. If a user need for single-NodeId multi-core throughput surfaces, **Pattern B is the path, not β**, until pyfory cp313t wheels exist — and even after they exist, Pattern B is the architectural prerequisite for β.
4. **Stop iterating on the sequential axis unless a specific need surfaces.** Small in-process wins (options A and B in the options list) exist if a specific workload demands them; revisit when there is concrete evidence. In the absence of that evidence, the engineering time is better spent on features users actually ask for.
5. **Retire the α/β/γ framing from earlier doc revisions.** The old options tree was built on a misread cProfile and a hypothesis that Python dispatch cost was ~0.4-0.5 ms per call. Samply measurements show the real server-side Python dispatch cost is ~136 µs per call spread across interpreter work that no γ-style fix removes, and the true levers for closing the *concurrent* gap are Pattern A (now) and Pattern B (when single-NodeId matters), not β.
6. **Watch `pyfory` for cp313t wheel availability.** The free-threaded-Python path (option E above) is blocked upstream at the pyfory codec layer, not in our code. A 2026-04-14 spike confirmed both that Apache Fory has not published cp313t wheels and that our current abi3 binding cannot load on cp313t anyway. When Apache Fory ships free-threaded wheels, the β migration from Pattern B is small — the supervisor/dispatcher shape stays identical, only the worker side upgrades from multi-process to multi-thread. Watch, don't wait.

## Reproducing

Python server + benchmark:

```bash
# Server (one terminal)
PYTHONUNBUFFERED=1 uv run python -m examples.python.mission_control.server

# Benchmark (another terminal)
PYTHONPATH=. uv run python examples/python/mission_control/benchmark.py <server-addr>
```

Native CPU profile with samply:

```bash
# Launch server under samply with a fixed sample duration
PYTHONUNBUFFERED=1 samply record --save-only -o /tmp/server_prof.json.gz -d 30 -- \
  uv run python -m examples.python.mission_control.server

# Run the benchmark multiple times during the 30s window to generate load
# Kill the server's python child when done — samply saves the profile on child exit
```

samply writes Firefox Profiler format (`.json.gz`). Open in <https://profiler.firefox.com/>. For offline analysis / symbolication, `nm -n bindings/python/aster/_aster.abi3.so` dumps the symbol table sorted by address; a small bisect-based lookup on top resolves the hex offsets samply records when it cannot find a `.dSYM` bundle at capture time.

cProfile the typed client (for Python-side per-call breakdown):

```python
import asyncio, cProfile, pstats
from aster import AsterClient
from examples.python.mission_control.services import MissionControl
from examples.python.mission_control.types import StatusRequest

async def main():
    c = AsterClient(address='<addr>')
    await c.connect()
    t = await c.client(MissionControl)
    for _ in range(50): await t.getStatus(StatusRequest(agent_id='w'))
    p = cProfile.Profile(); p.enable()
    for i in range(500): await t.getStatus(StatusRequest(agent_id=f'p{i}'))
    p.disable()
    pstats.Stats(p).sort_stats('tottime').print_stats(20)
    await c.close()

asyncio.run(main())
```

## See also

- [`aster-java-fory-threading.md`](aster-java-fory-threading.md) — how Java scales concurrent Fory workloads with `ThreadPoolFory`. Reference for any future Python concurrent codec work.
- [`benchmarking.md`](benchmarking.md) — how to run the benchmarks.
- [`reactor-ffi-guide.md`](reactor-ffi-guide.md) — the C ABI contract for the reactor. Authoritative for new bindings.
