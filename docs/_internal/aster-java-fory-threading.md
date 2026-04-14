# Java binding — Fory thread-safety decision

**Scope:** `bindings/java/aster-runtime` — how `ForyCodec` wraps Apache Fory v0.16 so one codec instance can be shared across every in-flight RPC on an `AsterServer`.

**Summary:** `ForyCodec` holds a single `ThreadSafeFory` built via `Fory.builder().buildThreadSafeForyPool(minPoolSize, maxPoolSize)`. This gives us `ClassLoaderForyPooled` — a real shared pool of `Fory` instances that borrows-and-returns per call, with a `SharedRegistry` so type registrations propagate across every pooled instance. Pool sized at `(2, max(CPU/2, 2))`.

> **⚠ Do not use `buildThreadSafeFory()`.** In Fory 0.16 that method's bytecode literally returns `buildThreadLocalFory()` — i.e. `ThreadLocalFory`, not `ThreadPoolFory`. On a virtual-thread-per-task server this is the trap described in Option 3 below: every call = fresh VT = fresh `Fory` + codec recompile, ~200 µs per RPC of pure cold-start. We shipped this bug in the first version of `ForyCodec` and a Stage-1 timing probe (`t3 − t2` = 214 µs p50 for a 30-byte StreamHeader decode) is what caught it. Fix: call `buildThreadSafeForyPool(...)` explicitly.

## The problem

`AsterServer` dispatches incoming calls on `Executors.newVirtualThreadPerTaskExecutor()`, one fresh virtual thread per call. Inside `dispatchCall` the shared `ForyCodec` is hit from the VT to decode `StreamHeader`, decode the request, and encode the response. With the naïve setup (`ForyCodec` holding a plain `Fory`), that's many VTs hammering one non-thread-safe serializer at once.

This isn't hypothetical. The regression test `site.aster.codec.ForyCodecConcurrencyTest` (32 virtual threads × 200 `StreamHeader` round-trips each) failed instantly on plain `Fory` with:

```
org.apache.fory.exception.SerializationException:
    java.lang.ArrayIndexOutOfBoundsException: Index 11 out of bounds for length 8
```

Zero successful iterations. Classic buffer corruption from concurrent writers sharing one `MemoryBuffer`.

## Options considered

### Option 1 — Plain `Fory` + synchronization

Wrap every `serialize`/`deserialize` in a `synchronized` block. Works, but it serializes every RPC through one lock under load. For a P2P RPC framework whose selling point is low-latency unary calls, this is a non-starter.

**Rejected:** single-lock contention defeats the server's concurrency model.

### Option 2 — Plain `Fory` + platform-thread executor

Swap the server's executor to `Executors.newFixedThreadPool(N)` or similar, then hold a plain `Fory` per worker thread via an actual `ThreadLocal` we manage ourselves. No contention, and `ThreadLocal` lives on the platform thread which is reused across calls, so the `Fory` stays warm.

This works but discards the reason we picked virtual threads: unbounded cheap concurrency on blocking I/O. Every future streaming or session-scoped service would be capped by the platform pool size.

**Rejected:** throws away virtual threads, not worth the win on Fory alone.

### Option 3 — Fory's `ThreadLocalFory`

`Fory.builder().buildThreadLocalFory()` gives us `ThreadLocalFory`, which keeps one `Fory` per thread inside a `ThreadLocal` (with a `SharedRegistry` so registrations propagate). Obvious fit, right?

**The trap:** in Java virtual threads, `ThreadLocal` is bound to the **virtual thread itself**, not the carrier platform thread. This is deliberate per JEP 425 — if thread-locals leaked through to carriers, unrelated VTs sharing a carrier would see each other's state. So with our `newVirtualThreadPerTaskExecutor()`:

1. New VT starts → empty thread-local map
2. First `fory.serialize(...)` → `ThreadLocal.withInitial(::newFory)` fires → **new `Fory` built from scratch**
3. VT completes the call and dies
4. `Fory` becomes garbage
5. Next call = fresh VT = fresh `Fory` = repeat

