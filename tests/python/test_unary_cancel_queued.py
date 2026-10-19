"""A unary call cancelled while queued for a pool slot never reaches the server.

asyncio's cancellation reaches a native future a loop iteration late, long
enough for a queued call whose slot frees to be dequeued and sent. The
transport therefore passes a synchronous cancel token to the native call.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass

import pytest

from aster import AsterClient, AsterServer, rpc, service, wire_type

from aster._aster import CancelToken
from aster.status import RpcError, StatusCode

from wait_budget import WAIT_BUDGET


@wire_type("test.cancelq/Req")
@dataclass
class Req:
    tag: str = ""


@wire_type("test.cancelq/Resp")
@dataclass
class Resp:
    tag: str = ""


@service(name="CancelQueuedService", version=1)
class CancelQueuedService:
    def __init__(self) -> None:
        self.seen: list[str] = []
        self.holder_in = asyncio.Event()
        self.release = asyncio.Event()

    @rpc()
    async def call(self, req: Req) -> Resp:
        self.seen.append(req.tag)
        if req.tag == "holder":
            self.holder_in.set()
            await self.release.wait()
        return Resp(tag=req.tag)


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_cancelled_queued_unary_never_reaches_the_server():
    impl = CancelQueuedService()
    async with AsterServer(services=[impl]) as server:
        async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as client:
            svc = await client.client(CancelQueuedService)
            native = svc._transport._client
            native.shared_pool_size = 1

            holder = asyncio.ensure_future(svc.call(Req(tag="holder")))
            await asyncio.wait_for(impl.holder_in.wait(), WAIT_BUDGET)

            # The only pool slot is held, so this call queues in the pool.
            queued = asyncio.ensure_future(svc.call(Req(tag="queued")))
            # Let it reach the native queue: yield until the task has run
            # its synchronous prefix and parked on the pending native future.
            for _ in range(20):
                await asyncio.sleep(0)
            assert not queued.done()

            queued.cancel()
            # Free the slot in the same loop iteration as the cancel, the
            # window in which a late cancel loses the race.
            impl.release.set()

            with pytest.raises(asyncio.CancelledError):
                await queued
            assert (await asyncio.wait_for(holder, WAIT_BUDGET)).tag == "holder"

            # The pool is reusable, and the cancelled call was never sent.
            after = await asyncio.wait_for(svc.call(Req(tag="after")), WAIT_BUDGET)
            assert after.tag == "after"
            assert impl.seen == ["holder", "after"]


@pytest.mark.asyncio
@pytest.mark.timeout(60)
async def test_cancel_token_wins_over_a_ready_call():
    """A token cancelled before the call polls beats a call that could run at
    once (free pool slot): the cancel is checked first, so nothing is sent."""
    impl = CancelQueuedService()
    async with AsterServer(services=[impl]) as server:
        async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as client:
            svc = await client.client(CancelQueuedService)
            transport = svc._transport
            token = CancelToken()
            token.cancel()
            with pytest.raises(RpcError) as raised:
                await asyncio.wait_for(
                    transport._client.unary(
                        "CancelQueuedService",
                        "call",
                        transport._codec.encode(Req(tag="precancelled")),
                        cancel_token=token,
                    ),
                    WAIT_BUDGET,
                )
            assert raised.value.code == StatusCode.CANCELLED

            after = await asyncio.wait_for(svc.call(Req(tag="after")), WAIT_BUDGET)
            assert after.tag == "after"
            assert impl.seen == ["after"]
