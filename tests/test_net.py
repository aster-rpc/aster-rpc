"""Phase 3: Net / QUIC layer tests."""
import asyncio
import pytest
import pytest_asyncio

from iroh_python import create_endpoint, IrohConnection, IrohSendStream, IrohRecvStream

ALPN = b"test/echo/1"


@pytest_asyncio.fixture
async def endpoint_pair():
    """Create two bare endpoints that can connect to each other."""
    ep_server = await create_endpoint(ALPN)
    ep_client = await create_endpoint(ALPN)
    yield ep_server, ep_client
    # cleanup — close endpoints
    # (bare endpoints don't have a shutdown method; they'll be dropped)


@pytest.mark.asyncio
async def test_create_endpoint():
    """Bare endpoint can be created and has a non-empty ID."""
    ep = await create_endpoint(ALPN)
    eid = ep.endpoint_id()
    assert len(eid) > 0, "endpoint ID should be non-empty"


@pytest.mark.asyncio
async def test_bistream_echo(endpoint_pair):
    """Two-node echo: client sends data, server echoes it back via bi-stream."""
    ep_server, ep_client = endpoint_pair
    server_id = ep_server.endpoint_id()
    payload = b"Hello QUIC from Python!"
    echo_result = {}

    async def server_side():
        try:
            conn = await ep_server.accept()
            send, recv = await conn.accept_bi()
            data = await recv.read_to_end(65536)
            await send.write_all(data)
            await send.finish()
            # Keep connection alive until client reads the echo
            await asyncio.sleep(2)
        except Exception as e:
            echo_result["server_error"] = e
            raise

    async def client_side():
        # Small delay to let server start accepting
        await asyncio.sleep(0.5)
        conn = await ep_client.connect(server_id, ALPN)
        send, recv = await conn.open_bi()
        await send.write_all(payload)
        await send.finish()
        echo = await recv.read_to_end(65536)
        echo_result["echo"] = echo

    results = await asyncio.wait_for(
        asyncio.gather(server_side(), client_side(), return_exceptions=True),
        timeout=30,
    )
    # Check for errors from either side
    for r in results:
        if isinstance(r, Exception):
            raise r

    assert echo_result.get("echo") == payload, f"Expected {payload!r}, got {echo_result!r}"


@pytest.mark.asyncio
async def test_connection_remote_id(endpoint_pair):
    """The connection reports the correct remote endpoint ID."""
    ep_server, ep_client = endpoint_pair
    server_id = ep_server.endpoint_id()
    client_id = ep_client.endpoint_id()

    async def server_side():
        conn = await ep_server.accept()
        rid = conn.remote_id()
        # Server sees the client's ID
        assert rid == client_id, f"Server expected {client_id}, got {rid}"
        # Open and close a stream to unblock the client
        send, recv = await conn.accept_bi()
        await recv.read_to_end(1024)
        await send.write_all(b"ok")
        await send.finish()
        # Keep connection alive until client reads
        await asyncio.sleep(2)

    async def client_side():
        await asyncio.sleep(0.5)
        conn = await ep_client.connect(server_id, ALPN)
        rid = conn.remote_id()
        assert rid == server_id, f"Client expected {server_id}, got {rid}"
        send, recv = await conn.open_bi()
        await send.write_all(b"hi")
        await send.finish()
        await recv.read_to_end(1024)

    results = await asyncio.wait_for(
        asyncio.gather(server_side(), client_side(), return_exceptions=True),
        timeout=30,
    )
    for r in results:
        if isinstance(r, Exception):
            raise r
