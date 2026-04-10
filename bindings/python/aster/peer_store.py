"""
aster.peer_store -- Per-peer admission attribute store.

Bridges the gap between admission (where attributes are determined)
and RPC dispatch (where attributes are needed for authorization).

Both consumer admission and delegated admission handlers write to this
store on successful admission. The RPC server reads from it when
building CallContext for each call.

Expiry: each admission records the credential's ``expires_at`` (epoch
seconds). Lookups check expiry and lazily evict stale entries. A
background reaper sweeps entries that are never looked up again.

Config: ``ASTER_PEER_TTL_S`` (env var) sets the server-side upper bound
on how long a peer stays admitted, regardless of credential expiry.
Default 86400 (24 hours).
"""

from __future__ import annotations

import asyncio
import logging
import os
import time
from dataclasses import dataclass, field

logger = logging.getLogger(__name__)

PEER_TTL_S: float = float(os.environ.get("ASTER_PEER_TTL_S", "86400"))
"""Server-side upper bound on peer admission lifetime (seconds).
Applied regardless of credential expiry. Whichever is sooner wins."""

_REAPER_INTERVAL_S: float = 300.0


@dataclass
class PeerAdmission:
    """Record of a successful peer admission."""

    endpoint_id: str
    handle: str = ""
    attributes: dict[str, str] = field(default_factory=dict)
    admitted_at: float = field(default_factory=time.time)
    expires_at: float = 0.0
    admission_path: str = ""  # "consumer_admission" | "aster.admission"

    def is_expired(self) -> bool:
        now = time.time()
        if self.expires_at > 0 and now > self.expires_at:
            return True
        if now > self.admitted_at + PEER_TTL_S:
            return True
        return False


class PeerAttributeStore:
    """In-memory store mapping peer endpoint_id to admission attributes.

    Entries are lazily evicted on access when expired. A background
    reaper task (started via ``start_reaper()``) sweeps entries that
    are never accessed again.
    """

    def __init__(self) -> None:
        self._peers: dict[str, PeerAdmission] = {}
        self._reaper_task: asyncio.Task | None = None

    def admit(self, admission: PeerAdmission) -> None:
        """Record a successful admission."""
        self._peers[admission.endpoint_id] = admission

    def get(self, endpoint_id: str) -> PeerAdmission | None:
        """Look up admission record for a peer. Returns None if expired."""
        admission = self._peers.get(endpoint_id)
        if admission is None:
            return None
        if admission.is_expired():
            self._peers.pop(endpoint_id, None)
            logger.debug("Peer %s expired (admitted_at=%.0f, expires_at=%.0f)",
                         endpoint_id, admission.admitted_at, admission.expires_at)
            return None
        return admission

    def remove(self, endpoint_id: str) -> None:
        """Remove a peer on disconnect or revocation."""
        self._peers.pop(endpoint_id, None)

    def get_attributes(self, endpoint_id: str) -> dict[str, str]:
        """Get attributes dict for a peer, or empty if expired/not admitted."""
        admission = self.get(endpoint_id)
        return dict(admission.attributes) if admission else {}

    def sweep_expired(self) -> int:
        """Remove all expired entries. Returns count removed."""
        expired = [eid for eid, adm in self._peers.items() if adm.is_expired()]
        for eid in expired:
            self._peers.pop(eid, None)
        if expired:
            logger.debug("Reaped %d expired peer admissions", len(expired))
        return len(expired)

    @property
    def peer_count(self) -> int:
        return len(self._peers)

    def start_reaper(self) -> None:
        """Start background reaper task (call once at server start)."""
        if self._reaper_task is not None and not self._reaper_task.done():
            return
        self._reaper_task = asyncio.get_event_loop().create_task(self._reaper_loop())

    def stop_reaper(self) -> None:
        if self._reaper_task is not None:
            self._reaper_task.cancel()
            self._reaper_task = None

    async def _reaper_loop(self) -> None:
        try:
            while True:
                await asyncio.sleep(_REAPER_INTERVAL_S)
                self.sweep_expired()
        except asyncio.CancelledError:
            pass
