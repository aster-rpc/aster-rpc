"""Ordered local-only Docs batch reads through the real native store."""
from __future__ import annotations

import asyncio

import pytest

from aster import AsterConfig, Hash, Node, blobs_client, docs_client
from aster import _aster

pytestmark = [pytest.mark.network, pytest.mark.timeout(30)]


def config(**kwargs):
    return AsterConfig(
        relay_mode="disabled", bind_addr="127.0.0.1:0",
        gc_interval_ms=3600000, **kwargs,
    )


@pytest.fixture
async def node():
    node = await Node.start(config())
    try:
        yield node
    finally:
        await node.shutdown()


async def test_order_duplicates_binary_empty_missing_and_typed_hash(node):
    blobs = blobs_client(node)
    doc = await docs_client(node).create()
    async with blobs.custody() as scope:
        binary = b"\x00\xff\x80native\x00bytes"
        present = await scope.add_bytes(binary)
        empty = await scope.add_bytes(b"")
        missing = _aster.blake3_hex(b"not imported")
        before = await blobs.tag_list()
        values = await doc.read_entry_contents_if_complete(
            [missing, Hash(present), empty, present.upper(), missing]
        )
        assert values == [None, binary, b"", binary, None]
        assert all(value is None or isinstance(value, bytes) for value in values)
        assert await doc.read_entry_contents_if_complete([]) == []
        assert await blobs.tag_list() == before


async def test_malformed_input_fails_whole_batch_without_task_panic(node):
    blobs = blobs_client(node)
    doc = await docs_client(node).create()
    async with blobs.custody() as scope:
        present = await scope.add_bytes(b"valid before malformed")
        for invalid in ["invalid", "a" * 52, "g" * 64, "é" * 32]:
            with pytest.raises(Exception, match="exactly 64 ASCII hexadecimal characters"):
                await doc.read_entry_contents_if_complete([present, invalid, present])
        assert await doc.read_entry_contents_if_complete([present]) == [b"valid before malformed"]


async def test_batch_does_not_fetch_even_with_known_available_provider(node):
    provider = await Node.start(config())
    node.add_node_addr(provider)
    try:
        source, local = blobs_client(provider), blobs_client(node)
        doc = await docs_client(node).create()
        async with source.custody() as scope:
            content_hash = await scope.add_bytes(b"available only at provider")
            assert await doc.read_entry_contents_if_complete([content_hash, content_hash]) == [None, None]
            assert not await local.blob_has(content_hash)
            assert await local.tag_list() == []
    finally:
        await provider.shutdown()


async def test_custody_stabilizes_large_export_during_gc_and_release_allows_absence(tmp_path):
    node = await Node.start(config(storage_path=str(tmp_path / "store")))
    try:
        blobs = blobs_client(node)
        doc = await docs_client(node).create()
        scope = blobs.custody()
        large = bytes(range(256)) * 4097  # Above the native inline threshold.
        content_hash = await scope.add_bytes(large)
        values, _ = await asyncio.gather(
            doc.read_entry_contents_if_complete([content_hash, content_hash]),
            blobs.gc_run_once(),
        )
        assert values == [large, large]
        assert all(isinstance(value, bytes) for value in values)
        await scope.aclose()
        await blobs.gc_run_once()
        assert await doc.read_entry_contents_if_complete([content_hash]) == [None]
        assert await blobs.tag_list() == []
    finally:
        await node.shutdown()
