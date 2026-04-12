"""
Workloads and test services for chaos testing.

Each workload is an async function that drives a series of RPC operations
against a session, recording every invoke and completion in the History.
"""

from __future__ import annotations

import asyncio
import dataclasses
import uuid
from typing import Any, AsyncIterator

from aster.codec import ForyCodec, wire_type
from aster.decorators import service, rpc, client_stream
from aster.framing import CALL, CANCEL, HEADER, TRAILER, COMPRESSED, read_frame, write_frame
from aster.protocol import CallHeader, RpcStatus, StreamHeader
from aster.rpc_types import RpcScope, SerializationMode
from aster.session import (
    SessionServer,
    _ByteQueue,
    _FakeRecvStream,
    _FakeSendStream,
    _generate_session_stub_class,
)
from aster.status import RpcError, StatusCode

from .harness import History
from .nemeses import NemesisBase, NemesisRecvStream


# -- Wire types ---------------------------------------------------------------

@wire_type("chaos/EchoReq")
@dataclasses.dataclass
class EchoReq:
    value: int = 0
    delay: float = 0.0

@wire_type("chaos/EchoResp")
@dataclasses.dataclass
class EchoResp:
    value: int = 0

@wire_type("chaos/SumItem")
@dataclasses.dataclass
class SumItem:
    value: int = 0

@wire_type("chaos/SumResp")
@dataclasses.dataclass
class SumResp:
    total: int = 0

@wire_type("chaos/CounterReq")
@dataclasses.dataclass
class CounterReq:
    delay: float = 0.0

@wire_type("chaos/CounterResp")
@dataclasses.dataclass
class CounterResp:
    counter: int = 0

ALL_TYPES = [EchoReq, EchoResp, SumItem, SumResp, CounterReq, CounterResp]


# -- Service definitions ------------------------------------------------------

@service(name="ChaosSession", version=1, scoped="session")
class ChaosSession:
    def __init__(self, peer=None):
        self.counter = 0
        self.peer = peer

    @rpc
    async def echo(self, req: EchoReq) -> EchoResp:
        return EchoResp(value=req.value)

    @rpc
    async def slow_echo(self, req: EchoReq) -> EchoResp:
        await asyncio.sleep(req.delay)
        return EchoResp(value=req.value)

    @rpc
    async def increment(self, req: CounterReq) -> CounterResp:
        if req.delay > 0:
            await asyncio.sleep(req.delay)
        self.counter += 1
        return CounterResp(counter=self.counter)

    @client_stream
    async def sum_stream(self, reqs: AsyncIterator[SumItem]) -> SumResp:
        total = 0
        async for r in reqs:
            total += r.value
        return SumResp(total=total)


# -- Session factory ----------------------------------------------------------


def _make_codec() -> ForyCodec:
    return ForyCodec(mode=SerializationMode.XLANG, types=list(ALL_TYPES))


def create_chaos_session(
    nemesis: NemesisBase,
) -> tuple[Any, asyncio.Task]:
    """Create an in-process session with nemesis-wrapped client recv stream.

    Returns (session_stub, server_task).
    """
    from aster.decorators import _SERVICE_INFO_ATTR

    impl_class = nemesis.setup(ChaosSession)
    service_info = getattr(ChaosSession, _SERVICE_INFO_ATTR)
    codec = _make_codec()

    c2s = _ByteQueue()
    s2c = _ByteQueue()

    c_send = _FakeSendStream(c2s)
    s_recv = _FakeRecvStream(c2s)
    s_send = _FakeSendStream(s2c)
    c_recv_raw = _FakeRecvStream(s2c)

    # Wrap client recv with nemesis
    c_recv = NemesisRecvStream(c_recv_raw, nemesis)

    session_id = str(uuid.uuid4())
    header = StreamHeader(
        service=service_info.name,
        method="",
        version=service_info.version,
        callId=1,
        serializationMode=SerializationMode.XLANG.value,
    )

    session_server = SessionServer(
        service_class=impl_class,
        service_info=service_info,
        codec=codec,
    )

    server_task = asyncio.get_event_loop().create_task(
        session_server.run(header, s_send, s_recv, peer="chaos-peer")
    )

    stub_cls = _generate_session_stub_class(service_info)
    stub = stub_cls(
        send=c_send,
        recv=c_recv,
        codec=codec,
        service_info=service_info,
        interceptors=None,
        session_id=session_id,
    )

    return stub, server_task


def create_chaos_raw_session(
    nemesis: NemesisBase,
) -> tuple[Any, Any, Any, asyncio.Task, ForyCodec]:
    """Create raw pipes for low-level protocol tests (cancel workload).

    Returns (c_send, c_recv, c_recv_raw, server_task, codec).
    c_recv is nemesis-wrapped; c_recv_raw is unwrapped.
    """
    from aster.decorators import _SERVICE_INFO_ATTR

    impl_class = nemesis.setup(ChaosSession)
    service_info = getattr(ChaosSession, _SERVICE_INFO_ATTR)
    codec = _make_codec()

    c2s = _ByteQueue()
    s2c = _ByteQueue()

    c_send = _FakeSendStream(c2s)
    s_recv = _FakeRecvStream(c2s)
    s_send = _FakeSendStream(s2c)
    c_recv_raw = _FakeRecvStream(s2c)
    c_recv = NemesisRecvStream(c_recv_raw, nemesis)

    session_id = str(uuid.uuid4())
    header = StreamHeader(
        service=service_info.name,
        method="",
        version=service_info.version,
        callId=1,
        serializationMode=SerializationMode.XLANG.value,
    )

    session_server = SessionServer(
        service_class=impl_class,
        service_info=service_info,
        codec=codec,
    )

    server_task = asyncio.get_event_loop().create_task(
        session_server.run(header, s_send, s_recv, peer="chaos-peer")
    )

    return c_send, c_recv, c_recv_raw, server_task, codec


