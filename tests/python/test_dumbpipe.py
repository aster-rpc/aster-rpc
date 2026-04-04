"""Tests for the Python dumbpipe implementation.

These tests verify that:
1. The Python dumbpipe can communicate with itself (Python↔Python)
2. The protocol (ALPN, handshake) matches the Rust dumbpipe for network compatibility
3. All modes work: direct pipe, TCP forwarding, Unix socket forwarding
"""

import asyncio
import os
import sys
import tempfile

import pytest

# Import from our local dumbpipe module
from dumbpipe import (
    ALPN,
    HANDSHAKE,
    create_listener,
    accept_pipe,
    connect_pipe,
    send_handshake,
    recv_handshake,
    pipe_streams,
    listen_tcp,
    connect_tcp,
    listen_unix,
    connect_unix,
)
from aster_python import create_endpoint, NodeAddr

pytestmark = pytest.mark.asyncio


# ============================================================================
# Core Pipe Tests (Python ↔ Python)
# ============================================================================


async def test_simple_pipe_send_recv():
    """Connector sends data, listener receives it."""
    ep_listen, addr = await create_listener()

    async def listener_side():
        send, recv = await accept_pipe(ep_listen)
        data = await recv.read_to_end(65536)
        return data

    async def connector_side():
        ep_conn, send, recv = await connect_pipe(addr)
        await send.write_all(b"Hello from Python dumbpipe!")
        await send.finish()
        # Wait for listener to finish reading before closing the endpoint.
        # Closing immediately after finish() can race with in-flight delivery,
        # causing "connection lost" on the receiver side in slow environments.
        await asyncio.sleep(0.5)
        await ep_conn.close()

    listener_task = asyncio.create_task(listener_side())
    await asyncio.sleep(1.0)
    await connector_side()

    result = await asyncio.wait_for(listener_task, timeout=30)
    assert result == b"Hello from Python dumbpipe!"
    await ep_listen.close()


async def test_bidirectional_pipe():
    """Both sides can send and receive data."""
    ep_listen, addr = await create_listener()
    results = {}

    async def listener_side():
        send, recv = await accept_pipe(ep_listen)
        # Send a response
        await send.write_all(b"pong")
        await send.finish()
        # Read what connector sent
        data = await recv.read_to_end(65536)
        results["listener_received"] = data

    async def connector_side():
        ep_conn, send, recv = await connect_pipe(addr)
        # Send data
        await send.write_all(b"ping")
        await send.finish()
        # Read response
        data = await recv.read_to_end(65536)
        results["connector_received"] = data
        await ep_conn.close()

    await asyncio.wait_for(
        asyncio.gather(listener_side(), _delayed(connector_side, 0.3)),
        timeout=30,
    )
    assert results["listener_received"] == b"ping"
    assert results["connector_received"] == b"pong"
    await ep_listen.close()


async def _delayed(coro_fn, delay):
    """Helper: wait then run a coroutine function."""
    await asyncio.sleep(delay)
    return await coro_fn()


async def test_empty_payload():
    """Empty payload should work — just handshake + finish."""
    ep_listen, addr = await create_listener()

    async def listener_side():
        send, recv = await accept_pipe(ep_listen)
        data = await recv.read_to_end(65536)
        await send.finish()
        return data

    async def connector_side():
        ep_conn, send, recv = await connect_pipe(addr)
        await send.finish()
        await ep_conn.close()

    listener_task = asyncio.create_task(listener_side())
    await asyncio.sleep(1.0)
    await connector_side()

    result = await asyncio.wait_for(listener_task, timeout=30)
    assert result == b""
    await ep_listen.close()


