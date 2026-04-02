"""Example: sendme-compatible receiver using blob tickets.

Usage:
    uv run python examples/python/sendme_recv.py <blob-ticket> [output-file]

Accepts a blob ticket produced by sendme_send.py or the Rust sendme tool.
"""

import asyncio
import pathlib
import sys

from aster_python import IrohNode, blobs_client


async def main(ticket: str, output: str | None) -> None:
    node = await IrohNode.memory()
    blobs = blobs_client(node)

    try:
        data = await blobs.download_blob(ticket)
        if output is None:
            print(data.decode("utf-8", errors="replace"))
        else:
            out = pathlib.Path(output)
            out.write_bytes(data)
            print(f"Wrote {len(data)} bytes to {out}")
    finally:
        await node.shutdown()


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3):
        print(f"Usage: {sys.argv[0]} <blob-ticket> [output-file]")
        raise SystemExit(1)
    asyncio.run(main(sys.argv[1], sys.argv[2] if len(sys.argv) == 3 else None))