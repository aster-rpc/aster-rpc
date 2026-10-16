"""Data-plane parity with the Rust SDK (plan P6): blobs, docs, net.

Blobs: ``read_range``, ``FetchStrategy`` / ``FetchReport`` /
``ProviderOutcome``, ``download_hash`` / ``download_hash_to_store`` /
``download_hash_from`` / ``download_hash_to_store_from`` /
``download_collection_from``, ``tag_delete_prefix``, ``gc_run_once``, typed
``Hash`` arguments. Docs: authority report + fingerprint,
``retire_local_write_authority`` / ``RetireOutcome``, ``LocalAccess``,
``set_bytes_durable``, ``entry_exists``, entry content status, open / import
namespace, ``ShareMode`` / ``DownloadPolicy``, events as an async iterator.
Net: ``paths()`` / ``selected_path()`` / ``PathInfo``, stream priority.
"""

from __future__ import annotations

import asyncio
import os

import pytest
import pytest_asyncio

from aster import (
    AuthorityFingerprint,
    AuthorityReport,
    BlobFormat,
    BlobTransferCounts,
    BlobTransferStats,
    DownloadPolicy,
    FetchReport,
    FetchStrategy,
    Hash,
    LocalAccess,
    Node,
    NodeId,
    PathInfo,
    ProviderOutcome,
    RetireOutcome,
    ShareMode,
    blobs_client,
    create_endpoint,
    docs_client,
)


@pytest_asyncio.fixture
async def pair():
    a = await Node.start()
    b = await Node.start()
    a.add_node_addr(b)
    b.add_node_addr(a)
    yield a, b
    await a.shutdown()
    await b.shutdown()


@pytest_asyncio.fixture
async def one():
    n = await Node.start()
    yield n
    await n.shutdown()


# -- Blobs -------------------------------------------------------------------


@pytest.mark.asyncio
async def test_read_range_and_typed_hash_arguments(one):
    blobs = blobs_client(one)
    data = bytes(range(256)) * 4
    h = await blobs.add_bytes(data)
    typed = Hash(h)
    assert await blobs.read_range(typed, 10, 20) == data[10:30]
    assert await blobs.read_range(h, 0, len(data)) == data
    assert await blobs.read_to_bytes(typed) == data
    assert await blobs.blob_has(typed) is True
    status = await blobs.blob_status(typed)
    assert status.status == "complete" and status.is_complete and status.size == len(data)
    await blobs.tag_set("typed/tag", typed, BlobFormat.RAW)
    tag = await blobs.tag_get("typed/tag")
    assert tag.hash == h and tag.format == "raw"


@pytest.mark.asyncio
async def test_tag_delete_prefix(one):
    blobs = blobs_client(one)
    h = await blobs.add_bytes(b"tagged")
    for name in ("app/a", "app/b", "other/c"):
        await blobs.tag_set(name, h)
    assert await blobs.tag_delete_prefix("app/") == 2
    names = {t.name for t in await blobs.tag_list()}
    assert "other/c" in names and "app/a" not in names and "app/b" not in names


@pytest.mark.asyncio
async def test_gc_run_once_reclaims_untagged_and_keeps_tagged(one):
    blobs = blobs_client(one)
    kept = await blobs.add_bytes(b"keep me " * 100)
    dropped = await blobs.add_bytes(b"drop me " * 100)
    await blobs.tag_set("keep", kept)
    # Remove every tag protecting `dropped` (add_bytes auto-tags it).
    for tag in await blobs.tag_list():
        if tag.hash == dropped:
            await blobs.tag_delete(tag.name)
    await blobs.gc_run_once()
    assert await blobs.blob_has(kept) is True
    assert await blobs.blob_has(dropped) is False


@pytest.mark.asyncio
async def test_download_hash_and_to_store(pair):
    a, b = pair
    src, dst = blobs_client(a), blobs_client(b)
    data = os.urandom(10_000)
    h = await src.add_bytes(data)
    # Default format is raw; NodeId and Hash typed args accepted.
    assert await dst.download_hash(Hash(h), a.id) == data

    data2 = os.urandom(5_000)
    h2 = await src.add_bytes(data2)
    assert await dst.download_hash_to_store(h2, a.node_id(), BlobFormat.RAW) is None
    assert await dst.blob_has(h2) is True
    assert await dst.read_to_bytes(h2) == data2


