#!/usr/bin/env python3
"""gamma SPIKE benchmark -- sequential + concurrent unary getStatus only.

Apples-to-apples with the getStatus section of
`examples/python/mission_control/benchmark.py`. Does not call submitLog
or streaming methods because the γ spike dispatch path does not support
them. The hypothesis under test is "sync-dispatch worker pool closes the
sequential throughput gap with Java"; only getStatus matters for that.

Usage:
    python -m mission_control.server --gamma &
    python examples/python/mission_control/benchmark_gamma.py <addr>
"""

import asyncio
import sys
import time


def percentiles(latencies_ms):
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
        print("Usage: python benchmark_gamma.py <server-address>")
        sys.exit(1)

    from aster import AsterClient
    from examples.python.mission_control.services import MissionControl
    from examples.python.mission_control.types import StatusRequest

    client = AsterClient(address=address)
    await client.connect()

    print(f"Connected to {address[:30]}...")
    print("─" * 72)

    # ── Dynamic proxy ─────────────────────────────────────────────────
    mc = await client.proxy("MissionControl")

    for _ in range(20):
        await mc.getStatus({"agent_id": "warmup"})

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
    print(f"  Sequential (dyn)    {rps:>8,.0f} req/s   {fmt_lat(p50, p90, p99)}")

    for concurrency in [10, 50, 100]:
        t0 = time.perf_counter()
        tasks = [
            mc.getStatus({"agent_id": f"concurrent-{i}"})
            for i in range(concurrency)
        ]
        await asyncio.gather(*tasks)
        elapsed = time.perf_counter() - t0
        rps = concurrency / elapsed
        print(f"  Concurrent ({concurrency:>3})      {rps:>8,.0f} req/s   {elapsed*1000:.1f}ms")

    # ── Typed client ──────────────────────────────────────────────────
    print("  ── Typed client ────────────────────────────────────────────")
    typed = await client.client(MissionControl)

    for _ in range(20):
        await typed.getStatus(StatusRequest(agent_id="warmup"))

    lats = []
    t0 = time.perf_counter()
    for i in range(n_unary):
        t_call = time.perf_counter()
        await typed.getStatus(StatusRequest(agent_id=f"bench-{i}"))
        lats.append((time.perf_counter() - t_call) * 1000)
    elapsed = time.perf_counter() - t0
    rps = n_unary / elapsed
    p50, p90, p99 = percentiles(lats)
    print(f"  Sequential (typed)  {rps:>8,.0f} req/s   {fmt_lat(p50, p90, p99)}")

    for concurrency in [10, 50, 100]:
        t0 = time.perf_counter()
        tasks = [
            typed.getStatus(StatusRequest(agent_id=f"concurrent-{i}"))
            for i in range(concurrency)
        ]
        await asyncio.gather(*tasks)
        elapsed = time.perf_counter() - t0
        rps = concurrency / elapsed
        print(f"  Concurrent ({concurrency:>3})      {rps:>8,.0f} req/s   {elapsed*1000:.1f}ms")

    print("─" * 72)
    await client.close()


if __name__ == "__main__":
    asyncio.run(main())