# -- Workloads ----------------------------------------------------------------


async def unary_workload(nemesis: NemesisBase, history: History) -> None:
    """Repeatedly call echo. Every OK must match the request."""
    stub, server_task = create_chaos_session(nemesis)
    try:
        for i in range(20):
            op_id = history.invoke(
                workload="unary", nemesis=nemesis.name,
                service="ChaosSession", method="echo",
                request={"value": i},
            )
            try:
                resp = await asyncio.wait_for(
                    stub.echo(EchoReq(value=i)), timeout=3.0
                )
                val = resp.value if hasattr(resp, "value") else resp.get("value")
                history.complete(
                    op_id, "ok",
                    response={"value": val},
                    expected={"value": i},
                )
            except asyncio.TimeoutError:
                history.complete(op_id, "timeout", error="timeout")
                break
            except Exception as e:
                history.complete(op_id, "fail", error=str(e))
                break
    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


async def session_sequential_workload(nemesis: NemesisBase, history: History) -> None:
    """Sequential increments on a session. Counters must be monotonic."""
    stub, server_task = create_chaos_session(nemesis)
    try:
        for i in range(10):
            op_id = history.invoke(
                workload="session_seq", nemesis=nemesis.name,
                service="ChaosSession", method="increment",
                request={"seq": i},
            )
            try:
                resp = await asyncio.wait_for(
                    stub.increment(CounterReq()), timeout=3.0
                )
                counter = resp.counter if hasattr(resp, "counter") else resp.get("counter")
                history.complete(
                    op_id, "ok",
                    response={"counter": counter},
                    expected={"counter": i + 1},
                )
            except asyncio.TimeoutError:
                history.complete(op_id, "timeout", error="timeout")
                break
            except Exception as e:
                history.complete(op_id, "fail", error=str(e))
                break
    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


async def client_stream_workload(nemesis: NemesisBase, history: History) -> None:
    """Send N items via client stream, check aggregated result."""
    stub, server_task = create_chaos_session(nemesis)
    values = [1, 2, 3, 4, 5]
    expected_total = sum(values)

    op_id = history.invoke(
        workload="client_stream", nemesis=nemesis.name,
        service="ChaosSession", method="sum_stream",
        request={"values": values},
    )

    try:
        async def items():
            for v in values:
                yield SumItem(value=v)

        resp = await asyncio.wait_for(stub.sum_stream(items()), timeout=5.0)
        total = resp.total if hasattr(resp, "total") else resp.get("total")
        history.complete(
            op_id, "ok",
            response={"total": total},
            expected={"total": expected_total},
        )
    except asyncio.TimeoutError:
        history.complete(op_id, "timeout", error="timeout")
    except Exception as e:
        history.complete(op_id, "fail", error=str(e))
    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass


async def cancel_workload(nemesis: NemesisBase, history: History) -> None:
    """Start a slow call, send CANCEL, verify CANCELLED trailer arrives."""
    c_send, c_recv, _, server_task, codec = create_chaos_raw_session(nemesis)

    op_id = history.invoke(
        workload="cancel", nemesis=nemesis.name,
        service="ChaosSession", method="__cancel__",
        request={"action": "cancel_slow_increment"},
    )

    try:
        # Send CALL frame for slow_increment (5s delay)
        call_header = CallHeader(
            method="increment",
            callId=1,
        )
        await write_frame(c_send, codec.encode(call_header), flags=CALL)

        # Send request with delay
        await write_frame(c_send, codec.encode(CounterReq(delay=5.0)), flags=0)

        # Wait briefly for the server to start handling
        await asyncio.sleep(0.1)

        # Send CANCEL
        await write_frame(c_send, b"", flags=CANCEL)

        # Read frames until we get a CANCELLED trailer (or timeout)
        got_cancelled = False
        deadline = asyncio.get_event_loop().time() + 5.0

        while asyncio.get_event_loop().time() < deadline:
            try:
                frame = await asyncio.wait_for(read_frame(c_recv), timeout=3.0)
            except (asyncio.TimeoutError, Exception):
                break
            if frame is None:
                break
            payload, flags = frame
            if flags & TRAILER:
                try:
                    status = codec.decode(payload, RpcStatus)
                    if status.code == StatusCode.CANCELLED:
                        got_cancelled = True
                except Exception:
                    pass
                break

        if got_cancelled:
            history.complete(op_id, "cancel")
        else:
            history.complete(op_id, "timeout", error="no CANCELLED trailer received")

    except Exception as e:
        history.complete(op_id, "fail", error=str(e))
    finally:
        server_task.cancel()
        try:
            await server_task
        except (asyncio.CancelledError, Exception):
            pass