@pytest.mark.asyncio
async def test_download_hash_from_returns_fetch_report(pair):
    a, b = pair
    src, dst = blobs_client(a), blobs_client(b)
    data = os.urandom(20_000)
    h = await src.add_bytes(data)
    got, report = await dst.download_hash_from(h, [a.id])
    assert got == data
    assert isinstance(report, FetchReport)
    assert report.attempted() == [a.id]
    assert report.served() == [a.id]
    assert report.failed() == []
    assert report.bytes_transferred >= len(data)
    (outcome,) = report.providers
    assert isinstance(outcome, ProviderOutcome)
    assert outcome.node_id == a.id
    assert outcome.attempts >= 1 and outcome.failures == 0
    assert outcome.completed_requests >= 1
    # Per-provider bytes (Rust ProviderOutcome): the sole provider carried
    # the whole transfer; a finished fetch has nothing in flight.
    assert outcome.bytes_transferred == report.bytes_transferred
    assert outcome.in_flight == 0
    assert report.in_flight() == []
    # Both nodes serve zstd by default, so the wire bytes are reported.
    assert outcome.plain_fallbacks == 0 and report.plain_only() == []
    assert outcome.wire_bytes is not None and outcome.wire_bytes > 0

    # Already resident: nothing re-fetched.
    report2 = await dst.download_hash_to_store_from(h, [a.node_id()])
    assert report2.bytes_transferred == 0


@pytest.mark.asyncio
async def test_transfer_stats_count_both_sides(pair):
    a, b = pair
    src, dst = blobs_client(a), blobs_client(b)
    before = dst.transfer_stats()
    assert isinstance(before, BlobTransferStats)
    data = os.urandom(50_000)
    h = await src.add_bytes(data)
    await dst.download_hash_from(h, [a.id])
    fetched = dst.transfer_stats()
    assert fetched.fetched_payload_bytes - before.fetched_payload_bytes >= len(data)
    assert fetched.fetched_zstd_payload_bytes >= len(data)
    assert fetched.fetched_wire_bytes > 0
    assert isinstance(fetched.fetch_transfers, BlobTransferCounts)
    assert fetched.fetch_transfers.zstd_ok >= 1
    assert fetched.fetch_decode_errors == 0
    served = src.transfer_stats()
    assert served.served_payload_bytes >= len(data)
    assert served.serve_transfers.zstd_ok >= 1


@pytest.mark.asyncio
async def test_peer_view_traffic_and_blob_transfer():
    from aster import AsterConfig, PeerBlobStats, QuicTraffic

    cfg = AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0", enable_monitoring=True)
    a = await Node.start(cfg)
    b = await Node.start(cfg)
    try:
        for _ in range(200):
            if a.node_addr_info().direct_addresses and b.node_addr_info().direct_addresses:
                break
            await asyncio.sleep(0.01)
        a.add_node_addr(b)
        b.add_node_addr(a)
        data = os.urandom(40_000)
        h = await blobs_client(a).add_bytes(data)
        await blobs_client(b).download_hash_from(h, [a.id])

        async def until_counted():
            while True:
                view = b.topology().peer(a.id)
                if (
                    view is not None
                    and view.blob_transfer.fetched_payload_bytes >= len(data)
                    and view.quic_traffic.blobs_zstd.quic_bytes_received > 0
                ):
                    return view
                await asyncio.sleep(0.05)

        view = await asyncio.wait_for(until_counted(), 20)
        assert isinstance(view.quic_traffic, QuicTraffic)
        assert isinstance(view.blob_transfer, PeerBlobStats)
        assert view.blob_transfer.fetched_wire_bytes > 0
        total = view.quic_traffic.total()
        assert total.quic_bytes_received >= view.quic_traffic.blobs_zstd.quic_bytes_received
    finally:
        await a.shutdown()
        await b.shutdown()


