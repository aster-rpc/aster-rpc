"""
Tests for Phase 10: Service Registry & Discovery.

Covers:
- Publish contract + advertise endpoint (node A) → resolve + connect (node B)
- Registry doc sync uses NothingExcept download policy
- lease_seq monotonicity: stale writes rejected
- Lease expiry: consumer evicts without ENDPOINT_DOWN gossip
- Graceful withdraw: DRAINING → grace → delete → ENDPOINT_DOWN
- Consumer skips STARTING + DRAINING, prefers READY > DEGRADED
- All 6 gossip event types round-trip (encoding + 2-node wire)
- Endpoint selection: mandatory filters applied before strategy ranking
- ACL post-read filter: untrusted-author entries excluded
"""

from __future__ import annotations

import asyncio
import json
import time
from unittest.mock import AsyncMock, MagicMock

import pytest

from aster_python.aster.registry import (
    DEGRADED,
    DRAINING,
    READY,
    STARTING,
    ArtifactRef,
    EndpointLease,
    GossipEvent,
    GossipEventType,
    RegistryACL,
    RegistryClient,
    RegistryGossip,
    RegistryPublisher,
)
from aster_python.aster.registry.keys import (
    lease_key,
    lease_prefix,
    version_key,
    acl_key,
    REGISTRY_PREFIXES,
)

# ── Helpers ────────────────────────────────────────────────────────────────────


def _make_lease(
    *,
    service: str = "TestService",
    contract_id: str = "abc" * 21 + "d",  # 64 chars
    endpoint_id: str = "ep1",
    health_status: str = READY,
    lease_seq: int = 1,
    updated_at_epoch_ms: int | None = None,
    load: float | None = None,
    alpn: str = "aster/1",
    serialization_modes: list[str] | None = None,
    policy_realm: str | None = None,
) -> EndpointLease:
    now_ms = updated_at_epoch_ms if updated_at_epoch_ms is not None else int(time.time() * 1000)
    return EndpointLease(
        endpoint_id=endpoint_id,
        contract_id=contract_id,
        service=service,
        version=1,
        lease_expires_epoch_ms=now_ms + 45_000,
        lease_seq=lease_seq,
        alpn=alpn,
        serialization_modes=serialization_modes or ["fory-xlang"],
        feature_flags=[],
        relay_url=None,
        direct_addrs=[],
        load=load,
        language_runtime="python/3.13",
        aster_version="0.2.0",
        policy_realm=policy_realm,
        health_status=health_status,
        tags=[],
        updated_at_epoch_ms=now_ms,
    )


# ── GossipEvent encoding round-trips (all 6 types) ────────────────────────────


def test_gossip_event_contract_published_roundtrip():
    event = GossipEvent(
        type=GossipEventType.CONTRACT_PUBLISHED,
        contract_id="abc123",
        service="Svc",
        version=2,
        timestamp_ms=1000,
    )
    restored = GossipEvent.from_json(event.to_json())
    assert restored.type == GossipEventType.CONTRACT_PUBLISHED
    assert restored.contract_id == "abc123"
    assert restored.service == "Svc"
    assert restored.version == 2


def test_gossip_event_channel_updated_roundtrip():
    event = GossipEvent(
        type=GossipEventType.CHANNEL_UPDATED,
        service="Svc",
        channel="stable",
        contract_id="cid",
        timestamp_ms=2000,
    )
    restored = GossipEvent.from_json(event.to_json())
    assert restored.type == GossipEventType.CHANNEL_UPDATED
    assert restored.channel == "stable"
    assert restored.contract_id == "cid"


def test_gossip_event_endpoint_lease_upserted_roundtrip():
    event = GossipEvent(
        type=GossipEventType.ENDPOINT_LEASE_UPSERTED,
        endpoint_id="ep1",
        service="Svc",
        version=5,   # lease_seq
        contract_id="cid",
        timestamp_ms=3000,
    )
    restored = GossipEvent.from_json(event.to_json())
    assert restored.type == GossipEventType.ENDPOINT_LEASE_UPSERTED
    assert restored.endpoint_id == "ep1"
    assert restored.version == 5


