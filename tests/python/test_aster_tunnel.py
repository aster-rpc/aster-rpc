"""Tests for the Python tunneling surface.

Wire-level round-trip (reactor `FLAG_TUNNEL` peek inside `accept_bi`,
per-connection registry, TCP splice, replay rejection, per-connection
isolation) is covered at the Rust level by
`core/tests/tunnel_contract.rs`.

This file covers the Python-facing surface:

- Low-level FFI: `authorize_tunnel_tcp`, `authorize_tunnel_many`, and
  client-side ticket validation on `IrohConnection`.
- High-level handler API: the `TunnelHandle` attached to `CallContext`
  (target type dispatch + unsupported-variant errors).

Full E2E through `AsterServer` + `AsterClient` is deferred - exposing
the underlying connection on the client side for redemption needs a
separate small piece of plumbing on `ClientSession`.
"""
from __future__ import annotations

import asyncio

import pytest

from aster import Tcp, create_endpoint
from aster.tunnel import HttpProxy, TunnelHandle, Udp


ALPN = b"test/tunnel/py"


# ── Low-level FFI ────────────────────────────────────────────────────────────


@pytest.mark.asyncio
@pytest.mark.network
async def test_tunnel_authorize_validates_ticket_size():
    """Client-side `open_tunnel` rejects non-32-byte tickets with ValueError."""
    ep_a = await create_endpoint(ALPN)
    ep_b = await create_endpoint(ALPN)
    accept_task = asyncio.ensure_future(ep_a.accept())
    conn = await asyncio.wait_for(ep_b.connect(ep_a.endpoint_id(), ALPN), timeout=5.0)
    _ = await asyncio.wait_for(accept_task, timeout=5.0)

    with pytest.raises(ValueError):
        await conn.open_tunnel(b"too short")


@pytest.mark.asyncio
@pytest.mark.network
async def test_tunnel_authorize_rejects_excessive_ttl():
    """Server-side authorize_tunnel enforces the hard TTL cap (120s)."""
    ep_server = await create_endpoint(ALPN)
    ep_client = await create_endpoint(ALPN)
    accept_task = asyncio.ensure_future(ep_server.accept())
    _ = await asyncio.wait_for(
        ep_client.connect(ep_server.endpoint_id(), ALPN), timeout=5.0
    )
    server_conn = await asyncio.wait_for(accept_task, timeout=5.0)

    with pytest.raises(ValueError):
        # 1 hour > 120s hard cap
        server_conn.authorize_tunnel([("tcp", "127.0.0.1", 9)], 3600)


@pytest.mark.asyncio
@pytest.mark.network
async def test_authorize_tunnel_returns_single_ticket_for_multi_target():
    """A multi-target authorize returns ONE 32-byte ticket, not a list."""
    ep_server = await create_endpoint(ALPN)
    ep_client = await create_endpoint(ALPN)
    accept_task = asyncio.ensure_future(ep_server.accept())
    _ = await asyncio.wait_for(
        ep_client.connect(ep_server.endpoint_id(), ALPN), timeout=5.0
    )
    server_conn = await asyncio.wait_for(accept_task, timeout=5.0)

    targets = [
        ("tcp", "127.0.0.1", 5900),
        ("tcp", "127.0.0.1", 22),
        ("tcp", "127.0.0.1", 8080),
    ]
    ticket = server_conn.authorize_tunnel(targets, 30)
    assert isinstance(ticket, (bytes, bytearray))
    assert len(ticket) == 32


@pytest.mark.asyncio
@pytest.mark.network
async def test_authorize_tunnel_rejects_unknown_kind():
    """The FFI raises NotImplementedError for non-TCP target kinds."""
    ep_server = await create_endpoint(ALPN)
    ep_client = await create_endpoint(ALPN)
    accept_task = asyncio.ensure_future(ep_server.accept())
    _ = await asyncio.wait_for(
        ep_client.connect(ep_server.endpoint_id(), ALPN), timeout=5.0
    )
    server_conn = await asyncio.wait_for(accept_task, timeout=5.0)

    with pytest.raises(NotImplementedError):
        server_conn.authorize_tunnel([("udp", "127.0.0.1", 53)], 30)


@pytest.mark.asyncio
@pytest.mark.network
async def test_authorize_tunnel_rejects_empty_targets():
    """The FFI rejects empty target lists with ValueError."""
    ep_server = await create_endpoint(ALPN)
    ep_client = await create_endpoint(ALPN)
    accept_task = asyncio.ensure_future(ep_server.accept())
    _ = await asyncio.wait_for(
        ep_client.connect(ep_server.endpoint_id(), ALPN), timeout=5.0
    )
    server_conn = await asyncio.wait_for(accept_task, timeout=5.0)

    with pytest.raises(ValueError):
        server_conn.authorize_tunnel([], 30)


# ── High-level TunnelHandle ──────────────────────────────────────────────────


@pytest.mark.asyncio
@pytest.mark.network
async def test_tunnel_handle_authorize_tcp_returns_bytes():
    """`TunnelHandle.authorize(Tcp(...))` returns a single 32-byte ticket."""
    ep_server = await create_endpoint(ALPN)
    ep_client = await create_endpoint(ALPN)
    accept_task = asyncio.ensure_future(ep_server.accept())
    _ = await asyncio.wait_for(
        ep_client.connect(ep_server.endpoint_id(), ALPN), timeout=5.0
    )
    server_conn = await asyncio.wait_for(accept_task, timeout=5.0)

    handle = TunnelHandle(server_conn)

    # Single target.
    ticket = handle.authorize(Tcp("127.0.0.1", 5900), ttl_secs=10)
    assert isinstance(ticket, (bytes, bytearray))
    assert len(ticket) == 32

    # Multi-target preference list - still one ticket.
    ticket = handle.authorize(
        [Tcp("127.0.0.1", 5900), Tcp("127.0.0.1", 22)],
        ttl_secs=10,
    )
    assert isinstance(ticket, (bytes, bytearray))
    assert len(ticket) == 32


def test_tunnel_handle_rejects_udp_until_acceptor_ships():
    handle = TunnelHandle(_NeverCalledConn())
    with pytest.raises(NotImplementedError):
        handle.authorize(Udp("10.0.0.5", 53))


def test_tunnel_handle_rejects_http_proxy_until_acceptor_ships():
    handle = TunnelHandle(_NeverCalledConn())
    with pytest.raises(NotImplementedError):
        handle.authorize(HttpProxy("10.0.0.5", 8080, host_header="local.dev"))


def test_tunnel_handle_rejects_unknown_target_type():
    handle = TunnelHandle(_NeverCalledConn())
    with pytest.raises(TypeError):
        handle.authorize("not-a-target")  # type: ignore[arg-type]


def test_tunnel_handle_rejects_empty_targets():
    handle = TunnelHandle(_NeverCalledConn())
    with pytest.raises(ValueError):
        handle.authorize([])


class _NeverCalledConn:
    """Stand-in for `IrohConnection`. Any reach into the binding from
    rejection paths fails the test."""

    def authorize_tunnel(self, *args, **kwargs):  # pragma: no cover
        raise AssertionError(
            "TunnelHandle should reject this case before reaching the FFI"
        )
