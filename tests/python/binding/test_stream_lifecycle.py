"""
Binding-layer tests for stream and connection object lifecycle.

These tests verify PyO3 object behaviour that protocol-level tests don't cover:
  - Streams and connections can be opened, used, and closed without panicking
  - Write after finish raises a clean Python exception (not a Rust panic)
  - Read from a finished stream returns None / raises cleanly
  - close() is idempotent (double-close doesn't panic)
  - remote_id() returns a non-empty string

All tests use real in-process QUIC endpoints (loopback) to exercise the
actual PyO3 wrapper code paths.
"""

import asyncio
import pytest
import pytest_asyncio

from aster import create_endpoint, IrohError, IrohConnection, IrohSendStream, IrohRecvStream


ALPN = b"test/binding/streams/1"


@pytest_asyncio.fixture
async def connected_pair():
    """Two endpoints with an established connection and one open bi-stream.

    In QUIC, a stream is not visible to the receiver until data is transmitted
    on it.  We therefore run both sides concurrently: the connector sends a
    one-byte probe so the listener's accept_bi() fires, then both sides await
    the handshake synchronisation event before being handed to the test.
    """
    ep_listen = await create_endpoint(ALPN)
    listen_addr = ep_listen.endpoint_addr_info()

    # Shared state populated by the listener half.
    _server: dict = {}
    # Signal that the listener has its stream and is ready.
    listener_ready = asyncio.Event()

    async def run_listener():
        conn = await ep_listen.accept()
        send, recv = await conn.accept_bi()
        _server["conn"] = conn
        _server["send"] = send
        _server["recv"] = recv
        listener_ready.set()

    listener_task = asyncio.create_task(run_listener())

    ep_conn = await create_endpoint(ALPN)
    conn = await ep_conn.connect_node_addr(listen_addr, ALPN)
    send, recv = await conn.open_bi()

    # Send one byte so the server sees the stream and accept_bi() fires.
    await send.write_all(b"\x00")

    await asyncio.wait_for(listener_ready.wait(), timeout=30)
    await listener_task

    yield (
        conn, send, recv,
        _server["conn"], _server["send"], _server["recv"],
    )

    await ep_conn.close()
    await ep_listen.close()


# ---------------------------------------------------------------------------
# IrohSendStream
# ---------------------------------------------------------------------------

async def test_write_and_finish(connected_pair):
    conn, send, recv, rconn, rsend, rrecv = connected_pair
    # The fixture already sent a probe byte (\x00) to trigger accept_bi().
    # Write more data, finish, and read everything on the server side.
    await send.write_all(b"hello")
    await send.finish()
    data = await rrecv.read_to_end(1024)
    assert data == b"\x00hello"


async def test_write_after_finish_raises(connected_pair):
    conn, send, recv, rconn, rsend, rrecv = connected_pair
    await send.finish()
    with pytest.raises((IrohError, Exception)):
        await send.write_all(b"too late")


async def test_finish_is_idempotent(connected_pair):
    """Calling finish() twice should not panic -- at most raise a clean error."""
    conn, send, recv, rconn, rsend, rrecv = connected_pair
    await send.finish()
    try:
        await send.finish()
    except (IrohError, Exception):
        pass  # acceptable -- what's NOT acceptable is a Rust panic


# ---------------------------------------------------------------------------
# IrohRecvStream
# ---------------------------------------------------------------------------

async def test_read_returns_none_at_end(connected_pair):
    conn, send, recv, rconn, rsend, rrecv = connected_pair
    await rsend.finish()
    result = await recv.read(1024)
    assert result is None


async def test_read_exact_raises_on_short_stream(connected_pair):
    conn, send, recv, rconn, rsend, rrecv = connected_pair
    await rsend.write_all(b"hi")
    await rsend.finish()
    with pytest.raises((IrohError, Exception)):
        await recv.read_exact(100)


async def test_stop_does_not_panic(connected_pair):
    """stop() should not cause a Rust panic regardless of stream state."""
    conn, send, recv, rconn, rsend, rrecv = connected_pair
    try:
        recv.stop(0)
    except (IrohError, Exception):
        pass  # clean Python exception is fine


# ---------------------------------------------------------------------------
# IrohConnection
# ---------------------------------------------------------------------------

async def test_remote_id_is_nonempty_string(connected_pair):
    conn, send, recv, rconn, rsend, rrecv = connected_pair
    rid = conn.remote_id()
    assert isinstance(rid, str)
    assert len(rid) > 0


async def test_connection_close_is_clean(connected_pair):
    conn, send, recv, rconn, rsend, rrecv = connected_pair
    conn.close(0, b"bye")


async def test_connection_double_close_does_not_panic(connected_pair):
    conn, send, recv, rconn, rsend, rrecv = connected_pair
    conn.close(0, b"first")
    try:
        conn.close(0, b"second")
    except (IrohError, Exception):
        pass  # clean error is acceptable; panic is not


# ---------------------------------------------------------------------------
# NetClient (endpoint) lifecycle
# ---------------------------------------------------------------------------

async def test_endpoint_close_is_clean():
    ep = await create_endpoint(ALPN)
    await ep.close()


async def test_endpoint_addr_info_returns_node_addr():
    from aster import NodeAddr
    ep = await create_endpoint(ALPN)
    addr = ep.endpoint_addr_info()
    assert isinstance(addr, NodeAddr)
    assert len(addr.endpoint_id) > 0
    await ep.close()
