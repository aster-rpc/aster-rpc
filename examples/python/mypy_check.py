"""Example code for mypy type checking verification."""

import asyncio
from aster_python import (
    IrohNode,
    BlobsClient,
    DocsClient,
    GossipClient,
    NetClient,
    blobs_client,
    docs_client,
    gossip_client,
    net_client,
)


async def main() -> None:
    node: IrohNode = await IrohNode.memory()
    node_id: str = node.node_id()
    addr: str = node.node_addr()

    blobs: BlobsClient = blobs_client(node)
    hash_hex: str = await blobs.add_bytes(b"hello")
    data: bytes = await blobs.read_to_bytes(hash_hex)

    docs: DocsClient = docs_client(node)
    doc = await docs.create()
    author: str = await docs.create_author()
    await doc.set_bytes(author, b"key", b"value")

    _gossip: GossipClient = gossip_client(node)
    _net: NetClient = net_client(node)

    await node.shutdown()


if __name__ == "__main__":
    asyncio.run(main())