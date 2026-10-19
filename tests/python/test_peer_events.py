"""``AsterServer.peer_events()`` over real nodes: the binding mapping of
``aster::rpc::peers`` (snapshot, connected, disconnected with a reason).

Binding wiring only; admission, per-peer counting, every reason and the
``Lagged`` behaviour are tested once in ``aster/tests/peer_events.rs``.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass

import pytest

from aster import (
    AsterClient,
    AsterServer,
    PeerConnected,
    PeerDisconnected,
    PeerEvents,
    PeerSnapshot,
    rpc,
    service,
    wire_type,
)

from wait_budget import WAIT_BUDGET


@wire_type("test.peers/Ping")
@dataclass
class Ping:
    text: str = ""


@service(name="PeerPing", version=1)
class PeerPing:
    @rpc()
    async def ping(self, req: Ping) -> Ping:
        return Ping(text=req.text)


async def _next(events: PeerEvents):
    return await asyncio.wait_for(events.__anext__(), WAIT_BUDGET)


@pytest.mark.timeout(60)
async def test_connect_and_close_are_reported():
    async with AsterServer(services=[PeerPing()]) as server:
        snapshot, events = server.peer_events()
        try:
            assert isinstance(snapshot, list)
            assert snapshot == []

            client = AsterClient(endpoint_addr=server.endpoint_addr_b64)
            await client.__aenter__()
            client_id = client._node.node_id()
            try:
                svc = await client.client(PeerPing)
                assert (await svc.ping(Ping(text="hi"))).text == "hi"

                connected = await _next(events)
                assert isinstance(connected, PeerConnected)
                assert connected.peer == client_id
                assert connected.alpn == "aster/1"
                assert connected.identity.endpoint_id == client_id
                assert connected.identity.principal is None
                assert connected.identity.roles == ()

                # A later snapshot has the peer, and the stream after it is empty.
                later, later_events = server.peer_events()
                try:
                    assert [p.peer for p in later] == [client_id]
                    assert isinstance(later[0], PeerSnapshot)
                    assert later[0].connections >= 1
                finally:
                    later_events.close()
            finally:
                await client.__aexit__(None, None, None)

            disconnected = await _next(events)
            assert isinstance(disconnected, PeerDisconnected)
            assert disconnected.peer == client_id
            assert disconnected.reason == "closed"
        finally:
            events.close()


@pytest.mark.timeout(60)
async def test_close_ends_the_stream_and_a_cancelled_receive_consumes_nothing():
    async with AsterServer(services=[PeerPing()]) as server:
        _, events = server.peer_events()
        pending = asyncio.ensure_future(events.__anext__())
        done, _ = await asyncio.wait({pending}, timeout=0.05)  # short-wait-ok: nothing is expected to arrive
        assert not done
        pending.cancel()
        with pytest.raises(asyncio.CancelledError):
            await pending

        async with AsterClient(endpoint_addr=server.endpoint_addr_b64) as client:
            svc = await client.client(PeerPing)
            await svc.ping(Ping(text="x"))
            assert isinstance(await _next(events), PeerConnected)

        events.close()
        with pytest.raises(StopAsyncIteration):
            await _next(events)
