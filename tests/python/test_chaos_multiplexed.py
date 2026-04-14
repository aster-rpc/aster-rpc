"""
Tier-2 chaos tests for the multiplexed-streams binding layer (Python port).

Mirrors ``tests/typescript/integration/chaos-multiplexed.test.ts`` and
``bindings/java/.../ChaosMultiplexedTest.java``. Every invariant pinned
here also has a sibling test in the TS and Java suites -- regressions
in one binding would show up in the others too if the chaos shape is
common enough, but having one copy per binding catches
binding-specific wire/encoding bugs that core can't see.

Coverage:

1. Session reap on connection close -- after the client closes, the
   server's per-connection state map must be empty (spec Sec. 7.5).
2. Handler exception isolation -- an exception on session A must not
   poison session A's instance for subsequent calls, and must not
   leak state to session B.
3. Graveyard enforcement (Sec. 7.5) under out-of-order arrival --
   session 2 used before session 1 must cause session 1 to be
   rejected with ``NOT_FOUND``.
4a. Session cap, sequenced -- exactly ``CAP`` succeed; rest fail with
    ``RESOURCE_EXHAUSTED``.
4b. Session cap under concurrent burst -- at most ``CAP`` succeed;
    rejections are either ``NOT_FOUND`` (graveyard race) or
    ``RESOURCE_EXHAUSTED`` (cap).
5. Cross-connection session id isolation -- the same ``sessionId`` on
   two distinct connections must resolve to two distinct server-side
   instances.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Any

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


def _client_connection(client: AsterClient) -> Any:
    """Return the first cached `CoreConnection` on this AsterClient.
    Used by chaos tests that call `ClientSession.for_test(...)` with
    an explicit sessionId; in those paths the client's monotonic
    allocator is bypassed so we reach into the connection cache
    directly. Production code uses `client.open_session()` which
    hides this.
    """
    conns = list(client._rpc_conns.values())  # type: ignore[attr-defined]
    if not conns:
        raise RuntimeError(
            "AsterClient has no cached connections yet -- call open_session() "
            "or client() first to establish the RPC connection"
        )
    return conns[0]


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


# ── 1. Session reap on connection close ────────────────────────────────────


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_reaps_per_connection_state_on_connection_close():
    _reset_chaos_state()
    async with AsterServer(
        services=[ChaosSessionService],
        allow_all_consumers=True,
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


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_handler_exception_does_not_poison_session_or_leak_across_sessions():
    _reset_chaos_state()
    async with AsterServer(
        services=[ChaosSessionService],
        allow_all_consumers=True,
    ) as server:
        addr_b64 = server.endpoint_addr_b64
        async with AsterClient(endpoint_addr=addr_b64) as client:
            sa = await client.open_session()
            sb = await client.open_session()
            ta = _session_transport(sa)
            tb = _session_transport(sb)

            # Session A: two bumps so counter=2.
            r1 = await ta.unary("ChaosSession", "bump", BumpRequest(message="a"))
            r2 = await ta.unary("ChaosSession", "bump", BumpRequest(message="a"))
            assert r1.reply == "a:1"
            assert r2.reply == "a:2"

            # Session A: handler throws. Must surface as RpcError.
            with pytest.raises(RpcError):
                await ta.unary("ChaosSession", "fail", FailRequest(message="a"))

            # Session A: next bump must still succeed and counter
            # continues from 2 to 3 -- the throw did NOT reset or
            # remove the instance.
            r3 = await ta.unary("ChaosSession", "bump", BumpRequest(message="a"))
            assert r3.reply == "a:3"

            # Session B: untouched. First bump sees counter=1.
            rb = await tb.unary("ChaosSession", "bump", BumpRequest(message="b"))
            assert rb.reply == "b:1"

            # Both sessions still alive on the server.
            snap = server.debug_connection_snapshot()
            assert len(snap) == 1
            (conn_info,) = snap.values()
            assert conn_info["active_session_count"] == 2

            await sa.close()
            await sb.close()


# ── 3. Graveyard enforcement under out-of-order arrival ────────────────────


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_graveyard_rejects_older_session_id_after_last_opened_advanced():
    _reset_chaos_state()
    async with AsterServer(
        services=[ChaosSessionService],
        allow_all_consumers=True,
    ) as server:
        addr_b64 = server.endpoint_addr_b64
        async with AsterClient(endpoint_addr=addr_b64) as client:
            # Prime the connection by opening one session normally --
            # this populates `client._rpc_conns` so
            # `_client_connection(client)` has something to return.
            # After that we bypass the monotonic allocator via
            # `for_test` with explicit sessionIds.
            _prime = await client.open_session()
            await _prime.close()
            connection = _client_connection(client)

            # Use session 100 FIRST (a high id so it's clearly distinct
            # from whatever the primer session used).
            s_high = ClientSession.for_test(client, connection, 100)
            t_high = _session_transport(s_high)
            r_high = await t_high.unary(
                "ChaosSession", "bump", BumpRequest(message="high")
            )
            assert r_high.reply == "high:1"

            # Now try session 50. It's <= last_opened_session_id (100)
            # and not in active → NOT_FOUND per Sec. 7.5.
            s_low = ClientSession.for_test(client, connection, 50)
            t_low = _session_transport(s_low)
            with pytest.raises(RpcError) as exc_info:
                await t_low.unary(
                    "ChaosSession", "bump", BumpRequest(message="low")
                )
            assert exc_info.value.code == StatusCode.NOT_FOUND

            # Session 100 still works after the graveyard rejection.
            r_high_b = await t_high.unary(
                "ChaosSession", "bump", BumpRequest(message="high")
            )
            assert r_high_b.reply == "high:2"

            # Snapshot: exactly one connection, one active session
            # (session 100). Primer session was opened and closed
            # from the client side but the server-side reap only
            # fires on connection close, so the primer is still in
            # the active set too -- at most 2 active sessions.
            snap = server.debug_connection_snapshot()
            assert len(snap) == 1
            (conn_info,) = snap.values()
            assert conn_info["last_opened_session_id"] == 100
            assert conn_info["active_session_count"] >= 1


# ── 4a. Session cap, sequenced ─────────────────────────────────────────────


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_session_cap_enforced_when_open_session_calls_are_serialised():
    _reset_chaos_state()
    CAP = 4
    EXTRA = 3
    async with AsterServer(
        services=[ChaosSessionService],
        allow_all_consumers=True,
        max_sessions_per_connection=CAP,
    ) as server:
        addr_b64 = server.endpoint_addr_b64
        async with AsterClient(endpoint_addr=addr_b64) as client:
            fulfilled: list[str] = []
            rejected_codes: list[StatusCode] = []
            for i in range(CAP + EXTRA):
                s = await client.open_session()
                t = _session_transport(s)
                try:
                    r = await t.unary(
                        "ChaosSession", "bump", BumpRequest(message=f"seq{i}")
                    )
                    fulfilled.append(r.reply)
                except RpcError as e:
                    rejected_codes.append(e.code)

            assert len(fulfilled) == CAP
            assert len(rejected_codes) == EXTRA
            for code in rejected_codes:
                assert code == StatusCode.RESOURCE_EXHAUSTED

            snap = server.debug_connection_snapshot()
            (conn_info,) = snap.values()
            assert conn_info["active_session_count"] == CAP


# ── 4b. Session cap, concurrent burst ──────────────────────────────────────


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_session_cap_under_concurrent_burst():
    _reset_chaos_state()
    CAP = 4
    BURST = 12
    async with AsterServer(
        services=[ChaosSessionService],
        allow_all_consumers=True,
        max_sessions_per_connection=CAP,
    ) as server:
        addr_b64 = server.endpoint_addr_b64
        async with AsterClient(endpoint_addr=addr_b64) as client:

            async def one_call(i: int) -> Any:
                s = await client.open_session()
                t = _session_transport(s)
                return await t.unary(
                    "ChaosSession", "bump", BumpRequest(message=f"burst{i}")
                )

            results = await asyncio.gather(
                *(one_call(i) for i in range(BURST)),
                return_exceptions=True,
            )
            fulfilled = [r for r in results if not isinstance(r, BaseException)]
            rejected = [r for r in results if isinstance(r, RpcError)]
            other_errors = [
                r for r in results if isinstance(r, BaseException) and not isinstance(r, RpcError)
            ]

            # Chaos invariant: concurrent burst never exceeds CAP
            # successes, and every rejection is a legitimate shape.
            assert len(fulfilled) <= CAP, f"expected ≤ CAP successes, got {len(fulfilled)}"
            assert len(fulfilled) + len(rejected) == BURST, (
                f"missing results: other_errors={other_errors}"
            )
            for err in rejected:
                assert err.code in (StatusCode.NOT_FOUND, StatusCode.RESOURCE_EXHAUSTED), (
                    f"unexpected rejection code: {err.code}"
                )

            snap = server.debug_connection_snapshot()
            (conn_info,) = snap.values()
            assert conn_info["active_session_count"] == len(fulfilled)


# ── 5. Cross-connection session id isolation ──────────────────────────────


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_same_session_id_on_two_distinct_connections_resolves_to_distinct_instances():
    _reset_chaos_state()
    async with AsterServer(
        services=[ChaosSessionService],
        allow_all_consumers=True,
    ) as server:
        addr_b64 = server.endpoint_addr_b64

        # Two independent clients → two distinct QUIC connections.
        async with AsterClient(endpoint_addr=addr_b64) as client_a, AsterClient(
            endpoint_addr=addr_b64
        ) as client_b:
            # Prime each client so its connection is cached.
            pa = await client_a.open_session()
            pb = await client_b.open_session()
            conn_a = _client_connection(client_a)
            conn_b = _client_connection(client_b)
            assert conn_a is not conn_b

            # Both use sessionId=200 via for_test (distinct from
            # whatever the primers allocated). Valid only because
            # they're on distinct connections.
            sa = ClientSession.for_test(client_a, conn_a, 200)
            sb = ClientSession.for_test(client_b, conn_b, 200)
            ta = _session_transport(sa)
            tb = _session_transport(sb)

            # Bump A twice, bump B once.
            await ta.unary("ChaosSession", "bump", BumpRequest(message="A"))
            await ta.unary("ChaosSession", "bump", BumpRequest(message="A"))
            await tb.unary("ChaosSession", "bump", BumpRequest(message="B"))

            # If the server collapsed both into one instance (keyed
            # by sessionId alone instead of (connectionId,
            # sessionId)), we'd see a shared counter. With proper
            # per-(connection, session) keying:
            #   A's counter = 2 → next bump sees 3
            #   B's counter = 1 → next bump sees 2
            ra = await ta.unary("ChaosSession", "bump", BumpRequest(message="A"))
            rb = await tb.unary("ChaosSession", "bump", BumpRequest(message="B"))
            assert ra.reply == "A:3"
            assert rb.reply == "B:2"

            # Two connections on the server.
            snap = server.debug_connection_snapshot()
            assert len(snap) == 2
            for conn_info in snap.values():
                assert conn_info["last_opened_session_id"] == 200

            await pa.close()
            await pb.close()


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
        allow_all_consumers=True,
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