`Fory`'s own javadoc for `ThreadLocalFory` spells out exactly this hazard:

> A thread safe serialization entrance for Fory by binding a Fory for every thread. Note that the thread shouldn't be created and destroyed frequently, otherwise the Fory will be created and destroyed frequently, which is slow.

A fresh `Fory` costs "1-2 ms" per the javadoc, plus codec compilation on first touch of each registered type — on the critical path of every unary call. Catastrophic for a system whose unary budget is sub-millisecond.

Confirming signal: Fory's own `VirtualThreadSafeForyTest` has **zero** tests for `ThreadLocalFory` under virtual threads. It only tests `ThreadPoolFory`. The second test in that file (`testVirtualThreadsUseFixedSizeThreadPoolFory`) explicitly asserts that 8 VTs borrowing from a pool of size 2 see exactly 2 distinct `Fory` identities — proof that `ThreadPoolFory` slots are shared across VTs rather than bound to any one.

**Rejected:** fresh `Fory` per virtual thread = fresh codec compile per call = killed our latency budget.

### Option 4 — Fory's `ThreadPoolFory` via `buildThreadSafeForyPool(...)` *(chosen)*

`Fory.builder().buildThreadSafeForyPool(minPoolSize, maxPoolSize)` returns a `ClassLoaderForyPooled`-backed `ThreadSafeFory`:

- Shared pool of `Fory` instances (we size `(2, max(CPU/2, 2))`; Fory's default overload takes explicit `int, int`).
- Borrow/return per-call with a thread-agnostic slot scan. No thread ownership, no thread-local state, virtual-thread safe by construction.
- Only blocks on a semaphore when every pooled instance is in use — at which point your machine is already saturated and Fory isn't the bottleneck.
- `SharedRegistry` means `codec.fory().register(Foo.class, "tag")` is seen by every pool entry immediately, and by any entry created later if the pool ever grows.
- `registerCallback` replays pending registrations onto any future `Fory` instance Fory creates, so we don't have to manage lifecycle.

Trade-off: N × the memory of one `Fory`, where N = pool size. At `4 × CPU` that's ~32-64 instances on a typical server. A single `Fory` is ~1-2 MB after codec compilation, so we're looking at tens of MB — trivially acceptable for an RPC server.

## Verification

`site.aster.codec.ForyCodecConcurrencyTest` is the regression guard:

- 32 virtual threads × 200 iterations = 6400 concurrent `StreamHeader` round-trips through a single shared `ForyCodec`
- Fails with `SerializationException` on plain `Fory` (0 successes)
- Passes with `ThreadPoolFory` (6400 successes, ~200 ms)

If that test ever goes red, the first thing to check is whether `ForyCodec` has been reverted to a plain `Fory`.

## API impact

`ForyCodec.fory()` used to return `Fory` (Fory's non-thread-safe root class). It now returns `BaseFory` — the common supertype of `Fory` and `ThreadSafeFory` that still carries every `register(...)` overload. In-tree callers (`AsterServer`, `AsterClient`, test codecs) only ever touched `.fory().register(Class, String)`, so this change is invisible at the callsite.

## Revisit when

- **Fory releases its reactor/async API:** if Fory ever exposes a fully zero-lock serialization path (e.g. per-call `Fory.get()` with no pool or locks), we might drop the pool. Track upstream.
- **Bench shows pool saturation:** if under real load we see borrowers queued on the semaphore, bump the pool via `buildThreadSafeForyPool(N)` rather than switching implementations. The default `4 × CPU` is generous but not infinite.
- **G.2 streaming lands:** multi-frame server streams will hit the codec more often per call. Re-run `ForyCodecConcurrencyTest` after the Flow emitter is in place; the test's shape matches server dispatch so it should still cover streaming.
