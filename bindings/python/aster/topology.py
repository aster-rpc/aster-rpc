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

Without an ``admitted`` filter every holder of the secret counts. With one,
records whose author it rejects (and vertices it rejects) are ignored::

    flt = AdmittedFilter(lambda node_id: node_id in allowed)   # or async def
    await flt.prefetch(known_ids)                              # optional warm-up
    await topo.enable_shared(secret, admitted=flt)
    flt.set(revoked_id, False)                                 # push a revocation

The filter is consulted synchronously on Rust's read path (every view
derivation, at most once a second), so it never waits for Python: it answers
from a cache filled by **batched** upcalls into your predicate (sync or
async, it makes no difference to Rust). A fresh entry is a hit; a stale one
(older than ``ttl``) answers the cached value and is refreshed in the
background; an unknown id gets the ``topology_admitted`` failure-mode answer
(fail-closed: filtered) until its batch is answered -- in practice a new
producer counts from the next view (~1 s). A revocation takes effect after
``ttl`` unless you push it with :meth:`AdmittedFilter.set` /
:meth:`AdmittedFilter.invalidate`. A failed batch caches the fallback for
``failure_retry`` and never overwrites a real cached answer.

A placement :class:`aster.coordination.TopologySource` returns a
:class:`TopologyView` (the serialized form of a view);
:meth:`TopologySnapshot.from_view` builds the same snapshot Rust does.
Durations are seconds.
"""

from __future__ import annotations

import asyncio
import datetime as _dt
import inspect
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Iterable, Literal, Optional, Tuple

from ._aster import NodeId, Topology, TopologySnapshot, _AdmittedFilter

if TYPE_CHECKING:  # pragma: no cover
    from .upcalls import UpcallRuntime

__all__ = [
    "AdmittedCacheStats",
    "AdmittedFilter",
    "ClusterView",
    "LadderLevel",
    "PeerBlobStats",
    "PeerView",
    "QuicBytes",
    "QuicTraffic",
    "SeparationVerdict",
    "Topology",
    "TopologySnapshot",
    "TopologyTiming",
    "TopologyView",
]

#: Locality ladder level; lower is closer.
LadderLevel = Literal["same_host", "same_lan", "same_site", "same_region", "far"]
#: ``separated()`` verdict. ``unknown`` is a real answer, never evidence of
#: distance.
SeparationVerdict = Literal["unknown", "connected", "separated"]


@dataclass(frozen=True)
class QuicBytes:
    """QUIC-level bytes on a peer's connections: UDP datagram payload, so QUIC
    headers, ACKs and retransmits are included (reads somewhat above the
    application payload)."""

    quic_bytes_sent: int = 0
    quic_bytes_received: int = 0


@dataclass(frozen=True)
class QuicTraffic:
    """A peer's QUIC bytes by protocol (one connection per ALPN), cumulative
    since this node started and across reconnects. Monotonic."""

    #: Plain blobs transfers, including docs entry content.
    blobs: QuicBytes = QuicBytes()
    #: zstd blobs transfers.
    blobs_zstd: QuicBytes = QuicBytes()
    #: The docs sync protocol (entries, not their content).
    docs: QuicBytes = QuicBytes()
    gossip: QuicBytes = QuicBytes()
    #: Aster RPC (``aster/1``), every service on one connection.
    rpc: QuicBytes = QuicBytes()
    #: Anything else: admission, enrollment, custom ALPNs.
    other: QuicBytes = QuicBytes()

    def total(self) -> QuicBytes:
        """The sum over every protocol."""
        parts = (self.blobs, self.blobs_zstd, self.docs, self.gossip, self.rpc, self.other)
        return QuicBytes(
            quic_bytes_sent=sum(p.quic_bytes_sent for p in parts),
            quic_bytes_received=sum(p.quic_bytes_received for p in parts),
        )


@dataclass(frozen=True)
class PeerBlobStats:
    """Exact blob payload (and zstd wire) bytes exchanged with one peer, since
    this node started and across reconnects. Monotonic. Fetch figures are
    counted when each attempt ends."""

    #: Payload bytes fetched from this peer, plain and zstd.
    fetched_payload_bytes: int = 0
    #: The part of ``fetched_payload_bytes`` that came over zstd.
    fetched_zstd_payload_bytes: int = 0
    #: Compressed bytes received from this peer while fetching over zstd.
    fetched_wire_bytes: int = 0
    #: Protocol bytes those decompressed to.
    fetched_plain_bytes: int = 0
    #: Payload bytes served to this peer, plain and zstd.
    served_payload_bytes: int = 0
    #: The part of ``served_payload_bytes`` that went out over zstd.
    served_zstd_payload_bytes: int = 0
    #: Compressed bytes sent to this peer while serving over zstd.
    served_wire_bytes: int = 0
    #: Protocol bytes those were compressed from.
    served_plain_bytes: int = 0
    #: Fetch attempts against this peer that fell back to plain transport.
    plain_fallbacks: int = 0


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
    #: QUIC bytes on this peer's connections, by protocol.
    quic_traffic: QuicTraffic = QuicTraffic()
    #: Blob payload / wire bytes exchanged with this peer.
    blob_transfer: PeerBlobStats = PeerBlobStats()


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


@dataclass(frozen=True)
class TopologyView:
    """A serialized shared view -- what a placement ``TopologySource``
    returns: the clusters, the corroborated far pairs (node id pairs, any
    order), and the separation coverage the reader required."""

    clusters: Tuple[ClusterView, ...] = ()
    far_pairs: Tuple[Tuple[NodeId, NodeId], ...] = ()
    separation_coverage: int = 0


@dataclass(frozen=True)
class AdmittedCacheStats:
    """:class:`AdmittedFilter` counters."""

    #: Lookups answered from a fresh entry.
    hits: int
    #: Lookups answered from a stale entry (refresh queued).
    stale: int
    #: Lookups with no entry (failure-mode answer, resolution queued).
    misses: int
    #: Batched upcalls issued.
    upcalls: int


class AdmittedFilter:
    """The read-time topology admission filter for ``enable_shared``,
    answered by ``predicate`` -- ``(NodeId) -> bool | Awaitable[bool]``, or
    an object with ``admitted_batch(node_ids) -> Sequence[bool]`` (sync or
    async) to answer a whole batch at once. Cache semantics: see the module
    docstring. ``ttl`` / ``failure_retry`` are seconds; ``max_batch`` caps
    ids per upcall."""

    def __init__(
        self,
        predicate: Any,
        *,
        ttl: float = 30.0,
        failure_retry: float = 1.0,
        max_batch: int = 256,
        runtime: Optional["UpcallRuntime"] = None,
    ) -> None:
        self.predicate = predicate
        self.upcall_runtime = runtime
        self._native = _AdmittedFilter(
            self, ttl=ttl, failure_retry=failure_retry, max_batch=max_batch
        )

    async def _admitted_batch(self, node_ids: Tuple[NodeId, ...]) -> Tuple[bool, ...]:
        batch = getattr(self.predicate, "admitted_batch", None)
        if callable(batch):
            answers = batch(node_ids)
            if inspect.isawaitable(answers):
                answers = await answers
            return tuple(answers)
        answers = [self.predicate(n) for n in node_ids]
        pending = [a for a in answers if inspect.isawaitable(a)]
        if pending:
            done = iter(await asyncio.gather(*pending))
            answers = [next(done) if inspect.isawaitable(a) else a for a in answers]
        return tuple(answers)

    def is_admitted(self, node_id) -> bool:
        """Look ``node_id`` up exactly as Rust's read path does (never
        waits; a miss queues a resolution)."""
        return self._native.is_admitted(node_id)

    async def prefetch(self, node_ids: Iterable) -> None:
        """Resolve ``node_ids`` now with one upcall and cache the answers;
        raises :class:`aster.upcalls.UpcallError` if the batch failed."""
        await self._native.prefetch(list(node_ids))

    def set(self, node_id, admitted: bool) -> None:
        """Push an answer (e.g. on revocation); served for ``ttl``."""
        self._native.set(node_id, bool(admitted))

    def invalidate(self, node_id) -> None:
        """Forget ``node_id``; the next lookup re-resolves."""
        self._native.invalidate(node_id)

    def clear(self) -> None:
        """Forget every id."""
        self._native.clear()

    def stats(self) -> AdmittedCacheStats:
        return AdmittedCacheStats(*self._native.stats())


def _as_admitted_filter(obj: Any) -> _AdmittedFilter:
    # Called by ``Topology.enable_shared(admitted=...)``.
    if isinstance(obj, AdmittedFilter):
        return obj._native
    if not callable(obj) and not callable(getattr(obj, "admitted_batch", None)):
        raise TypeError(
            "admitted must be an AdmittedFilter, a predicate (NodeId) -> bool, or an "
            "object with admitted_batch()"
        )
    return AdmittedFilter(obj)._native