def test_gossip_event_endpoint_down_roundtrip():
    event = GossipEvent(
        type=GossipEventType.ENDPOINT_DOWN,
        endpoint_id="ep2",
        service="Svc",
        timestamp_ms=4000,
    )
    restored = GossipEvent.from_json(event.to_json())
    assert restored.type == GossipEventType.ENDPOINT_DOWN
    assert restored.endpoint_id == "ep2"
    assert restored.service == "Svc"


def test_gossip_event_acl_changed_roundtrip():
    event = GossipEvent(
        type=GossipEventType.ACL_CHANGED,
        key_prefix="_aster/acl/",
        timestamp_ms=5000,
    )
    restored = GossipEvent.from_json(event.to_json())
    assert restored.type == GossipEventType.ACL_CHANGED
    assert restored.key_prefix == "_aster/acl/"


def test_gossip_event_compatibility_published_roundtrip():
    event = GossipEvent(
        type=GossipEventType.COMPATIBILITY_PUBLISHED,
        contract_id="src_cid",
        endpoint_id="dst_cid",
        timestamp_ms=6000,
    )
    restored = GossipEvent.from_json(event.to_json())
    assert restored.type == GossipEventType.COMPATIBILITY_PUBLISHED
    assert restored.contract_id == "src_cid"
    assert restored.endpoint_id == "dst_cid"


# ── EndpointLease JSON round-trip ─────────────────────────────────────────────


def test_endpoint_lease_json_roundtrip():
    lease = _make_lease(endpoint_id="ep_x", health_status=READY, lease_seq=7)
    restored = EndpointLease.from_json(lease.to_json())
    assert restored.endpoint_id == "ep_x"
    assert restored.health_status == READY
    assert restored.lease_seq == 7


def test_artifact_ref_json_roundtrip():
    ref = ArtifactRef(
        contract_id="c" * 64,
        collection_hash="d" * 64,
        published_by="author1",
        published_at_epoch_ms=99_000,
    )
    restored = ArtifactRef.from_json(ref.to_json())
    assert restored.contract_id == "c" * 64
    assert restored.collection_hash == "d" * 64
    assert restored.published_by == "author1"


# ── Lease freshness ────────────────────────────────────────────────────────────


def test_lease_is_fresh_when_recent():
    lease = _make_lease()  # updated_at = now
    assert lease.is_fresh(45)


def test_lease_is_stale_when_old():
    old_ms = int(time.time() * 1000) - 60_000  # 60 s ago
    lease = _make_lease(updated_at_epoch_ms=old_ms)
    assert not lease.is_fresh(45)


def test_lease_is_routable_ready():
    assert _make_lease(health_status=READY).is_routable()


def test_lease_is_routable_degraded():
    assert _make_lease(health_status=DEGRADED).is_routable()


def test_lease_not_routable_starting():
    assert not _make_lease(health_status=STARTING).is_routable()


def test_lease_not_routable_draining():
    assert not _make_lease(health_status=DRAINING).is_routable()


# ── RegistryClient: mandatory filters ─────────────────────────────────────────


def _make_client(**kwargs) -> RegistryClient:
    doc = MagicMock()
    return RegistryClient(doc, **kwargs)


def test_filter_skips_starting():
    client = _make_client()
    leases = [_make_lease(health_status=STARTING)]
    filtered = client._apply_mandatory_filters(leases)
    assert filtered == []


def test_filter_skips_draining():
    client = _make_client()
    leases = [_make_lease(health_status=DRAINING)]
    filtered = client._apply_mandatory_filters(leases)
    assert filtered == []


