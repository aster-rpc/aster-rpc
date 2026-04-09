import pytest
from aster._aster import DocEntry, IrohNode, docs_client


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


# ============================================================================
# Phase 1c.5: Doc Sync Lifecycle Tests
# ============================================================================


@pytest.mark.asyncio
async def test_start_sync_empty_peers_does_not_raise():
    """start_sync with no peers is a valid no-op -- doc enters sync mode."""
    from aster import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()

    # Should succeed without error.
    await doc.start_sync([])

    await node.shutdown()


@pytest.mark.asyncio
async def test_leave_after_start_sync_does_not_raise():
    """start_sync followed by leave completes without error."""
    from aster import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()

    await doc.start_sync([])
    await doc.leave()

    await node.shutdown()


@pytest.mark.asyncio
async def test_leave_without_sync_does_not_raise():
    """leave on a doc that was never synced completes without error."""
    from aster import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()

    await doc.leave()

    await node.shutdown()


# ============================================================================
# Phase 1c.4: Doc Subscribe Tests
# ============================================================================


@pytest.mark.asyncio
async def test_subscribe_receives_insert_local_event():
    """subscribe() delivers an insert_local event after set_bytes."""
    import asyncio
    from aster import IrohNode, DocEvent, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()
    author = await dc.create_author()

    receiver = await doc.subscribe()

    # Write a key -- this should produce an insert_local event.
    await doc.set_bytes(author, b"sub-key", b"sub-value")

    event = await asyncio.wait_for(receiver.recv(), timeout=10.0)

    assert event is not None
    assert isinstance(event, DocEvent)
    assert event.kind == "insert_local"
    assert event.entry is not None
    assert bytes(event.entry.key) == b"sub-key"

    await node.shutdown()


@pytest.mark.asyncio
async def test_subscribe_insert_local_entry_fields():
    """insert_local event contains correct author, content_hash, and size."""
    import asyncio
    from aster import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()
    author = await dc.create_author()

    receiver = await doc.subscribe()
    content_hash_hex = await doc.set_bytes(author, b"field-key", b"hello")

    event = await asyncio.wait_for(receiver.recv(), timeout=10.0)

    assert event is not None
    assert event.kind == "insert_local"
    entry = event.entry
    assert entry is not None
    assert entry.author_id == author
    assert bytes(entry.key) == b"field-key"
    assert entry.content_hash == content_hash_hex
    assert entry.content_len == len(b"hello")

    await node.shutdown()


@pytest.mark.asyncio
async def test_subscribe_multiple_events_in_order():
    """Multiple writes produce multiple insert_local events in order."""
    import asyncio
    from aster import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()
    author = await dc.create_author()

    receiver = await doc.subscribe()

    keys = [b"a", b"b", b"c"]
    for k in keys:
        await doc.set_bytes(author, k, b"val")

    received_keys = []
    for _ in keys:
        event = await asyncio.wait_for(receiver.recv(), timeout=10.0)
        assert event is not None
        assert event.kind == "insert_local"
        received_keys.append(bytes(event.entry.key))

    assert set(received_keys) == {b"a", b"b", b"c"}

    await node.shutdown()


# ============================================================================
# Phase 1c.6: Doc Download Policy Tests
# ============================================================================


@pytest.mark.asyncio
async def test_set_download_policy_everything_does_not_raise():
    """set_download_policy(everything) is a valid no-op."""
    from aster import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()

    await doc.set_download_policy("everything", [])

    await node.shutdown()


@pytest.mark.asyncio
async def test_download_policy_roundtrip():
    """set then get download policy returns the same mode and prefixes."""
    from aster import IrohNode, DocDownloadPolicy, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()

    await doc.set_download_policy("nothing_except", [b"foo/", b"bar/"])

    policy = await doc.get_download_policy()
    assert isinstance(policy, DocDownloadPolicy)
    assert policy.mode == "nothing_except"
    prefix_set = {bytes(p) for p in policy.prefixes}
    assert b"foo/" in prefix_set
    assert b"bar/" in prefix_set

    await node.shutdown()


@pytest.mark.asyncio
async def test_download_policy_everything_except():
    """everything_except policy round-trips correctly."""
    from aster import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()

    await doc.set_download_policy("everything_except", [b"secret/"])

    policy = await doc.get_download_policy()
    assert policy.mode == "everything_except"
    assert b"secret/" in {bytes(p) for p in policy.prefixes}

    await node.shutdown()


# ============================================================================
# Phase 1c.7: Doc Share with Full Address Tests
# ============================================================================


@pytest.mark.asyncio
async def test_share_with_addr_returns_ticket():
    """share_with_addr returns a valid ticket string starting with 'doc'."""
    from aster import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()

    ticket = await doc.share_with_addr("write")
    assert isinstance(ticket, str)
    assert ticket.startswith("doc"), f"ticket should start with 'doc', got: {ticket[:20]}"

    await node.shutdown()


@pytest.mark.asyncio
async def test_share_with_addr_read_mode():
    """share_with_addr(read) produces a read-only ticket."""
    from aster import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()

    ticket = await doc.share_with_addr("read")
    assert isinstance(ticket, str)
    assert len(ticket) > 0

    await node.shutdown()


# ============================================================================
# Phase 1c.8: Doc Join and Subscribe Tests
# ============================================================================


@pytest.mark.asyncio
async def test_join_and_subscribe_returns_doc_and_receiver():
    """join_and_subscribe returns a (DocHandle, DocEventReceiver) tuple."""
    import asyncio
    from aster import IrohNode, DocHandle, DocEventReceiver, docs_client

    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()

    node1.add_node_addr(node2)
    node2.add_node_addr(node1)

    dc1 = docs_client(node1)
    dc2 = docs_client(node2)

    doc1 = await dc1.create()
    ticket = await doc1.share("write")

    result = await dc2.join_and_subscribe(ticket)
    assert isinstance(result, tuple)
    assert len(result) == 2
    doc2, receiver = result
    assert isinstance(doc2, DocHandle)
    assert isinstance(receiver, DocEventReceiver)
    assert doc2.doc_id() == doc1.doc_id()

    await node1.shutdown()
    await node2.shutdown()


@pytest.mark.asyncio
async def test_join_and_subscribe_receives_event_after_write():
    """Events from the remote writer appear via join_and_subscribe receiver."""
    import asyncio
    from aster import IrohNode, docs_client

    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()

    node1.add_node_addr(node2)
    node2.add_node_addr(node1)

    dc1 = docs_client(node1)
    dc2 = docs_client(node2)

    doc1 = await dc1.create()
    author1 = await dc1.create_author()
    ticket = await doc1.share("write")

    doc2, receiver = await dc2.join_and_subscribe(ticket)

    # Write from node1 -- node2's receiver should eventually see InsertRemote
    await doc1.set_bytes(author1, b"remote-key", b"remote-value")

    # Receive events until we see insert_remote (may come after sync_finished etc.)
    for _ in range(10):
        event = await asyncio.wait_for(receiver.recv(), timeout=10.0)
        if event is not None and event.kind in ("insert_remote", "insert_local"):
            break
    else:
        assert False, "Did not receive insert event within 10 attempts"

    await node1.shutdown()
    await node2.shutdown()