"""Phase 3: Net / QUIC layer tests."""
import asyncio
import struct

import pytest
import pytest_asyncio

from iroh_python import (
    NodeAddr,
    create_endpoint,
)

ALPN = b"test/echo/1"


@pytest_asyncio.fixture
async def endpoint_pair():
    ep_server = await create_endpoint(ALPN)
    ep_client = await create_endpoint(ALPN)
    yield ep_server, ep_client


@pytest.mark.asyncio
async def test_create_endpoint():
    ep = await create_endpoint(ALPN)
    assert ep.endpoint_id()


@pytest.mark.asyncio
async def test_bistream_echo(endpoint_pair):
    ep_server, ep_client = endpoint_pair
    payload = b"Hello QUIC from Python!"
    echo_result = {}

    async def server_side():
        conn = await ep_server.accept()
        send, recv = await conn.accept_bi()
        data = await recv.read_to_end(65536)
        await send.write_all(data)
        await send.finish()
        await asyncio.sleep(2)

    async def client_side():
        await asyncio.sleep(0.5)
        conn = await ep_client.connect(ep_server.endpoint_id(), ALPN)
        send, recv = await conn.open_bi()
        await send.write_all(payload)
        await send.finish()
        echo_result["echo"] = await recv.read_to_end(65536)

    await asyncio.wait_for(asyncio.gather(server_side(), client_side()), timeout=30)
    assert echo_result["echo"] == payload


@pytest.mark.asyncio
async def test_connection_remote_id(endpoint_pair):
    ep_server, ep_client = endpoint_pair

    async def server_side():
        conn = await ep_server.accept()
        assert conn.remote_id() == ep_client.endpoint_id()
        send, recv = await conn.accept_bi()
        await recv.read_to_end(1024)
        await send.write_all(b"ok")
        await send.finish()
        await asyncio.sleep(2)

    async def client_side():
        await asyncio.sleep(0.5)
        conn = await ep_client.connect(ep_server.endpoint_id(), ALPN)
        assert conn.remote_id() == ep_server.endpoint_id()
        send, recv = await conn.open_bi()
        await send.write_all(b"hi")
        await send.finish()
        await recv.read_to_end(1024)

    await asyncio.wait_for(asyncio.gather(server_side(), client_side()), timeout=30)


@pytest.mark.asyncio
async def test_connect_node_addr_and_read_primitives(endpoint_pair):
    ep_server, ep_client = endpoint_pair
    payload = b"hello framed world"
    server_done = asyncio.Event()

    async def server_side():
        conn = await ep_server.accept()
        recv = await conn.accept_uni()
        size = await recv.read_exact(4)
        expected = struct.unpack(">I", size)[0]
        body = await recv.read(expected)
        assert body == payload
        assert await recv.read(1) is None
        server_done.set()

    async def client_side():
        await asyncio.sleep(0.2)
        conn = await ep_client.connect_node_addr(ep_server.endpoint_addr_info(), ALPN)
        send = await conn.open_uni()
        await send.write_all(struct.pack(">I", len(payload)))
        await send.write_all(payload)
        await send.finish()
        # Keep connection alive until server has fully read
        await server_done.wait()

    await asyncio.wait_for(asyncio.gather(server_side(), client_side()), timeout=30)


@pytest.mark.asyncio
async def test_stream_stop_and_stopped(endpoint_pair):
    ep_server, ep_client = endpoint_pair
    result = {}

    async def server_side():
        conn = await ep_server.accept()
        recv = await conn.accept_uni()
        assert await recv.read_exact(1) == b"x"
        recv.stop(23)
        await asyncio.sleep(1)

    async def client_side():
        await asyncio.sleep(0.2)
        conn = await ep_client.connect(ep_server.endpoint_id(), ALPN)
        send = await conn.open_uni()
        await send.write_all(b"x")
        result["code"] = await send.stopped()

    await asyncio.wait_for(asyncio.gather(server_side(), client_side()), timeout=30)
    assert result["code"] == 23


@pytest.mark.asyncio
async def test_connection_close_smoke(endpoint_pair):
    ep_server, ep_client = endpoint_pair

    async def server_side():
        conn = await ep_server.accept()
        recv = await conn.accept_uni()
        assert await recv.read_exact(1) == b"x"

    async def client_side():
        await asyncio.sleep(0.2)
        conn = await ep_client.connect(ep_server.endpoint_id(), ALPN)
        send = await conn.open_uni()
        await send.write_all(b"x")
        await send.finish()
        await asyncio.sleep(0.5)
        conn.close(7, b"bye")

    await asyncio.wait_for(asyncio.gather(server_side(), client_side()), timeout=30)


@pytest.mark.asyncio
async def test_node_addr_manual_roundtrip():
    addr = NodeAddr("node123", relay_url=None, direct_addresses=["127.0.0.1:9999"])
    assert NodeAddr.from_bytes(addr.to_bytes()).direct_addresses == ["127.0.0.1:9999"]
