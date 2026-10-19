"""
Tier-2 chaos tests for the multiplexed-streams binding layer (Python port).

The session rules themselves (graveyard, per-connection cap, per-connection
keying, reap on close) live in the Rust dispatcher and are tested once in
``aster/tests/it/rpc_sessions.rs``:
``a_session_id_at_or_below_last_opened_that_is_not_active_is_not_found``,
``a_new_session_over_the_cap_is_resource_exhausted_without_bumping_last_opened``,
``a_second_connection_cannot_reach_another_connections_session``,
``closing_a_connection_reaps_its_sessions_and_only_its_sessions`` and
``sessions_are_per_connection_and_reaped_on_close_over_iroh``.

What stays here is the Python side of that wiring: per-session instances
built and kept by ``aster._dispatch`` (a handler exception does not drop the
instance), the ``max_sessions_per_connection`` config reaching Rust and its
``RESOURCE_EXHAUSTED`` mapping, the ``close()`` hook on reap, and
``ClientSession.client()`` for session-scoped classes.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass

import pytest

from aster import (
    AsterClient,
    AsterServer,
    rpc,
    service,
    wire_type,
)
from aster.codec import ForyCodec
from aster.rpc_types import SerializationMode
from aster.runtime import ClientSession
from aster.status import RpcError, StatusCode
from aster.transport.iroh import IrohTransport


def _make_codec() -> ForyCodec:
    """Build a codec that knows about every chaos wire type. Chaos
    tests use this with an IrohTransport built directly on the
    session's native connection; ``ClientSession.client()`` also
    works for session-scoped classes now (see the regression test
    below) but the direct-transport path gives finer control over
    which error maps to which assertion.
    """
    return ForyCodec(
        mode=SerializationMode.XLANG,
        types=[BumpRequest, BumpResponse, FailRequest, FailResponse],
    )


def _session_transport(session: ClientSession) -> IrohTransport:
    """Build an IrohTransport bound to this ClientSession's
    `(connection, session_id)` so unary calls route through the
    per-connection multiplexed-stream pool with the right session id.
    """
    return IrohTransport(
        connection=session.connection,
        codec=_make_codec(),
        session_id=session.session_id,
    )


# ── Test wire types ─────────────────────────────────────────────────────────


@wire_type("test.chaos/BumpRequest")
@dataclass
class BumpRequest:
    message: str = ""


@wire_type("test.chaos/BumpResponse")
@dataclass
class BumpResponse:
    reply: str = ""


@wire_type("test.chaos/FailRequest")
@dataclass
class FailRequest:
    message: str = ""


@wire_type("test.chaos/FailResponse")
@dataclass
class FailResponse:
    reply: str = ""


# ── Chaos service: session-scoped with a counter and a throwing method -----


@service(name="ChaosSession", version=1, scoped="session")
class ChaosSessionService:
    """Session-scoped service with per-instance state.

    - ``counter`` -- bumped on each ``bump`` call; proves per-session
      isolation (a throw on one session must not reset another's).
    - ``on_close_fired`` / ``live_instances`` -- class-level counters
      observed by the tests to assert reap semantics.
    """

    on_close_fired = 0
    live_instances = 0

    def __init__(self, peer: str = "") -> None:
        self.peer = peer
        self.counter = 0
        ChaosSessionService.live_instances += 1

    # Note: Python's server reaps session instances by calling `close()`
    # on them (see `Server._on_reactor_connection_closed`). TypeScript
    # and Java use `onClose()` -- this cross-language convention
    # mismatch is a real binding gap we document via this test.
    async def close(self) -> None:
        ChaosSessionService.on_close_fired += 1
        ChaosSessionService.live_instances -= 1

    @rpc()
    async def bump(self, req: BumpRequest) -> BumpResponse:
        self.counter += 1
        return BumpResponse(reply=f"{req.message}:{self.counter}")

    @rpc()
    async def fail(self, _req: FailRequest) -> FailResponse:
        raise RuntimeError("chaos/expected-throw")


# ── Helpers ─────────────────────────────────────────────────────────────────


async def _wait_for(predicate, timeout_s: float, step_s: float = 0.05) -> bool:
    deadline = asyncio.get_event_loop().time() + timeout_s
    while asyncio.get_event_loop().time() < deadline:
        if predicate():
            return True
        await asyncio.sleep(step_s)
    return predicate()


def _reset_chaos_state() -> None:
    ChaosSessionService.on_close_fired = 0
    ChaosSessionService.live_instances = 0


# ── 1. Session wiring: instances, cap config, reap on connection close ─────


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_session_instances_cap_and_reap_on_connection_close():
    _reset_chaos_state()
    async with AsterServer(
        services=[ChaosSessionService],
        max_sessions_per_connection=3,
    ) as server:
        addr_b64 = server.endpoint_addr_b64
        async with AsterClient(endpoint_addr=addr_b64) as client:
            # Open 3 sessions, drive one call on each so they're
            # materialised server-side.
            # Note: we build one IrohTransport per session but do NOT
            # call `transport.close()` -- that closes the underlying
            # QUIC connection. The connection is owned by the
            # `async with AsterClient` context.
            sessions = []
            for i in range(3):
                s = await client.open_session()
                transport = _session_transport(s)
                resp = await transport.unary(
                    "ChaosSession", "bump", BumpRequest(message=f"s{i}")
                )
                assert resp.reply == f"s{i}:1"
                sessions.append(s)

            # A handler exception maps to RpcError and keeps the instance.
            t0 = _session_transport(sessions[0])
            with pytest.raises(RpcError):
                await t0.unary("ChaosSession", "fail", FailRequest(message="s0"))
            resp = await t0.unary("ChaosSession", "bump", BumpRequest(message="s0"))
            assert resp.reply == "s0:2"

            # The configured cap reaches Rust: a fourth session is refused.
            extra = await client.open_session()
            with pytest.raises(RpcError) as over:
                await _session_transport(extra).unary(
                    "ChaosSession", "bump", BumpRequest(message="s3")
                )
            assert over.value.code == StatusCode.RESOURCE_EXHAUSTED

            # Pre-close: 1 connection, 3 active sessions.
            snap = server.debug_connection_snapshot()
            assert len(snap) == 1
            (conn_info,) = snap.values()
            assert conn_info["active_session_count"] == 3

            # Close client sessions then the client -- the latter is
            # what tears down the QUIC connection; session.close() is a
            # no-op on the wire per the class docstring.
            for s in sessions:
                await s.close()

        # Leaving the `async with AsterClient` closes the connection.
        reaped = await _wait_for(
            lambda: len(server.debug_connection_snapshot()) == 0,
            timeout_s=5.0,
        )
        assert reaped, "server did not reap connection state within 5s"
        assert len(server.debug_connection_snapshot()) == 0

        # on_close fired 3 times, one per session, and every instance
        # was reaped.
        assert ChaosSessionService.on_close_fired == 3
        assert ChaosSessionService.live_instances == 0


# ── 2. Handler exception isolation ─────────────────────────────────────────


# ── 3. Graveyard enforcement under out-of-order arrival ────────────────────


# ── 4a. Session cap, sequenced ─────────────────────────────────────────────


# ── 4b. Session cap, concurrent burst ──────────────────────────────────────


# ── 5. Cross-connection session id isolation ──────────────────────────────


# ── 6. ClientSession.client() works for session-scoped classes ────────────


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_client_session_client_accepts_session_scoped_classes():
    """Regression guard for the Python ``create_client`` bug: a
    ``ClientSession.client(SessionScopedCls)`` call must return a
    typed stub and every invocation through it must route through
    the session's bound transport.

    Pre-fix, ``ClientSession.client`` called ``create_client`` which
    unconditionally raised ``ClientError`` for any session-scoped
    class, silently breaking the ``examples/python/session_chat.py``
    path. The fix (client.py) skips the scope check when a
    pre-built transport is supplied because
    ``ClientSession.client`` builds an ``IrohTransport(session_id=...)``
    before handing it off.
    """
    _reset_chaos_state()
    async with AsterServer(
        services=[ChaosSessionService],
    ) as server:
        addr_b64 = server.endpoint_addr_b64
        async with AsterClient(endpoint_addr=addr_b64) as client:
            session = await client.open_session()
            # This call used to raise
            # `ClientError: ChaosSessionService is session-scoped ...`.
            stub = await session.client(ChaosSessionService, codec=_make_codec())
            r1 = await stub.bump(BumpRequest(message="stubbed"))
            r2 = await stub.bump(BumpRequest(message="stubbed"))
            assert r1.reply == "stubbed:1"
            assert r2.reply == "stubbed:2"
            await session.close()
