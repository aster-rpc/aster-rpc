"""
Regression test for spec §4.4 (Aster-multiplexed-streams.md): streaming
substreams must bypass the per-connection pool so that concurrent unary
calls on the same connection are not starved by long-running streaming
calls.

Before the fix (`CoreConnection::open_streaming_substream`, 2026-04-13),
every RPC pattern routed through `CoreConnection::acquire_stream`, so
streaming calls counted against `shared_pool_size` (default 8). Holding
8 concurrent server-streams open would block the 9th unary call on
POOL_FULL until `stream_acquire_timeout` (5s default) expired.

This test holds the pool's worth of server-streams open at their first
yield and then fires a unary call with a 2-second wait-for ceiling; if
the fix regresses, the unary will either time out or raise
`StreamAcquireError: POOL_FULL` and the assertion fails.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import AsyncIterator

import pytest

from aster import (
    AsterClient,
    AsterServer,
    rpc,
    server_stream,
    service,
    wire_type,
)


# ── Test wire types ─────────────────────────────────────────────────────────


@wire_type("test.bypass/PingRequest")
@dataclass
class PingRequest:
    payload: str = ""


@wire_type("test.bypass/PingResponse")
@dataclass
class PingResponse:
    reply: str = ""


@wire_type("test.bypass/HoldRequest")
@dataclass
class HoldRequest:
    token: str = ""


@wire_type("test.bypass/HoldItem")
@dataclass
class HoldItem:
    value: int = 0


# ── Test service ────────────────────────────────────────────────────────────


@service(name="StreamingBypassService", version=1)
class StreamingBypassService:
    """Server-stream that yields one item and then blocks forever. The
    test uses this to hold a streaming substream open for the duration
    of a concurrent unary call.
    """

    @rpc()
    async def ping(self, req: PingRequest) -> PingResponse:
        return PingResponse(reply=f"pong:{req.payload}")

    @server_stream()
    async def hold(self, req: HoldRequest) -> AsyncIterator[HoldItem]:
        yield HoldItem(value=1)
        # Sleep for long enough to outlast the unary wait below. The
        # test cancels the iterator before this completes.
        await asyncio.sleep(30)


# ── Test ────────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_streaming_substreams_bypass_the_pool():
    """Hold `shared_pool_size + 1` server-streams open; the unary must
    still complete immediately.

    With the default `shared_pool_size=8`, 9 concurrent streaming calls
    through `acquire_stream` would mean the 9th queues on POOL_FULL.
    With the fix, streaming calls open dedicated substreams via
    `open_streaming_substream` and don't contend for pool slots at all
    -- the unary in this test can grab a slot from an 8-slot SHARED
    pool with 0 occupants.
    """
    N_STREAMS = 9  # shared_pool_size (default 8) + 1

    async with AsterServer(
        services=[StreamingBypassService()],
        allow_all_consumers=True,
    ) as server:
        addr_b64 = server.endpoint_addr_b64

        async with AsterClient(endpoint_addr=addr_b64) as client:
            svc = await client.client(StreamingBypassService)

            # Start N streaming calls concurrently, pull the first item
            # on each so the server-side dispatcher has the substream
            # alive and the client has received its first frame. The
            # iterators are kept in scope until the end of the block so
            # their underlying substreams stay open.
            iterators = [svc.hold(HoldRequest(token=f"s{i}")) for i in range(N_STREAMS)]
            first_items = await asyncio.gather(
                *(anext(it) for it in iterators)
            )
            assert all(item.value == 1 for item in first_items)

            # Now issue a unary call; must complete quickly. Pre-fix this
            # would queue on POOL_FULL for `stream_acquire_timeout` (5s)
            # and then raise.
            resp = await asyncio.wait_for(
                svc.ping(PingRequest(payload="fresh")),
                timeout=2.0,
            )
            assert isinstance(resp, PingResponse)
            assert resp.reply == "pong:fresh"

            # Clean up the streaming iterators so their substreams close
            # before the server shuts down.
            for it in iterators:
                await it.aclose()
