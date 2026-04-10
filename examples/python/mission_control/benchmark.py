#!/usr/bin/env python3
"""
Mission Control benchmark -- Python client.

Connects to a running Mission Control server (Python or TypeScript)
and measures throughput, latency percentiles, and memory usage.

Usage:
    # Start a server (either language):
    python -m mission_control.server
    #   or
    bun run examples/typescript/missionControl/server.ts

    # Run the benchmark:
    python examples/python/mission_control/benchmark.py aster1...
"""

import asyncio
import os
import statistics
import sys
import time


def mem_mb():
    """Current process RSS in MB."""
    try:
        import resource
        return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / (1024 * 1024)
    except Exception:
        return 0.0


def percentiles(latencies_ms):
    """Compute p50, p90, p99 from a list of latencies in ms."""
    if not latencies_ms:
        return 0, 0, 0
    s = sorted(latencies_ms)
    n = len(s)
    return s[n // 2], s[int(n * 0.9)], s[int(n * 0.99)]


def fmt_lat(p50, p90, p99):
    return f"p50={p50:.2f}ms  p90={p90:.2f}ms  p99={p99:.2f}ms"


async def main():
    address = sys.argv[1] if len(sys.argv) > 1 else None
    if not address:
        print("Usage: python benchmark.py <server-address>")
        sys.exit(1)

    from aster import AsterClient

    mem_start = mem_mb()
    client = AsterClient(address=address)
    await client.connect()
    mc = client.proxy("MissionControl")

    print(f"Connected to {address[:30]}...")
    print(f"Client memory at start: {mem_start:.1f} MB")
    print(f"{'─' * 72}")
    print("  ── Dynamic proxy client ────────────────────────────────────")

    # ── Warmup ────────────────────────────────────────────────────────
    for _ in range(20):
        await mc.getStatus({"agent_id": "warmup"})

    # ── Unary: getStatus ──────────────────────────────────────────────
    n_unary = 1000
    lats = []
    t0 = time.perf_counter()
    for i in range(n_unary):
        t_call = time.perf_counter()
        await mc.getStatus({"agent_id": f"bench-{i}"})
        lats.append((time.perf_counter() - t_call) * 1000)
    elapsed = time.perf_counter() - t0
    rps = n_unary / elapsed
    p50, p90, p99 = percentiles(lats)
    print(f"  Unary (getStatus)    {rps:>8,.0f} req/s   {fmt_lat(p50, p90, p99)}")

    # ── Unary: submitLog ──────────────────────────────────────────────
    n_log = 1000
    lats = []
    t0 = time.perf_counter()
    for i in range(n_log):
        t_call = time.perf_counter()
        await mc.submitLog({
            "timestamp": time.time(),
            "level": "info",
            "message": f"bench log {i}",
            "agent_id": "bench",
        })
        lats.append((time.perf_counter() - t_call) * 1000)
    elapsed = time.perf_counter() - t0
    rps = n_log / elapsed
    p50, p90, p99 = percentiles(lats)
    print(f"  Unary (submitLog)    {rps:>8,.0f} req/s   {fmt_lat(p50, p90, p99)}")

    # ── Client streaming: ingestMetrics ───────────────────────────────
    for batch_size in [100, 1_000, 10_000]:
        async def metrics(n):
            for i in range(n):
                yield {
                    "name": "cpu.usage",
                    "value": 42.0 + (i % 100) * 0.1,
                    "timestamp": time.time(),
                }

        t0 = time.perf_counter()
        result = await mc.ingestMetrics(metrics(batch_size))
        elapsed = time.perf_counter() - t0
        mps = batch_size / elapsed
        accepted = result.get("accepted", result) if isinstance(result, dict) else result
        label = f"{batch_size:>5,}"
        print(f"  Client stream ({label})  {mps:>8,.0f} msg/s   {elapsed*1000:.1f}ms total   accepted={accepted}")

    # ── Concurrent unary ──────────────────────────────────────────────
    for concurrency in [10, 50, 100]:
        t0 = time.perf_counter()
        tasks = [
            mc.getStatus({"agent_id": f"concurrent-{i}"})
            for i in range(concurrency)
        ]
        await asyncio.gather(*tasks)
        elapsed = time.perf_counter() - t0
        rps = concurrency / elapsed
        print(f"  Concurrent ({concurrency:>3})      {rps:>8,.0f} req/s   {elapsed*1000:.1f}ms total")

    # ── Typed client ──────────────────────────────────────────────────
    print("  ── Typed client (generated from contract) ──────────────────")
    from examples.python.mission_control.services import MissionControl
    from examples.python.mission_control.types import StatusRequest, LogEntry

    typed = await client.client(MissionControl)

    # Warmup
    for _ in range(20):
        await typed.getStatus(StatusRequest(agent_id="warmup"))

    # Unary getStatus
    n_unary = 1000
    lats = []
    t0 = time.perf_counter()
    for i in range(n_unary):
        t_call = time.perf_counter()
        await typed.getStatus(StatusRequest(agent_id=f"bench-{i}"))
        lats.append((time.perf_counter() - t_call) * 1000)
    elapsed = time.perf_counter() - t0
    rps = n_unary / elapsed
    p50, p90, p99 = percentiles(lats)
    print(f"  Unary (getStatus)    {rps:>8,.0f} req/s   {fmt_lat(p50, p90, p99)}")

    # Unary submitLog
    n_log = 1000
    lats = []
    t0 = time.perf_counter()
    for i in range(n_log):
        t_call = time.perf_counter()
        await typed.submitLog(LogEntry(
            timestamp=time.time(),
            level="info",
            message=f"bench log {i}",
            agent_id="bench",
        ))
        lats.append((time.perf_counter() - t_call) * 1000)
    elapsed = time.perf_counter() - t0
    rps = n_log / elapsed
    p50, p90, p99 = percentiles(lats)
    print(f"  Unary (submitLog)    {rps:>8,.0f} req/s   {fmt_lat(p50, p90, p99)}")

    # Concurrent
    for concurrency in [10, 50, 100]:
        t0 = time.perf_counter()
        tasks = [
            typed.getStatus(StatusRequest(agent_id=f"concurrent-{i}"))
            for i in range(concurrency)
        ]
        await asyncio.gather(*tasks)
        elapsed = time.perf_counter() - t0
        rps = concurrency / elapsed
        print(f"  Concurrent ({concurrency:>3})      {rps:>8,.0f} req/s   {elapsed*1000:.1f}ms total")

    # ── Memory ────────────────────────────────────────────────────────
    mem_end = mem_mb()
    print(f"{'─' * 72}")
    print(f"  Memory: start={mem_start:.1f}MB  end={mem_end:.1f}MB  delta={mem_end - mem_start:+.1f}MB")

    await client.close()


if __name__ == "__main__":
    asyncio.run(main())
