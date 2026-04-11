#!/usr/bin/env python3
"""Loopback benchmark: reactor next_call vs poll_calls.

Runs server and client in the same process over iroh loopback QUIC.
Tests both sequential (1 client) and concurrent (N clients) patterns.

Usage:
    uv run python benchmarks/bench_reactor_poll.py
    uv run python benchmarks/bench_reactor_poll.py --clients 8
    uv run python benchmarks/bench_reactor_poll.py --mode next_call --clients 4
"""
import asyncio
import argparse
import json
import struct
import time

FLAG_HEADER = 0x04
FLAG_TRAILER = 0x02
ASTER_ALPN = b"aster/1"
N_WARMUP = 50
N_CALLS = 2000


def percentiles(lats):
    if not lats:
        return 0, 0, 0
    s = sorted(lats)
    n = len(s)
    return s[n // 2], s[int(n * 0.9)], s[int(n * 0.99)]


def encode_frame(payload: bytes, flags: int) -> bytes:
    return struct.pack("<I", len(payload) + 1) + bytes([flags]) + payload


HEADER_FRAME = encode_frame(
    json.dumps({
        "svc": "Bench", "rpcMethod": "echo", "version": 1,
        "callId": "", "deadlineEpochMs": 0, "serializationMode": 3,
        "metadataKeys": [], "metadataValues": [],
    }).encode(),
    FLAG_HEADER,
)

TRAILER_BYTES = encode_frame(
    json.dumps({"code": 0, "message": "", "detailKeys": [], "detailValues": []}).encode(),
    FLAG_TRAILER,
)


async def server_next_call(reactor):
    while True:
        result = await reactor.next_call()
        if result is None:
            break
        _cid, _hdr, _hf, request, _rf, _peer, _sess, sender = result
        sender.submit(encode_frame(request, 0), TRAILER_BYTES)


async def server_poll_calls(reactor, batch_size):
    while True:
        calls = await reactor.poll_calls(batch_size)
        if not calls:
            break
        for _cid, _hdr, _hf, request, _rf, _peer, _sess, sender in calls:
            sender.submit(encode_frame(request, 0), TRAILER_BYTES)


async def read_frame(recv) -> tuple[bytes, int]:
    len_buf = await recv.read_exact(4)
    body_len = struct.unpack("<I", len_buf)[0]
    body = await recv.read_exact(body_len)
    return body[1:], body[0]


async def client_worker(conn, n_warmup, n_calls):
    """Run sequential calls on one connection, return latencies."""
    async def do_call(i):
        send, recv = await conn.open_bi()
        req = encode_frame(json.dumps({"i": i}).encode(), 0)
        await send.write_all(HEADER_FRAME + req)
        await send.finish()
        await read_frame(recv)
        await read_frame(recv)

    for i in range(n_warmup):
        await do_call(i)

    lats = []
    for i in range(n_calls):
        tc = time.perf_counter()
        await do_call(i)
        lats.append((time.perf_counter() - tc) * 1000)
    return lats


async def bench(mode, n_clients, batch_size):
    from aster._aster import IrohNode, start_reactor, net_client

    server_node = await IrohNode.memory_with_alpns([ASTER_ALPN])
    reactor = start_reactor(server_node, 256)

    if mode == "next_call":
        server_task = asyncio.create_task(server_next_call(reactor))
    else:
        server_task = asyncio.create_task(server_poll_calls(reactor, batch_size))

    await asyncio.sleep(0.05)

    # Create client nodes and connections
    conns = []
    client_nodes = []
    for _ in range(n_clients):
        cn = await IrohNode.memory()
        cn.add_node_addr(server_node)
        ep = net_client(cn)
        conn = await ep.connect(server_node.node_id(), ASTER_ALPN)
        conns.append(conn)
        client_nodes.append(cn)

    calls_per_client = N_CALLS // n_clients

    t0 = time.perf_counter()
    tasks = [
        asyncio.create_task(client_worker(conn, N_WARMUP, calls_per_client))
        for conn in conns
    ]
    all_lats_lists = await asyncio.gather(*tasks)
    elapsed = time.perf_counter() - t0

    all_lats = []
    for lats in all_lats_lists:
        all_lats.extend(lats)

    total_calls = calls_per_client * n_clients
    rps = total_calls / elapsed
    p50, p90, p99 = percentiles(all_lats)

    server_task.cancel()
    try:
        await server_task
    except asyncio.CancelledError:
        pass

    for cn in client_nodes:
        await cn.shutdown()
    await server_node.shutdown()

    label = f"{mode} ({n_clients}c)"
    print(f"  {label:30s} {rps:>8,.0f} req/s  p50={p50:.2f}ms  p90={p90:.2f}ms  p99={p99:.2f}ms")
    return rps


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", nargs="*", default=["next_call", "poll_calls"])
    parser.add_argument("--clients", nargs="*", type=int, default=[1, 4, 8])
    parser.add_argument("--batch", type=int, default=32)
    args = parser.parse_args()

    print(f"  {N_CALLS} total calls per run, batch_size={args.batch}")
    print()

    results = {}
    for n_clients in args.clients:
        for mode in args.mode:
            key = (mode, n_clients)
            results[key] = await bench(mode, n_clients, args.batch)
        print()

    # Print comparison for matching client counts
    for n_clients in args.clients:
        nc_key = ("next_call", n_clients)
        pc_key = ("poll_calls", n_clients)
        if nc_key in results and pc_key in results and results[nc_key] > 0:
            ratio = results[pc_key] / results[nc_key]
            print(f"  {n_clients} clients: poll_calls / next_call = {ratio:.2f}x")


if __name__ == "__main__":
    asyncio.run(main())
