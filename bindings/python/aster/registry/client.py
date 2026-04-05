"""
aster.registry.client — RegistryClient for the Aster service registry.

Spec references:
- §11.8: Client resolution flow
- §11.9: Mandatory filters + ranking strategies
- §11.10: Consistency model (lease expiry, lease_seq monotonicity)

Two-step resolution:
  (service_name + version|channel) → contract_id → list[EndpointLease]

Mandatory filters (§11.9, normative order):
  1. contract_id match
  2. alpn supported by caller
  3. health in {READY, DEGRADED}
  4. lease freshness (now - updated_at_epoch_ms ≤ lease_duration_s * 1000)
  5. policy_realm compatible (if caller configures one)

Ranking strategies (after filters):
  round_robin  — stateful round-robin (default)
  least_load   — lowest lease.load (fallback to round_robin if unavailable)
  random       — uniform random selection

READY preferred over DEGRADED within each strategy.
"""

from __future__ import annotations

import asyncio
import logging
import random as _random
from collections import defaultdict
from typing import Awaitable, Callable, TYPE_CHECKING

from .keys import (
    contract_key,
    lease_prefix,
    version_key,
    channel_key,
    REGISTRY_PREFIXES,
)
from .models import EndpointLease, GossipEvent, READY, DEGRADED

if TYPE_CHECKING:
    from .acl import RegistryACL
    from .gossip import RegistryGossip

logger = logging.getLogger(__name__)

_DEFAULT_LEASE_DURATION_S = 45


