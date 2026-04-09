"""
aster.trust.drift -- Clock drift detection for the producer mesh.

Spec reference: Aster-trust-spec.md §2.10.  Plan: ASTER_PLAN.md §14.7.

Every ProducerMessage carries an ``epoch_ms`` field.  Receivers compute
``offset = now_ms - msg.epoch_ms`` for each peer and maintain a running
mesh-median offset.  A peer whose offset deviates from the median by more than
``drift_tolerance_ms`` is *isolated* -- ContractPublished / LeaseUpdate messages
from that peer are skipped until the peer recovers (i.e. sends a fresh message
whose offset is within tolerance).

If *this* node's own clock appears to be the outlier, it self-departs (broadcasts
Depart) and sets ``mesh_dead = True``, suppressing further gossip sends.

Grace period: drift decisions are suppressed for ``grace_period_ms`` after joining
to allow clock calibration across the first few messages.

Clock offset convention: positive offset = peer's clock is behind local clock.
"""

from __future__ import annotations

import logging
import statistics
import time

from .mesh import ClockDriftConfig

logger = logging.getLogger(__name__)


class ClockDriftDetector:
    """Track clock offsets for all mesh peers and detect anomalous drift.

    Usage::

        cfg = ClockDriftConfig()
        detector = ClockDriftDetector(cfg)

        # Called on every valid ProducerMessage (after sig check):
        detector.track_offset(msg.sender, msg.epoch_ms)

        # Check if a peer is in drift:
        if detector.peer_in_drift("peer_endpoint_id"):
            state.drift_isolated.add("peer_endpoint_id")

        # Check self drift:
        if detector.self_in_drift(my_offset_estimate):
            # self-depart path
            ...
    """

    def __init__(
        self,
        config: ClockDriftConfig,
        mesh_joined_at_epoch_ms: int = 0,
    ) -> None:
        self._cfg = config
        self._peer_offsets: dict[str, int] = {}
        self._mesh_joined_at_ms = mesh_joined_at_epoch_ms or int(time.time() * 1000)

    # ── Public API ────────────────────────────────────────────────────────────

    def track_offset(self, peer: str, epoch_ms: int) -> None:
        """Record the clock offset for ``peer`` based on ``epoch_ms``.

        offset = now_ms - msg.epoch_ms.  Positive → peer's clock is behind ours.
        """
        now_ms = int(time.time() * 1000)
        offset = now_ms - epoch_ms
        self._peer_offsets[peer] = offset
        logger.debug("drift: peer=%s offset=%dms", peer, offset)

    def peer_offsets(self) -> dict[str, int]:
        """Return a copy of the current peer offset map."""
        return dict(self._peer_offsets)

    def mesh_median_offset(self) -> int | None:
        """Median clock offset across all tracked peers.

        Returns ``None`` if fewer than ``min_peers_for_median`` peers are tracked
        (not enough data for meaningful drift detection).

        Uses ``statistics.median_high`` so the result is always one of the
        observed values (deterministic for an even-length sequence).
        """
        offsets = list(self._peer_offsets.values())
        if len(offsets) < self._cfg.min_peers_for_median:
            return None
        return statistics.median_high(offsets)

    def peer_in_drift(self, peer: str) -> bool:
        """True if ``peer``'s offset deviates from the mesh median by more than
        ``drift_tolerance_ms``.

        Returns False if the median is unavailable (too few peers) or if the
        grace period has not yet elapsed -- in those cases, no isolation decision
        is made.
        """
        if self._in_grace_period():
            return False
        median = self.mesh_median_offset()
        if median is None:
            return False
        offset = self._peer_offsets.get(peer)
        if offset is None:
            return False
        return abs(offset - median) > self._cfg.drift_tolerance_ms

    def self_in_drift(self, self_offset_estimate: int) -> bool:
        """True if this node's own clock appears to be the outlier.

        ``self_offset_estimate`` should be ``now_ms - msg.epoch_ms`` computed
        when this node sends a message (i.e. it is ~0 if the clock is correct).

        Returns False during the grace period or when there are too few peers.
        """
        if self._in_grace_period():
            return False
        median = self.mesh_median_offset()
        if median is None:
            return False
        return abs(self_offset_estimate - median) > self._cfg.drift_tolerance_ms

    def remove_peer(self, peer: str) -> None:
        """Remove a peer from the offset tracking table (called on Depart)."""
        self._peer_offsets.pop(peer, None)

    # ── Internal helpers ──────────────────────────────────────────────────────

    def _in_grace_period(self) -> bool:
        now_ms = int(time.time() * 1000)
        return (now_ms - self._mesh_joined_at_ms) < self._cfg.grace_period_ms
