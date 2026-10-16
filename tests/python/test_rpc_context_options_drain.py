"""Handler CallContext (principal, provenance, cancellation, effective
deadline), per-call ``CallOptions`` on every pattern, and the Rust server
drain, over real nodes through ``AsterServer`` / ``AsterClient``.

Mirrors ``aster::rpc::CallContext``, ``CallOptions`` and
``AsterServer::drain`` (``docs/_internal/bindings/mirror-2026-10.md``, items
H, C and D).
"""

from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass
from typing import AsyncIterator

import pytest

from aster import (
    AsterClient,
    AsterServer,
    CallContext,
    CallOptions,
    DrainReport,
    DRAINING_DETAIL_KEY,
    bidi_stream,
    client_stream,
    rpc,
    server_stream,
    service,
    wire_type,
)
from aster.status import RpcError, StatusCode


@wire_type("test.mirror/Req")
@dataclass
class Req:
    text: str = ""
    n: int = 0


@wire_type("test.mirror/Resp")
@dataclass
class Resp:
    text: str = ""
    n: int = 0
    peer: str = ""
    principal: str = ""
    provenance: str = ""
    meta: str = ""
    remaining_ms: int = -1


def _resp(ctx: CallContext, text: str = "", n: int = 0) -> Resp:
    remaining = ctx.remaining_seconds
    return Resp(
        text=text,
        n=n,
        peer=ctx.peer or "",
        principal=ctx.principal or "",
        provenance=ctx.provenance,
        meta=",".join(f"{k}={v}" for k, v in sorted(ctx.metadata.items())),
        remaining_ms=-1 if remaining is None else int(remaining * 1000),
    )


class Gate:
    """Lets a test hold a handler until it says so."""

    def __init__(self) -> None:
        self.entered = asyncio.Event()
        self.release = asyncio.Event()
        self.saw_cancel = asyncio.Event()
        self.ctx: CallContext | None = None


GATE = Gate()


@service(name="MirrorCtx", version=1)
class MirrorCtx:
    @rpc()
    async def who(self, req: Req, ctx: CallContext) -> Resp:
        return _resp(ctx, req.text)

    @server_stream()
    async def who_stream(self, req: Req, ctx: CallContext) -> AsyncIterator[Resp]:
        yield _resp(ctx, req.text)

    @client_stream()
    async def who_client(self, reqs: AsyncIterator[Req], ctx: CallContext) -> Resp:
        n = 0
        async for _ in reqs:
            n += 1
        return _resp(ctx, n=n)

    @bidi_stream()
    async def who_bidi(self, reqs: AsyncIterator[Req], ctx: CallContext) -> AsyncIterator[Resp]:
        async for r in reqs:
            yield _resp(ctx, r.text)

    @rpc()
    async def hold(self, req: Req, ctx: CallContext) -> Resp:
        gate = GATE
        gate.ctx = ctx

        async def watch() -> None:
            await ctx.cancelled()
            gate.saw_cancel.set()

        # Spawned work holding the context observes the cancel.
        asyncio.get_running_loop().create_task(watch())
        gate.entered.set()
        await gate.release.wait()
        return Resp(text="released", n=int(ctx.is_cancelled()))


@pytest.fixture
async def pair():
    global GATE
    GATE = Gate()
    async with AsterServer(services=[MirrorCtx()]) as server:
        async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as client:
            yield server, client


async def _wait(event: asyncio.Event, what: str, timeout: float = 10.0) -> None:
    try:
        await asyncio.wait_for(event.wait(), timeout)
    except asyncio.TimeoutError:
        raise AssertionError(f"timed out waiting for {what}") from None


# -- handler context ----------------------------------------------------------


@pytest.mark.timeout(30)
async def test_principal_peer_provenance_and_effective_deadline(pair):
    server, client = pair
    svc = await client.client(MirrorCtx)
    got = await svc.who(Req(text="a"))
    me = client._node.node_id()
    # No authenticator: the principal is the transport peer.
    assert got.peer == me and got.principal == me
    assert got.provenance == "iroh_authenticated"
    # The effective deadline is Rust's: the server handler timeout (300 s)
    # applies even when the caller sets none.
    assert 0 < got.remaining_ms <= 300_000


