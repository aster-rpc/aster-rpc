"""gRPC Mission Control benchmark -- baseline comparison.

Matches the Aster benchmark: 2000 sequential unary getStatus calls,
measuring throughput and latency percentiles.

Usage:
    # Terminal 1:
    python server.py

    # Terminal 2:
    python benchmark.py
"""

import asyncio
import time

import grpc

import mission_control_pb2 as pb2
import mission_control_pb2_grpc as pb2_grpc


async def main():
    channel = grpc.aio.insecure_channel("localhost:50055")
    stub = pb2_grpc.MissionControlStub(channel)

    # Warmup
    for _ in range(50):
        await stub.GetStatus(pb2.StatusRequest(agent_id="warmup"))

    # Benchmark: getStatus
    N = 2000
    lats = []
    t0 = time.perf_counter()
    for i in range(N):
        tc = time.perf_counter()
        await stub.GetStatus(pb2.StatusRequest(agent_id=f"bench-{i}"))
        lats.append((time.perf_counter() - tc) * 1000)
    elapsed = time.perf_counter() - t0
    rps = N / elapsed

    lats.sort()
    n = len(lats)
    p50 = lats[n // 2]
    p90 = lats[int(n * 0.9)]
    p99 = lats[int(n * 0.99)]

    print(f"gRPC Python -> Python: {rps:,.0f} req/s  p50={p50:.2f}ms  p90={p90:.2f}ms  p99={p99:.2f}ms")
    print(f"  ({N} calls in {elapsed:.2f}s)")

    # Benchmark: submitLog
    lats = []
    t0 = time.perf_counter()
    for i in range(N):
        tc = time.perf_counter()
        await stub.SubmitLog(pb2.LogEntry(
            timestamp=time.time(),
            level="info",
            message=f"benchmark log {i}",
            agent_id=f"bench-{i}",
        ))
        lats.append((time.perf_counter() - tc) * 1000)
    elapsed = time.perf_counter() - t0
    rps = N / elapsed

    lats.sort()
    n = len(lats)
    p50 = lats[n // 2]
    p90 = lats[int(n * 0.9)]
    p99 = lats[int(n * 0.99)]

    print(f"gRPC Python submitLog: {rps:,.0f} req/s  p50={p50:.2f}ms  p90={p90:.2f}ms  p99={p99:.2f}ms")
    print(f"  ({N} calls in {elapsed:.2f}s)")

    await channel.close()


if __name__ == "__main__":
    asyncio.run(main())
