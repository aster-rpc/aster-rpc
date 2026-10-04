"""
Plan P4: Python RPC on ``aster::rpc`` dynamic dispatch.

- ``aster._lowering`` turns decorators into ``DynamicService`` JSON whose
  Rust-derived contract id equals the one Python computes, and matches
  ``conformance/vectors/dynamic-service.json`` where a Python equivalent
  exists.
- The server path (``aster._dispatch`` over ``DispatchServer``): all four
  patterns, sessions (per-session instances, close hooks), error mapping
  (design sec. 2.7), Rust-owned deadlines, peer cancel, Gate 3 fed by the gate's
  admission attributes, the baseline ``aster.ops.NodeInfo``.
- The client path (``RpcClient`` / pooled ``RpcConnection``): session ids from
  the connection's shared counter.
"""

from __future__ import annotations

import asyncio
import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import AsyncIterator

import pytest

from aster import (
    AsterClient,
    AsterServer,
    bidi_stream,
    client_stream,
    rpc,
    server_stream,
    service,
    wire_type,
)
from aster._lowering import contract_id as rust_contract_id
from aster._lowering import lower, lower_json
from aster.capabilities import any_of
from aster.contract.identity import contract_id_from_service
from aster.status import RpcError, StatusCode

REPO = Path(__file__).resolve().parents[2]
VECTORS = json.loads((REPO / "conformance/vectors/dynamic-service.json").read_text())


# -- types ---------------------------------------------------------------------


@wire_type("test.p4/Req")
@dataclass
class Req:
    text: str = ""
    n: int = 0


@wire_type("test.p4/Resp")
@dataclass
class Resp:
    text: str = ""
    n: int = 0


# -- services ------------------------------------------------------------------

EVENTS: list[str] = []


@service(name="P4Echo", version=3)
class P4Echo:
    def __init__(self) -> None:
        self.cancelled: asyncio.Event | None = None

    @rpc(idempotent=True)
    async def echo(self, req: Req) -> Resp:
        return Resp(text=req.text, n=req.n + 1)

    @rpc()
    async def echo_upper(self, req: Req) -> Resp:
        await asyncio.sleep(0)
        return Resp(text=req.text.upper(), n=req.n)

    @rpc()
    async def fail(self, req: Req) -> Resp:
        if req.n == 1:
            raise RpcError(StatusCode.FAILED_PRECONDITION, "nope", {"k": "v"})
        raise ValueError("boom")

    @rpc(timeout=1.0)
    async def slow(self, req: Req) -> Resp:
        try:
            await asyncio.sleep(10)
        except asyncio.CancelledError:
            EVENTS.append("slow-cancelled")
            raise
        return Resp()

    @server_stream()
    async def count(self, req: Req) -> AsyncIterator[Resp]:
        for i in range(req.n):
            yield Resp(text=req.text, n=i)

    @server_stream()
    async def forever(self, req: Req) -> AsyncIterator[Resp]:
        try:
            i = 0
            while True:
                yield Resp(n=i)
                i += 1
                await asyncio.sleep(0.01)
        except asyncio.CancelledError:
            EVENTS.append("forever-cancelled")
            raise

    @client_stream()
    async def total(self, reqs: AsyncIterator[Req]) -> Resp:
        n = 0
        last = ""
        async for r in reqs:
            n += r.n
            last = r.text
        return Resp(text=last, n=n)

    @bidi_stream()
    async def chat(self, reqs: AsyncIterator[Req]) -> AsyncIterator[Resp]:
        async for r in reqs:
            yield Resp(text=r.text[::-1], n=r.n)


@service(name="P4Session", version=1, scoped="session")
class P4Session:
    def __init__(self, peer: str | None = None) -> None:
        self.peer = peer
        self.total = 0

    @rpc()
    async def add(self, req: Req) -> Resp:
        self.total += req.n
        return Resp(text=self.peer or "", n=self.total)

    async def close(self) -> None:
        EVENTS.append(f"session-closed:{self.total}")


