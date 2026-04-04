"""
aster.registry.publisher — RegistryPublisher for the Aster service registry.

Spec references:
- §11.6: Endpoint leases and health state machine
- §11.8: Publication and advertisement flows

The publisher writes EndpointLease + ArtifactRef entries to the registry doc
and maintains a background refresh task. Graceful shutdown transitions through
DRAINING state before deleting the lease and emitting ENDPOINT_DOWN gossip.
"""

from __future__ import annotations

import asyncio
import logging
import time
from typing import TYPE_CHECKING

from .keys import contract_key, lease_key, version_key, channel_key
from .models import (
    ArtifactRef,
    EndpointLease,
    STARTING,
    DRAINING,
)

if TYPE_CHECKING:
    from .gossip import RegistryGossip

logger = logging.getLogger(__name__)

_DEFAULT_LEASE_DURATION_S = 45
_DEFAULT_REFRESH_INTERVAL_S = 15


class RegistryPublisher:
    """Publishes contracts and manages endpoint lifecycle in the registry.

    Typical usage::

        publisher = RegistryPublisher(doc, author_id, gossip, blobs)
        await publisher.register_endpoint(contract_id, "MyService", version=1)
        await publisher.set_health(READY)
        # ... serve requests ...
        await publisher.withdraw()
    """

    def __init__(
        self,
        doc: object,
        author_id: str,
        gossip: "RegistryGossip | None" = None,
        blobs: object | None = None,
        *,
        lease_duration_s: int = _DEFAULT_LEASE_DURATION_S,
        lease_refresh_interval_s: int = _DEFAULT_REFRESH_INTERVAL_S,
        mesh_gossip_handle: object | None = None,
        mesh_sender_id: str = "",
        mesh_signing_key: bytes = b"",
        mesh_heartbeat_interval_ms: int = 900_000,
    ) -> None:
        """
        Args:
            doc:        ``DocHandle`` for the registry doc.
            author_id:  The local author's ID for writing doc entries.
            gossip:     Optional ``RegistryGossip`` for change notifications.
            blobs:      Optional ``BlobsClient`` for contract blob upload.
            lease_duration_s:          Lease TTL (default 45 s).
            lease_refresh_interval_s:  Background refresh cadence (default 15 s).
            mesh_gossip_handle:        Optional ``GossipTopicHandle`` for the
                                       producer mesh.  When provided (together
                                       with ``mesh_sender_id`` and
                                       ``mesh_signing_key``), a signed
                                       LeaseUpdate heartbeat is broadcast every
                                       ``mesh_heartbeat_interval_ms``.
            mesh_sender_id:            This node's endpoint_id hex string.
            mesh_signing_key:          32-byte raw ed25519 private key seed.
            mesh_heartbeat_interval_ms: Heartbeat cadence in ms (default 15 min).
        """
        self._doc = doc
        self._author_id = author_id
        self._gossip = gossip
        self._blobs = blobs
        self._lease_duration_s = lease_duration_s
        self._refresh_interval_s = lease_refresh_interval_s

        # Producer-mesh heartbeat params
        self._mesh_gossip_handle = mesh_gossip_handle
        self._mesh_sender_id = mesh_sender_id
        self._mesh_signing_key = mesh_signing_key
        self._mesh_heartbeat_interval_ms = mesh_heartbeat_interval_ms

        # Current lease state (set when register_endpoint is called)
        self._lease: EndpointLease | None = None
        self._refresh_task: asyncio.Task | None = None
        self._heartbeat_task: asyncio.Task | None = None

    # ── Contract publication ───────────────────────────────────────────────────

    async def publish_contract(
        self,
        contract_bytes: bytes,
        service: str,
        version: int,
        *,
        channel: str | None = None,
        published_by: str = "",
        type_defs: dict | None = None,
    ) -> str:
        """Publish a contract to the registry doc and blob store.

        When ``type_defs`` is provided AND a blobs client is configured, all
        collection entries (contract.bin, manifest.json, types/*.bin) are
        uploaded individually via :func:`~aster.contract.publication.upload_collection`
        and an ``"index"`` format ArtifactRef is written.

        When ``type_defs`` is not provided (or no blobs client), a single-blob
        upload is performed (``collection_hash == contract_id``).

        Args:
            contract_bytes:  Canonical XLANG bytes of the ServiceContract.
            service:         Service name.
            version:         Version number.
            channel:         Optional channel alias (e.g. "stable").
            published_by:    Author descriptor (human-readable, not AuthorId).
            type_defs:       Optional dict of TypeDef objects for multi-file upload.

        Returns:
            contract_id (64-char hex string).
        """
        import blake3  # type: ignore[import]

        contract_id = blake3.blake3(contract_bytes).hexdigest()

        # Determine collection format and hash
        collection_hash = contract_id       # default: single-blob
        collection_format = "raw"

        if self._blobs is not None and type_defs is not None:
            # Multi-file collection upload
            from aster_python.aster.contract.publication import (
                upload_collection as _upload_collection,
            )
            # Build entries: contract.bin first, then types, manifest last
            # We have contract_bytes already; build a minimal collection
            entries: list[tuple[str, bytes]] = [("contract.bin", contract_bytes)]
            for fqn, td in type_defs.items():
                from aster_python.aster.contract.identity import (
                    canonical_xlang_bytes as _cb,
                    compute_type_hash as _th,
                )
                td_bytes = _cb(td)
                h_hex = _th(td_bytes).hex()
                entries.append((f"types/{h_hex}.bin", td_bytes))

            collection_hash = await _upload_collection(self._blobs, entries)
            collection_format = "index"
        elif self._blobs is not None:
            # Single-blob upload
            await self._blobs.add_bytes(contract_bytes)

        now_ms = int(time.time() * 1000)
        ref = ArtifactRef(
            contract_id=contract_id,
            collection_hash=collection_hash,
            published_by=published_by or self._author_id,
            published_at_epoch_ms=now_ms,
            collection_format=collection_format,
        )
        await self._doc.set_bytes(
            self._author_id,
            contract_key(contract_id),
            ref.to_json().encode(),
        )

        # Version pointer (append-only by convention)
        await self._doc.set_bytes(
            self._author_id,
            version_key(service, version),
            contract_id.encode(),
        )

        # Optional channel alias
        if channel is not None:
            await self._doc.set_bytes(
                self._author_id,
                channel_key(service, channel),
                contract_id.encode(),
            )

        # Gossip
        if self._gossip is not None:
            try:
                await self._gossip.broadcast_contract_published(
                    contract_id, service, version
                )
            except Exception as exc:  # noqa: BLE001
                logger.warning("gossip broadcast_contract_published failed: %s", exc)

        logger.debug("Published contract %s for %s v%d", contract_id, service, version)
        return contract_id

    # ── Endpoint registration ──────────────────────────────────────────────────

    async def register_endpoint(
        self,
        contract_id: str,
        service: str,
        version: int,
        *,
        endpoint_id: str,
        alpn: str = "aster/1",
        serialization_modes: list[str] | None = None,
        direct_addrs: list[str] | None = None,
        relay_url: str | None = None,
        feature_flags: list[str] | None = None,
        tags: list[str] | None = None,
        policy_realm: str | None = None,
        health_status: str = STARTING,
        language_runtime: str | None = None,
        aster_version: str = "0.2.0",
    ) -> None:
        """Write initial EndpointLease (health=STARTING) and start refresh.

        Emits ENDPOINT_LEASE_UPSERTED gossip after writing the lease.
        """
        now_ms = int(time.time() * 1000)
        self._lease = EndpointLease(
            endpoint_id=endpoint_id,
            contract_id=contract_id,
            service=service,
            version=version,
            lease_expires_epoch_ms=now_ms + self._lease_duration_s * 1000,
            lease_seq=1,
            alpn=alpn,
            serialization_modes=list(serialization_modes or ["fory-xlang"]),
            feature_flags=list(feature_flags or []),
            relay_url=relay_url,
            direct_addrs=list(direct_addrs or []),
            load=None,
            language_runtime=language_runtime,
            aster_version=aster_version,
            policy_realm=policy_realm,
            health_status=health_status,
            tags=list(tags or []),
            updated_at_epoch_ms=now_ms,
        )
        await self._write_lease()

        # Start background refresh task
        self._refresh_task = asyncio.create_task(self._refresh_loop())

        # Start producer-mesh lease heartbeat if configured
        if (
            self._mesh_gossip_handle is not None
            and self._mesh_sender_id
            and self._mesh_signing_key
        ):
            from aster_python.aster.trust.gossip import start_lease_heartbeat

            self._heartbeat_task = start_lease_heartbeat(
                gossip_topic_handle=self._mesh_gossip_handle,
                sender=self._mesh_sender_id,
                signing_key_raw=self._mesh_signing_key,
                service_name=service,
                version=version,
                contract_id=contract_id,
                health_getter=lambda: self._lease.health_status if self._lease else "UNKNOWN",
                heartbeat_interval_ms=self._mesh_heartbeat_interval_ms,
            )

    async def _write_lease(self) -> None:
        """Persist the current lease to the doc and emit gossip."""
        assert self._lease is not None
        lease = self._lease
        now_ms = int(time.time() * 1000)
        lease.updated_at_epoch_ms = now_ms
        lease.lease_expires_epoch_ms = now_ms + self._lease_duration_s * 1000

        await self._doc.set_bytes(
            self._author_id,
            lease_key(lease.service, lease.contract_id, lease.endpoint_id),
            lease.to_json().encode(),
        )
        if self._gossip is not None:
            try:
                await self._gossip.broadcast_endpoint_lease_upserted(
                    lease.endpoint_id,
                    lease.service,
                    lease.lease_seq,
                    lease.contract_id,
                )
            except Exception as exc:  # noqa: BLE001
                logger.warning("gossip broadcast_endpoint_lease_upserted failed: %s", exc)

    # ── Health transitions ─────────────────────────────────────────────────────

    async def set_health(self, status: str) -> None:
        """Transition health state; bumps lease_seq and writes new lease row."""
        if self._lease is None:
            raise RuntimeError("No active lease — call register_endpoint first")
        self._lease.health_status = status
        self._lease.lease_seq += 1
        await self._write_lease()
        logger.debug(
            "Health set to %s (seq=%d) for %s",
            status,
            self._lease.lease_seq,
            self._lease.service,
        )

    # ── Background refresh ─────────────────────────────────────────────────────

    async def _refresh_loop(self) -> None:
        """Background task: refreshes the lease every ``lease_refresh_interval_s``."""
        try:
            while True:
                await asyncio.sleep(self._refresh_interval_s)
                if self._lease is None:
                    break
                self._lease.lease_seq += 1
                await self._write_lease()
                logger.debug(
                    "Lease refreshed (seq=%d) for %s",
                    self._lease.lease_seq,
                    self._lease.service,
                )
        except asyncio.CancelledError:
            pass

    # ── Graceful withdraw ──────────────────────────────────────────────────────

    async def withdraw(self, grace_period_s: float = 5.0) -> None:
        """Graceful shutdown state machine.

        1. set_health(DRAINING) — bumps lease_seq, writes updated lease
        2. Wait ``grace_period_s`` for in-flight calls to drain
        3. Delete the lease row from the doc
        4. Broadcast ENDPOINT_DOWN gossip
        5. Cancel the refresh background task
        """
        if self._lease is None:
            return  # nothing to withdraw

        # 1. Transition to DRAINING
        await self.set_health(DRAINING)

        # 2. Grace period
        if grace_period_s > 0:
            await asyncio.sleep(grace_period_s)

        # 3. Delete lease — overwrite with tombstone sentinel.
        #    iroh-docs has no true delete; b"null" signals deletion to readers.
        lease = self._lease
        await self._doc.set_bytes(
            self._author_id,
            lease_key(lease.service, lease.contract_id, lease.endpoint_id),
            b"null",
        )

        # 4. ENDPOINT_DOWN gossip
        if self._gossip is not None:
            try:
                await self._gossip.broadcast_endpoint_down(
                    lease.endpoint_id, lease.service
                )
            except Exception as exc:  # noqa: BLE001
                logger.warning("gossip broadcast_endpoint_down failed: %s", exc)

        # 5. Cancel refresh task
        self._cancel_refresh()
        self._lease = None
        logger.debug("Withdrew endpoint %s from %s", lease.endpoint_id, lease.service)

    def _cancel_refresh(self) -> None:
        if self._refresh_task is not None and not self._refresh_task.done():
            self._refresh_task.cancel()
        self._refresh_task = None
        if self._heartbeat_task is not None and not self._heartbeat_task.done():
            self._heartbeat_task.cancel()
        self._heartbeat_task = None

    async def close(self) -> None:
        """Stop the background refresh task and heartbeat (without graceful withdraw)."""
        self._cancel_refresh()