def test_filter_skips_stale_lease():
    client = _make_client(lease_duration_s=10)
    old_ms = int(time.time() * 1000) - 30_000  # 30 s ago — stale for 10 s lease
    leases = [_make_lease(updated_at_epoch_ms=old_ms)]
    filtered = client._apply_mandatory_filters(leases)
    assert filtered == []


def test_filter_skips_alpn_mismatch():
    client = _make_client(caller_alpn="aster/2")
    leases = [_make_lease(alpn="aster/1")]  # endpoint says aster/1, caller wants aster/2
    filtered = client._apply_mandatory_filters(leases)
    assert filtered == []


def test_filter_skips_no_shared_serialization_mode():
    client = _make_client(caller_serialization_modes=["proto"])
    leases = [_make_lease(serialization_modes=["fory-xlang"])]
    filtered = client._apply_mandatory_filters(leases)
    assert filtered == []


def test_filter_skips_policy_realm_mismatch():
    client = _make_client(caller_policy_realm="corp")
    leases = [_make_lease(policy_realm="public")]
    filtered = client._apply_mandatory_filters(leases)
    assert filtered == []


def test_filter_passes_compatible_lease():
    client = _make_client(
        caller_alpn="aster/1",
        caller_serialization_modes=["fory-xlang"],
        caller_policy_realm=None,
    )
    leases = [_make_lease(health_status=READY)]
    filtered = client._apply_mandatory_filters(leases)
    assert len(filtered) == 1


def test_filter_passes_degraded_as_fallback():
    client = _make_client()
    leases = [_make_lease(health_status=DEGRADED)]
    filtered = client._apply_mandatory_filters(leases)
    assert len(filtered) == 1


# ── RegistryClient: endpoint selection strategy ───────────────────────────────


def test_strategy_prefers_ready_over_degraded():
    client = _make_client()
    leases = [
        _make_lease(endpoint_id="ep_d", health_status=DEGRADED),
        _make_lease(endpoint_id="ep_r", health_status=READY),
    ]
    ranked = client._rank(leases, "round_robin", "cid")
    assert ranked[0].endpoint_id == "ep_r"


def test_strategy_round_robin_cycles():
    client = _make_client()
    leases = [
        _make_lease(endpoint_id="ep1"),
        _make_lease(endpoint_id="ep2"),
        _make_lease(endpoint_id="ep3"),
    ]
    cid = leases[0].contract_id
    first = client._rank(list(leases), "round_robin", cid)[0].endpoint_id
    second = client._rank(list(leases), "round_robin", cid)[0].endpoint_id
    third = client._rank(list(leases), "round_robin", cid)[0].endpoint_id
    # All three should appear across three calls
    assert {first, second, third} == {"ep1", "ep2", "ep3"}


def test_strategy_least_load_picks_minimum():
    client = _make_client()
    leases = [
        _make_lease(endpoint_id="ep_heavy", load=0.9),
        _make_lease(endpoint_id="ep_light", load=0.1),
        _make_lease(endpoint_id="ep_mid", load=0.5),
    ]
    ranked = client._rank(leases, "least_load", "cid")
    assert ranked[0].endpoint_id == "ep_light"


def test_strategy_random_returns_all_candidates():
    client = _make_client()
    leases = [_make_lease(endpoint_id=f"ep{i}") for i in range(5)]
    ranked = client._rank(list(leases), "random", "cid")
    assert {lz.endpoint_id for lz in ranked} == {f"ep{i}" for i in range(5)}


# ── RegistryClient: lease_seq monotonicity ────────────────────────────────────


def test_lease_seq_monotonicity_rejects_stale():
    client = _make_client()
    new_lease = _make_lease(lease_seq=5)
    old_lease = _make_lease(lease_seq=3)

    # Accept new
    assert client._check_seq(new_lease) is True
    # Reject anything ≤ 5
    assert client._check_seq(old_lease) is False
    same_lease = _make_lease(lease_seq=5)
    assert client._check_seq(same_lease) is False