async def test_large_payload():
    """Transfer a larger payload (1 MB) through the pipe."""
    ep_listen, addr = await create_listener()
    payload = os.urandom(1024 * 1024)  # 1 MB

    async def listener_side():
        send, recv = await accept_pipe(ep_listen)
        data = await recv.read_to_end(2 * 1024 * 1024)
        await send.finish()
        return data

    async def connector_side():
        ep_conn, send, recv = await connect_pipe(addr)
        await send.write_all(payload)
        await send.finish()
        # Wait for listener to finish reading before closing the endpoint.
        # Closing immediately after finish() can race with in-flight delivery,
        # causing "connection lost" on the receiver side in slow environments.
        await asyncio.sleep(1.0)
        await ep_conn.close()

    listener_task = asyncio.create_task(listener_side())
    await asyncio.sleep(1.0)
    await connector_side()

    result = await asyncio.wait_for(listener_task, timeout=60)
    assert result == payload
    await ep_listen.close()


# ============================================================================
# Handshake Validation
# ============================================================================


async def test_wrong_handshake_rejected():
    """Listener should reject a connection with a wrong handshake."""
    ep = await create_endpoint(ALPN)
    addr = ep.endpoint_addr_info()

    async def listener_side():
        conn = await ep.accept()
        send, recv = await conn.accept_bi()
        with pytest.raises(ValueError, match="unexpected handshake"):
            await recv_handshake(recv)

    async def bad_connector():
        ep2 = await create_endpoint(ALPN)
        conn = await ep2.connect_node_addr(addr, ALPN)
        send, recv = await conn.open_bi()
        # Send wrong handshake
        await send.write_all(b"wrong")
        await send.finish()
        await ep2.close()

    await asyncio.wait_for(
        asyncio.gather(listener_side(), _delayed(bad_connector, 0.3)),
        timeout=30,
    )
    await ep.close()


# ============================================================================
# Raw QUIC interop — verify a raw endpoint can talk dumbpipe protocol
# ============================================================================


async def test_raw_quic_to_dumbpipe():
    """A raw QUIC endpoint using the correct ALPN + handshake can talk to dumbpipe."""
    ep_listen, addr = await create_listener()

    async def listener_side():
        send, recv = await accept_pipe(ep_listen)
        data = await recv.read_to_end(65536)
        await send.write_all(b"echo:" + data)
        await send.finish()

    async def raw_client():
        ep_raw = await create_endpoint(ALPN)
        conn = await ep_raw.connect_node_addr(addr, ALPN)
        send, recv = await conn.open_bi()
        # Manually do the dumbpipe handshake
        await send.write_all(b"hello")
        await send.write_all(b"test data")
        await send.finish()
        response = await recv.read_to_end(65536)
        # Wait before closing to avoid race with in-flight delivery
        await asyncio.sleep(0.5)
        await ep_raw.close()
        return response

    listener_task = asyncio.create_task(listener_side())
    await asyncio.sleep(1.0)
    response = await asyncio.wait_for(raw_client(), timeout=30)
    await listener_task
    assert response == b"echo:test data"
    await ep_listen.close()


# ============================================================================
# TCP Forwarding Tests
# ============================================================================


async def test_tcp_forwarding():
    """Test TCP listen/connect forwarding through dumbpipe."""
    # Start a simple TCP echo server
    async def echo_handler(reader, writer):
        data = await reader.read(65536)
        writer.write(b"echo:" + data)
        await writer.drain()
        writer.close()

    echo_server = await asyncio.start_server(echo_handler, "127.0.0.1", 0)
    echo_port = echo_server.sockets[0].getsockname()[1]

    # Create dumbpipe listener forwarding to echo server
    ep_listen = await create_endpoint(ALPN)
    listen_addr = ep_listen.endpoint_addr_info()

    async def dumbpipe_listener():
        """Accept one QUIC connection, one bi-stream, forward to TCP echo."""
        conn = await ep_listen.accept()
        send, recv = await conn.accept_bi()
        await recv_handshake(recv)
        tcp_reader, tcp_writer = await asyncio.open_connection("127.0.0.1", echo_port)
        await pipe_streams(send, recv, tcp_reader, tcp_writer)

    # Create dumbpipe connector
    async def dumbpipe_connector():
        ep_conn = await create_endpoint(ALPN)
        conn = await ep_conn.connect_node_addr(listen_addr, ALPN)
        send, recv = await conn.open_bi()
        await send_handshake(send)
        await send.write_all(b"forwarded data")
        await send.finish()
        result = await recv.read_to_end(65536)
        await ep_conn.close()
        return result

    listener_task = asyncio.create_task(dumbpipe_listener())
    await asyncio.sleep(1.0)
    result = await asyncio.wait_for(dumbpipe_connector(), timeout=30)
    await listener_task

    assert result == b"echo:forwarded data"
    echo_server.close()
    await ep_listen.close()


