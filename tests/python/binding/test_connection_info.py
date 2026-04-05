"""
Binding-layer tests for ConnectionInfo and RemoteInfo.

Verifies that the PyO3 structs expose the expected fields with the right
types.  A renamed field or a type mismatch in the Rust From impl would
fail here before any higher-level test would catch it.
"""

import asyncio
import pytest
import pytest_asyncio

from aster_python import create_endpoint, IrohError


ALPN = b"test/binding/conninfo/1"


@pytest_asyncio.fixture
async def live_connection():
    """An established QUIC connection with monitoring enabled."""
    from aster_python import EndpointConfig, create_endpoint_with_config

    cfg = EndpointConfig(alpns=[ALPN], enable_monitoring=True)
    ep_listen = await create_endpoint_with_config(cfg)
    listen_addr = ep_listen.endpoint_addr_info()

    ready = asyncio.Event()
    _state = {}

    async def listener():
        ready.set()
        conn = await ep_listen.accept()
        _state["server_conn"] = conn

    task = asyncio.create_task(listener())
    await asyncio.wait_for(ready.wait(), timeout=5)

    ep_conn = await create_endpoint_with_config(cfg)
    conn = await ep_conn.connect_node_addr(listen_addr, ALPN)
    await asyncio.wait_for(task, timeout=30)

    yield conn, ep_listen, ep_conn

    await ep_conn.close()
    await ep_listen.close()


# ---------------------------------------------------------------------------
# ConnectionInfo field types
# ---------------------------------------------------------------------------

async def test_connection_info_fields(live_connection):
    conn, ep_listen, ep_conn = live_connection
    info = conn.connection_info()  # synchronous
    assert isinstance(info.connection_type, str)
    assert len(info.connection_type) > 0
    assert isinstance(info.bytes_sent, int)
    assert isinstance(info.bytes_received, int)
    assert info.rtt_ns is None or isinstance(info.rtt_ns, int)
    assert isinstance(info.alpn, bytes)
    assert isinstance(info.is_connected, bool)
    assert info.is_connected is True


async def test_connection_info_alpn_matches(live_connection):
    conn, ep_listen, ep_conn = live_connection
    info = conn.connection_info()
    assert info.alpn == ALPN


async def test_connection_info_connection_type_valid(live_connection):
    conn, ep_listen, ep_conn = live_connection
    info = conn.connection_info()
    # loopback connections should be direct
    assert info.connection_type in ("udp_direct", "udp_relay", "mixed")


# ---------------------------------------------------------------------------
# RemoteInfo field types (requires monitoring enabled)
# ---------------------------------------------------------------------------

async def test_remote_info_list_returns_list(live_connection):
    conn, ep_listen, ep_conn = live_connection
    # Give the connection a moment to populate monitoring data
    await asyncio.sleep(0.1)
    infos = ep_conn.remote_info_list()  # synchronous
    assert isinstance(infos, list)


async def test_remote_info_fields(live_connection):
    conn, ep_listen, ep_conn = live_connection
    await asyncio.sleep(0.1)
    infos = ep_conn.remote_info_list()
    if not infos:
        pytest.skip("no remote_info_list populated yet")
    info = infos[0]
    assert isinstance(info.node_id, str)
    assert len(info.node_id) > 0
    assert info.relay_url is None or isinstance(info.relay_url, str)
    assert isinstance(info.connection_type, str)
    assert info.last_handshake_ns is None or isinstance(info.last_handshake_ns, int)
    assert isinstance(info.bytes_sent, int)
    assert isinstance(info.bytes_received, int)