def test_lease_seq_monotonicity_accepts_incremented():
    client = _make_client()
    lease_v1 = _make_lease(lease_seq=1)
    lease_v2 = _make_lease(lease_seq=2)
    lease_v3 = _make_lease(lease_seq=3)

    assert client._check_seq(lease_v1) is True
    assert client._check_seq(lease_v2) is True
    assert client._check_seq(lease_v3) is True


def test_lease_seq_independent_per_endpoint():
    """Different (service, contract_id, endpoint_id) tuples have independent caches."""
    client = _make_client()
    lease_a = _make_lease(endpoint_id="ep_a", lease_seq=10)
    lease_b = _make_lease(endpoint_id="ep_b", lease_seq=1)  # different endpoint

    assert client._check_seq(lease_a) is True
    assert client._check_seq(lease_b) is True  # ep_b seq starts fresh


# ── RegistryClient: resolve_all with mock doc ─────────────────────────────────


@pytest.mark.asyncio
async def test_resolve_all_returns_empty_when_no_contract():
    doc = AsyncMock()
    doc.query_key_exact = AsyncMock(return_value=[])
    doc.set_download_policy = AsyncMock()
    client = RegistryClient(doc)
    result = await client.resolve_all("UnknownService", version=1)
    assert result == []


@pytest.mark.asyncio
async def test_resolve_all_filters_stale_leases():
    """Leases older than lease_duration_s are excluded even without ENDPOINT_DOWN."""
    doc = AsyncMock()
    doc.set_download_policy = AsyncMock()

    contract_id = "c" * 64
    old_ms = int(time.time() * 1000) - 60_000  # 60 s ago

    stale_lease = _make_lease(
        contract_id=contract_id, updated_at_epoch_ms=old_ms
    )

    # Simulate: version_key returns contract_id; lease_prefix returns stale lease
    async def mock_query_key_exact(key):
        if key == version_key("TestService", 1):
            entry = MagicMock()
            entry.author_id = "author1"
            entry.timestamp = 1000
            entry.content_hash = "hash1"
            return [entry]
        return []

    async def mock_query_key_prefix(prefix):
        if prefix == lease_prefix("TestService", contract_id):
            entry = MagicMock()
            entry.author_id = "author1"
            entry.content_hash = "hash2"
            return [entry]
        return []

    async def mock_read_entry_content(content_hash):
        if content_hash == "hash1":
            return contract_id.encode()
        if content_hash == "hash2":
            return stale_lease.to_json().encode()
        return b""

    doc.query_key_exact = mock_query_key_exact
    doc.query_key_prefix = mock_query_key_prefix
    doc.read_entry_content = mock_read_entry_content

    client = RegistryClient(doc, lease_duration_s=45)
    result = await client.resolve_all("TestService", version=1)
    # Stale lease should be evicted
    assert result == []


@pytest.mark.asyncio
async def test_resolve_skips_starting_and_draining():
    """STARTING and DRAINING endpoints are excluded from resolve results."""
    doc = AsyncMock()
    doc.set_download_policy = AsyncMock()

    contract_id = "d" * 64

    starting_lease = _make_lease(
        contract_id=contract_id, endpoint_id="ep_starting", health_status=STARTING
    )
    draining_lease = _make_lease(
        contract_id=contract_id, endpoint_id="ep_draining", health_status=DRAINING
    )
    ready_lease = _make_lease(
        contract_id=contract_id, endpoint_id="ep_ready", health_status=READY
    )

    async def mock_query_key_exact(key):
        if key == version_key("TestService", 1):
            entry = MagicMock()
            entry.author_id = "author1"
            entry.timestamp = 1000
            entry.content_hash = "ver_hash"
            return [entry]
        return []

    async def mock_query_key_prefix(prefix):
        entries = []
        for i, lease in enumerate([starting_lease, draining_lease, ready_lease]):
            entry = MagicMock()
            entry.author_id = "author1"
            entry.content_hash = f"lease_hash_{i}"
            entries.append(entry)
        return entries

    leases = [starting_lease, draining_lease, ready_lease]

    async def mock_read_entry_content(content_hash):
        if content_hash == "ver_hash":
            return contract_id.encode()
        for i, lease in enumerate(leases):
            if content_hash == f"lease_hash_{i}":
                return lease.to_json().encode()
        return b""

    doc.query_key_exact = mock_query_key_exact
    doc.query_key_prefix = mock_query_key_prefix
    doc.read_entry_content = mock_read_entry_content

    client = RegistryClient(doc)
    results = await client.resolve_all("TestService", version=1)
    endpoint_ids = {r.endpoint_id for r in results}
    assert "ep_ready" in endpoint_ids
    assert "ep_starting" not in endpoint_ids
    assert "ep_draining" not in endpoint_ids


