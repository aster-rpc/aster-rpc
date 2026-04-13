#!/usr/bin/env python3
"""Aster reactor benchmark server -- runs on remote machine.

Uses the Rust-driven reactor for minimal FFI crossings.

Usage:
    cd ~/bench && source .venv/bin/activate
    python remote/aster_reactor_server.py
"""
import asyncio
import struct
import json


async def main():
    from aster._aster import IrohNode, start_reactor

    node = await IrohNode.memory_with_alpns([b"aster/1"])
    print(node.node_id(), flush=True)

    reactor = start_reactor(node, 256)
    TRAILER_FLAG = 0x02

    while True:
        result = await reactor.next_call()
        if result is None:
            break
        call_id, header, hflags, request, rflags, peer, is_session, sender = result

        try:
            req = json.loads(request)
        except Exception:
            req = {}

        agent_id = req.get("agent_id", "")

        if is_session:
            resp = {"task_id": "continue", "command": ""}
            resp_bytes = json.dumps(resp).encode()
            resp_frame = struct.pack("<I", len(resp_bytes) + 1) + bytes([0]) + resp_bytes
            sender.submit(resp_frame, b"")
        else:
            resp = {"agent_id": agent_id, "status": "running", "uptime_secs": 3600}
            resp_bytes = json.dumps(resp).encode()
            resp_frame = struct.pack("<I", len(resp_bytes) + 1) + bytes([0]) + resp_bytes
            trailer = json.dumps({"code": 0, "message": "", "detailKeys": [], "detailValues": []}).encode()
            trailer_frame = struct.pack("<I", len(trailer) + 1) + bytes([TRAILER_FLAG]) + trailer
            sender.submit(resp_frame, trailer_frame)


if __name__ == "__main__":
    asyncio.run(main())
