"""Example: sendme-compatible sender using blob tickets.

Usage:
    uv run python examples/sendme_send.py <file>

Prints a blob ticket that can be used by sendme_recv.py or the Rust sendme tool.
"""

import asyncio
import pathlib
import sys

from iroh_python import IrohNode, blobs_client


async def main(path: str) -> None:
    file_path = pathlib.Path(path)
    data = file_path.read_bytes()

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    hash_hex = await blobs.add_bytes(data)
    ticket = blobs.create_ticket(hash_hex)

    print(f"File: {file_path}")
    print(f"Size: {len(data)} bytes")
    print(f"Hash: {hash_hex}")
    print("\nBlob ticket:\n")
    print(ticket)
    print("\nKeep this process running while the receiver downloads the blob.")

    try:
        while True:
            await asyncio.sleep(3600)
    finally:
        await node.shutdown()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <file>")
        raise SystemExit(1)
    asyncio.run(main(sys.argv[1]))