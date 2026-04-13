#!/usr/bin/env python3
"""Benchmark client -- runs locally, connects to remote servers.

Usage:
    # Aster:
    python bench_client.py aster <aster-address>

    # gRPC with TLS:
    python bench_client.py grpc 192.168.1.140:50055

    # gRPC without TLS (baseline):
    python bench_client.py grpc-insecure 192.168.1.140:50055
"""
import asyncio
import sys
import time


def percentiles(lats):
    if not lats:
        return 0, 0, 0
    s = sorted(lats)
    n = len(s)
    return s[n // 2], s[int(n * 0.9)], s[int(n * 0.99)]


async def bench_aster(address):
    from aster import AsterClient

    client = AsterClient(address=address)
    await client.connect()
    mc = client.proxy("MissionControl")

    for _ in range(50):
        await mc.getStatus({"agent_id": "warmup"})

    N = 2000
    lats = []
    t0 = time.perf_counter()
    for i in range(N):
        tc = time.perf_counter()
        await mc.getStatus({"agent_id": f"bench-{i}"})
        lats.append((time.perf_counter() - tc) * 1000)
    elapsed = time.perf_counter() - t0
    rps = N / elapsed
    p50, p90, p99 = percentiles(lats)
    print(f"Aster Python:     {rps:>8,.0f} req/s  p50={p50:.2f}ms  p90={p90:.2f}ms  p99={p99:.2f}ms")
    print(f"  ({N} calls in {elapsed:.2f}s)")
    await client.close()


async def bench_grpc(address, use_tls=True):
    import grpc

    # Import generated stubs
    sys.path.insert(0, "benchmarks/grpc-baseline")
    import mission_control_pb2 as pb2
    import mission_control_pb2_grpc as pb2_grpc

    if use_tls:
        with open("benchmarks/remote/certs/server.crt", "rb") as f:
            root_cert = f.read()
        creds = grpc.ssl_channel_credentials(root_certificates=root_cert)
        channel = grpc.aio.secure_channel(address, creds)
        label = "gRPC TLS"
    else:
        channel = grpc.aio.insecure_channel(address)
        label = "gRPC insecure"

    stub = pb2_grpc.MissionControlStub(channel)

    for _ in range(50):
        await stub.GetStatus(pb2.StatusRequest(agent_id="warmup"))

    N = 2000
    lats = []
    t0 = time.perf_counter()
    for i in range(N):
        tc = time.perf_counter()
        await stub.GetStatus(pb2.StatusRequest(agent_id=f"bench-{i}"))
        lats.append((time.perf_counter() - tc) * 1000)
    elapsed = time.perf_counter() - t0
    rps = N / elapsed
    p50, p90, p99 = percentiles(lats)
    print(f"{label:17s} {rps:>8,.0f} req/s  p50={p50:.2f}ms  p90={p90:.2f}ms  p99={p99:.2f}ms")
    print(f"  ({N} calls in {elapsed:.2f}s)")
    await channel.close()


async def bench_aster_session(address):
    from aster import AsterClient

    client = AsterClient(address=address)
    await client.connect()

    session = await client.session("AgentSession")
    await session.register({"agent_id": "bench", "capabilities": ["cpu"], "load_avg": 0.5})

    for _ in range(50):
        await session.heartbeat({"agent_id": "bench", "capabilities": ["cpu"], "load_avg": 0.1})

    N = 2000
    lats = []
    t0 = time.perf_counter()
    for i in range(N):
        tc = time.perf_counter()
        await session.heartbeat({"agent_id": "bench", "capabilities": ["cpu"], "load_avg": float(i % 100) / 100})
        lats.append((time.perf_counter() - tc) * 1000)
    elapsed = time.perf_counter() - t0
    rps = N / elapsed
    p50, p90, p99 = percentiles(lats)
    print(f"Aster session:    {rps:>8,.0f} req/s  p50={p50:.2f}ms  p90={p90:.2f}ms  p99={p99:.2f}ms")
    print(f"  ({N} calls on one persistent stream in {elapsed:.2f}s)")
    await session.close()
    await client.close()


async def main():
    if len(sys.argv) < 3:
        print("Usage: python bench_client.py <aster|aster-session|grpc|grpc-insecure> <address>")
        sys.exit(1)

    mode = sys.argv[1]
    address = sys.argv[2]

    if mode == "aster":
        await bench_aster(address)
    elif mode == "aster-session":
        await bench_aster_session(address)
    elif mode == "grpc":
        await bench_grpc(address, use_tls=True)
    elif mode == "grpc-insecure":
        await bench_grpc(address, use_tls=False)
    else:
        print(f"Unknown mode: {mode}")
        sys.exit(1)


if __name__ == "__main__":
    asyncio.run(main())