@pytest.mark.timeout(30)
async def test_cancelled_resolves_on_peer_cancel(pair):
    _, client = pair
    svc = await client.client(MirrorCtx)
    call = asyncio.ensure_future(svc.hold(Req()))
    await _wait(GATE.entered, "handler entry")
    assert GATE.ctx is not None and not GATE.ctx.is_cancelled()
    call.cancel()
    with pytest.raises(asyncio.CancelledError):
        await call
    await _wait(GATE.saw_cancel, "ctx.cancelled() after a peer cancel")
    assert GATE.ctx.is_cancelled()


def test_plain_context_principal_defaults_to_peer():
    ctx = CallContext(service="S", method="m", peer="abc")
    assert ctx.principal == "abc" and ctx.provenance == "other"
    assert not ctx.is_cancelled()
    ctx._mark_cancelled()
    assert ctx.is_cancelled()
    assert CallContext(service="S", method="m", peer="p", principal="alice").principal == "alice"


async def test_plain_context_cancelled_resolves_at_deadline():
    ctx = CallContext(service="S", method="m", deadline=time.time() + 0.05)
    await asyncio.wait_for(ctx.cancelled(), 5)
    assert ctx.is_cancelled()


# -- per-call options ---------------------------------------------------------


@pytest.mark.timeout(30)
async def test_options_on_all_four_patterns(pair):
    _, client = pair
    base = await client.client(MirrorCtx)
    svc = base.with_options(CallOptions(timeout=0.75, metadata={"tenant": "t1"}))
    assert base.options == CallOptions()

    got = await svc.who(Req(text="u"))
    assert got.meta == "tenant=t1"
    # The header carries whole seconds (rounded up); the client enforces the
    # exact sub-second deadline (see test_options_deadline_*).
    assert 0 < got.remaining_ms <= 1000

    # Per-call options and explicit metadata merge over the client's.
    got = await svc.who(Req(), options=CallOptions(metadata={"k": "v"}), metadata={"x": "1"})
    assert got.meta == "k=v,tenant=t1,x=1"

    items = [r async for r in svc.who_stream(Req(text="s"))]
    assert [r.meta for r in items] == ["tenant=t1"]
    assert 0 < items[0].remaining_ms <= 1000

    async def reqs():
        for i in range(3):
            yield Req(n=i)

    got = await svc.who_client(reqs(), options=CallOptions(metadata={"tenant": "t2"}))
    assert got.n == 3 and got.meta == "tenant=t2"

    ch = svc.who_bidi()
    async with ch:
        await ch.send(Req(text="b"))
        got = await ch.recv()
        assert got.meta == "tenant=t1" and 0 < got.remaining_ms <= 1000
        await ch.close()


@pytest.mark.timeout(30)
async def test_options_deadline_reaches_handler_and_expires(pair):
    _, client = pair
    svc = (await client.client(MirrorCtx)).with_options(CallOptions(timeout=0.3))
    started = time.monotonic()
    with pytest.raises(RpcError) as e:
        await svc.hold(Req())
    assert e.value.code == StatusCode.DEADLINE_EXCEEDED
    # Sub-second: the client gives up at 0.3 s, not at a rounded-up second.
    assert time.monotonic() - started < 0.9
    await _wait(GATE.saw_cancel, "ctx.cancelled() at the deadline")
    GATE.release.set()


@pytest.mark.timeout(30)
async def test_proxy_client_with_options(pair):
    _, client = pair
    mc = await client.proxy("MirrorCtx")
    scoped = mc.with_options(CallOptions(metadata={"via": "proxy"}, timeout=2.0))
    got = await scoped.who({"text": "p"})
    assert got["meta"] == "via=proxy" and 0 < got["remaining_ms"] <= 2000
    items = [r async for r in scoped.who_stream.stream({"text": "s"})]
    assert [r["meta"] for r in items] == ["via=proxy"]
    plain = await mc.who({"text": "p"})
    assert plain["meta"] == ""