# ============================================================================
# Unix Socket Forwarding Tests
# ============================================================================


@pytest.mark.skipif(sys.platform == "win32", reason="Unix sockets not supported on Windows")
async def test_unix_socket_forwarding():
    """Test Unix socket forwarding through dumbpipe."""
    with tempfile.TemporaryDirectory() as tmpdir:
        echo_sock = os.path.join(tmpdir, "echo.sock")

        # Start a simple Unix socket echo server
        async def echo_handler(reader, writer):
            data = await reader.read(65536)
            writer.write(b"unix-echo:" + data)
            await writer.drain()
            writer.close()

        echo_server = await asyncio.start_unix_server(echo_handler, path=echo_sock)

        # Create dumbpipe listener forwarding to Unix echo
        ep_listen = await create_endpoint(ALPN)
        listen_addr = ep_listen.endpoint_addr_info()

        async def dumbpipe_listener():
            conn = await ep_listen.accept()
            send, recv = await conn.accept_bi()
            await recv_handshake(recv)
            unix_reader, unix_writer = await asyncio.open_unix_connection(echo_sock)
            await pipe_streams(send, recv, unix_reader, unix_writer)

        async def dumbpipe_connector():
            ep_conn = await create_endpoint(ALPN)
            conn = await ep_conn.connect_node_addr(listen_addr, ALPN)
            send, recv = await conn.open_bi()
            await send_handshake(send)
            await send.write_all(b"unix forwarded")
            await send.finish()
            result = await recv.read_to_end(65536)
            await ep_conn.close()
            return result

        listener_task = asyncio.create_task(dumbpipe_listener())
        await asyncio.sleep(1.0)
        result = await asyncio.wait_for(dumbpipe_connector(), timeout=30)
        await listener_task

        assert result == b"unix-echo:unix forwarded"
        echo_server.close()
        await ep_listen.close()


# ============================================================================
# Multiple streams over one connection
# ============================================================================


async def test_multiple_streams():
    """Multiple bi-streams over a single QUIC connection (like listen-tcp mode)."""
    ep_listen = await create_endpoint(ALPN)
    listen_addr = ep_listen.endpoint_addr_info()
    results = []

    async def listener_side():
        conn = await ep_listen.accept()
        for _ in range(3):
            send, recv = await conn.accept_bi()
            await recv_handshake(recv)
            data = await recv.read_to_end(65536)
            results.append(data)
            await send.write_all(b"ack:" + data)
            await send.finish()

    async def connector_side():
        ep_conn = await create_endpoint(ALPN)
        conn = await ep_conn.connect_node_addr(listen_addr, ALPN)
        for i in range(3):
            send, recv = await conn.open_bi()
            await send_handshake(send)
            await send.write_all(f"msg{i}".encode())
            await send.finish()
            ack = await recv.read_to_end(65536)
            assert ack == f"ack:msg{i}".encode()
        await ep_conn.close()

    await asyncio.wait_for(
        asyncio.gather(listener_side(), _delayed(connector_side, 0.3)),
        timeout=30,
    )
    assert len(results) == 3
    assert results[0] == b"msg0"
    assert results[1] == b"msg1"
    assert results[2] == b"msg2"
    await ep_listen.close()