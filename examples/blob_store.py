"""Example: store and retrieve content-addressed blobs."""
import asyncio
from iroh_python import IrohNode, blobs_client


async def main():
    node = await IrohNode.memory()
    blobs = blobs_client(node)

    # Store some data
    data = b"Hello from iroh-python blobs!"
    hash_str = await blobs.add_bytes(data)
    print(f"Stored blob: {hash_str}")

    # Retrieve it
    retrieved = await blobs.read_to_bytes(hash_str)
    print(f"Retrieved: {retrieved.decode()}")
    assert retrieved == data

    # Store empty blob
    empty_hash = await blobs.add_bytes(b"")
    print(f"Empty blob hash: {empty_hash}")

    # Create a share ticket
    ticket = blobs.create_ticket(hash_str)
    print(f"Blob ticket: {ticket}")

    await node.shutdown()
    print("Done.")


if __name__ == "__main__":
    asyncio.run(main())