@pytest.mark.asyncio
async def test_download_hash_to_store_from_and_collection_split(pair):
    a, b = pair
    src, dst = blobs_client(a), blobs_client(b)
    data = os.urandom(8_000)
    h = await src.add_bytes(data)
    report = await dst.download_hash_to_store_from(h, [a.id], BlobFormat.RAW, FetchStrategy.WHOLE)
    assert report.served() == [a.id]
    assert await dst.read_to_bytes(h) == data

    entries = [("one.bin", os.urandom(3_000)), ("two.bin", os.urandom(4_000))]
    coll = await src.add_collection(entries)
    files, creport = await dst.download_collection_from(
        coll, [a.id], FetchStrategy.SPLIT_CHILDREN
    )
    assert sorted(files) == sorted(entries)
    assert creport.served() == [a.id]


@pytest.mark.asyncio
async def test_download_hash_from_argument_errors(one):
    blobs = blobs_client(one)
    h = await blobs.add_bytes(b"x")
    with pytest.raises(ValueError):
        await blobs.download_hash_from(h, [])
    with pytest.raises(ValueError):
        await blobs.download_hash_from(
            h, [one.id], BlobFormat.RAW, FetchStrategy.SPLIT_CHILDREN
        )
    with pytest.raises(ValueError):
        await blobs.download_hash(h, one.id, "zip")
    assert FetchStrategy.WHOLE != FetchStrategy.SPLIT_CHILDREN


# -- Docs --------------------------------------------------------------------


@pytest.mark.asyncio
async def test_set_bytes_durable_entry_exists_and_content_status(one):
    docs = docs_client(one)
    doc = await docs.create()
    author = await docs.default_author()
    assert author == one.node_id()  # default author == node id

    h = await doc.set_bytes_durable(author, b"k", b"durable value")
    assert await doc.get_exact(author, b"k") == b"durable value"
    assert await doc.entry_exists(author, b"k") is True
    assert await doc.entry_exists(NodeId(author), b"missing") is False

    status = await doc.entry_content_status(Hash(h))
    assert status.status == "complete" and status.size == len(b"durable value")
    unknown = "00" * 32
    assert (await doc.entry_content_status(unknown)).status == "not_found"
    assert await doc.read_entry_contents_if_complete([h, unknown]) == [b"durable value", None]

    # A deletion tombstone still "exists".
    await doc.delete(author, b"k")
    assert await doc.get_exact(author, b"k") is None
    assert await doc.entry_exists(author, b"k") is True


@pytest.mark.asyncio
async def test_authority_report_and_fingerprint(one):
    docs = docs_client(one)
    doc = await docs.create()
    ns_hex = doc.doc_id()
    report = await doc.authority_report()
    assert isinstance(report, AuthorityReport)
    assert report.access == LocalAccess.WRITE
    assert report.namespace_hex == ns_hex
    assert report.namespace_id == bytes.fromhex(ns_hex)
    fp = report.fingerprint
    assert fp == AuthorityFingerprint.of(ns_hex, LocalAccess.WRITE)
    assert fp == AuthorityFingerprint.of(bytes.fromhex(ns_hex), LocalAccess.WRITE)
    assert fp != AuthorityFingerprint.of(ns_hex, LocalAccess.READ)
    assert len(fp.to_bytes()) == 32 and fp.to_hex() == fp.to_bytes().hex()

    by_id = await docs.authority_report(bytes.fromhex(ns_hex))
    assert by_id.access == LocalAccess.WRITE and by_id.fingerprint == fp

    # Unknown namespace: an error, never "read".
    with pytest.raises(Exception):
        await docs.authority_report(os.urandom(32))


@pytest.mark.asyncio
async def test_open_and_import_namespaces(one):
    docs = docs_client(one)
    assert await docs.open(os.urandom(32)) is None

    doc = await docs.create()
    reopened = await docs.open(doc.doc_id())
    assert reopened is not None and reopened.doc_id() == doc.doc_id()

    # A read import of a foreign namespace reports READ.
    foreign = os.urandom(32)
    ro = await docs.open_or_import_read_namespace(foreign)
    assert (await ro.authority_report()).access == LocalAccess.READ

    # A read import never narrows an existing write capability.
    again = await docs.open_or_import_read_namespace(doc.doc_id())
    assert (await again.authority_report()).access == LocalAccess.WRITE