def test_call_options_validation_and_merge():
    with pytest.raises(ValueError):
        CallOptions(timeout=-1)
    with pytest.raises(ValueError):
        CallOptions(timeout=float("inf"))
    a = CallOptions(timeout=1.0, metadata={"a": "1", "b": "1"})
    b = CallOptions(metadata={"b": "2"})
    assert a.merged(b) == CallOptions(timeout=1.0, metadata={"a": "1", "b": "2"})
    assert a.merged(None) is a


# -- drain --------------------------------------------------------------------


@pytest.mark.timeout(60)
async def test_drain_lets_in_flight_finish_and_refuses_new_calls(pair):
    server, client = pair
    svc = await client.client(MirrorCtx)
    held = asyncio.ensure_future(svc.hold(Req()))
    await _wait(GATE.entered, "handler entry")
    assert not server.is_draining

    drain = asyncio.ensure_future(server.drain(10.0))

    async def until_draining() -> None:
        while not server.is_draining:
            await asyncio.sleep(0.01)

    await asyncio.wait_for(until_draining(), 10)
    # A new call is refused while draining.
    with pytest.raises(RpcError) as e:
        await svc.who(Req())
    assert e.value.is_draining
    assert e.value.code == StatusCode.UNAVAILABLE
    assert DRAINING_DETAIL_KEY in e.value.details

    GATE.release.set()
    assert (await held).text == "released"
    report = await asyncio.wait_for(drain, 20)
    assert isinstance(report, DrainReport)
    assert report.calls_in_flight == 1
    assert report.calls_completed == 1
    assert report.calls_cancelled == 0
    assert report.calls_refused >= 1
    assert report.deadline_reached is False
    assert report.elapsed > 0
    await _assert_closed_after_drain(server, client, svc)


async def _assert_closed_after_drain(server, client, svc) -> None:
    """drain() closes the server, as Rust/Go/.NET/Java/TS do: no further
    drain, and the address no longer serves. A server left up after its drain
    would accept a fresh connection and refuse it as draining."""
    with pytest.raises(RuntimeError, match="closed"):
        await server.drain(1.0)
    # The existing connection was closed by the drain (the client reports
    # that close as draining).
    with pytest.raises(BaseException):
        await asyncio.wait_for(svc.who(Req()), 15)


    async def fresh_call() -> None:
        async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as fresh:
            fresh_svc = await fresh.client(MirrorCtx)
            await fresh_svc.who(Req())

    with pytest.raises(Exception) as e:
        await asyncio.wait_for(fresh_call(), 30)
    assert not (isinstance(e.value, RpcError) and e.value.is_draining), e.value


@pytest.mark.timeout(60)
async def test_concurrent_drains_share_one_report_and_close_once(pair):
    server, client = pair
    first = asyncio.ensure_future(server.drain(10.0))
    second = asyncio.ensure_future(server.drain(10.0))
    a, b = await asyncio.wait_for(asyncio.gather(first, second), 30)
    assert a.calls_completed == b.calls_completed
    assert a.calls_refused == b.calls_refused


@pytest.mark.timeout(60)
async def test_drain_deadline_cancels_stuck_handlers(pair):
    server, client = pair
    svc = await client.client(MirrorCtx)
    held = asyncio.ensure_future(svc.hold(Req()))
    await _wait(GATE.entered, "handler entry")
    report = await asyncio.wait_for(server.drain(0.2), 20)
    assert report.deadline_reached is True
    assert report.calls_cancelled == 1
    assert report.connections_closed >= 1
    await _wait(GATE.saw_cancel, "handler cancel at the drain deadline")
    with pytest.raises((RpcError, Exception)) as e:
        await asyncio.wait_for(held, 10)
    if isinstance(e.value, RpcError):
        assert e.value.is_draining
    GATE.release.set()
    await _assert_closed_after_drain(server, client, svc)


def test_rpc_error_is_draining():
    assert RpcError(StatusCode.UNAVAILABLE, "x", {DRAINING_DETAIL_KEY: "true"}).is_draining
    assert not RpcError(StatusCode.UNAVAILABLE, "x").is_draining
    assert not RpcError(StatusCode.INTERNAL, "x", {DRAINING_DETAIL_KEY: "true"}).is_draining
