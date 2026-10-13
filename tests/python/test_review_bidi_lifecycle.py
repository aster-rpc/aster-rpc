"""Review regressions: exercise the real channel with a controlled native boundary."""

import asyncio
from dataclasses import dataclass
from typing import AsyncIterator
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest
from aster import AsterClient, AsterServer, bidi_stream, server_stream, service, wire_type
from aster.config import AsterConfig
from aster.status import StatusCode
from aster.transport.iroh import IrohBidiChannel


def channel(native):
    transport = SimpleNamespace(
        _client=native,
        _codec=SimpleNamespace(encode=lambda x: x, decode=lambda x: x),
        _version_of=lambda _: 1,
        _session_id=0,
    )
    return IrohBidiChannel(transport, "Echo", "chat", None, 0, 1)


async def test_review_concurrent_send_recv_open_one_bidi_call():
    entered = asyncio.Event()
    release = asyncio.Event()
    pairs = []

    async def open_call(*args, **kwargs):
        sink = SimpleNamespace(send=AsyncMock(), finish=AsyncMock())
        stream = SimpleNamespace(next_batch=AsyncMock(return_value=[b"reply"]))
        pairs.append((sink, stream))
        entered.set()
        await release.wait()
        return sink, stream

    ch = channel(SimpleNamespace(bidi_open=open_call))
    sending = asyncio.create_task(ch.send(b"request"))
    await entered.wait()
    receiving = asyncio.create_task(ch.recv())
    await asyncio.sleep(0)  # both directions run while the first open is pending
    release.set()
    await asyncio.gather(sending, receiving)
    await ch.close()
    assert len(pairs) == 1, "one logical channel opened multiple independent native RPCs"


async def test_review_close_before_first_send_finishes_empty_request_stream():
    sink = SimpleNamespace(send=AsyncMock(), finish=AsyncMock())
    stream = SimpleNamespace(next_batch=AsyncMock(return_value=[]))
    native = SimpleNamespace(bidi_open=AsyncMock(return_value=(sink, stream)))
    ch = channel(native)
    await ch.close()
    await ch.wait_for_trailer()
    sink.finish.assert_awaited_once()


@wire_type("test.review.bidi/Item")
@dataclass
class Item:
    value: str = ""


@service(name="ReviewBidiNative", version=1)
class NativeProbe:
    def __init__(self):
        self.invocations = 0

    @bidi_stream
    async def echo(self, requests: AsyncIterator[Item]) -> AsyncIterator[Item]:
        self.invocations += 1
        async for item in requests:
            yield Item(item.value)

    @server_stream
    async def empty(self, request: Item) -> AsyncIterator[Item]:
        if False:
            yield request


@pytest.mark.timeout(20)
@pytest.mark.parametrize("phase", ["concurrent_open", "empty_then_recover", "zero_responses"])
async def test_native_bidi_and_empty_stream_lifecycle(phase):
    probe = NativeProbe()
    config = AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0")
    tasks = []
    entered, release = asyncio.Event(), asyncio.Event()
    async with AsterServer(services=[probe], config=config) as server:
        async with AsterClient(address=server.address, config=config) as owner:
            client = await owner.client(NativeProbe)
            native = client._transport._client
            opens = []

            class ObservedNative:
                def __getattr__(self, name):
                    return getattr(native, name)

                async def bidi_open(self, *args, **kwargs):
                    pair = await native.bidi_open(*args, **kwargs)
                    opens.append(pair)
                    entered.set()
                    await release.wait()
                    return pair

            try:
                if phase == "concurrent_open":
                    client._transport._client_obj = ObservedNative()
                    channel = client.echo()
                    sending = asyncio.create_task(channel.send(Item("one")))
                    tasks.append(sending)
                    await asyncio.wait_for(entered.wait(), 3)
                    receiving = asyncio.create_task(channel.recv())
                    tasks.append(receiving)
                    await asyncio.sleep(0)
                    release.set()
                    await asyncio.wait_for(sending, 3)
                    assert (await asyncio.wait_for(receiving, 3)).value == "one"
                    await channel.close()
                    await channel.close()
                    assert (await asyncio.wait_for(channel.wait_for_trailer(), 3))[0] == StatusCode.OK
                    assert len(opens) == 1 and probe.invocations == 1
                elif phase == "empty_then_recover":
                    channel = client.echo()
                    await channel.close()
                    await channel.close()
                    trailer = await asyncio.wait_for(channel.wait_for_trailer(), 3)
                    assert trailer[0] in (StatusCode.INVALID_ARGUMENT, StatusCode.CANCELLED)
                    assert probe.invocations == 0
                else:
                    assert [item async for item in client.empty(Item("unused"))] == []

                # A new native call proves empty/error completion did not poison
                # the connection or retain admission. No mocked response path.
                client._transport._client_obj = native
                recovered = client.echo()
                await recovered.send(Item("healthy"))
                await recovered.close()
                assert (await asyncio.wait_for(recovered.recv(), 3)).value == "healthy"
                assert (await asyncio.wait_for(recovered.wait_for_trailer(), 3))[0] == StatusCode.OK
                dispatcher = server._server._dispatcher
                async with asyncio.timeout(3):
                    while dispatcher.in_flight or dispatcher.server.live_calls():
                        await asyncio.sleep(0.01)
            finally:
                release.set()
                client._transport._client_obj = native
                for task in tasks:
                    if not task.done():
                        task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)
