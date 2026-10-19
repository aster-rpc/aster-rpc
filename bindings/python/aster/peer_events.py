"""Which peers are connected to an :class:`~aster.AsterServer`, and when that
changes (``AsterServer.peer_events()``; semantics in Rust
``aster::rpc::peers``).

Events are per peer, not per connection: ``PeerConnected`` on a peer's first
admitted ``aster/1`` connection (past the node's gates, so a peer enrollment
or a drain refuses never appears), ``PeerDisconnected`` when its last one
ends. A consumer that falls behind the bounded queue (1024 events) gets one
``PeerLagged``; its recovery is to call ``peer_events()`` again and rebuild
from the new snapshot. Connections are never held up by a slow consumer.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Literal, Union

__all__ = [
    "PeerIdentity",
    "PeerSnapshot",
    "PeerConnected",
    "PeerDisconnected",
    "PeerLagged",
    "PeerEvent",
    "PeerEvents",
    "DisconnectReason",
]

DisconnectReason = Literal["closed", "timed_out", "revoked", "drained", "error"]


@dataclass(frozen=True)
class PeerIdentity:
    """Who a connected peer is, as verified when its first connection was
    admitted."""

    #: The peer's verified endpoint id.
    endpoint_id: str
    #: The principal its credential names; ``None`` unless the peer has a live
    #: enrollment session.
    principal: str | None = None
    #: Roles its credential grants (sorted); empty without enrollment.
    roles: tuple[str, ...] = ()


def _identity(raw: dict[str, Any]) -> PeerIdentity:
    return PeerIdentity(
        endpoint_id=raw["endpoint_id"],
        principal=raw["principal"],
        roles=tuple(raw["roles"]),
    )


@dataclass(frozen=True)
class PeerSnapshot:
    """A peer connected when ``peer_events()`` was called."""

    peer: str
    #: ALPN of its first admitted connection (``"aster/1"``).
    alpn: str
    connections: int
    identity: PeerIdentity


@dataclass(frozen=True)
class PeerConnected:
    """A peer's first admitted connection opened."""

    peer: str
    alpn: str
    identity: PeerIdentity


@dataclass(frozen=True)
class PeerDisconnected:
    """A peer's last connection ended; ``reason`` is that connection's."""

    peer: str
    reason: DisconnectReason


@dataclass(frozen=True)
class PeerLagged:
    """The stream fell behind its bounded queue and ``missed`` events were
    dropped. The consumer's state is stale: call ``peer_events()`` again."""

    missed: int


PeerEvent = Union[PeerConnected, PeerDisconnected, PeerLagged]


def _snapshot(raw: list[dict[str, Any]]) -> list[PeerSnapshot]:
    return [
        PeerSnapshot(
            peer=p["peer"],
            alpn=p["alpn"],
            connections=p["connections"],
            identity=_identity(p["identity"]),
        )
        for p in raw
    ]


class PeerEvents:
    """Async iterator of :data:`PeerEvent` from the moment ``peer_events()``
    returned it. Ends when closed or the server ends. Cancelling a pending
    ``__anext__`` consumes nothing."""

    def __init__(self, native: Any) -> None:
        self._native = native

    def __aiter__(self) -> "PeerEvents":
        return self

    async def __anext__(self) -> PeerEvent:
        raw = await self._native.next()
        if raw is None:
            raise StopAsyncIteration
        kind = raw["type"]
        if kind == "connected":
            return PeerConnected(
                peer=raw["peer"], alpn=raw["alpn"], identity=_identity(raw["identity"])
            )
        if kind == "disconnected":
            return PeerDisconnected(peer=raw["peer"], reason=raw["reason"])
        if kind == "lagged":
            return PeerLagged(missed=raw["missed"])
        raise RuntimeError(f"unknown peer event type {kind!r}")

    def close(self) -> None:
        """Stop the stream; a pending and every later ``__anext__`` ends it."""
        self._native.close()


def _open(native: Any) -> tuple[list[PeerSnapshot], PeerEvents]:
    raw, stream = native.peer_events()
    return _snapshot(raw), PeerEvents(stream)
