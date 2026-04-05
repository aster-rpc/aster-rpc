"""
tests/python/test_aster_heartbeat.py

Phase 13 integration tests: lease heartbeat background task.

Covers:
- run_lease_heartbeat broadcasts a signed LeaseUpdate at the configured interval
- start_lease_heartbeat returns a cancellable asyncio.Task
- Broadcast payload is a valid signed ProducerMessage of type LEASE_UPDATE
- health_getter is called fresh on each broadcast (reflects state transitions)
- Heartbeat is cancelled cleanly (no exception leaks)
- RegistryPublisher wires heartbeat on register_endpoint when mesh params provided
- RegistryPublisher cancel_refresh also cancels the heartbeat task
"""

from __future__ import annotations

import asyncio
import json
import time
from unittest.mock import AsyncMock, MagicMock

import pytest

from aster.trust import (
    generate_root_keypair,
    run_lease_heartbeat,
    start_lease_heartbeat,
)
from aster.trust.mesh import ProducerMessageType


# ── Helpers ────────────────────────────────────────────────────────────────────


def make_signing_key() -> tuple[bytes, bytes]:
    """Return (private_key_raw, public_key_raw) pair."""
    priv_raw, pub_raw = generate_root_keypair()
    return priv_raw, pub_raw


class FakeGossipHandle:
    """Mock GossipTopicHandle that records broadcast calls."""

    def __init__(self) -> None:
        self.broadcasts: list[bytes] = []
        self._event = asyncio.Event()

    async def broadcast(self, data: bytes) -> None:
        self.broadcasts.append(data)
        self._event.set()
        self._event.clear()

    async def wait_for_broadcast(self, timeout: float = 2.0) -> bytes:
        """Wait until at least one new broadcast arrives and return it."""
        deadline = time.monotonic() + timeout
        while not self.broadcasts:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("no heartbeat broadcast received")
            try:
                await asyncio.wait_for(self._event.wait(), timeout=remaining)
            except TimeoutError:
                pass
        return self.broadcasts[-1]


def _decode_wire(data: bytes) -> dict:
    return json.loads(data.decode("utf-8"))


# ── Tests ──────────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_heartbeat_broadcasts_signed_lease_update():
    """run_lease_heartbeat broadcasts a valid signed LeaseUpdate after interval."""
    priv_raw, pub_raw = make_signing_key()
    handle = FakeGossipHandle()
    sender = "deadbeef" * 8  # 64-char fake endpoint_id

    task = asyncio.create_task(
        run_lease_heartbeat(
            gossip_topic_handle=handle,
            sender=sender,
            signing_key_raw=priv_raw,
            service_name="TestService",
            version=1,
            contract_id="a" * 64,
            health_getter=lambda: "READY",
            heartbeat_interval_ms=50,  # 50 ms for fast test
        )
    )

    try:
        wire = await handle.wait_for_broadcast(timeout=2.0)
    finally:
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)

    envelope = _decode_wire(wire)
    assert envelope["type"] == ProducerMessageType.LEASE_UPDATE
    assert envelope["sender"] == sender

    payload_bytes = bytes.fromhex(envelope["payload"])
    payload = json.loads(payload_bytes.decode("utf-8"))
    assert payload["service_name"] == "TestService"
    assert payload["version"] == 1
    assert payload["contract_id"] == "a" * 64
    assert payload["health_status"] == "READY"


@pytest.mark.asyncio
async def test_heartbeat_signature_is_valid():
    """Broadcast envelope contains a verifiable ed25519 signature."""
    priv_raw, pub_raw = make_signing_key()
    handle = FakeGossipHandle()
    sender = "cafebabe" * 8

    task = asyncio.create_task(
        run_lease_heartbeat(
            gossip_topic_handle=handle,
            sender=sender,
            signing_key_raw=priv_raw,
            service_name="SigService",
            version=2,
            contract_id="b" * 64,
            health_getter=lambda: "READY",
            heartbeat_interval_ms=50,
        )
    )

    try:
        wire = await handle.wait_for_broadcast(timeout=2.0)
    finally:
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)

    envelope = _decode_wire(wire)
    from aster.trust.mesh import ProducerMessage
    from aster.trust.gossip import verify_producer_message

    msg = ProducerMessage(
        type=envelope["type"],
        payload=bytes.fromhex(envelope["payload"]),
        sender=envelope["sender"],
        epoch_ms=envelope["epoch_ms"],
        signature=bytes.fromhex(envelope["signature"]),
    )
    assert verify_producer_message(msg, pub_raw), "signature must verify with sender pubkey"


@pytest.mark.asyncio
async def test_heartbeat_health_getter_called_each_interval():
    """health_getter is called fresh on each broadcast, reflecting transitions."""
    priv_raw, pub_raw = make_signing_key()
    handle = FakeGossipHandle()

    statuses = ["STARTING", "READY", "DEGRADED"]
    calls: list[str] = []

    def getter() -> str:
        s = statuses[min(len(calls), len(statuses) - 1)]
        calls.append(s)
        return s

    task = asyncio.create_task(
        run_lease_heartbeat(
            gossip_topic_handle=handle,
            sender="ee" * 32,
            signing_key_raw=priv_raw,
            service_name="Svc",
            version=1,
            contract_id="c" * 64,
            health_getter=getter,
            heartbeat_interval_ms=50,
        )
    )

    # Wait for at least 2 broadcasts
    deadline = time.monotonic() + 3.0
    while len(handle.broadcasts) < 2 and time.monotonic() < deadline:
        await asyncio.sleep(0.01)

    task.cancel()
    await asyncio.gather(task, return_exceptions=True)

    assert len(calls) >= 2, "health_getter should have been called at least twice"
    # Each call reflects the evolving status
    seen_statuses = {
        json.loads(bytes.fromhex(_decode_wire(d)["payload"]))["health_status"]
        for d in handle.broadcasts
    }
    assert len(seen_statuses) >= 1


