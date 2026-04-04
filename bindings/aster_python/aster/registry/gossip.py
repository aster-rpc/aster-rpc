"""
aster.registry.gossip — Gossip layer for the Aster service registry.

Spec reference: Aster-SPEC.md §11.7.

All 6 normative event types are broadcast as JSON-encoded GossipEvent bytes.
The underlying transport is the iroh-gossip GossipTopicHandle.
"""

from __future__ import annotations

import json
import logging
import time
from typing import AsyncIterator

from .models import GossipEvent, GossipEventType

logger = logging.getLogger(__name__)


class RegistryGossip:
    """Wrapper around a GossipTopicHandle for registry-level events.

    Usage::

        gossip_client = gossip_client(node)
        topic_handle = await gossip_client.subscribe(topic_bytes, peer_ids)
        rg = RegistryGossip(topic_handle)

        # Broadcast
        await rg.broadcast_contract_published("abc123", "MyService", 1)

        # Listen
        async for event in rg.listen():
            print(event)
    """

    def __init__(self, topic_handle: object) -> None:
        """
        Args:
            topic_handle: A ``GossipTopicHandle`` from ``GossipClient.subscribe()``.
        """
        self._handle = topic_handle

    # ── Broadcast methods (all 6 normative event types) ───────────────────────

    async def broadcast_contract_published(
        self, contract_id: str, service: str, version: int
    ) -> None:
        event = GossipEvent(
            type=GossipEventType.CONTRACT_PUBLISHED,
            contract_id=contract_id,
            service=service,
            version=version,
            timestamp_ms=int(time.time() * 1000),
        )
        await self._broadcast(event)

    async def broadcast_channel_updated(
        self, service: str, channel: str, contract_id: str
    ) -> None:
        event = GossipEvent(
            type=GossipEventType.CHANNEL_UPDATED,
            service=service,
            channel=channel,
            contract_id=contract_id,
            timestamp_ms=int(time.time() * 1000),
        )
        await self._broadcast(event)

    async def broadcast_endpoint_lease_upserted(
        self, endpoint_id: str, service: str, lease_seq: int, contract_id: str
    ) -> None:
        event = GossipEvent(
            type=GossipEventType.ENDPOINT_LEASE_UPSERTED,
            endpoint_id=endpoint_id,
            service=service,
            version=lease_seq,   # re-use version field for lease_seq in transport
            contract_id=contract_id,
            timestamp_ms=int(time.time() * 1000),
        )
        await self._broadcast(event)

    async def broadcast_endpoint_down(
        self, endpoint_id: str, service: str
    ) -> None:
        event = GossipEvent(
            type=GossipEventType.ENDPOINT_DOWN,
            endpoint_id=endpoint_id,
            service=service,
            timestamp_ms=int(time.time() * 1000),
        )
        await self._broadcast(event)

    async def broadcast_acl_changed(self, key_prefix: str) -> None:
        event = GossipEvent(
            type=GossipEventType.ACL_CHANGED,
            key_prefix=key_prefix,
            timestamp_ms=int(time.time() * 1000),
        )
        await self._broadcast(event)

    async def broadcast_compatibility_published(
        self, source_contract_id: str, target_contract_id: str
    ) -> None:
        event = GossipEvent(
            type=GossipEventType.COMPATIBILITY_PUBLISHED,
            contract_id=source_contract_id,
            endpoint_id=target_contract_id,   # overload: use endpoint_id for target
            timestamp_ms=int(time.time() * 1000),
        )
        await self._broadcast(event)

    async def _broadcast(self, event: GossipEvent) -> None:
        await self._handle.broadcast(event.to_json().encode())

    # ── Listen ────────────────────────────────────────────────────────────────

    async def listen(self) -> AsyncIterator[GossipEvent]:
        """Async generator that yields GossipEvents from the topic.

        Skips non-"received" gossip wire events (neighbor up/down, lag notices).
        Silently drops messages that cannot be decoded as GossipEvent.
        """
        while True:
            event_type, data = await self._handle.recv()
            if event_type != "received" or not data:
                continue
            try:
                yield GossipEvent.from_json(data)
            except (json.JSONDecodeError, KeyError, ValueError) as exc:
                logger.debug("Skipping undecipherable gossip message: %s", exc)
