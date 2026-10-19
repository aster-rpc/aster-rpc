"""Plan P7 binding wiring on live nodes: fenced leases (in-memory serializer
and a designated grantor over RPC), pinned-author policy docs carrying sealed
grants, and the topology v1 peer view. Offline: loopback only, relays
disabled.

Lease and topology behaviour (renewal past the TTL, restart, takeover races,
v2 cluster convergence) is tested once in Rust: ``aster/tests/it/lease.rs``,
``rpc_lease.rs``, ``topology_shared.rs`` and ``rpc_topology.rs``.
"""

from __future__ import annotations

import asyncio
import dataclasses
import inspect
import os
import time

import pytest
import pytest_asyncio

from aster import (
    AcquireOutcome,
    AsterConfig,
    Fence,
    FenceRejectedError,
    GrantContext,
    GrantRecord,
    Lease,
    LeaseOp,
    LeaseOptions,
    LeaseSnapshot,
    MemorySerializer,
    NamespaceCapability,
    Node,
    NodeId,
    PeerView,
    PolicyDoc,
    docs_client,
    grant_key,
    net_client,
    reconcile_namespace_grant,
    seal_namespace_grant,
)

from wait_budget import WAIT_BUDGET

pytestmark = pytest.mark.network

FAST = LeaseOptions(ttl=0.6, renew_interval=0.15, holder_margin=0.1)
RPC_ALPN = b"aster/1"


def _offline(**kw) -> AsterConfig:
    return AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0", **kw)


async def _ready(node: Node) -> None:
    for _ in range(200):
        if node.node_addr_info().direct_addresses:
            return
        await asyncio.sleep(0.01)


async def _until(what: str, f, timeout: float = 20.0):
    deadline = time.monotonic() + timeout
    while True:
        v = f()
        if inspect.isawaitable(v):
            v = await v
        if v is not None:
            return v
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out waiting for {what}")
        await asyncio.sleep(0.2)


def _granted(outcome: AcquireOutcome) -> Lease:
    assert isinstance(outcome, AcquireOutcome)
    assert outcome.granted, f"expected a grant, got {outcome.reason}"
    assert outcome.lease is not None
    return outcome.lease


# -- lease: in-memory serializer ---------------------------------------------


async def test_acquire_deny_fenced_mutation_release_reacquire():
    ser = MemorySerializer()
    lease = _granted(await ser.acquire("tree/42", "n1", FAST))
    assert (lease.resource, lease.epoch, lease.holder, lease.status) == ("tree/42", 1, "n1", "held")
    fence = lease.fence()
    assert fence == Fence("tree/42", 1, "n1")

    denied = await ser.acquire("tree/42", "n2", FAST)
    assert not denied.granted and denied.lease is None
    assert denied.reason == "held_by_other"
    assert isinstance(denied.snapshot, LeaseSnapshot)
    assert (denied.snapshot.epoch, denied.snapshot.holder, denied.snapshot.held) == (1, "n1", True)
    assert denied.snapshot.remaining > 0

    assert ser.mutate_fenced(fence, "n1", lambda: "written") == "written"
    # The fence is not a bearer token: another writer is refused.
    with pytest.raises(FenceRejectedError) as e:
        ser.mutate_fenced(fence, "n2", lambda: "nope")
    assert e.value.kind == "not_holder"

    await lease.release()
    assert lease.status == "released"
    assert lease.fence() is None
    await lease.release()  # idempotent

    ran = []
    with pytest.raises(FenceRejectedError) as e:
        ser.mutate_fenced(fence, "n1", lambda: ran.append(1))
    assert (e.value.kind, e.value.carried, e.value.current) == ("stale_epoch", 1, 2)
    assert ran == []

    nxt = _granted(await ser.acquire("tree/42", "n2", FAST))
    assert nxt.epoch == 3
    await nxt.release()
    assert await ser.snapshot("tree/42") == LeaseSnapshot("tree/42", 4, None, False, None)


