"""Incremental client-stream ownership tests without native scheduling."""
import asyncio
import traceback
from types import SimpleNamespace

import pytest

from aster.client import ServiceClient
from aster.status import RpcError, StatusCode
from aster.transport.base import ConnectionLostError, TransportError
from aster.transport.iroh import IrohTransport


class Codec:
    def __init__(self, error=None):
        self.error = error

    def encode(self, value):
        if self.error is not None:
            raise self.error
        return b"request"

    def decode(self, value):
        return value


class Sink:
    def __init__(self, error=None):
        self.error = error
        self.cancelled = asyncio.Event()
        self.sends = 0
        self.finishes = 0

    async def send(self, payload):
        self.sends += 1
        if self.error is not None:
            raise self.error

    async def finish(self):
        self.finishes += 1

    async def cancel(self):
        self.cancelled.set()


class Native:
    def __init__(self, sink, response):
        self.sink = sink
        self.response = response

    async def client_stream_open(self, *args, **kwargs):
        return self.sink, self

    async def result(self):
        return await self.response


class Transport(IrohTransport):
    @property
    def _client(self):
        return self.native


def transport(codec=None, send_error=None):
    result = asyncio.get_running_loop().create_future()
    sink = Sink(send_error)
    value = Transport(None, codec or Codec())
    value.native = Native(sink, result)
    return value, sink, result


@pytest.mark.parametrize("phase", ["source", "encode", "native_validation"])
async def test_local_failure_does_not_wait_for_parked_native_response(phase):
    error = ConnectionLostError("frame must not be empty") if phase == "native_validation" else ValueError(phase)
    value, sink, response = transport(
        Codec(error if phase == "encode" else None),
        error if phase == "native_validation" else None,
    )

    async def source():
        if phase == "source":
            raise error
        yield "first"

    with pytest.raises(type(error)) as caught:
        await asyncio.wait_for(value.client_stream("service", "method", source()), 1)
    assert caught.value is error
    await asyncio.wait_for(sink.cancelled.wait(), 1)
    await asyncio.sleep(0)
    assert response.cancelled()
    assert sink.finishes == 0


async def test_empty_source_refuses_without_finishing_or_waiting_for_native():
    value, sink, response = transport()

    async def source():
        if False:
            yield None

    with pytest.raises(TransportError, match="at least one request frame"):
        await asyncio.wait_for(value.client_stream("service", "method", source()), 1)
    await asyncio.wait_for(sink.cancelled.wait(), 1)
    assert sink.sends == sink.finishes == 0
    assert response.cancelled()


@pytest.mark.parametrize("through_service_client", [False, True])
async def test_cleared_error_traceback_cannot_break_owned_deferred_cleanup(through_service_client):
    value, sink, response = transport()
    parked, release, cleaned = asyncio.Event(), asyncio.Event(), asyncio.Event()

    async def source():
        try:
            yield "first"
            parked.set()
            try:
                await release.wait()
            except asyncio.CancelledError:
                await release.wait()
            yield "late"
        finally:
            cleaned.set()

    if through_service_client:
        owner = ServiceClient.__new__(ServiceClient)
        owner._transport = value
        owner._service_info = SimpleNamespace(name="service")
        owner._interceptors = []
        owner._request_cleanup_tasks = set()
        owner._get_deadline = lambda timeout: 0
        owner._resolve_serialization_mode = lambda *args: 0
        owner._build_context = lambda *args, **kwargs: SimpleNamespace(metadata={})

        async def invoke_direct(context, request, invoke, **kwargs):
            return await invoke(request)

        owner._run_call_with_interceptors = invoke_direct
        operation = owner._call_client_stream(SimpleNamespace(name="method"), source())
    else:
        operation = value.client_stream("service", "method", source())
    call = asyncio.create_task(operation)
    try:
        await asyncio.wait_for(parked.wait(), 1)
        response.set_exception(RpcError(StatusCode.PERMISSION_DENIED, "rejected"))
        try:
            await asyncio.wait_for(call, 1)
        except RpcError as error:
            traceback.clear_frames(error.__traceback__)
        else:
            pytest.fail("native rejection did not reach the public call")
        assert not cleaned.is_set()
    finally:
        release.set()
        await asyncio.wait_for(cleaned.wait(), 1)
    await asyncio.wait_for(sink.cancelled.wait(), 1)
    async with asyncio.timeout(1):
        while value._stream_tasks:
            await asyncio.sleep(0)
    assert sink.sends == 1
    assert sink.finishes == 0


async def test_finite_source_finishes_once_without_extra_aclose():
    value, sink, response = transport()

    class FiniteSource:
        def __init__(self):
            self.items = iter(["first", "second"])
            self.closes = 0

        def __aiter__(self):
            return self

        async def __anext__(self):
            try:
                return next(self.items)
            except StopIteration:
                raise StopAsyncIteration

        async def aclose(self):
            self.closes += 1

    async def finish():
        sink.finishes += 1
        response.set_result(b"response")

    sink.finish = finish
    source = FiniteSource()
    assert await asyncio.wait_for(value.client_stream("service", "method", source), 1) == b"response"
    assert sink.sends == 2
    assert sink.finishes == 1
    assert source.closes == 0