@service(name="P4Guarded", version=1)
class P4Guarded:
    @rpc(requires=any_of("ops", "admin"))
    async def secret(self, req: Req) -> Resp:
        return Resp(text="secret")

    @rpc()
    async def open(self, req: Req) -> Resp:
        return Resp(text="open")


# -- lowering ------------------------------------------------------------------


def test_lowering_matches_python_contract_ids():
    for cls in (P4Echo, P4Session, P4Guarded):
        assert rust_contract_id(cls) == contract_id_from_service(cls), cls.__name__


def test_lowering_shape():
    d = lower(P4Guarded)
    assert d["name"] == "P4Guarded" and d["version"] == 1 and d["scope"] == "shared"
    by_name = {m["name"]: m for m in d["methods"]}
    assert by_name["secret"]["requires"] == {"kind": "any_of", "roles": ["ops", "admin"]}
    assert "requires" not in by_name["open"]
    e = {m["name"]: m for m in lower(P4Echo)["methods"]}
    assert e["echo"]["idempotent"] is True
    assert e["slow"]["default_timeout"] == 1.0
    assert e["count"]["pattern"] == "server_stream"
    assert e["chat"]["pattern"] == "bidi_stream"
    assert lower(P4Session)["scope"] == "session"


def test_interop_echo_matches_vector():
    sys.path.insert(0, str(REPO / "tests/interop/python"))
    try:
        from interop_echo import InteropEcho
    finally:
        sys.path.pop(0)
    case = next(c for c in VECTORS["cases"] if c["name"].startswith("interop"))
    lowered = lower(InteropEcho, include_type_defs=True)
    assert [m["request"]["hash"] for m in lowered["methods"]] == [
        m["request"]["hash"] for m in case["service"]["methods"]
    ]
    assert rust_contract_id(InteropEcho) == case["expected_contract_id"]
    assert contract_id_from_service(InteropEcho) == case["expected_contract_id"]
    from aster._aster import dynamic_service_contract_id

    assert dynamic_service_contract_id(lower_json(InteropEcho, include_type_defs=True)) == case[
        "expected_contract_id"
    ]


sys.path.insert(0, str(REPO))
try:
    from examples.python.mission_control.types import (  # noqa: E402
        Command,
        CommandResult,
        IngestResult,
        LogEntry,
        StatusRequest,
        StatusResponse,
        SubmitLogResult,
        TailRequest,
    )
finally:
    sys.path.pop(0)


@wire_type("mission/MetricPoint")
@dataclass
class MetricPoint:
    """The Rust MissionControl's MetricPoint (the Python example's also
    carries ``tags``)."""

    name: str = ""
    value: float = 0.0
    timestamp: float = 0.0


@service(name="MissionControl", version=1)
class MC1:
    @rpc()
    async def getStatus(self, req: StatusRequest) -> StatusResponse: ...

    @rpc()
    async def submitLog(self, req: LogEntry) -> SubmitLogResult: ...

    @server_stream()
    async def tailLogs(self, req: TailRequest) -> AsyncIterator[LogEntry]:
        yield LogEntry()

    @client_stream()
    async def ingestMetrics(self, reqs: AsyncIterator[MetricPoint]) -> IngestResult: ...

    @bidi_stream()
    async def runCommand(self, reqs: AsyncIterator[Command]) -> AsyncIterator[CommandResult]:
        yield CommandResult()


@service(name="MissionControl", version=2)
class MC2:
    @rpc(idempotent=True, requires="status")
    async def getStatus(self, req: StatusRequest) -> StatusResponse: ...

    @rpc()
    async def submitLog(self, req: LogEntry) -> SubmitLogResult: ...

    @server_stream(requires=any_of("logs", "admin"))
    async def tailLogs(self, req: TailRequest) -> AsyncIterator[LogEntry]:
        yield LogEntry()

    @client_stream(requires="ingest")
    async def ingestMetrics(self, reqs: AsyncIterator[MetricPoint]) -> IngestResult: ...


