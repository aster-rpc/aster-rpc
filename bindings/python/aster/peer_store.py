"""
aster.peer_store -- Per-peer admission attribute store.

Bridges the gap between admission (where attributes are determined)
and RPC dispatch (where attributes are needed for authorization).

Both consumer admission and delegated admission handlers write to this
store on successful admission. The RPC server reads from it when
building CallContext for each call.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field


@dataclass
class PeerAdmission:
    """Record of a successful peer admission."""

    endpoint_id: str
    handle: str = ""
    attributes: dict[str, str] = field(default_factory=dict)
    admitted_at: float = field(default_factory=time.time)
    admission_path: str = ""  # "consumer_admission" | "aster.admission"


class PeerAttributeStore:
    """In-memory store mapping peer endpoint_id to admission attributes.

    Thread-safe for concurrent reads/writes from admission handlers
    and the RPC server.
    """

    def __init__(self) -> None:
        self._peers: dict[str, PeerAdmission] = {}

    def admit(self, admission: PeerAdmission) -> None:
        """Record a successful admission."""
        self._peers[admission.endpoint_id] = admission

    def get(self, endpoint_id: str) -> PeerAdmission | None:
        """Look up admission record for a peer."""
        return self._peers.get(endpoint_id)

    def remove(self, endpoint_id: str) -> None:
        """Remove a peer on disconnect or revocation."""
        self._peers.pop(endpoint_id, None)

    def get_attributes(self, endpoint_id: str) -> dict[str, str]:
        """Get attributes dict for a peer, or empty if not admitted."""
        admission = self._peers.get(endpoint_id)
        return dict(admission.attributes) if admission else {}
