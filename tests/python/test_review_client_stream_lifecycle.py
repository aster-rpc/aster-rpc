"""A real client stream must start before an arbitrary source has finished."""
import asyncio
from dataclasses import dataclass
from typing import AsyncIterator

import pytest
from aster import AsterClient, AsterServer, client_stream, service, wire_type
from aster.config import AsterConfig
from aster.status import RpcError, StatusCode


@wire_type("test.review.clientstream/Item")
@dataclass
class Item:
    value: str = ""


@service(name="ReviewClientStream", version=1)
class Probe:
    calls = 0

    @client_stream
    async def answer(self, requests: AsyncIterator[Item]) -> Item:
        Probe.calls += 1
        async for item in requests:
            if item.value == "reject":
                raise RpcError(StatusCode.PERMISSION_DENIED, "rejected first item")
            if item.value == "wait":
                await asyncio.Event().wait()
            return item
        raise AssertionError("empty input must not dispatch")


@pytest.mark.timeout(20)
@pytest.mark.parametrize("phase", ["success", "reject", "deadline", "cancel"])
async def test_native_client_stream_ends_with_a_parked_source(phase):
    parked, release, cleaned = asyncio.Event(), asyncio.Event(), asyncio.Event()
    tag = "wait" if phase in ("deadline", "cancel") else phase

    async def source():
        try:
            yield Item(tag)
            parked.set()
            try:
                await release.wait()
            except asyncio.CancelledError:
                # Arbitrary application iterators may ignore one cancellation.
                await release.wait()
            yield Item("late")
        finally:
            cleaned.set()

    config = AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0")
    async with AsterServer(services=[Probe()], allow_all_consumers=True, config=config) as server:
        async with AsterClient(address=server.address, config=config) as owner:
            client = await owner.client(Probe)
            call = asyncio.ensure_future(client.answer(source(), timeout=0.2 if phase == "deadline" else 5))
            try:
                await asyncio.wait_for(parked.wait(), 3)
                if phase == "cancel":
                    call.cancel()
                done, _ = await asyncio.wait({call}, timeout=2 if phase == "deadline" else 1)
                assert done, "RPC waited for a parked producer instead of its terminal result"
                if phase == "success":
                    assert (await call).value == "success"
                elif phase == "cancel":
                    with pytest.raises(asyncio.CancelledError):
                        await call
                else:
                    with pytest.raises(RpcError) as error:
                        await call
                    assert error.value.code == (StatusCode.PERMISSION_DENIED if phase == "reject" else StatusCode.DEADLINE_EXCEEDED)
                assert not cleaned.is_set(), "fixture did not retain the independent user wait"
                dispatcher = server._server._dispatcher
                async with asyncio.timeout(2):
                    while dispatcher.in_flight or dispatcher.server.live_calls():
                        await asyncio.sleep(0.01)
            finally:
                release.set()
                if not call.done():
                    call.cancel()
                await asyncio.wait_for(asyncio.gather(call, return_exceptions=True), 3)
                await asyncio.wait_for(cleaned.wait(), 3)

            async def healthy():
                yield Item("healthy")

            assert (await asyncio.wait_for(client.answer(healthy()), 2)).value == "healthy"


async def test_native_local_frame_validation_ends_client_stream_without_dispatch():
    config = AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0")
    async with AsterServer(services=[Probe()], allow_all_consumers=True, config=config) as server:
        async with AsterClient(address=server.address, config=config) as owner:
            client = await owner.client(Probe)
            original_codec = client._transport._codec
            before = Probe.calls

            class InvalidCodec:
                def encode(self, value):
                    return b""

                def decode(self, value):
                    return original_codec.decode(value)

            async def source():
                yield Item("invalid")
                await asyncio.Event().wait()

            client._transport._codec = InvalidCodec()
            try:
                with pytest.raises(RpcError) as error:
                    await asyncio.wait_for(client.answer(source()), 1)
                assert error.value.code == StatusCode.UNKNOWN
                assert error.value.message == "empty payload only allowed with TRAILER, CANCEL or END_STREAM flags"
                dispatcher = server._server._dispatcher
                async with asyncio.timeout(2):
                    while dispatcher.in_flight or dispatcher.server.live_calls():
                        await asyncio.sleep(0.01)
                assert Probe.calls == before
            finally:
                client._transport._codec = original_codec

            async def healthy():
                yield Item("healthy")

            assert (await asyncio.wait_for(client.answer(healthy()), 2)).value == "healthy"