class RegistryClient:
    """Resolves service endpoints from the registry doc.

    Typical usage::

        client = RegistryClient(doc, acl=acl, gossip=gossip)
        lease = await client.resolve("MyService", version=1)
        # Connect to lease.endpoint_id via iroh QUIC
    """

    def __init__(
        self,
        doc: object,
        *,
        acl: "RegistryACL | None" = None,
        gossip: "RegistryGossip | None" = None,
        blobs: object | None = None,
        lease_duration_s: int = _DEFAULT_LEASE_DURATION_S,
        caller_alpn: str = "aster/1",
        caller_serialization_modes: list[str] | None = None,
        caller_policy_realm: str | None = None,
    ) -> None:
        """
        Args:
            doc:           ``DocHandle`` for the registry doc.
            acl:           Optional ``RegistryACL`` for author filtering.
            gossip:        Optional ``RegistryGossip`` for change events.
            blobs:         Optional ``BlobsClient`` for contract fetch.
            lease_duration_s:             Lease TTL for freshness check.
            caller_alpn:                  ALPN this caller supports.
            caller_serialization_modes:   Modes the caller supports.
            caller_policy_realm:          Policy realm filter (or None = any).
        """
        self._doc = doc
        self._acl = acl
        self._gossip = gossip
        self._blobs = blobs
        self._lease_duration_s = lease_duration_s
        self._caller_alpn = caller_alpn
        self._caller_modes = set(caller_serialization_modes or ["fory-xlang"])
        self._caller_realm = caller_policy_realm

        # lease_seq monotonicity: (service, contract_id, endpoint_id) → latest seq
        self._seq_cache: dict[tuple[str, str, str], int] = {}
        # round-robin state: contract_id → last-used index
        self._rr_state: dict[str, int] = defaultdict(int)

        # Apply download policy to limit which doc keys are synced
        self._policy_applied = False

    async def _ensure_policy(self) -> None:
        """Apply NothingExcept download policy on first use."""
        if not self._policy_applied:
            try:
                await self._doc.set_download_policy("nothing_except", REGISTRY_PREFIXES)
                self._policy_applied = True
            except Exception as exc:  # noqa: BLE001
                logger.debug("set_download_policy not available: %s", exc)

    # ── Lease-seq monotonicity ────────────────────────────────────────────────

    def _check_seq(self, lease: EndpointLease) -> bool:
        """Return True if this lease is newer than the latest seen seq.

        Updates the cache if accepted; rejects if seq ≤ latest.
        """
        key = (lease.service, lease.contract_id, lease.endpoint_id)
        latest = self._seq_cache.get(key, 0)
        if lease.lease_seq <= latest:
            logger.debug(
                "Rejected stale lease for %s/%s (seq %d ≤ %d)",
                lease.service,
                lease.endpoint_id,
                lease.lease_seq,
                latest,
            )
            return False
        self._seq_cache[key] = lease.lease_seq
        return True

    # ── Doc reads ─────────────────────────────────────────────────────────────

    async def _read_pointer(self, key: bytes) -> str | None:
        """Read a plain text pointer (contract_id) at ``key``."""
        entries = await self._doc.query_key_exact(key)
        if self._acl is not None:
            entries = self._acl.filter_trusted(entries)
        if not entries:
            return None
        # Pick the most recent by timestamp
        entry = max(entries, key=lambda e: e.timestamp)
        raw = await self._doc.read_entry_content(entry.content_hash)
        if not raw:
            return None
        return raw.decode().strip()

    async def _list_leases(
        self, service: str, contract_id: str
    ) -> list[EndpointLease]:
        """Read all EndpointLease entries for a (service, contract_id) pair."""
        prefix = lease_prefix(service, contract_id)
        entries = await self._doc.query_key_prefix(prefix)
        if self._acl is not None:
            entries = self._acl.filter_trusted(entries)

        leases: list[EndpointLease] = []
        for entry in entries:
            raw = await self._doc.read_entry_content(entry.content_hash)
            if not raw or raw == b"null":
                continue  # empty or tombstone = deleted entry (withdraw)
            try:
                lease = EndpointLease.from_json(raw)
            except Exception as exc:  # noqa: BLE001
                logger.debug("Skipping malformed lease entry: %s", exc)
                continue
            leases.append(lease)
        return leases

    # ── Resolution ────────────────────────────────────────────────────────────

    async def _resolve_contract_id(
        self,
        service: str,
        *,
        contract_id: str | None = None,
        version: int | None = None,
        channel: str | None = None,
    ) -> str | None:
        """Step 1: resolve a service + version/channel/contract_id to a contract_id."""
        if contract_id is not None:
            return contract_id
        if version is not None:
            cid = await self._read_pointer(version_key(service, version))
            if cid:
                return cid
        if channel is not None:
            cid = await self._read_pointer(channel_key(service, channel))
            if cid:
                return cid
        return None

    def _apply_mandatory_filters(
        self, leases: list[EndpointLease]
    ) -> list[EndpointLease]:
        """Apply §11.9 mandatory filters in normative order."""
        result = []
        for lease in leases:
            # 1. health in {READY, DEGRADED}
            if not lease.is_routable():
                logger.debug("Filter: %s health=%s", lease.endpoint_id, lease.health_status)
                continue
            # 2. lease freshness
            if not lease.is_fresh(self._lease_duration_s):
                logger.debug("Filter: %s lease expired", lease.endpoint_id)
                continue
            # 3. ALPN match
            if self._caller_alpn and lease.alpn != self._caller_alpn:
                logger.debug("Filter: %s alpn mismatch (%s)", lease.endpoint_id, lease.alpn)
                continue
            # 4. serialization_modes overlap
            if self._caller_modes and not self._caller_modes.intersection(
                lease.serialization_modes
            ):
                logger.debug("Filter: %s no shared serialization modes", lease.endpoint_id)
                continue
            # 5. policy_realm
            if self._caller_realm is not None and lease.policy_realm is not None:
                if lease.policy_realm != self._caller_realm:
                    logger.debug("Filter: %s policy_realm mismatch", lease.endpoint_id)
                    continue
            result.append(lease)
        return result

    def _rank(
        self, candidates: list[EndpointLease], strategy: str, contract_id: str
    ) -> list[EndpointLease]:
        """Rank survivors by strategy; READY preferred over DEGRADED."""
        if not candidates:
            return candidates

        # Partition: READY first, then DEGRADED
        ready = [lz for lz in candidates if lz.health_status == READY]
        degraded = [lz for lz in candidates if lz.health_status == DEGRADED]

        def _apply_strategy(group: list[EndpointLease]) -> list[EndpointLease]:
            if strategy == "round_robin":
                if not group:
                    return group
                idx = self._rr_state[contract_id] % len(group)
                self._rr_state[contract_id] = (idx + 1) % len(group)
                return [group[idx]] + group[:idx] + group[idx + 1:]
            elif strategy == "least_load":
                with_load = [lz for lz in group if lz.load is not None]
                without_load = [lz for lz in group if lz.load is None]
                with_load.sort(key=lambda lz: lz.load)  # type: ignore[arg-type]
                return with_load + without_load
            elif strategy == "random":
                shuffled = list(group)
                _random.shuffle(shuffled)
                return shuffled
            else:
                return group

        return _apply_strategy(ready) + _apply_strategy(degraded)

    async def resolve(
        self,
        service_name: str,
        *,
        contract_id: str | None = None,
        version: int | None = None,
        channel: str | None = None,
        tag: str | None = None,
        strategy: str = "round_robin",
    ) -> EndpointLease:
        """Resolve a service name to the best available EndpointLease.

        Applies mandatory filters then ranks by strategy. Returns the top
        candidate. Raises ``LookupError`` if no suitable endpoint is found.
        """
        await self._ensure_policy()
        all_leases = await self.resolve_all(
            service_name,
            contract_id=contract_id,
            version=version,
            channel=channel,
            strategy=strategy,
        )
        if not all_leases:
            raise LookupError(
                f"No available endpoint for service {service_name!r} "
                f"(version={version}, channel={channel!r})"
            )
        return all_leases[0]

    async def resolve_all(
        self,
        service_name: str,
        *,
        contract_id: str | None = None,
        version: int | None = None,
        channel: str | None = None,
        strategy: str = "round_robin",
    ) -> list[EndpointLease]:
        """Resolve all surviving candidate endpoints (unranked → ranked).

        Returns an empty list when no endpoints pass the mandatory filters.
        """
        await self._ensure_policy()
        cid = await self._resolve_contract_id(
            service_name,
            contract_id=contract_id,
            version=version,
            channel=channel,
        )
        if cid is None:
            return []

        raw_leases = await self._list_leases(service_name, cid)

        # lease_seq monotonicity check
        fresh_leases = [lz for lz in raw_leases if self._check_seq(lz)]

        # Mandatory filters
        filtered = self._apply_mandatory_filters(fresh_leases)

        # Rank
        return self._rank(filtered, strategy, cid)

    # ── Contract fetch ─────────────────────────────────────────────────────────

    async def fetch_contract(self, contract_id: str) -> bytes | None:
        """Fetch contract bytes from the blob store by contract_id.

        Reads the ArtifactRef from the doc, then downloads the blob.  Handles
        both collection formats:

        ``"raw"``   — reads the single blob by ``collection_hash`` directly.
        ``"index"`` — reads the collection index blob, then fetches
                      ``contract.bin`` from it, and verifies the BLAKE3 hash.

        Returns None if not found locally or if the blob store is not configured.
        """
        entries = await self._doc.query_key_exact(contract_key(contract_id))
        if self._acl is not None:
            entries = self._acl.filter_trusted(entries)
        if not entries:
            return None
        entry = max(entries, key=lambda e: e.timestamp)
        raw = await self._doc.read_entry_content(entry.content_hash)
        if not raw:
            return None
        from .models import ArtifactRef
        ref = ArtifactRef.from_json(raw)

        if self._blobs is None:
            return None

        if getattr(ref, "collection_format", "raw") == "index":
            # Multi-file collection: use publication.fetch_contract
            from aster.contract.publication import fetch_contract as _fetch
            try:
                return await _fetch(
                    contract_id, self._blobs, collection_hash=ref.collection_hash
                )
            except Exception as exc:  # noqa: BLE001
                logger.debug("fetch_contract (index): failed for %s: %s", contract_id[:12], exc)
                return None

        # Single-blob ("raw") mode
        # Check local cache first (blob_local_info if available)
        try:
            local_info = await self._blobs.blob_local_info(ref.collection_hash)
            if local_info is not None:
                return await self._blobs.read_to_bytes(ref.collection_hash)
        except Exception:  # noqa: BLE001
            pass

        # Wait for download (blob_observe_complete if available)
        try:
            await self._blobs.blob_observe_complete(ref.collection_hash)
            return await self._blobs.read_to_bytes(ref.collection_hash)
        except Exception:  # noqa: BLE001
            pass

        # Fallback: direct read
        try:
            return await self._blobs.read_to_bytes(ref.collection_hash)
        except Exception:  # noqa: BLE001
            return None

    # ── Change notifications ───────────────────────────────────────────────────

    def on_change(
        self, callback: Callable[[GossipEvent], Awaitable[None]]
    ) -> asyncio.Task:
        """Subscribe to gossip change events.

        Returns a background ``asyncio.Task`` that calls ``callback`` for each
        received GossipEvent. Cancel the task to stop listening.
        """
        if self._gossip is None:
            raise RuntimeError("on_change requires a gossip handle")
        return asyncio.create_task(self._listen_loop(callback))

    async def _listen_loop(
        self, callback: Callable[[GossipEvent], Awaitable[None]]
    ) -> None:
        try:
            async for event in self._gossip.listen():
                try:
                    await callback(event)
                except Exception as exc:  # noqa: BLE001
                    logger.warning("on_change callback raised: %s", exc)
        except asyncio.CancelledError:
            pass