@pytest.mark.asyncio
async def test_resolve_raises_lookup_error_when_empty():
    doc = AsyncMock()
    doc.query_key_exact = AsyncMock(return_value=[])
    doc.set_download_policy = AsyncMock()
    client = RegistryClient(doc)
    with pytest.raises(LookupError, match="TestService"):
        await client.resolve("TestService", version=1)


# ── RegistryACL: post-read filter ─────────────────────────────────────────────


def test_acl_open_mode_trusts_all():
    doc = MagicMock()
    acl = RegistryACL(doc, "admin_author")
    # In open mode, any author is trusted
    assert acl.is_trusted_writer("unknown_author") is True
    assert acl.is_trusted_writer("") is True


def test_acl_restricted_mode_filters_untrusted():
    doc = MagicMock()
    acl = RegistryACL(doc, "admin_author")
    # Manually switch to restricted mode (simulates add_writer being called)
    acl._writers = {"trusted_author"}
    acl._open = False
    assert acl.is_trusted_writer("trusted_author") is True
    assert acl.is_trusted_writer("untrusted_author") is False


def test_acl_filter_trusted_excludes_unknown():
    doc = MagicMock()
    acl = RegistryACL(doc, "admin")
    acl._writers = {"alice"}
    acl._open = False

    # Build mock DocEntry-like objects
    entry_alice = MagicMock()
    entry_alice.author_id = "alice"
    entry_bob = MagicMock()
    entry_bob.author_id = "bob"

    filtered = acl.filter_trusted([entry_alice, entry_bob])
    assert len(filtered) == 1
    assert filtered[0].author_id == "alice"


@pytest.mark.asyncio
async def test_acl_reload_switches_to_restricted():
    doc = AsyncMock()
    acl = RegistryACL(doc, "admin")

    # Simulate ACL writers entry in doc
    writers_entry = MagicMock()
    writers_entry.author_id = "admin"
    writers_entry.content_hash = "acl_hash"

    async def mock_query_key_exact(key):
        if key == acl_key("writers"):
            return [writers_entry]
        return []

    async def mock_read_entry_content(content_hash):
        if content_hash == "acl_hash":
            return json.dumps(["alice", "bob"]).encode()
        return b""

    doc.query_key_exact = mock_query_key_exact
    doc.read_entry_content = mock_read_entry_content

    await acl.reload()
    assert not acl._open
    assert "alice" in acl._writers
    assert "bob" in acl._writers


# ── RegistryPublisher: health state machine ───────────────────────────────────


@pytest.mark.asyncio
async def test_publisher_register_sets_starting_health():
    doc = AsyncMock()
    publisher = RegistryPublisher(doc, "author1")
    await publisher.register_endpoint(
        "cid123",
        "MyService",
        version=1,
        endpoint_id="ep1",
    )
    assert publisher._lease is not None
    assert publisher._lease.health_status == STARTING
    assert publisher._lease.lease_seq == 1
    # Cleanup
    await publisher.close()


