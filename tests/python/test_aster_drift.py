"""
tests/python/test_aster_drift.py

Phase 13 tests for ClockDriftDetector.

Covers offset tracking, mesh median, grace period suppression,
peer isolation, self-drift detection, peer removal, and the
self-departure trigger pattern.

Spec reference: Aster-trust-spec.md §2.10; ASTER_PLAN.md §14.7
"""

from __future__ import annotations

import time


from aster.trust.drift import ClockDriftDetector
from aster.trust.mesh import ClockDriftConfig


# ── Helpers ───────────────────────────────────────────────────────────────────


def _detector_past_grace(tolerance_ms: int = 100, min_peers: int = 2) -> ClockDriftDetector:
    """Return a detector whose grace period has already elapsed."""
    cfg = ClockDriftConfig(
        drift_tolerance_ms=tolerance_ms,
        grace_period_ms=0,
        min_peers_for_median=min_peers,
    )
    # joined_at = 0 → always past grace
    return ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)


def _detector_in_grace(tolerance_ms: int = 100, min_peers: int = 2) -> ClockDriftDetector:
    """Return a detector whose grace period has NOT yet elapsed."""
    cfg = ClockDriftConfig(
        drift_tolerance_ms=tolerance_ms,
        grace_period_ms=999_999_999,  # effectively infinite
        min_peers_for_median=min_peers,
    )
    return ClockDriftDetector(cfg)


# ── track_offset and mesh_median_offset ───────────────────────────────────────


def test_track_offset_records_offset():
    """track_offset stores a computed offset for each peer."""
    cfg = ClockDriftConfig(drift_tolerance_ms=500, grace_period_ms=0, min_peers_for_median=1)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)

    now_ms = int(time.time() * 1000)
    # Simulate a message sent 50 ms ago
    det.track_offset("peer_a", now_ms - 50)

    offsets = det.peer_offsets()
    assert "peer_a" in offsets
    # Offset should be approximately 50 ms (±20 ms for execution time)
    assert 30 <= offsets["peer_a"] <= 100


def test_mesh_median_offset_three_peers():
    """mesh_median_offset returns median_high across 3 peers."""
    cfg = ClockDriftConfig(drift_tolerance_ms=500, grace_period_ms=0, min_peers_for_median=2)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)

    now_ms = int(time.time() * 1000)
    # Inject offsets directly via track_offset using known epoch values
    # offset = now - epoch_ms; set epoch_ms = now - desired_offset
    det.track_offset("peer_a", now_ms - 10)
    det.track_offset("peer_b", now_ms - 20)
    det.track_offset("peer_c", now_ms - 30)

    median = det.mesh_median_offset()
    assert median is not None
    # median_high of [10, 20, 30] is 20
    assert 15 <= median <= 25


def test_mesh_median_offset_even_count_uses_median_high():
    """With an even count, median_high returns the higher of the two middle values."""
    cfg = ClockDriftConfig(drift_tolerance_ms=500, grace_period_ms=0, min_peers_for_median=2)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)

    now_ms = int(time.time() * 1000)
    det.track_offset("peer_a", now_ms - 10)
    det.track_offset("peer_b", now_ms - 30)

    median = det.mesh_median_offset()
    assert median is not None
    # median_high([10, 30]) = 30 (higher of the two)
    assert median >= 20


def test_mesh_median_offset_returns_none_too_few_peers():
    """mesh_median_offset returns None when fewer than min_peers_for_median tracked."""
    cfg = ClockDriftConfig(drift_tolerance_ms=500, grace_period_ms=0, min_peers_for_median=3)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)

    now_ms = int(time.time() * 1000)
    det.track_offset("peer_a", now_ms - 10)
    det.track_offset("peer_b", now_ms - 20)

    assert det.mesh_median_offset() is None


# ── peer_in_drift ─────────────────────────────────────────────────────────────


def test_peer_in_drift_false_during_grace_period():
    """peer_in_drift returns False during the grace period regardless of offset."""
    det = _detector_in_grace()
    now_ms = int(time.time() * 1000)
    # Add peers with wildly different offsets
    det.track_offset("peer_a", now_ms - 10)
    det.track_offset("peer_b", now_ms - 10_000)  # 10 seconds behind

    assert det.peer_in_drift("peer_b") is False


def test_peer_in_drift_false_for_in_tolerance_peer():
    """peer_in_drift returns False for a peer within tolerance."""
    det = _detector_past_grace(tolerance_ms=100)
    now_ms = int(time.time() * 1000)
    det.track_offset("peer_a", now_ms - 20)
    det.track_offset("peer_b", now_ms - 30)

    # Both within 100 ms of each other → no drift
    assert det.peer_in_drift("peer_a") is False
    assert det.peer_in_drift("peer_b") is False


