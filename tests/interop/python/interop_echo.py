"""Cross-binding interop smoke: the Python side of ``tests/interop/``.

Shared contract (must stay identical to ``tests/interop/rust/interop_echo.rs``
and ``tests/interop/ts/src/``)::

    service InteropEcho v1
      echo(interop/EchoRequest) -> interop/EchoReply           (unary)
    interop/EchoRequest { message: string, count: varint32, data: binary }
    interop/EchoReply   { message: string, count: varint32, data: binary, server_lang: string }

Usage (from the repo root, after ``scripts/build.sh``)::

    uv run --no-sync python tests/interop/python/interop_echo.py contract
    uv run --no-sync python tests/interop/python/interop_echo.py server
    uv run --no-sync python tests/interop/python/interop_echo.py client <aster1-ticket>

Output protocol (parsed by ``tests/interop/run.py``): ``CONTRACT_ID <hex>``,
``ADDRESS <aster1...>`` (server), ``CASE <name> OK|FAIL ...`` (client).
Open gate, relay disabled, loopback bind: works offline.
"""

from __future__ import annotations

import asyncio
import sys
from dataclasses import dataclass

import pyfory

from aster import (
    AsterConfig,
    AsterServer,
    AsterTicket,
    EndpointConfig,
    IrohNode,
    NodeAddr,
    RPC_ALPN,
    create_client,
    net_client,
    rpc,
    service,
    wire_type,
)
from aster.contract.identity import contract_id_from_service


@wire_type("interop/EchoRequest")
@dataclass
class EchoRequest:
    message: str = ""
    count: pyfory.Int32 = 0  # xlang varint32 == Rust i32
    data: bytes = b""


@wire_type("interop/EchoReply")
@dataclass
class EchoReply:
    message: str = ""
    count: pyfory.Int32 = 0
    data: bytes = b""
    server_lang: str = ""


@service(name="InteropEcho", version=1)
class InteropEcho:
    @rpc()
    async def echo(self, req: EchoRequest) -> EchoReply:
        return EchoReply(
            message=req.message,
            count=_wrap_i32(req.count + 1),
            data=req.data,
            server_lang="python",
        )


def _wrap_i32(v: int) -> int:
    return (v + 2**31) % 2**32 - 2**31


def cases(client_lang: str) -> list[tuple[str, EchoRequest]]:
    """The shared cases. Must match ``cases()`` in the Rust and TS drivers."""
    return [
        ("small", EchoRequest(message=f"hello from {client_lang}", count=41, data=bytes([0, 1, 2, 255]))),
        # > 4096 bytes on the wire and highly compressible: exercises zstd.
        ("zstd", EchoRequest(
            message="héllo ✓ " * 200,
            count=-123_456,
            data=bytes(i % 251 for i in range(20_000)),
        )),
        # A single string field longer than 4095 bytes (Fory string-length edge).
        ("long_string", EchoRequest(message="x" * 5000, count=2**31 - 2, data=b"")),
    ]


def check(req: EchoRequest, reply: EchoReply) -> str:
    if reply.message != req.message:
        raise AssertionError(f"message mismatch (len {len(reply.message)} vs {len(req.message)})")
    if reply.count != _wrap_i32(req.count + 1):
        raise AssertionError(f"count {reply.count} != {_wrap_i32(req.count + 1)}")
    if bytes(reply.data) != req.data:
        raise AssertionError(f"data mismatch (len {len(reply.data)} vs {len(req.data)})")
    if not reply.server_lang:
        raise AssertionError("empty server_lang")
    return reply.server_lang


async def _wait_stdin_eof() -> None:
    loop = asyncio.get_running_loop()
    while await loop.run_in_executor(None, sys.stdin.readline):
        pass


async def serve() -> None:
    config = AsterConfig.from_env()
    config.relay_mode = "disabled"
    config.bind_addr = "127.0.0.1:0"
    srv = AsterServer(services=[InteropEcho()], config=config, allow_all_consumers=True)
    await srv.start()
    for _ in range(100):
        if srv.node.node_addr_info().direct_addresses:
            break
        await asyncio.sleep(0.05)
    print(f"ADDRESS {srv.address}", flush=True)
    serve_task = srv.serve()
    try:
        await _wait_stdin_eof()
    finally:
        serve_task.cancel()
        await srv.close()


async def run_client(ticket_str: str) -> bool:
    cfg = EndpointConfig(alpns=[RPC_ALPN], relay_mode="disabled", bind_addr="127.0.0.1:0")
    node = await IrohNode.memory_with_alpns([RPC_ALPN], cfg)
    try:
        t = AsterTicket.from_string(ticket_str)
        addr = NodeAddr(endpoint_id=t.endpoint_id, relay_url=None, direct_addresses=t.direct_addrs)
        conn = await asyncio.wait_for(net_client(node).connect_node_addr(addr, RPC_ALPN), 15)
    except Exception as exc:  # noqa: BLE001
        print(f"CASE connect FAIL {type(exc).__name__}: {exc}", flush=True)
        await node.close()
        return False

    client = create_client(InteropEcho, connection=conn)
    all_ok = True
    for name, req in cases("python"):
        try:
            reply = await asyncio.wait_for(client.echo(req), 15)
            server_lang = check(req, reply)
            print(f"CASE {name} OK server_lang={server_lang}", flush=True)
        except Exception as exc:  # noqa: BLE001
            all_ok = False
            print(f"CASE {name} FAIL {type(exc).__name__}: {exc}", flush=True)
    conn.close(0, b"done")
    await node.close()
    return all_ok


def main() -> int:
    print(f"CONTRACT_ID {contract_id_from_service(InteropEcho)}", flush=True)
    args = sys.argv[1:]
    if args == ["contract"]:
        return 0
    if args == ["server"]:
        asyncio.run(serve())
        return 0
    if len(args) == 2 and args[0] == "client":
        return 0 if asyncio.run(run_client(args[1])) else 1
    print("usage: interop_echo.py contract | server | client <aster1-ticket>", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
