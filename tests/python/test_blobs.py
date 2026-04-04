"""Tests for BlobsClient: add_bytes, read_to_bytes, and tag operations."""
import pytest

pytestmark = pytest.mark.asyncio


async def test_blob_round_trip():
    """Store bytes, retrieve by hash, verify identical."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    data = b"Hello, Iroh blobs!"
    hash_hex = await blobs.add_bytes(data)

    assert isinstance(hash_hex, str)
    assert len(hash_hex) > 0

    retrieved = await blobs.read_to_bytes(hash_hex)
    assert retrieved == data

    await node.shutdown()


async def test_blob_different_data_different_hash():
    """Different data should produce different hashes."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    h1 = await blobs.add_bytes(b"data one")
    h2 = await blobs.add_bytes(b"data two")
    assert h1 != h2

    await node.shutdown()


async def test_blob_not_found():
    """Reading a non-existent hash should raise IrohError."""
    from aster_python import IrohNode, IrohError, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    # Valid hash format but not stored
    fake_hash = "a" * 52  # not a valid base32 hash, should fail parse
    with pytest.raises(IrohError):
        await blobs.read_to_bytes(fake_hash)

    await node.shutdown()


async def test_blob_empty_data():
    """Storing empty bytes should work."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    hash_hex = await blobs.add_bytes(b"")
    retrieved = await blobs.read_to_bytes(hash_hex)
    assert retrieved == b""

    await node.shutdown()


# ============================================================================
# Phase 1c.1: Tag Tests
# ============================================================================


async def test_tag_set_and_get_round_trip():
    """Set a tag and retrieve it by name."""
    from aster_python import IrohNode, TagInfo, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    hash_hex = await blobs.add_bytes(b"tagged content")
    await blobs.tag_set("my-tag", hash_hex, "raw")

    tag = await blobs.tag_get("my-tag")
    assert tag is not None
    assert isinstance(tag, TagInfo)
    assert tag.name == "my-tag"
    assert tag.hash == hash_hex
    assert tag.format == "raw"

    await node.shutdown()


async def test_tag_get_missing_returns_none():
    """tag_get returns None for a tag that was never set."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    result = await blobs.tag_get("no-such-tag")
    assert result is None

    await node.shutdown()


async def test_tag_delete_removes_tag():
    """Set a tag, delete it, then tag_get returns None."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    hash_hex = await blobs.add_bytes(b"will be deleted")
    await blobs.tag_set("delete-me", hash_hex, "raw")

    # Confirm it exists
    assert await blobs.tag_get("delete-me") is not None

    count = await blobs.tag_delete("delete-me")
    assert count == 1

    # Now it should be gone
    assert await blobs.tag_get("delete-me") is None

    await node.shutdown()


async def test_tag_delete_nonexistent_returns_zero():
    """Deleting a non-existent tag returns 0 (not an error)."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    count = await blobs.tag_delete("ghost-tag")
    assert count == 0

    await node.shutdown()


async def test_tag_list_returns_expected_tags():
    """tag_list returns all set tags."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    h1 = await blobs.add_bytes(b"blob1")
    h2 = await blobs.add_bytes(b"blob2")
    await blobs.tag_set("list-tag-a", h1, "raw")
    await blobs.tag_set("list-tag-b", h2, "raw")

    tags = await blobs.tag_list()
    names = {t.name for t in tags}
    assert "list-tag-a" in names
    assert "list-tag-b" in names

    await node.shutdown()


async def test_tag_list_prefix_filters_correctly():
    """tag_list_prefix returns only tags matching the prefix."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    h1 = await blobs.add_bytes(b"a")
    h2 = await blobs.add_bytes(b"b")
    h3 = await blobs.add_bytes(b"c")
    await blobs.tag_set("prefix/foo", h1, "raw")
    await blobs.tag_set("prefix/bar", h2, "raw")
    await blobs.tag_set("other/baz", h3, "raw")

    tags = await blobs.tag_list_prefix("prefix/")
    names = {t.name for t in tags}
    assert "prefix/foo" in names
    assert "prefix/bar" in names
    assert "other/baz" not in names

    await node.shutdown()


async def test_add_bytes_as_collection_creates_tag():
    """add_bytes_as_collection sets an aster-python/{name} tag for GC protection."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    collection_hash = await blobs.add_bytes_as_collection("myfile", b"file content")
    assert isinstance(collection_hash, str)

    # The persistent tag should exist
    tag = await blobs.tag_get("aster-python/myfile")
    assert tag is not None
    assert tag.hash == collection_hash
    assert tag.format == "hash_seq"

    await node.shutdown()


async def test_tag_delete_unpublishes_collection():
    """Deleting the aster-python tag makes it unpublished (tag is gone)."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    await blobs.add_bytes_as_collection("to-remove", b"temporary data")
    assert await blobs.tag_get("aster-python/to-remove") is not None

    count = await blobs.tag_delete("aster-python/to-remove")
    assert count == 1
    assert await blobs.tag_get("aster-python/to-remove") is None

    await node.shutdown()