async def test_lease_handle_api_maps_to_native():
    """The rest of the handle surface, once each: a Python exception raised
    inside ``mutate_fenced`` propagates unchanged; ``async with`` releases;
    ``stop_renewing`` hands back the fence and leaves the row held; a raw
    transition maps its outcome; invalid options are ValueError. The
    behaviour is aster/tests/it/lease.rs (stop_renewing_returns_the_fence_
    without_releasing, restart_rearms_ttl_without_advancing_fence, ...)."""
    ser = MemorySerializer()
    lease = _granted(await ser.acquire("r", "n1", FAST))

    def boom():
        raise KeyError("inside")

    with pytest.raises(KeyError):
        ser.mutate_fenced(lease.fence(), "n1", boom)
    await lease.release()

    async with _granted(await ser.acquire("scoped", "n1", FAST)) as scoped:
        assert scoped.fence() is not None
    assert (await ser.snapshot("scoped")).held is False

    lease = _granted(await ser.acquire("final", "n1", FAST))
    fence = lease.stop_renewing()
    assert fence == Fence("final", 1, "n1")
    assert lease.status == "released"
    assert lease.stop_renewing() is None
    assert (await ser.snapshot("final")).held is True

    out = await ser.transition("raw", LeaseOp.acquire("n1", 5.0))
    assert out.kind == "granted" and out.fence == Fence("raw", 1, "n1")
    denied = await ser.transition("raw", LeaseOp.acquire("n2", 5.0))
    assert denied.kind == "denied" and denied.deny_reason == "held_by_other"

    with pytest.raises(ValueError, match="renew_interval"):
        await ser.acquire("r", "n1", LeaseOptions(ttl=1.0, renew_interval=0.95, holder_margin=0.1))
    with pytest.raises(ValueError, match="ttl"):
        await ser.acquire("r", "n1", LeaseOptions(ttl=-1))
    with pytest.raises(dataclasses.FrozenInstanceError):
        FAST.ttl = 2  # type: ignore[misc]


async def test_revoke_fences_holder_and_status_changes_ends():
    ser = MemorySerializer()
    lease = _granted(await ser.acquire("r", "n1", FAST))
    seen: list[str] = []

    async def watch():
        async for s in lease.status_changes():
            seen.append(s)

    watching = asyncio.ensure_future(watch())
    await asyncio.sleep(0)
    assert (await ser.revoke("r", "operator")).kind == "revoked"
    await asyncio.wait_for(watching, WAIT_BUDGET)
    assert seen == ["fenced"]
    assert lease.status == "fenced"
    assert lease.fence() is None
    # Already terminal: a new iterator yields the status and ends.
    assert [s async for s in lease.status_changes()] == ["fenced"]


# -- lease: designated grantor over RPC --------------------------------------


@pytest_asyncio.fixture
async def grantor_trio():
    grantor = await Node.start(_offline(), alpns=[RPC_ALPN])
    a, b = await Node.start(_offline()), await Node.start(_offline())
    for n in (grantor, a, b):
        await _ready(n)
    a.add_node_addr(grantor)
    b.add_node_addr(grantor)
    store = MemorySerializer()
    server = grantor.serve_lease_grantor(store)
    yield grantor, a, b, store
    server.close()
    server.close()  # idempotent
    for n in (grantor, a, b):
        await n.shutdown()


async def test_grantor_binds_grants_to_authenticated_caller(grantor_trio):
    grantor, a, b, store = grantor_trio
    sa = await asyncio.wait_for(a.lease_grantor(grantor.id), 10)
    sb = await asyncio.wait_for(b.lease_grantor(str(grantor.id)), 10)
    assert sa.self_id == a.id and isinstance(sa.self_id, NodeId)

    outcome = await sa.acquire("db/primary", FAST)
    lease = _granted(outcome)
    assert lease.holder == a.id
    assert lease.fence() == Fence("db/primary", 1, str(a.id))

    # The grantor-side store is the co-located enforcement point.
    assert store.mutate_fenced(lease.fence(), a.id, lambda: "ok") == "ok"

    denied = await sb.acquire("db/primary", FAST)
    assert not denied.granted
    assert denied.reason == "held_by_other"
    assert denied.snapshot.holder == a.id

    # Renewal over RPC past the TTL is aster/tests/it/rpc_lease.rs
    # lease_handle_renews_over_rpc; here only the snapshot mapping.
    snap = await sb.snapshot("db/primary")
    assert (snap.epoch, snap.holder, snap.held) == (1, a.id, True)

    # revoke is operator-gated by default.
    with pytest.raises(Exception):
        await sa.revoke("db/primary")

    await lease.release()
    nxt = _granted(await sb.acquire("db/primary", FAST))
    assert nxt.epoch == 3
    await nxt.release()