@pytest.mark.asyncio
async def test_publisher_set_health_bumps_seq():
    doc = AsyncMock()
    publisher = RegistryPublisher(doc, "author1")
    await publisher.register_endpoint("cid", "Svc", version=1, endpoint_id="ep1")
    initial_seq = publisher._lease.lease_seq
    await publisher.set_health(READY)
    assert publisher._lease.health_status == READY
    assert publisher._lease.lease_seq == initial_seq + 1
    await publisher.close()


@pytest.mark.asyncio
async def test_publisher_set_health_requires_active_lease():
    doc = AsyncMock()
    publisher = RegistryPublisher(doc, "author1")
    with pytest.raises(RuntimeError, match="No active lease"):
        await publisher.set_health(READY)


@pytest.mark.asyncio
async def test_publisher_withdraw_state_machine():
    """Withdraw transitions: STARTING → DRAINING → deleted → ENDPOINT_DOWN gossip."""
    doc = AsyncMock()
    gossip = MagicMock()
    gossip.broadcast_endpoint_down = AsyncMock()
    gossip.broadcast_endpoint_lease_upserted = AsyncMock()

    publisher = RegistryPublisher(doc, "author1", gossip=gossip)
    await publisher.register_endpoint("cid", "Svc", version=1, endpoint_id="ep1")

    await publisher.withdraw(grace_period_s=0)

    # After withdraw, lease should be None
    assert publisher._lease is None
    # DRAINING was set (set_health was called with DRAINING before delete)
    # delete = tombstone bytes written
    call_args_list = doc.set_bytes.call_args_list
    payloads = [c[0][2] for c in call_args_list]  # (author, key, value)
    assert b"null" in payloads  # tombstone = delete

    # ENDPOINT_DOWN gossip should have been broadcast
    gossip.broadcast_endpoint_down.assert_called_once_with("ep1", "Svc")


@pytest.mark.asyncio
async def test_publisher_withdraw_transitions_through_draining():
    """Verify DRAINING health appears in the doc before the empty-bytes delete."""
    doc = AsyncMock()
    calls = []

    async def capture_set_bytes(author, key, value):
        if value and value != b"null":
            try:
                lease = EndpointLease.from_json(value)
                calls.append(("lease", lease.health_status, lease.lease_seq))
            except Exception:
                calls.append(("raw", value))
        else:
            calls.append(("delete", key))

    doc.set_bytes = capture_set_bytes

    publisher = RegistryPublisher(doc, "author1")
    await publisher.register_endpoint("cid", "Svc", version=1, endpoint_id="ep1")
    await publisher.withdraw(grace_period_s=0)

    # Should see STARTING, then DRAINING, then delete
    health_states = [c[1] for c in calls if c[0] == "lease"]
    assert STARTING in health_states
    assert DRAINING in health_states
    # Delete (empty bytes write) should come after DRAINING
    draining_idx = next(i for i, c in enumerate(calls) if c[0] == "lease" and c[1] == DRAINING)
    delete_idx = next(i for i, c in enumerate(calls) if c[0] == "delete")
    assert delete_idx > draining_idx


# ── Registry doc NothingExcept policy ─────────────────────────────────────────


@pytest.mark.asyncio
async def test_registry_client_applies_nothing_except_policy():
    """RegistryClient applies NothingExcept download policy on first use."""
    doc = AsyncMock()
    doc.query_key_exact = AsyncMock(return_value=[])
    doc.set_download_policy = AsyncMock()
    client = RegistryClient(doc)
    # Trigger policy application
    await client.resolve_all("Svc", version=1)
    doc.set_download_policy.assert_called_once_with("nothing_except", REGISTRY_PREFIXES)


# ── End-to-end: two-node registry publish + resolve ───────────────────────────


