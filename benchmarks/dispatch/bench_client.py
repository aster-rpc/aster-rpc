"""Dispatch benchmark -- Python own-language client (``benchmarks/dispatch/``).

Drives an ``InteropEcho`` server (``tests/interop/``) with the generated Python
client over one connection and prints one ``RESULT {json}`` line per
measurement, in the same schema as ``driver.rs``. Orchestrated by
``benchmarks/dispatch/run.py``::

    uv run --no-sync python benchmarks/dispatch/bench_client.py <aster1-ticket> \\
        [--duration S] [--warmup S] [--concurrency 1,10,50,100] \\
        [--stream-frames 100,10000] [--client-stream 10000] [--skip-streams]

Unary goes through the binding's pooled fast path (``create_client`` ->
``AsterCall.unary_fast_path``). The request is the same as the Rust driver's:
``EchoRequest(message="bench", count=1, data=64 x 0xab)``.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import statistics
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tests/interop/python"))

from aster import RPC_ALPN, AsterTicket, EndpointConfig, IrohNode, NodeAddr, create_client, net_client  # noqa: E402
from interop_echo import EchoRequest, InteropEcho  # noqa: E402


def bench_request() -> EchoRequest:
    return EchoRequest(message="bench", count=1, data=b"\xab" * 64)


def stream_request(frames: int) -> EchoRequest:
    return EchoRequest(message="s", count=frames, data=b"\xcd" * 64)


def pct(sorted_ns: list[int], p: float) -> float:
    if not sorted_ns:
        return 0.0
    return sorted_ns[min(int(len(sorted_ns) * p), len(sorted_ns) - 1)] / 1e6


async def run_unary(client, conc: int, duration: float) -> tuple[int, int, float, list[int]]:
    stop = False
    req = bench_request()
    errors = 0
    first_err: list[str] = []

    async def worker(lats: list[int]) -> None:
        nonlocal errors
        while not stop:
            t = time.perf_counter_ns()
            try:
                reply = await client.echo(req)
                if reply.count != 2:
                    raise AssertionError(f"bad reply count {reply.count}")
                lats.append(time.perf_counter_ns() - t)
            except Exception as exc:  # noqa: BLE001
                errors += 1
                if not first_err:
                    first_err.append(f"{type(exc).__name__}: {exc}")

    buckets: list[list[int]] = [[] for _ in range(conc)]
    start = time.perf_counter()
    tasks = [asyncio.create_task(worker(b)) for b in buckets]
    await asyncio.sleep(duration)
    stop = True
    await asyncio.gather(*tasks)
    elapsed = time.perf_counter() - start
    if first_err:
        print(f"worker error: {first_err[0]}", file=sys.stderr)
    lats = sorted(x for b in buckets for x in b)
    return len(lats), errors, elapsed, lats


async def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("ticket")
    ap.add_argument("--duration", type=float, default=5.0)
    ap.add_argument("--warmup", type=float, default=1.0)
    ap.add_argument("--concurrency", default="1,10,50,100")
    ap.add_argument("--stream-frames", default="100,10000")
    ap.add_argument("--client-stream", default="10000")
    ap.add_argument("--skip-streams", action="store_true")
    args = ap.parse_args()
    ints = lambda s: [int(x) for x in s.split(",") if x]  # noqa: E731

    cfg = EndpointConfig(alpns=[RPC_ALPN], relay_mode="disabled", bind_addr="127.0.0.1:0")
    node = await IrohNode.memory_with_alpns([RPC_ALPN], cfg)
    t = AsterTicket.from_string(args.ticket)
    addr = NodeAddr(endpoint_id=t.endpoint_id, relay_url=None, direct_addresses=t.direct_addrs)
    conn = await asyncio.wait_for(net_client(node).connect_node_addr(addr, RPC_ALPN), 20)
    client = create_client(InteropEcho, connection=conn)

    reply = await client.echo(bench_request())
    assert reply.count == 2, reply
    if args.warmup > 0:
        await run_unary(client, 10, args.warmup)

    for conc in ints(args.concurrency):
        if args.warmup > 0:
            await run_unary(client, conc, min(args.warmup, 1.0))
        n, errors, elapsed, lats = await run_unary(client, conc, args.duration)
        print("RESULT " + json.dumps({
            "kind": "unary", "mode": "python", "conc": conc, "calls": n, "errors": errors,
            "secs": round(elapsed, 3), "rps": round(n / elapsed, 1),
            "p50_ms": round(pct(lats, 0.50), 3), "p90_ms": round(pct(lats, 0.90), 3),
            "p99_ms": round(pct(lats, 0.99), 3),
        }), flush=True)

    if not args.skip_streams:
        for frames in ints(args.stream_frames):
            rates: list[float] = []
            budget = time.perf_counter()
            while len(rates) < 3 or (time.perf_counter() - budget < 2.0 and len(rates) < 200):
                t0 = time.perf_counter()
                got = 0
                async for _ in client.generate(stream_request(frames)):
                    got += 1
                assert got == frames, f"server-stream got {got} != {frames}"
                rates.append(frames / (time.perf_counter() - t0))
            print("RESULT " + json.dumps({
                "kind": "server_stream", "frames": frames, "iterations": len(rates),
                "frames_per_s": round(statistics.median(rates), 1),
            }), flush=True)
        for n in ints(args.client_stream):
            rates = []
            for _ in range(3):
                reqs = [stream_request(1) for _ in range(n)]

                async def gen(items=reqs):
                    for r in items:
                        yield r

                t0 = time.perf_counter()
                reply = await client.ingest(gen())
                assert reply.count == n, f"client-stream count {reply.count} != {n}"
                rates.append(n / (time.perf_counter() - t0))
            print("RESULT " + json.dumps({
                "kind": "client_stream", "messages": n, "iterations": 3,
                "msgs_per_s": round(statistics.median(rates), 1),
            }), flush=True)

    conn.close(0, b"done")
    await node.close()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