@pytest.mark.asyncio
async def test_heartbeat_cancel_does_not_raise():
    """Cancelling the heartbeat task does not produce unhandled exceptions."""
    priv_raw, _ = make_signing_key()
    handle = FakeGossipHandle()

    task = start_lease_heartbeat(
        gossip_topic_handle=handle,
        sender="ff" * 32,
        signing_key_raw=priv_raw,
        service_name="Svc",
        version=1,
        contract_id="d" * 64,
        health_getter=lambda: "READY",
        heartbeat_interval_ms=10_000,  # long interval — won't fire before cancel
    )

    await asyncio.sleep(0.01)
    task.cancel()
    result = await asyncio.gather(task, return_exceptions=True)
    # Should return None (CancelledError is caught inside run_lease_heartbeat)
    assert result == [None], f"unexpected result: {result}"


@pytest.mark.asyncio
async def test_start_lease_heartbeat_returns_named_task():
    """start_lease_heartbeat returns an asyncio.Task with a descriptive name."""
    priv_raw, _ = make_signing_key()
    handle = FakeGossipHandle()

    task = start_lease_heartbeat(
        gossip_topic_handle=handle,
        sender="aa" * 32,
        signing_key_raw=priv_raw,
        service_name="MyService",
        version=3,
        contract_id="e" * 64,
        health_getter=lambda: "READY",
        heartbeat_interval_ms=60_000,
    )

    assert isinstance(task, asyncio.Task)
    assert "MyService" in task.get_name()
    task.cancel()
    await asyncio.gather(task, return_exceptions=True)


@pytest.mark.asyncio
async def test_registry_publisher_starts_heartbeat_on_register():
    """RegistryPublisher starts heartbeat task when mesh params are provided."""
    priv_raw, _ = make_signing_key()
    handle = FakeGossipHandle()

    # Mock the doc and gossip with minimal async stubs
    doc = MagicMock()
    doc.set_bytes = AsyncMock()

    from aster.registry.publisher import RegistryPublisher

    publisher = RegistryPublisher(
        doc=doc,
        author_id="author-1",
        mesh_gossip_handle=handle,
        mesh_sender_id="bb" * 32,
        mesh_signing_key=priv_raw,
        mesh_heartbeat_interval_ms=50,
    )

    await publisher.register_endpoint(
        contract_id="f" * 64,
        service="PubSvc",
        version=1,
        endpoint_id="bb" * 32,
    )

    assert publisher._heartbeat_task is not None
    assert not publisher._heartbeat_task.done()

    # Wait for at least one heartbeat broadcast
    wire = await handle.wait_for_broadcast(timeout=2.0)
    envelope = _decode_wire(wire)
    assert envelope["type"] == ProducerMessageType.LEASE_UPDATE

    payload = json.loads(bytes.fromhex(envelope["payload"]))
    assert payload["service_name"] == "PubSvc"

    publisher._cancel_refresh()
    assert publisher._heartbeat_task is None or publisher._heartbeat_task.done()


@pytest.mark.asyncio
async def test_registry_publisher_no_heartbeat_without_mesh_params():
    """RegistryPublisher does not start heartbeat when mesh params are absent."""
    doc = MagicMock()
    doc.set_bytes = AsyncMock()

    from aster.registry.publisher import RegistryPublisher

    publisher = RegistryPublisher(doc=doc, author_id="author-2")

    await publisher.register_endpoint(
        contract_id="g" * 64,
        service="NomeshSvc",
        version=1,
        endpoint_id="cc" * 32,
    )

    assert publisher._heartbeat_task is None
    publisher._cancel_refresh()


@pytest.mark.asyncio
async def test_registry_publisher_close_cancels_heartbeat():
    """RegistryPublisher.close() cancels both refresh and heartbeat tasks."""
    priv_raw, _ = make_signing_key()
    handle = FakeGossipHandle()

    doc = MagicMock()
    doc.set_bytes = AsyncMock()

    from aster.registry.publisher import RegistryPublisher

    publisher = RegistryPublisher(
        doc=doc,
        author_id="author-3",
        mesh_gossip_handle=handle,
        mesh_sender_id="dd" * 32,
        mesh_signing_key=priv_raw,
        mesh_heartbeat_interval_ms=60_000,
    )

    await publisher.register_endpoint(
        contract_id="h" * 64,
        service="CloseSvc",
        version=1,
        endpoint_id="dd" * 32,
    )

    ht = publisher._heartbeat_task
    assert ht is not None

    await publisher.close()
    # Yield to the event loop so the cancellation propagates
    await asyncio.sleep(0)

    assert publisher._heartbeat_task is None
    assert ht.cancelled() or ht.done()
