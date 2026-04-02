"""Tests for BlobsClient: add_bytes and read_to_bytes."""
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