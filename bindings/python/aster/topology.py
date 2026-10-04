"""Topology -- "where are my peers?" (Rust ``aster::topology``).

v1 is a passive per-peer proximity view (locality ladder, RTT, loss,
throughput, confidence) from connections the node already makes; it needs a
node started with ``enable_monitoring=True``. v2 joins a shared topology swarm
keyed by a namespace secret (usually delivered in a sealed grant) and derives
the swarm-wide cluster view::

    topo = node.topology()
    topo.peers()                                         # v1
    await topo.enable_shared(secret, bootstrap=[root_id])
    snap = await topo.snapshot()                         # v2: one consistent view
    snap.clusters; snap.separated(a, b)                  # "unknown" | "connected" | "separated"

The read-time ``admitted`` filter of ``enable_shared`` (a Python callback)
waits on the binding upcall mechanism (plan R6); every holder of the secret
counts. Durations are seconds.
"""

from __future__ import annotations

import datetime as _dt
from dataclasses import dataclass
from typing import Literal, Optional, Tuple

from ._aster import NodeId, Topology, TopologySnapshot

__all__ = [
    "ClusterView",
    "LadderLevel",
    "PeerView",
    "SeparationVerdict",
    "Topology",
    "TopologySnapshot",
    "TopologyTiming",
]

#: Locality ladder level; lower is closer.
LadderLevel = Literal["same_host", "same_lan", "same_site", "same_region", "far"]
#: ``separated()`` verdict. ``unknown`` is a real answer, never evidence of
#: distance.
SeparationVerdict = Literal["unknown", "connected", "separated"]


@dataclass(frozen=True)
class PeerView:
    """One peer's proximity view (v1)."""

    node_id: NodeId
    level: LadderLevel
    #: 0 (``same_host``) ... 4 (``far``).
    level_rank: int
    #: The signal that justified the level.
    level_reason: str
    #: Smoothed RTT, microseconds (``None`` until measured).
    rtt_us: Optional[int]
    #: EWMA of |dRTT|, microseconds (``None`` until two samples).
    jitter_us: Optional[int]
    #: Smoothed loss, parts per million of sent datagrams.
    loss_ppm: int
    #: Observed goodput (both directions), bits/s.
    throughput_bps: int
    #: cwnd/RTT ceiling estimate, bits/s.
    cwnd_ceiling_bps: int
    #: The selected path is direct (not relayed).
    direct: bool
    relay_url: Optional[str]
    #: The selected direct path's ``ip:port``.
    remote_addr: Optional[str]
    congestion_events: int
    samples: int
    #: 0 ... 0.99 (grows with samples, decays with staleness).
    confidence: float
    last_measured: _dt.datetime
    #: Since when RTT has held under the cluster threshold (``None`` = not held).
    cluster_held_since: Optional[_dt.datetime]
    is_connected: bool


def _utc_ms(ms: int) -> _dt.datetime:
    return _dt.datetime.fromtimestamp(ms / 1000.0, tz=_dt.timezone.utc)


def _peer_view(
    *, last_measured_unix_ms: int, cluster_held_since_unix_ms: int, **fields
) -> PeerView:
    # Built by the native module.
    return PeerView(
        last_measured=_utc_ms(last_measured_unix_ms),
        cluster_held_since=(
            _utc_ms(cluster_held_since_unix_ms) if cluster_held_since_unix_ms > 0 else None
        ),
        **fields,
    )


@dataclass(frozen=True)
class ClusterView:
    """One derived cluster (v2)."""

    #: Sorted member ids (admitted and live only).
    members: Tuple[NodeId, ...]
    bridge: NodeId
    #: Top members by bridge score (includes the bridge).
    witnesses: Tuple[NodeId, ...]


@dataclass(frozen=True)
class TopologyTiming:
    """Reader timing / coverage rules for ``enable_shared`` (seconds;
    ``None`` keeps the design-doc default)."""

    min_hold: Optional[float] = None
    liveness_ttl: Optional[float] = None
    edge_ttl: Optional[float] = None
    max_skew: Optional[float] = None
    separation_coverage: Optional[int] = None
    witness_r: Optional[int] = None
