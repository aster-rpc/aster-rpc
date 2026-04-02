"""Example: sendme-compatible sender using blob tickets.

Usage:
    uv run python examples/python/sendme_send.py <file>

Prints a blob ticket that can be used by sendme_recv.py or the Rust sendme tool.

IMPORTANT: This uses Collection (HashSeq) format, which is compatible with
the `sendme` CLI tool. The old Raw format is NOT compatible with sendme.
"""

import asyncio
import pathlib
import sys

from aster_python import IrohNode, blobs_client


async def main(path: str) -> None:
    file_path = pathlib.Path(path)
    data = file_path.read_bytes()

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    # Store as a Collection (HashSeq) — this is what sendme expects
    collection_hash = await blobs.add_bytes_as_collection(file_path.name, data)
    ticket = blobs.create_collection_ticket(collection_hash)

    print(f"File: {file_path}")
    print(f"Size: {len(data)} bytes")
    print(f"Hash: {collection_hash}")
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