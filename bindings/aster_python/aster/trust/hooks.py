"""
aster.trust.hooks — Gate 0 connection-level access control.

Spec reference: Aster-trust-spec.md §3.3.

``MeshEndpointHook`` maintains the admitted-peer allowlist and implements
the connection-time access-control decision.  Admission ALPNs are always
allowed — they carry credential presentation; after successful admission,
the server calls ``add_peer(endpoint_id)``.

The hook integrates with Iroh's HookManager (Phase 1b FFI) via a background
task that polls ``HookReceiver.recv()`` and calls ``should_allow()`` to
make decisions.  See ``run_hook_loop()`` for the recommended wiring.

ALPN constants:
  ALPN_PRODUCER_ADMISSION = b"aster.producer_admission"
  ALPN_CONSUMER_ADMISSION = b"aster.consumer_admission"

Threat model note:
  If Gate 0 is absent/misconfigured and the NodeID leaks (logs, discovery),
  unenrolled peers can open connections.  This exposes all blobs served by
  the endpoint and any RPC service default-denied at Gate 2.  Gate 0 is the
  only control preventing this access class.  Open nodes (``allow_unenrolled``
  = True) MUST accept this risk explicitly.
"""

from __future__ import annotations

import asyncio
import logging
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    pass

logger = logging.getLogger(__name__)

# ── ALPN constants ────────────────────────────────────────────────────────────

ALPN_PRODUCER_ADMISSION = b"aster.producer_admission"
ALPN_CONSUMER_ADMISSION = b"aster.consumer_admission"

_ADMISSION_ALPNS = frozenset([ALPN_PRODUCER_ADMISSION, ALPN_CONSUMER_ADMISSION])


class MeshEndpointHook:
    """Connection-level admission gate (Gate 0, §3.3).

    Maintains an allowlist of admitted peer endpoint IDs.  The decision logic
    is:

    - Admission ALPNs (``aster.producer_admission``, ``aster.consumer_admission``)
      → **always allow** (credential presentation must be possible).
    - Any other ALPN, peer in ``admitted`` set → **allow**.
    - Any other ALPN, peer NOT in ``admitted`` and ``allow_unenrolled=False``
      → **deny** (logs, no diagnostic to peer).
    - ``allow_unenrolled=True`` → **allow all** (local/dev mode; must be
      explicit opt-in).

    Usage::

        hook = MeshEndpointHook()
        # After successful admission of a peer:
        hook.add_peer(new_endpoint_id)
        # In the hook-loop callback:
        if hook.should_allow(info.remote_endpoint_id, bytes(info.alpn)):
            event.send_decision(HookDecision.create_allow())
        else:
            event.send_decision(HookDecision.create_deny(403, b"not admitted"))
    """

    def __init__(self, allow_unenrolled: bool = False) -> None:
        """
        Args:
            allow_unenrolled: When True, all connections are permitted
                regardless of admission state.  Use only in local/dev
                deployments.  Defaults to False (production-safe).
        """
        self.admitted: set[str] = set()
        self.allow_unenrolled = allow_unenrolled

    # ── Decision logic ────────────────────────────────────────────────────────

    def should_allow(self, remote_endpoint_id: str, alpn: bytes) -> bool:
        """Return True if this connection should be allowed.

        Args:
            remote_endpoint_id: NodeId of the connecting peer (from handshake).
            alpn:                ALPN negotiated for this connection.
        """
        # Admission ALPNs are always open — credential presentation
        if alpn in _ADMISSION_ALPNS:
            return True
        # Admitted peers are always allowed
        if remote_endpoint_id in self.admitted:
            return True
        # Open-mode bypass (local/dev only)
        if self.allow_unenrolled:
            return True
        return False

    # ── Allowlist management ──────────────────────────────────────────────────

    def add_peer(self, endpoint_id: str) -> None:
        """Add a peer to the admitted set after successful credential check."""
        self.admitted.add(endpoint_id)
        logger.debug("Gate 0: admitted peer %s", endpoint_id)

    def remove_peer(self, endpoint_id: str) -> None:
        """Remove a peer from the admitted set (e.g., on lease expiry)."""
        self.admitted.discard(endpoint_id)
        logger.debug("Gate 0: removed peer %s", endpoint_id)

    # ── Iroh hook-loop integration ────────────────────────────────────────────

    async def run_hook_loop(self, hook_receiver: object) -> None:
        """Background task: poll HookReceiver and apply Gate 0 decisions.

        Wires this hook to Iroh's Phase 1b ``HookReceiver``.  Run as a
        ``asyncio.create_task()`` after obtaining the receiver from
        ``net_client.take_hook_receiver()``.

        Args:
            hook_receiver: A ``HookRegistration`` object (Phase 1b FFI).
        """
        from aster_python import HookDecision

        try:
            while True:
                event = await hook_receiver.recv()
                if event is None:
                    break
                # event is (HookConnectInfo | HookHandshakeInfo, send_fn)
                # The Phase 1b API returns (info, decision_sender)
                info, send_decision = event
                alpn = bytes(info.alpn) if not isinstance(info.alpn, bytes) else info.alpn
                if self.should_allow(info.remote_endpoint_id, alpn):
                    await send_decision(HookDecision.create_allow())
                else:
                    logger.info(
                        "Gate 0: denied %s on alpn=%r",
                        info.remote_endpoint_id,
                        alpn,
                    )
                    await send_decision(HookDecision.create_deny(403, b"not admitted"))
        except asyncio.CancelledError:
            pass
        except Exception as exc:  # noqa: BLE001
            logger.error("Hook loop error: %s", exc)