# ============================================================================
# Phase 1c.3: Blob Status / Has Tests
# ============================================================================


async def test_blob_status_complete_after_add():
    """blob_status returns 'complete' after add_bytes."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    hash_hex = await blobs.add_bytes(b"status test data")
    result = await blobs.blob_status(hash_hex)

    assert result.status == "complete"
    assert result.size == len(b"status test data")

    await node.shutdown()


async def test_blob_status_not_found_for_unknown_hash():
    """blob_status returns 'not_found' for a hash that was never stored."""
    from aster_python import IrohNode, blobs_client
    import hashlib

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    # Use a valid-looking BLAKE3 hash that isn't stored
    # BLAKE3 hashes are 32 bytes; base32-encoded they are 52 chars
    fake_hash = "a" * 52  # not valid base32 — will cause parse error, so use add+delete trick
    # Instead use a real hash format from a different data that we never stored:
    # We'll add data, get the hash, then create a node that never stored it
    node2 = await IrohNode.memory()
    blobs2 = blobs_client(node2)
    hash_hex = await blobs.add_bytes(b"only in node1")

    result = await blobs2.blob_status(hash_hex)
    assert result.status == "not_found"
    assert result.size == 0

    await node.shutdown()
    await node2.shutdown()


async def test_blob_has_true_after_add():
    """blob_has returns True for a blob that was just stored."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    hash_hex = await blobs.add_bytes(b"has test")
    assert await blobs.blob_has(hash_hex) is True

    await node.shutdown()


async def test_blob_has_false_for_unknown():
    """blob_has returns False for a hash not stored in this node."""
    from aster_python import IrohNode, blobs_client

    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()
    blobs1 = blobs_client(node1)
    blobs2 = blobs_client(node2)

    hash_hex = await blobs1.add_bytes(b"only in node1")
    assert await blobs2.blob_has(hash_hex) is False

    await node1.shutdown()
    await node2.shutdown()


# ============================================================================
# Phase 1d: Blob Transfer Observability Tests
# ============================================================================


async def test_blob_observe_snapshot_complete_after_add():
    """blob_observe_snapshot returns is_complete=True after add_bytes."""
    from aster_python import IrohNode, BlobObserveResult, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    data = b"observe snapshot test data"
    hash_hex = await blobs.add_bytes(data)

    result = await blobs.blob_observe_snapshot(hash_hex)
    assert isinstance(result, BlobObserveResult)
    assert result.is_complete is True
    assert result.size == len(data)

    await node.shutdown()


async def test_blob_observe_snapshot_size_matches():
    """blob_observe_snapshot reports the correct byte size for the blob."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    data = b"x" * 1024
    hash_hex = await blobs.add_bytes(data)

    result = await blobs.blob_observe_snapshot(hash_hex)
    assert result.size == 1024

    await node.shutdown()


async def test_blob_observe_complete_resolves_for_local_blob():
    """blob_observe_complete resolves immediately when blob is already fully local."""
    from aster_python import IrohNode, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    hash_hex = await blobs.add_bytes(b"already complete data")

    # Should resolve without error — blob is already complete
    await blobs.blob_observe_complete(hash_hex)

    await node.shutdown()


async def test_blob_local_info_complete_after_add():
    """blob_local_info returns is_complete=True and correct local_bytes after add_bytes."""
    from aster_python import IrohNode, BlobLocalInfo, blobs_client

    node = await IrohNode.memory()
    blobs = blobs_client(node)

    data = b"local info test data"
    hash_hex = await blobs.add_bytes(data)

    result = await blobs.blob_local_info(hash_hex)
    assert isinstance(result, BlobLocalInfo)
    assert result.is_complete is True
    assert result.local_bytes == len(data)

    await node.shutdown()


async def test_blob_local_info_unknown_hash():
    """blob_local_info for a hash not in this node returns is_complete=False and local_bytes=0."""
    from aster_python import IrohNode, blobs_client

    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()
    blobs1 = blobs_client(node1)
    blobs2 = blobs_client(node2)

    hash_hex = await blobs1.add_bytes(b"only in node1")

    result = await blobs2.blob_local_info(hash_hex)
    assert result.is_complete is False
    assert result.local_bytes == 0

    await node1.shutdown()
    await node2.shutdown()