@pytest.mark.asyncio
async def test_retire_local_write_authority(one):
    docs = docs_client(one)
    secret = os.urandom(32)
    doc = await docs.open_or_import_write_namespace(secret)
    author = await docs.default_author()
    await doc.set_bytes(author, b"kept", b"value")
    ns = bytes.fromhex(doc.doc_id())

    busy = await docs.retire_local_write_authority(ns)
    assert isinstance(busy, RetireOutcome)
    assert busy.retired is False and not busy
    assert busy.handles >= 1 and busy.access is None

    await doc.shutdown_for_role_swap(5.0)
    done = await docs.retire_local_write_authority(ns)
    assert done.retired is True and bool(done)
    assert done.access == LocalAccess.READ and done.handles is None

    reopened = await docs.open(ns)
    assert (await reopened.authority_report()).access == LocalAccess.READ
    assert await reopened.get_exact(author, b"kept") == b"value"


@pytest.mark.asyncio
async def test_share_mode_and_download_policy_types(one):
    docs = docs_client(one)
    doc = await docs.create()
    assert (await doc.share(ShareMode.READ)).startswith("doc")
    assert (await doc.share_with_addr(ShareMode.WRITE)).startswith("doc")
    with pytest.raises(ValueError):
        await doc.share("admin")

    policy = DownloadPolicy.nothing_except([b"a/", b"b/"])
    await doc.set_download_policy(policy)
    got = await doc.get_download_policy()
    assert isinstance(got, DownloadPolicy)
    assert got == policy and got.mode == "nothing_except"
    await doc.set_download_policy(DownloadPolicy.everything())
    assert (await doc.get_download_policy()) == DownloadPolicy.everything()
    # Legacy string form still works.
    await doc.set_download_policy("everything_except", [b"x"])
    assert (await doc.get_download_policy()) == DownloadPolicy.everything_except([b"x"])
    with pytest.raises(ValueError):
        DownloadPolicy("sometimes")


@pytest.mark.asyncio
async def test_doc_events_async_iterator(one):
    docs = docs_client(one)
    doc = await docs.create()
    author = await docs.default_author()
    events = doc.events()

    async def first_insert():
        async for ev in events:
            if ev.kind == "insert_local":
                return ev

    waiter = asyncio.ensure_future(first_insert())
    await doc.set_bytes(author, b"evt", b"payload")
    ev = await asyncio.wait_for(waiter, timeout=10)
    assert ev.entry.key == b"evt"
    assert ev.entry.author_id == author

    # subscribe() returns the same async-iterable receiver.
    rx = await doc.subscribe()
    await doc.set_bytes(author, b"evt2", b"payload2")
    ev2 = await asyncio.wait_for(rx.__anext__(), timeout=10)
    assert ev2.kind == "insert_local" and ev2.entry.key == b"evt2"


# -- Net ---------------------------------------------------------------------

ALPN = b"test/p6/1"


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_paths_selected_path_and_stream_priority():
    server = await create_endpoint(ALPN)
    client = await create_endpoint(ALPN)
    try:
        async def serve():
            conn = await server.accept()
            send, recv = await conn.accept_bi()
            data = await recv.read_to_end(1024)
            await send.write_all(data)
            await send.finish()
            await asyncio.sleep(1)

        task = asyncio.ensure_future(serve())
        conn = await client.connect_node_addr(server.endpoint_addr_info(), ALPN)
        send, recv = await conn.open_bi()
        assert await send.priority() == 0
        await send.set_priority(7)
        assert await send.priority() == 7
        await send.write_all(b"ping")
        await send.finish()
        assert await recv.read_to_end(1024) == b"ping"

        paths = conn.paths()
        assert paths and all(isinstance(p, PathInfo) for p in paths)
        for p in paths:
            assert p.kind in ("direct", "relay", "other")
            if p.kind == "direct":
                assert p.addr and p.relay_url is None
            if p.kind == "relay":
                assert p.relay_url and p.addr is None
        selected = conn.selected_path()
        assert selected is not None and selected.is_selected
        assert sum(1 for p in paths if p.is_selected) == 1
        await task
    finally:
        await client.close()
        await server.close()
