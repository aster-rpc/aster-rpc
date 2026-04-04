import pytest
from aster_python._aster_python import DocEntry, IrohNode, docs_client


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


# ============================================================================
# Phase 1b: Doc Query Method Tests
# ============================================================================


@pytest.mark.asyncio
async def test_query_key_exact_returns_entry():
    """Write an entry and retrieve it via query_key_exact."""
    node = await IrohNode.memory()
    dc = docs_client(node)

    doc = await dc.create()
    author = await dc.create_author()

    await doc.set_bytes(author, b"mykey", b"myvalue")

    entries = await doc.query_key_exact(b"mykey")
    assert len(entries) >= 1
    entry = entries[0]
    assert isinstance(entry, DocEntry)
    assert entry.author_id == author
    assert bytes(entry.key) == b"mykey"
    assert isinstance(entry.content_hash, str)
    assert len(entry.content_hash) > 0
    assert entry.content_len == len(b"myvalue")
    assert isinstance(entry.timestamp, int)

    await node.shutdown()


@pytest.mark.asyncio
async def test_query_key_exact_missing_returns_empty():
    """query_key_exact returns empty list for a key that was never written."""
    node = await IrohNode.memory()
    dc = docs_client(node)

    doc = await dc.create()

    entries = await doc.query_key_exact(b"nosuchkey")
    assert entries == []

    await node.shutdown()


@pytest.mark.asyncio
async def test_query_key_exact_multiple_authors():
    """Two authors writing the same key both appear in query_key_exact."""
    node = await IrohNode.memory()
    dc = docs_client(node)

    doc = await dc.create()
    author1 = await dc.create_author()
    author2 = await dc.create_author()

    await doc.set_bytes(author1, b"shared", b"from_author1")
    await doc.set_bytes(author2, b"shared", b"from_author2")

    entries = await doc.query_key_exact(b"shared")
    assert len(entries) == 2
    author_ids = {e.author_id for e in entries}
    assert author1 in author_ids
    assert author2 in author_ids

    await node.shutdown()


@pytest.mark.asyncio
async def test_query_key_prefix_returns_matching_entries():
    """query_key_prefix returns all entries whose key starts with the prefix."""
    node = await IrohNode.memory()
    dc = docs_client(node)

    doc = await dc.create()
    author = await dc.create_author()

    await doc.set_bytes(author, b"foo/a", b"val_a")
    await doc.set_bytes(author, b"foo/b", b"val_b")
    await doc.set_bytes(author, b"bar/c", b"val_c")

    entries = await doc.query_key_prefix(b"foo/")
    assert len(entries) == 2
    keys = {bytes(e.key) for e in entries}
    assert b"foo/a" in keys
    assert b"foo/b" in keys

    await node.shutdown()


@pytest.mark.asyncio
async def test_query_key_prefix_no_match_returns_empty():
    """query_key_prefix returns empty list when no keys match the prefix."""
    node = await IrohNode.memory()
    dc = docs_client(node)

    doc = await dc.create()
    author = await dc.create_author()

    await doc.set_bytes(author, b"abc", b"val")

    entries = await doc.query_key_prefix(b"xyz/")
    assert entries == []

    await node.shutdown()


@pytest.mark.asyncio
async def test_read_entry_content_roundtrip():
    """Write an entry, query it, then read its content via content_hash."""
    node = await IrohNode.memory()
    dc = docs_client(node)

    doc = await dc.create()
    author = await dc.create_author()

    payload = b"hello content roundtrip"
    await doc.set_bytes(author, b"testkey", payload)

    entries = await doc.query_key_exact(b"testkey")
    assert len(entries) == 1
    entry = entries[0]

    content = await doc.read_entry_content(entry.content_hash)
    assert bytes(content) == payload

    await node.shutdown()


@pytest.mark.asyncio
async def test_query_and_filter_by_author():
    """Write with two authors, query by key, then filter by author."""
    node = await IrohNode.memory()
    dc = docs_client(node)

    doc = await dc.create()
    author_a = await dc.create_author()
    author_b = await dc.create_author()

    await doc.set_bytes(author_a, b"k", b"from_a")
    await doc.set_bytes(author_b, b"k", b"from_b")

    entries = await doc.query_key_exact(b"k")
    assert len(entries) == 2

    a_entries = [e for e in entries if e.author_id == author_a]
    b_entries = [e for e in entries if e.author_id == author_b]
    assert len(a_entries) == 1
    assert len(b_entries) == 1

    content_a = await doc.read_entry_content(a_entries[0].content_hash)
    content_b = await doc.read_entry_content(b_entries[0].content_hash)
    assert bytes(content_a) == b"from_a"
    assert bytes(content_b) == b"from_b"

    await node.shutdown()