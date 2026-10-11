"""GC custody and verified retained imports through the real native store."""
from __future__ import annotations

import asyncio
import pytest
from aster import AsterConfig, BlobCustody, BlobFormat, Node, blobs_client
from aster import _aster

pytestmark = [pytest.mark.network, pytest.mark.timeout(30)]


def digest(data):
    return _aster.blake3_hex(data)


@pytest.fixture
async def blobs():
    node = await Node.start(AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0", gc_interval_ms=3600000))
    try:
        yield blobs_client(node)
    finally:
        await node.shutdown()


async def test_multi_root_custody_discovery_and_release_without_anonymous_tags(blobs):
    scope = blobs.custody()
    assert isinstance(scope, BlobCustody)
    expected = digest(b"before fetch")
    await scope.protect(expected)
    assert await scope.add_bytes(b"before fetch") == expected
    discovered = await scope.add_bytes(b"discovered during verification")
    assert await blobs.tag_list() == []
    await blobs.gc_run_once()
    assert await scope.read(expected) == b"before fetch"
    assert await scope.read(discovered) == b"discovered during verification"
    await scope.aclose()
    with pytest.raises(Exception, match="closed"):
        scope.read(expected)
    await blobs.gc_run_once()
    assert not await blobs.blob_has(expected)
    assert not await blobs.blob_has(discovered)
    await scope.aclose()


async def test_hashseq_scope_handover_is_not_a_collection_and_keeps_children(blobs):
    async with blobs.custody() as scope:
        child = await scope.add_bytes(b"arbitrary hashseq child")
        sequence = bytes.fromhex(child)
        root = await scope.add_bytes(sequence, format=BlobFormat.HASH_SEQ)
        with pytest.raises(Exception, match="not protected"):
            await scope.retain(root, "wrong-format", format=BlobFormat.RAW)
        assert await scope.read(root) == sequence
        # read adds a Raw guard too; existing HashSeq custody is preserved.
        with pytest.raises(Exception, match="not protected"):
            await scope.retain(child, "wrong-format", format=BlobFormat.HASH_SEQ)
        await scope.retain(root, "retained/seq", format=BlobFormat.HASH_SEQ)
    await blobs.gc_run_once()
    assert await blobs.blob_has(child)
    tag = await blobs.tag_get("retained/seq")
    assert tag.hash == root and tag.format == "hash_seq"
    assert [tag.name for tag in await blobs.tag_list()] == ["retained/seq"]
    await blobs.tag_delete("retained/seq")
    await blobs.gc_run_once()
    assert not await blobs.blob_has(root) and not await blobs.blob_has(child)


async def test_verified_bytes_mismatch_and_replacement_have_only_exact_named_tag(blobs):
    expected = digest(b"original")
    assert await blobs.add_bytes_verified(b"original", expected, "stable") == expected
    with pytest.raises(ValueError, match="hash mismatch"):
        await blobs.add_bytes_verified(b"wrong", expected, "stable")
    assert (await blobs.tag_get("stable")).hash == expected
    await blobs.gc_run_once()
    assert not await blobs.blob_has(digest(b"wrong"))
    replacement = digest(b"replacement")
    await blobs.add_bytes_verified(b"replacement", replacement, "stable")
    await blobs.gc_run_once()
    assert not await blobs.blob_has(expected)
    tags = await blobs.tag_list()
    assert [(tag.name, tag.hash, tag.format) for tag in tags] == [("stable", replacement, "raw")]
    with pytest.raises(Exception):
        await blobs.add_bytes_verified(b"", digest(b""), "")
    with pytest.raises(ValueError):
        blobs.add_bytes_verified(b"", digest(b""), "invalid", format="invalid")


async def test_verified_path_copy_and_hashseq_format_preserve_retention(blobs, tmp_path):
    # Retain an arbitrary one-hash sequence through the verified bytes path.
    scope = blobs.custody()
    child = await scope.add_bytes(b"kept child")
    sequence = bytes.fromhex(child)
    root = digest(sequence)
    await blobs.add_bytes_verified(sequence, root, "typed", format="hash_seq")
    await scope.aclose()
    await blobs.gc_run_once()
    assert await blobs.blob_has(child)
    assert (await blobs.tag_get("typed")).format == "hash_seq"
    sequence_path = tmp_path / "sequence"
    sequence_path.write_bytes(sequence)
    await blobs.add_path_verified(str(sequence_path), root, "typed-path", format=BlobFormat.HASH_SEQ)
    sequence_path.write_bytes(b"changed source sequence")
    await blobs.tag_delete("typed")
    await blobs.gc_run_once()
    assert await blobs.blob_has(child)
    assert await blobs.read_to_bytes(root) == sequence
    assert (await blobs.tag_get("typed-path")).format == "hash_seq"
    path = tmp_path / "source"
    path.write_bytes(b"copied verified")
    expected = digest(path.read_bytes())
    assert await blobs.add_path_verified(str(path), expected, "copy") == expected
    path.write_bytes(b"changed")
    assert await blobs.read_to_bytes(expected) == b"copied verified"
    with pytest.raises(ValueError, match="hash mismatch"):
        await blobs.add_path_verified(str(path), expected, "copy")
    assert (await blobs.tag_get("copy")).hash == expected


async def test_cancel_before_first_step_has_no_import_or_commit(blobs):
    data = b"never start native work"
    task = asyncio.create_task(blobs.add_bytes_verified(data, digest(data), "never"))
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert await blobs.tag_get("never") is None
    scope = blobs.custody()
    task = asyncio.create_task(scope.add_bytes(data))
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    await scope.aclose()
    await blobs.gc_run_once()
    assert not await blobs.blob_has(digest(data))


async def test_active_verified_path_cancel_joins_cleanup_or_returns_committed_success(blobs, tmp_path):
    previous = digest(b"previous")
    await blobs.add_bytes_verified(b"previous", previous, "stable")
    path = tmp_path / "large"
    contents = b"active-cancellation" * (2 * 1024 * 1024)
    path.write_bytes(contents)
    expected = digest(contents)
    task = asyncio.create_task(blobs.add_path_verified(str(path), expected, "stable"))
    await asyncio.sleep(0)
    task.cancel()
    try:
        result = await task
    except asyncio.CancelledError:
        assert (await blobs.tag_get("stable")).hash == previous
        await blobs.gc_run_once()
        assert not await blobs.blob_has(expected)
    else:
        assert result == expected
        assert (await blobs.tag_get("stable")).hash == expected
    assert len(await blobs.tag_list()) == 1


async def test_admitted_operation_survives_close_and_aclose_joins_it(blobs):
    scope = blobs.custody()
    pending = scope.add_bytes(b"admitted before close")
    scope.close()
    assert await pending == digest(b"admitted before close")
    await scope.aclose()
    await blobs.gc_run_once()
    assert not await blobs.blob_has(digest(b"admitted before close"))


async def test_scope_download_retains_arbitrary_hashseq_through_gc_and_close():
    cfg = AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0", gc_interval_ms=3600000)
    provider = await Node.start(cfg)
    consumer = await Node.start(cfg)
    consumer.add_node_addr(provider)
    try:
        source, dest = blobs_client(provider), blobs_client(consumer)
        async with source.custody() as keep:
            child = await keep.add_bytes(b"downloaded child")
            sequence = bytes.fromhex(child)
            root = await keep.add_bytes(sequence, format="hash_seq")
            scope = dest.custody()
            report = await scope.download(root, [provider.id], format=BlobFormat.HASH_SEQ)
            assert report.bytes_transferred > 0
            await dest.gc_run_once()
            assert await scope.read(root) == sequence
            assert await scope.read(child) == b"downloaded child"
            await scope.aclose()
            await dest.gc_run_once()
            assert not await dest.blob_has(root) and not await dest.blob_has(child)
            assert await dest.tag_list() == []
    finally:
        await consumer.shutdown()
        await provider.shutdown()


async def test_python_cancellation_waits_terminal_and_returns_raced_committed_success():
    from aster.custody import _await_operation

    terminal = asyncio.get_running_loop().create_future()

    class Pending:
        def start(self):
            return terminal

    class Cancellation:
        requested = False

        def request_cancel(self):
            self.requested = True

    cancellation = Cancellation()
    waiter = asyncio.create_task(_await_operation(Pending(), cancellation))
    await asyncio.sleep(0)
    waiter.cancel()
    await asyncio.sleep(0)
    assert cancellation.requested and not waiter.done()
    # Repeated cancellation still cannot manufacture a cleanup terminal.
    waiter.cancel()
    await asyncio.sleep(0)
    assert not waiter.done()
    terminal.set_result("committed-success")
    assert await waiter == "committed-success"