@pytest.mark.asyncio
async def test_publish_and_resolve_single_node():
    """Publish contract + endpoint on one node, resolve from same doc handle."""
    from aster_python import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()
    author = await dc.create_author()

    import blake3
    contract_bytes = b"fake_canonical_xlang_bytes_for_test"
    contract_id = blake3.blake3(contract_bytes).hexdigest()

    # Publisher
    publisher = RegistryPublisher(doc, author)
    returned_cid = await publisher.publish_contract(
        contract_bytes, "TestService", version=1
    )
    assert returned_cid == contract_id

    await publisher.register_endpoint(
        contract_id,
        "TestService",
        version=1,
        endpoint_id=node.node_id(),
        health_status=READY,
    )
    await publisher.close()

    # Client (same doc)
    client = RegistryClient(doc, caller_alpn="aster/1")
    lease = await client.resolve("TestService", version=1)
    assert lease.endpoint_id == node.node_id()
    assert lease.contract_id == contract_id
    assert lease.health_status == READY

    await node.shutdown()


@pytest.mark.asyncio
async def test_publish_and_resolve_cross_node():
    """Publish + advertise on node A; resolve from node B (real doc sync)."""
    from aster_python import IrohNode, docs_client

    node_a = await IrohNode.memory()
    node_b = await IrohNode.memory()

    node_a.add_node_addr(node_b)
    node_b.add_node_addr(node_a)

    dc_a = docs_client(node_a)
    dc_b = docs_client(node_b)

    doc_a = await dc_a.create()
    author_a = await dc_a.create_author()

    import blake3
    contract_bytes = b"cross_node_contract_bytes"
    contract_id = blake3.blake3(contract_bytes).hexdigest()

    # Node A publishes
    publisher = RegistryPublisher(doc_a, author_a)
    await publisher.publish_contract(contract_bytes, "CrossSvc", version=1)
    await publisher.register_endpoint(
        contract_id,
        "CrossSvc",
        version=1,
        endpoint_id=node_a.node_id(),
        health_status=READY,
    )
    await publisher.close()

    # Node B joins the doc with subscribe to know when sync + content download complete
    ticket = await doc_a.share("write")
    doc_b, receiver_b = await dc_b.join_and_subscribe(ticket)

    # Wait for content_ready events: both version_key and lease_key need blobs
    # content_ready fires after the blob is downloaded (after insert_remote).
    content_ready_count = 0
    for _ in range(50):
        event = await asyncio.wait_for(receiver_b.recv(), timeout=10.0)
        if event is not None and event.kind == "content_ready":
            content_ready_count += 1
            # We published 3 keys: contract_key, version_key, lease_key
            if content_ready_count >= 3:
                break

    # Node B resolves — content is now locally available
    client = RegistryClient(doc_b, caller_alpn="aster/1")
    lease = await client.resolve("CrossSvc", version=1)
    assert lease.endpoint_id == node_a.node_id()
    assert lease.contract_id == contract_id

    await node_a.shutdown()
    await node_b.shutdown()


# ── Graceful withdraw cross-node ──────────────────────────────────────────────


@pytest.mark.asyncio
async def test_withdraw_endpoint_down_gossip():
    """Graceful withdraw emits ENDPOINT_DOWN gossip; endpoint no longer resolved."""
    from aster_python import IrohNode, docs_client, gossip_client

    node_a = await IrohNode.memory()
    node_b = await IrohNode.memory()

    node_a.add_node_addr(node_b)
    node_b.add_node_addr(node_a)

    dc_a = docs_client(node_a)
    gc_a = gossip_client(node_a)
    gc_b = gossip_client(node_b)

    doc_a = await dc_a.create()
    author_a = await dc_a.create_author()

    # Agree on a gossip topic (deterministic from contract_id prefix)
    topic = bytes(32)  # all zeros for test

    # Both nodes subscribe to gossip topic concurrently
    node_a_id = node_a.node_id()
    node_b_id = node_b.node_id()
    handle_a, handle_b = await asyncio.wait_for(
        asyncio.gather(
            gc_a.subscribe(topic, [node_b_id]),
            gc_b.subscribe(topic, [node_a_id]),
        ),
        timeout=30,
    )
    rg_a = RegistryGossip(handle_a)
    rg_b = RegistryGossip(handle_b)

    import blake3
    contract_bytes = b"withdraw_test_bytes"
    contract_id = blake3.blake3(contract_bytes).hexdigest()

    publisher = RegistryPublisher(doc_a, author_a, gossip=rg_a)
    await publisher.register_endpoint(
        contract_id, "WithdrawSvc", version=1,
        endpoint_id=node_a_id, health_status=READY,
    )

    # Start collecting gossip events on node B
    received_events: list[GossipEvent] = []

    async def collect_events():
        async for event in rg_b.listen():
            received_events.append(event)

    collect_task = asyncio.create_task(collect_events())

    # Withdraw (short grace period for test speed)
    await publisher.withdraw(grace_period_s=0.05)

    # Allow gossip to propagate
    await asyncio.sleep(0.5)
    collect_task.cancel()
    await asyncio.gather(collect_task, return_exceptions=True)

    # Verify ENDPOINT_DOWN event was received
    down_events = [e for e in received_events if e.type == GossipEventType.ENDPOINT_DOWN]
    assert len(down_events) >= 1
    assert down_events[0].endpoint_id == node_a_id

    await node_a.shutdown()
    await node_b.shutdown()


