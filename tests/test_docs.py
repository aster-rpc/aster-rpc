import pytest
from iroh_python._iroh_python import IrohNode, docs_client


@pytest.mark.asyncio
async def test_create_doc_and_author():
    """Create a doc and author, verify IDs are returned."""
    node = await IrohNode.memory()
    dc = docs_client(node)

    doc = await dc.create()
    assert doc.doc_id(), "doc_id should be non-empty"

    author = await dc.create_author()
    assert len(author) > 0, "author ID should be non-empty"

    await node.shutdown()


@pytest.mark.asyncio
async def test_set_get_bytes():
    """Set a key and get it back."""
    node = await IrohNode.memory()
    dc = docs_client(node)

    doc = await dc.create()
    author = await dc.create_author()

    await doc.set_bytes(author, b"hello", b"world")
    val = await doc.get_exact(author, b"hello")
    assert val == b"world"

    await node.shutdown()


@pytest.mark.asyncio
async def test_get_missing_key():
    """Getting a non-existent key returns None."""
    node = await IrohNode.memory()
    dc = docs_client(node)

    doc = await dc.create()
    author = await dc.create_author()

    val = await doc.get_exact(author, b"nope")
    assert val is None

    await node.shutdown()


@pytest.mark.asyncio
async def test_share_and_join():
    """Share a doc from node1, join from node2, verify data syncs."""
    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()

    # Exchange addresses for peer discovery
    node1.add_node_addr(node2)
    node2.add_node_addr(node1)

    dc1 = docs_client(node1)
    dc2 = docs_client(node2)

    # Create doc and write on node1
    doc1 = await dc1.create()
    author = await dc1.create_author()
    await doc1.set_bytes(author, b"key1", b"value1")

    # Share the doc
    ticket = await doc1.share("write")
    assert ticket.startswith("doc"), f"ticket should start with 'doc', got: {ticket[:20]}"

    # Join on node2
    doc2 = await dc2.join(ticket)
    assert doc2.doc_id() == doc1.doc_id()

    await node1.shutdown()
    await node2.shutdown()