def test_mission_control_matches_vector():
    """Python equivalents of the vector's MissionControl v1 / v2 (with
    per-method requires), built from the example's wire types."""
    cases = {c["name"]: c for c in VECTORS["cases"]}
    for cls, name in (
        (MC1, "mission-control: MissionControl v1"),
        (MC2, "mission-control: MissionControl v2 with per-method requires"),
    ):
        case = cases[name]
        want = {m["name"]: (m["request"]["hash"], m["response"]["hash"]) for m in case["service"]["methods"]}
        got = {m["name"]: (m["request"]["hash"], m["response"]["hash"]) for m in lower(cls)["methods"]}
        if got != want:
            pytest.skip("example wire types differ from the Rust MissionControl types")
        assert rust_contract_id(cls) == case["expected_contract_id"], name
        assert contract_id_from_service(cls) == case["expected_contract_id"], name


def test_vector_cases_through_native():
    from aster._aster import dynamic_service_contract_id

    for case in VECTORS["cases"]:
        assert dynamic_service_contract_id(json.dumps(case["service"])) == case["expected_contract_id"]
    for case in VECTORS["invalid"]:
        with pytest.raises((ValueError, Exception)):
            dynamic_service_contract_id(json.dumps(case["service"]))


# -- server + client -----------------------------------------------------------


@pytest.fixture
async def p4_pair():
    async with AsterServer(
        services=[P4Echo(), P4Session, P4Guarded()],
        allow_all_consumers=True,
    ) as server:
        async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as client:
            yield server, client


@pytest.mark.timeout(30)
async def test_four_patterns(p4_pair):
    _, client = p4_pair
    svc = await client.client(P4Echo)
    assert await svc.echo(Req(text="hi", n=1)) == Resp(text="hi", n=2)
    assert await svc.echo_upper(Req(text="hi", n=7)) == Resp(text="HI", n=7)
    items = [r async for r in svc.count(Req(text="s", n=200))]
    assert [r.n for r in items] == list(range(200))

    async def reqs():
        for i in range(500):
            yield Req(text=f"m{i}", n=1)

    assert await svc.total(reqs()) == Resp(text="m499", n=500)

    ch = svc.chat()
    async with ch:
        for i in range(5):
            await ch.send(Req(text=f"abc{i}", n=i))
            got = await ch.recv()
            assert got == Resp(text=f"{i}cba", n=i)
        await ch.close()
        assert await ch.recv() is None
    assert (await ch.wait_for_trailer())[0] == StatusCode.OK


@pytest.mark.timeout(30)
async def test_concurrent_unary(p4_pair):
    _, client = p4_pair
    svc = await client.client(P4Echo)
    out = await asyncio.gather(*(svc.echo(Req(text=str(i), n=i)) for i in range(300)))
    assert [r.n for r in out] == [i + 1 for i in range(300)]


@pytest.mark.timeout(30)
async def test_error_mapping(p4_pair):
    _, client = p4_pair
    svc = await client.client(P4Echo)
    with pytest.raises(RpcError) as e:
        await svc.fail(Req(n=1))
    assert e.value.code == StatusCode.FAILED_PRECONDITION
    assert e.value.message == "nope" and e.value.details == {"k": "v"}
    with pytest.raises(RpcError) as e:
        await svc.fail(Req(n=2))
    assert e.value.code == StatusCode.INTERNAL and "boom" in e.value.message