# ── ACL post-read filter (two authors, single node) ───────────────────────────


@pytest.mark.asyncio
async def test_acl_excludes_untrusted_author_entries():
    """Leases from untrusted authors are excluded from resolve results."""
    from aster_python import IrohNode, docs_client

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()

    trusted_author = await dc.create_author()
    untrusted_author = await dc.create_author()

    import blake3
    contract_bytes = b"acl_test_contract"
    contract_id = blake3.blake3(contract_bytes).hexdigest()

    # Trusted author writes version pointer + lease
    await doc.set_bytes(
        trusted_author,
        version_key("AclSvc", 1),
        contract_id.encode(),
    )
    trusted_lease = _make_lease(
        service="AclSvc", contract_id=contract_id,
        endpoint_id="trusted_ep", health_status=READY,
    )
    await doc.set_bytes(
        trusted_author,
        lease_key("AclSvc", contract_id, "trusted_ep"),
        trusted_lease.to_json().encode(),
    )

    # Untrusted author writes a competing lease
    untrusted_lease = _make_lease(
        service="AclSvc", contract_id=contract_id,
        endpoint_id="untrusted_ep", health_status=READY,
    )
    await doc.set_bytes(
        untrusted_author,
        lease_key("AclSvc", contract_id, "untrusted_ep"),
        untrusted_lease.to_json().encode(),
    )

    # Create ACL that only trusts trusted_author
    acl = RegistryACL(doc, trusted_author)
    acl._writers = {trusted_author}
    acl._open = False

    client = RegistryClient(doc, acl=acl, caller_alpn="aster/1")
    results = await client.resolve_all("AclSvc", version=1)
    endpoint_ids = {r.endpoint_id for r in results}

    # Only trusted_ep should appear
    assert "trusted_ep" in endpoint_ids
    assert "untrusted_ep" not in endpoint_ids

    await node.shutdown()


# ── Download policy integration ───────────────────────────────────────────────


@pytest.mark.asyncio
async def test_registry_doc_nothing_except_policy():
    """set_download_policy('nothing_except', REGISTRY_PREFIXES) round-trips."""
    from aster_python import IrohNode, docs_client, DocDownloadPolicy

    node = await IrohNode.memory()
    dc = docs_client(node)
    doc = await dc.create()

    await doc.set_download_policy("nothing_except", REGISTRY_PREFIXES)
    policy = await doc.get_download_policy()
    assert isinstance(policy, DocDownloadPolicy)
    assert policy.mode == "nothing_except"
    prefix_set = {bytes(p) for p in policy.prefixes}
    for expected_prefix in REGISTRY_PREFIXES:
        assert expected_prefix in prefix_set, f"Missing prefix: {expected_prefix}"

    await node.shutdown()