# -- policy doc + sealed grants ----------------------------------------------


async def test_policy_doc_pins_root_author_and_delivers_grant_across_nodes():
    root, member = await Node.start(_offline()), await Node.start(_offline())
    try:
        await _ready(root)
        await _ready(member)
        root.add_node_addr(member)
        member.add_node_addr(root)

        root_docs = docs_client(root)
        root_author = await root_docs.default_author()
        forger = await root_docs.create_author()
        doc = await root_docs.create()
        ns_id = doc.doc_id()

        topo_secret = os.urandom(32)
        ctx = GrantContext(
            app="py/topo-grant/v1",
            granter=root.id,
            recipient=member.id,
            resource=bytes.fromhex(ns_id),
            path=grant_key("/py/v1", member.id),
            role="write",
        )
        record = GrantRecord.active(
            "write", 1, seal_namespace_grant(ctx, NamespaceCapability.write(topo_secret))
        )
        await doc.set_bytes(root_author, ctx.path.encode(), record.encode())
        await doc.set_bytes(forger, b"/py/v1/grants/forged", b"x")

        # Locally: only the pinned author is visible.
        local = PolicyDoc.wrap(doc, root_author)
        assert local.author == root_author
        entries = await local.list_prefix("/py/v1/grants/")
        assert [bytes(e.key).decode() for e in entries] == [ctx.path]
        assert await local.get("/py/v1/grants/forged") is None
        assert await PolicyDoc.wrap(doc, forger).get(b"/py/v1/grants/forged") == b"x"

        # Remotely: import read-only, sync from the root, reconcile. (The
        # root only answers sync for a doc it is live-syncing.)
        await doc.start_sync([])
        policy = await PolicyDoc.import_read_only(docs_client(member), ns_id, root_author)
        await policy.doc().start_sync([root.id])
        fetched = await _until(
            "grant record to sync", lambda: policy.fetch_grant("/py/v1", member.id)
        )
        outcome = reconcile_namespace_grant(member.secret_key(), ctx, fetched)
        assert outcome.kind == "active"
        assert outcome.value.can_write
        assert outcome.value.namespace_secret() == topo_secret
        # The root's own identity cannot open the member's grant.
        assert reconcile_namespace_grant(root.secret_key(), ctx, fetched).kind == "unopenable"
    finally:
        await root.shutdown()
        await member.shutdown()


async def test_policy_doc_is_policy_write_filters_events():
    node = await Node.start(_offline())
    try:
        docs = docs_client(node)
        author = await docs.default_author()
        other = await docs.create_author()
        doc = await docs.create()
        policy = PolicyDoc.wrap(doc, author)
        events = await policy.subscribe()
        await doc.set_bytes(other, b"k/other", b"1")
        await doc.set_bytes(author, b"k/root", b"2")
        kept = []
        async for ev in events:
            if ev.kind == "insert_local":
                kept.append((policy.is_policy_write(ev), bytes(ev.entry.key)))
            if len(kept) == 2:
                break
        assert kept == [(False, b"k/other"), (True, b"k/root")]
    finally:
        await node.shutdown()


# -- topology -----------------------------------------------------------------


@pytest_asyncio.fixture
async def monitored_trio():
    nodes = [
        await Node.start(_offline(enable_monitoring=True), alpns=[b"p7/probe"]) for _ in range(3)
    ]
    for n in nodes:
        await _ready(n)
    for x in nodes:
        for y in nodes:
            if x is not y:
                x.add_node_addr(y)
    yield nodes
    for n in nodes:
        await n.shutdown()


async def test_topology_v1_sees_connected_peer_on_same_host(monitored_trio):
    a, b, _ = monitored_trio
    assert a.topology().has_monitoring is True
    accepted = asyncio.ensure_future(b.accept_aster())
    held = await asyncio.wait_for(net_client(a).connect(b.node_id(), b"p7/probe"), 10)
    await asyncio.wait_for(accepted, 10)
    view = await _until("peer view", lambda: a.topology().peer(b.id))
    assert isinstance(view, PeerView)
    assert view.node_id == b.id
    assert view.level == "same_host" and view.level_rank == 0
    assert view.direct is True
    assert view.last_measured.tzinfo is not None
    assert any(p.node_id == b.id for p in a.topology().peers())
    assert a.topology().peer("00" * 32) is None
    del held
