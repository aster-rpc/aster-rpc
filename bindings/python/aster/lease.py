"""Single-owner fenced leases (Rust ``aster::lease``).

The *grant* is a cheap CAS at a serializer plus a TTL; correctness comes from
the *fence*: every protected action carries ``(resource, epoch)`` and the
resource rejects anything that is not the current epoch from the current
holder::

    serializer = MemorySerializer()
    outcome = await serializer.acquire("tree/42", node.id, LeaseOptions(ttl=30))
    if outcome.granted:
        async with outcome.lease as lease:        # released on exit
            fence = lease.fence()                 # take at send time; None = stop
            if fence:
                serializer.mutate_fenced(fence, node.id, write)
    else:
        ...  # back off; outcome.snapshot.remaining is a hint (seconds)

Renewal runs in Rust; :meth:`Lease.fence` returns ``None`` once the holder
must stop (released, fenced, lost, or past its self-limit). Durations are
seconds.

Backends: :class:`MemorySerializer` (in-process reference) and
:class:`GrantorSerializer` (a remote designated grantor,
``await node.lease_grantor(grantor_id)``; serve one with
``node.serve_lease_grantor(serializer)``). Python-implemented serializers and
fenced resources wait on the binding upcall mechanism (plan R6); meanwhile
:func:`lease_transition` and :func:`check_fence` give a Python-owned resource
the same rules to apply under its own lock.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal, Optional, Union

from ._aster import (
    GrantorSerializer,
    Lease,
    LeaseGrantorServer,
    LeaseStatusStream,
    MemorySerializer,
    NodeId,
    _check_fence,
    _lease_transition,
)

__all__ = [
    "AcquireOutcome",
    "DenyReason",
    "Fence",
    "FenceRejectedError",
    "FenceRejectionKind",
    "GrantorSerializer",
    "Lease",
    "LeaseGrantorServer",
    "LeaseOp",
    "LeaseOptions",
    "LeaseRowState",
    "LeaseSnapshot",
    "LeaseStatus",
    "LeaseStatusStream",
    "LeaseTransition",
    "MemorySerializer",
    "TransitionOutcome",
    "check_fence",
    "lease_transition",
]

#: Why a transition was refused.
DenyReason = Literal[
    "held_by_other", "epoch_mismatch", "not_holder", "expired", "not_held", "unauthorized"
]
#: Holder-side lease status.
LeaseStatus = Literal["held", "released", "fenced", "lost"]
#: Why the resource rejected a fenced action.
FenceRejectionKind = Literal["stale_epoch", "not_holder", "not_held", "future_epoch"]

Actor = Union[NodeId, str]


@dataclass(frozen=True)
class Fence:
    """The fencing token for a protected action. ``holder`` is the id the
    epoch was issued to -- bind it to the *authenticated* writer."""

    resource: str
    epoch: int
    holder: str


@dataclass(frozen=True)
class LeaseSnapshot:
    """Advisory view of a serializer row (stale the moment it returns)."""

    resource: str
    epoch: int
    holder: Optional[str] = None
    held: bool = False
    #: Serializer-local hint of seconds until expiry; ``None`` when free.
    remaining: Optional[float] = None


@dataclass(frozen=True)
class LeaseOptions:
    """Holder-side timing, seconds. ``renew_interval`` must fit inside
    ``ttl - holder_margin``."""

    #: Serializer-side expiry interval.
    ttl: float = 30.0
    #: Renew cadence.
    renew_interval: float = 10.0
    #: Subtracted from ``ttl`` for the holder's self-limit.
    holder_margin: float = 1.0


@dataclass(frozen=True)
class AcquireOutcome:
    """The result of an acquire attempt: ``lease`` when ``granted``, else
    ``reason`` and ``snapshot`` (back off; ``snapshot.remaining`` is a
    hint)."""

    granted: bool
    lease: Optional[Lease] = None
    reason: Optional[DenyReason] = None
    snapshot: Optional[LeaseSnapshot] = None


@dataclass(frozen=True)
class TransitionOutcome:
    """The result of one transition at a serializer."""

    kind: Literal["granted", "renewed", "released", "revoked", "denied"]
    #: ``granted``: the issued fence.
    fence: Optional[Fence] = None
    #: ``denied``: why.
    deny_reason: Optional[DenyReason] = None
    #: ``denied``: the row after the decision.
    snapshot: Optional[LeaseSnapshot] = None


@dataclass(frozen=True)
class LeaseRowState:
    """The durable fence fields of a serializer row (``ttl`` in seconds)."""

    epoch: int = 0
    holder: Optional[str] = None
    held: bool = False
    ttl: float = 0.0


@dataclass(frozen=True)
class LeaseOp:
    """A requested transition; ``actor`` is the candidate (``acquire``),
    holder (``renew`` / ``release``) or authority (``revoke``)."""

    kind: Literal["acquire", "renew", "release", "revoke"]
    actor: str
    #: ``renew`` / ``release``.
    epoch: Optional[int] = None
    #: ``acquire``, seconds.
    ttl: Optional[float] = None

    @classmethod
    def acquire(cls, actor: Actor, ttl: float) -> "LeaseOp":
        return cls("acquire", str(actor), ttl=ttl)

    @classmethod
    def renew(cls, actor: Actor, epoch: int) -> "LeaseOp":
        return cls("renew", str(actor), epoch=epoch)

    @classmethod
    def release(cls, actor: Actor, epoch: int) -> "LeaseOp":
        return cls("release", str(actor), epoch=epoch)

    @classmethod
    def revoke(cls, actor: Actor) -> "LeaseOp":
        return cls("revoke", str(actor))


@dataclass(frozen=True)
class LeaseTransition:
    """What the pure state machine decided: ``apply`` (store ``next`` in one
    CAS, restart the expiry anchor when ``reset_grant``) or ``deny``."""

    kind: Literal["apply", "deny"]
    next: Optional[LeaseRowState] = None
    outcome: Optional[TransitionOutcome] = None
    reset_grant: Optional[bool] = None
    deny_reason: Optional[DenyReason] = None


class FenceRejectedError(Exception):
    """A protected action was rejected by the fence. Never routine -- surface
    it (metric + audit); never retry silently. ``future_epoch`` is a
    serializer-integrity alarm."""

    def __init__(
        self,
        kind: FenceRejectionKind,
        message: str,
        carried: Optional[int] = None,
        current: Optional[int] = None,
    ) -> None:
        super().__init__(message)
        self.kind = kind
        self.carried = carried
        self.current = current

    def __repr__(self) -> str:
        return (
            f"FenceRejectedError(kind={self.kind!r}, carried={self.carried}, "
            f"current={self.current})"
        )


def lease_transition(
    row: LeaseRowState, expired: bool, op: LeaseOp, resource: str
) -> LeaseTransition:
    """The normative lease state machine as a pure function -- for a resource
    that is its own serialization point: read the row, call this, apply
    ``next`` in the same transaction. ``expired`` is judged on the backend's
    own clock."""
    return _lease_transition(row, bool(expired), op, resource)


def check_fence(
    row: LeaseRowState, fence: Fence, writer: Actor
) -> Optional[FenceRejectedError]:
    """The fence rule (``epoch == row.epoch and writer == row.holder and
    row.held``) against ``row``. Returns the rejection (not raised), or
    ``None`` when ``writer`` may act. Evaluate it inside the resource's own
    serialization point."""
    return _check_fence(row, fence, writer)