def test_peer_in_drift_true_after_grace_period_with_out_of_tolerance_peer():
    """peer_in_drift returns True after grace period when a peer is out of tolerance."""
    det = _detector_past_grace(tolerance_ms=50)
    now_ms = int(time.time() * 1000)
    # peer_a: offset ~10 ms (nearly real-time)
    det.track_offset("peer_a", now_ms - 10)
    # peer_b: offset ~10 ms
    det.track_offset("peer_b", now_ms - 10)
    # peer_c: offset ~500 ms (heavily delayed/drifted)
    det.track_offset("peer_c", now_ms - 500)

    # peer_c should be isolated
    assert det.peer_in_drift("peer_c") is True


def test_peer_in_drift_false_for_unknown_peer():
    """peer_in_drift returns False for a peer not in the offset table."""
    det = _detector_past_grace()
    now_ms = int(time.time() * 1000)
    det.track_offset("peer_a", now_ms - 10)
    det.track_offset("peer_b", now_ms - 20)

    assert det.peer_in_drift("unknown_peer") is False


# ── self_in_drift ─────────────────────────────────────────────────────────────


def test_self_in_drift_true_when_self_is_outlier():
    """self_in_drift returns True when this node's offset is far from the mesh median."""
    det = _detector_past_grace(tolerance_ms=50)
    now_ms = int(time.time() * 1000)
    # Mesh peers all have small offsets (~10 ms)
    det.track_offset("peer_a", now_ms - 10)
    det.track_offset("peer_b", now_ms - 10)

    # Self estimate is 500 ms -- far outside tolerance
    assert det.self_in_drift(500) is True


def test_self_in_drift_false_when_self_is_within_tolerance():
    """self_in_drift returns False when this node's offset matches the mesh median."""
    det = _detector_past_grace(tolerance_ms=100)
    now_ms = int(time.time() * 1000)
    det.track_offset("peer_a", now_ms - 20)
    det.track_offset("peer_b", now_ms - 30)

    # Self offset of 25 ms is within 100 ms of the median (~20--30 ms)
    assert det.self_in_drift(25) is False


def test_self_in_drift_false_during_grace_period():
    """self_in_drift returns False during the grace period."""
    det = _detector_in_grace(tolerance_ms=50)
    now_ms = int(time.time() * 1000)
    det.track_offset("peer_a", now_ms - 10)
    det.track_offset("peer_b", now_ms - 10)

    assert det.self_in_drift(5000) is False


# ── remove_peer ───────────────────────────────────────────────────────────────


def test_remove_peer_removes_offset_tracking():
    """remove_peer removes the peer from the offset map."""
    det = _detector_past_grace()
    now_ms = int(time.time() * 1000)
    det.track_offset("peer_a", now_ms - 20)
    det.track_offset("peer_b", now_ms - 20)

    det.remove_peer("peer_a")
    offsets = det.peer_offsets()
    assert "peer_a" not in offsets
    assert "peer_b" in offsets


def test_remove_peer_noop_for_unknown_peer():
    """remove_peer does not raise if the peer is not tracked."""
    det = _detector_past_grace()
    # Should not raise
    det.remove_peer("nonexistent_peer")


def test_remove_peer_reduces_median_peer_count():
    """After remove_peer, mesh_median_offset may return None if too few peers remain."""
    cfg = ClockDriftConfig(drift_tolerance_ms=100, grace_period_ms=0, min_peers_for_median=2)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    now_ms = int(time.time() * 1000)
    det.track_offset("peer_a", now_ms - 10)
    det.track_offset("peer_b", now_ms - 20)

    assert det.mesh_median_offset() is not None
    det.remove_peer("peer_a")
    det.remove_peer("peer_b")
    assert det.mesh_median_offset() is None


# ── self-departure trigger pattern ────────────────────────────────────────────


def test_self_departure_trigger_pattern():
    """When self_in_drift is True, caller should depart from the mesh.

    This test verifies the calling pattern: if self_in_drift returns True,
    the caller invokes a 'depart' function (modeled here as a mock).
    """
    det = _detector_past_grace(tolerance_ms=50)
    now_ms = int(time.time() * 1000)
    det.track_offset("peer_a", now_ms - 10)
    det.track_offset("peer_b", now_ms - 10)

    depart_called = []

    def maybe_self_depart(offset_estimate: int) -> None:
        if det.self_in_drift(offset_estimate):
            depart_called.append(True)

    # Self is far out of sync
    maybe_self_depart(5000)

    assert len(depart_called) == 1, "depart should have been triggered once"


def test_self_departure_not_triggered_when_in_sync():
    """self_in_drift=False → depart is NOT called."""
    det = _detector_past_grace(tolerance_ms=200)
    now_ms = int(time.time() * 1000)
    det.track_offset("peer_a", now_ms - 20)
    det.track_offset("peer_b", now_ms - 20)

    depart_called = []

    def maybe_self_depart(offset_estimate: int) -> None:
        if det.self_in_drift(offset_estimate):
            depart_called.append(True)

    # Self is well within tolerance
    maybe_self_depart(15)

    assert len(depart_called) == 0, "depart should NOT have been triggered"
