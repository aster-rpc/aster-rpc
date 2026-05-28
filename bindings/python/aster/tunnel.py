"""Aster tunneling - handler-side ticket issuance.

An RPC handler that has validated a peer's tunnel request calls
``ctx.tunnel.authorize(...)`` to mint a capability ticket. The ticket
is 32 opaque bytes; the handler embeds it in its RPC response and the
peer redeems with ``conn.open_tunnel(ticket)``.

Target shape - tagged-union, one type per protocol::

    from aster.tunnel import Tcp, HttpProxy

    # Single target - the simplest case.
    ticket = ctx.tunnel.authorize(Tcp("10.0.0.5", 5900))

    # Ordered preference list. The acceptor tries them in order at
    # redeem and splices the first one that connects. Useful for
    # primary/standby backends or per-protocol fallback.
    ticket = ctx.tunnel.authorize(
        [
            Tcp("10.0.0.5", 5900),                 # primary VNC
            Tcp("10.0.0.6", 5900),                 # standby VNC
            HttpProxy("10.0.0.5", 8080, host_header="local.dev"),
        ],
        ttl_secs=30,
    )

One call → one ticket - regardless of how many targets are listed. The
ticket is one-shot at redeem; on redemption the server picks the first
reachable target and splices, dropping the rest. Per-connection cap
counts each ticket as one entry, not one entry per target.

v1 only supports :class:`Tcp`. :class:`Udp` and :class:`HttpProxy` are
defined here so call sites can pass them through, but the underlying
core rejects them with ``NotImplementedError`` until acceptors land
(see ``ffi_spec/Aster-tunneling.md`` §11).
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Iterable, Sequence, Union

if TYPE_CHECKING:
    from aster._aster import IrohConnection


@dataclass(frozen=True)
class Tcp:
    """Raw TCP tunnel target - bytes after redemption splice straight
    to ``host:port`` on the connector."""

    host: str
    port: int


@dataclass(frozen=True)
class Udp:
    """UDP tunnel target. **Not yet supported in v1** - passing this to
    :meth:`TunnelHandle.authorize` raises ``NotImplementedError``.
    See ``ffi_spec/Aster-tunneling.md`` §11."""

    host: str
    port: int


@dataclass(frozen=True)
class HttpProxy:
    """HTTP-aware tunnel target - the connector parses requests and
    rewrites ``Host``/``Origin`` headers before forwarding. **Not yet
    supported in v1**; raises ``NotImplementedError``."""

    host: str
    port: int
    host_header: str | None = None
    origin_header: str | None = None


TunnelTarget = Union[Tcp, Udp, HttpProxy]
"""Type alias for any tunnel target. Add new variants here as protocols ship."""


class TunnelHandle:
    """Handler-facing tunnel API. Held on :class:`CallContext` as
    ``ctx.tunnel`` and bound to the RPC's underlying connection - so
    tickets minted here are redeemable only on that connection.

    Don't construct directly; the server dispatcher attaches it.
    """

    __slots__ = ("_connection",)

    def __init__(self, connection: "IrohConnection") -> None:
        self._connection = connection

    def authorize(
        self,
        targets: TunnelTarget | Sequence[TunnelTarget],
        *,
        ttl_secs: int = 30,
    ) -> bytes:
        """Mint a single ticket covering one or more targets.

        Args:
            targets: A single :class:`Tcp` / :class:`Udp` / :class:`HttpProxy`
                or any iterable of them. When a list is passed, the
                order is treated as a preference: the acceptor tries
                each target at redeem and splices the first reachable
                one. Empty lists are rejected.
            ttl_secs: Ticket lifetime in seconds. ``0`` falls back to
                the node default (30s). The hard cap is 120s.

        Returns:
            32 raw bytes - the opaque capability for the handler's RPC
            response.

        Raises:
            ValueError: TTL is above the node hard cap, the
                per-connection cap is exceeded, or the targets list
                is empty.
            NotImplementedError: A target uses a variant not supported
                in v1 (UDP / HttpProxy).
            TypeError: An entry is not a recognised tunnel-target type.
        """
        items: list[TunnelTarget] = list(_iter_targets(targets))
        if not items:
            raise ValueError("authorize requires at least one target")
        wire: list[tuple[str, str, int]] = []
        for t in items:
            if isinstance(t, Tcp):
                wire.append(("tcp", t.host, t.port))
            elif isinstance(t, Udp):
                raise NotImplementedError(
                    "UDP tunnels not supported in v1; see Aster-tunneling.md §11"
                )
            elif isinstance(t, HttpProxy):
                raise NotImplementedError(
                    "HTTP-proxy tunnels not supported in v1; see Aster-tunneling.md §11"
                )
            else:
                raise TypeError(
                    f"unknown tunnel target type: {type(t).__name__}"
                )
        return self._connection.authorize_tunnel(wire, ttl_secs)


def _iter_targets(
    targets: TunnelTarget | Sequence[TunnelTarget] | Iterable[TunnelTarget],
) -> Iterable[TunnelTarget]:
    if isinstance(targets, (Tcp, Udp, HttpProxy)):
        yield targets
        return
    yield from targets