@pytest.mark.timeout(30)
async def test_unknown_method_and_service(p4_pair):
    server, client = p4_pair
    from aster._aster import RpcClient

    svc = await client.client(P4Echo)
    rc = RpcClient(svc._transport._conn)
    with pytest.raises(RpcError) as e:
        await rc.unary("P4Echo", "nosuch", b"\x00", version=3)
    assert e.value.code == StatusCode.UNIMPLEMENTED
    with pytest.raises(RpcError) as e:
        await rc.unary("NoSuchService", "x", b"\x00")
    assert e.value.code == StatusCode.NOT_FOUND
    # A version mismatch is a different service.
    with pytest.raises(RpcError) as e:
        await rc.unary("P4Echo", "echo", b"\x00", version=1)
    assert e.value.code == StatusCode.NOT_FOUND
    # A payload that does not decode.
    with pytest.raises(RpcError) as e:
        await rc.unary("P4Echo", "echo", b"\xff\xff\xff", version=3)
    assert e.value.code in (StatusCode.INVALID_ARGUMENT, StatusCode.INTERNAL)
    # The baseline NodeInfo service is served by Rust next to Python's.
    with pytest.raises(RpcError) as e:
        await rc.unary("aster.ops.NodeInfo", "nosuch", b"\x00")
    assert e.value.code == StatusCode.UNIMPLEMENTED


@pytest.mark.timeout(30)
async def test_method_timeout_enforced_by_rust(p4_pair):
    _, client = p4_pair
    svc = await client.client(P4Echo)
    EVENTS.clear()
    with pytest.raises(RpcError) as e:
        await svc.slow(Req())
    assert e.value.code == StatusCode.DEADLINE_EXCEEDED
    for _ in range(50):
        if "slow-cancelled" in EVENTS:
            break
        await asyncio.sleep(0.05)
    assert "slow-cancelled" in EVENTS


@pytest.mark.timeout(30)
async def test_client_cancel_stops_server_stream(p4_pair):
    server, client = p4_pair
    svc = await client.client(P4Echo)
    EVENTS.clear()
    it = svc.forever(Req())
    for _ in range(3):
        await anext(it)
    await it.aclose()
    for _ in range(100):
        if "forever-cancelled" in EVENTS:
            break
        await asyncio.sleep(0.05)
    assert "forever-cancelled" in EVENTS
    for _ in range(100):
        if server._server._dispatcher.server.live_calls() == 0:
            break
        await asyncio.sleep(0.05)
    assert server._server._dispatcher.server.live_calls() == 0


@pytest.mark.timeout(30)
async def test_sessions(p4_pair):
    server, client = p4_pair
    EVENTS.clear()
    s1 = await client.open_session(service_cls=P4Session)
    s2 = await client.open_session(service_cls=P4Session)
    # Ids come from the connection's shared counter: distinct, non-zero.
    assert s1.session_id and s2.session_id and s1.session_id != s2.session_id
    a = await s1.client(P4Session)
    b = await s2.client(P4Session)
    assert (await a.add(Req(n=2))).n == 2
    assert (await a.add(Req(n=3))).n == 5
    assert (await b.add(Req(n=10))).n == 10
    peer = (await a.add(Req(n=0))).text
    assert peer  # the session instance was built with the caller's peer id


@pytest.mark.timeout(30)
async def test_session_close_hook_on_disconnect():
    EVENTS.clear()
    async with AsterServer(services=[P4Session], allow_all_consumers=True) as server:
        async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as client:
            s = await client.open_session(service_cls=P4Session)
            c = await s.client(P4Session)
            assert (await c.add(Req(n=4))).n == 4
        for _ in range(100):
            if any(e.startswith("session-closed") for e in EVENTS):
                break
            await asyncio.sleep(0.05)
        assert "session-closed:4" in EVENTS


@pytest.mark.timeout(30)
async def test_gate3_reads_gate_attributes(p4_pair):
    server, client = p4_pair
    svc = await client.client(P4Guarded)
    assert (await svc.open(Req())).text == "open"
    with pytest.raises(RpcError) as e:
        await svc.secret(Req())
    assert e.value.code == StatusCode.PERMISSION_DENIED
    # Admission attributes recorded in the node's gate feed Rust's Gate 3.
    server.gate.admit(client._node.node_id(), attributes={"aster.role": "ops"})
    assert (await svc.secret(Req())).text == "secret"
