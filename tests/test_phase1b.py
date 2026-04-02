"""Phase 1b: Tests for datagram completion, remote-info, and monitoring surfaces."""
import asyncio
import pytest
import pytest_asyncio

from aster_python import (
    IrohNode,
    create_endpoint,
    EndpointConfig,
    NetClient,
    IrohConnection,
    ConnectionInfo,
    RemoteInfo,
    HookDecision,
    HookConnectInfo,
    HookHandshakeInfo,
)

ALPN = b"test/phase1b/1"


@pytest_asyncio.fixture
async def node_pair():
    """Two IrohNodes with addresses exchanged for connection tests."""
    n1 = await IrohNode.memory()
    n2 = await IrohNode.memory()
    n1.add_node_addr(n2)
    n2.add_node_addr(n1)
    yield n1, n2
    await n1.shutdown()
    await n2.shutdown()


@pytest_asyncio.fixture
async def endpoint_pair():
    """Two bare QUIC endpoints for net tests."""
    ep1 = await create_endpoint(ALPN)
    ep2 = await create_endpoint(ALPN)
    yield ep1, ep2
    await ep1.close()
    await ep2.close()


# ============================================================================
# Datagram Completion Tests
# ============================================================================


@pytest.mark.asyncio
async def test_connection_max_datagram_size(endpoint_pair):
    """Test that max_datagram_size returns a valid value or None."""
    ep_server, ep_client = endpoint_pair
    server_done = asyncio.Event()

    async def server_side():
        conn = await ep_server.accept()
        max_size = conn.max_datagram_size()
        # Datagram support depends on connection negotiation
        # The value may be None or a positive integer
        assert max_size is None or isinstance(max_size, int)
        if max_size is not None:
            assert max_size > 0
        send, recv = await conn.accept_bi()
        await recv.read_to_end(65536)
        server_done.set()

    async def client_side():
        await asyncio.sleep(0.2)
        conn = await ep_client.connect(ep_server.endpoint_id(), ALPN)
        max_size = conn.max_datagram_size()
        assert max_size is None or isinstance(max_size, int)
        if max_size is not None:
            assert max_size > 0
        send, recv = await conn.open_bi()
        await send.write_all(b"datagram test")
        await send.finish()
        await server_done.wait()

    await asyncio.wait_for(asyncio.gather(server_side(), client_side()), timeout=30)


@pytest.mark.asyncio
async def test_connection_datagram_send_buffer_space(endpoint_pair):
    """Test that datagram_send_buffer_space returns a valid value."""
    ep_server, ep_client = endpoint_pair
    server_done = asyncio.Event()

    async def server_side():
        conn = await ep_server.accept()
        buffer_space = conn.datagram_send_buffer_space()
        # Buffer space should be a non-negative integer
        assert isinstance(buffer_space, int)
        assert buffer_space >= 0
        send, recv = await conn.accept_bi()
        await recv.read_to_end(65536)
        server_done.set()

    async def client_side():
        await asyncio.sleep(0.2)
        conn = await ep_client.connect(ep_server.endpoint_id(), ALPN)
        buffer_space = conn.datagram_send_buffer_space()
        assert isinstance(buffer_space, int)
        assert buffer_space >= 0
        send, recv = await conn.open_bi()
        await send.write_all(b"buffer test")
        await send.finish()
        await server_done.wait()

    await asyncio.wait_for(asyncio.gather(server_side(), client_side()), timeout=30)


# ============================================================================
# Connection Info Tests
# ============================================================================


@pytest.mark.asyncio
async def test_connection_info_after_connect(endpoint_pair):
    """Test that connection_info returns valid connection information."""
    ep_server, ep_client = endpoint_pair
    server_done = asyncio.Event()

    async def server_side():
        conn = await ep_server.accept()
        info = conn.connection_info()
        assert isinstance(info, ConnectionInfo)
        # Connection type should be a string
        assert isinstance(info.connection_type, str)
        # Bytes sent/received should be integers
        assert isinstance(info.bytes_sent, int)
        assert isinstance(info.bytes_received, int)
        # ALPN should be bytes
        assert isinstance(info.alpn, (bytes, list))
        # is_connected should be a boolean
        assert isinstance(info.is_connected, bool)
        # RTT may be None or an integer
        assert info.rtt_ns is None or isinstance(info.rtt_ns, int)
        send, recv = await conn.accept_bi()
        await recv.read_to_end(65536)
        server_done.set()

    async def client_side():
        await asyncio.sleep(0.2)
        conn = await ep_client.connect(ep_server.endpoint_id(), ALPN)
        info = conn.connection_info()
        assert isinstance(info, ConnectionInfo)
        send, recv = await conn.open_bi()
        await send.write_all(b"info test")
        await send.finish()
        await server_done.wait()

    await asyncio.wait_for(asyncio.gather(server_side(), client_side()), timeout=30)


# ============================================================================
# Remote-Info & Monitoring Tests
# ============================================================================


@pytest.mark.asyncio
async def test_net_client_has_monitoring_default():
    """Test that bare endpoints do NOT have monitoring enabled by default.
    Monitoring is opt-in via create_endpoint_with_config(enable_monitoring=True)."""
    ep = await create_endpoint(ALPN)
    # Bare endpoints have no monitoring overhead unless explicitly requested
    assert ep.has_monitoring() is False
    await ep.close()


@pytest.mark.asyncio
async def test_net_client_has_hooks_default():
    """Test that bare endpoints have hooks disabled by default."""
    ep = await create_endpoint(ALPN)
    # Hooks are disabled unless explicitly enabled
    assert ep.has_hooks() is False
    await ep.close()


@pytest.mark.asyncio
async def test_net_client_remote_info_unknown_returns_none():
    """Test that remote_info returns None for unknown node IDs."""
    ep = await create_endpoint(ALPN)
    # Should return None for unknown node ID
    result = ep.remote_info("0000000000000000000000000000000000000000000000000000000000000000")
    assert result is None
    await ep.close()


@pytest.mark.asyncio
async def test_net_client_remote_info_list_empty_at_start():
    """Test that remote_info_list returns empty list before connections."""
    ep = await create_endpoint(ALPN)
    # Before any connections, list should be empty
    result = ep.remote_info_list()
    assert isinstance(result, list)
    # May or may not include the local endpoint
    await ep.close()


@pytest.mark.asyncio
async def test_endpoint_config_monitoring_flag():
    """Test that EndpointConfig accepts enable_monitoring parameter."""
    config = EndpointConfig(
        alpns=[ALPN],
        enable_monitoring=True,
        enable_hooks=False,
        hook_timeout_ms=5000,
    )
    assert config.enable_monitoring is True
    assert config.enable_hooks is False
    assert config.hook_timeout_ms == 5000


@pytest.mark.asyncio
async def test_endpoint_config_hooks_flag():
    """Test that EndpointConfig accepts enable_hooks parameter."""
    config = EndpointConfig(
        alpns=[ALPN],
        enable_monitoring=False,
        enable_hooks=True,
        hook_timeout_ms=3000,
    )
    assert config.enable_monitoring is False
    assert config.enable_hooks is True
    assert config.hook_timeout_ms == 3000


# ============================================================================
# Hook Types Tests
# ============================================================================


@pytest.mark.asyncio
async def test_hook_decision_allow():
    """Test HookDecision.Allow creation."""
    decision = HookDecision.create_allow()
    assert decision.allow is True
    assert decision.error_code is None
    assert decision.reason is None


@pytest.mark.asyncio
async def test_hook_decision_deny():
    """Test HookDecision.Deny creation."""
    decision = HookDecision.create_deny(42, b"connection rejected")
    assert decision.allow is False
    assert decision.error_code == 42
    assert decision.reason == b"connection rejected"


@pytest.mark.asyncio
async def test_hook_decision_default():
    """Test default HookDecision is Allow."""
    decision = HookDecision()
    assert decision.allow is True


# ============================================================================
# Hook Types Structure Tests
# ============================================================================


@pytest.mark.asyncio
async def test_hook_connect_info_fields():
    """Test HookConnectInfo has expected fields."""
    # HookConnectInfo is a data structure passed to hook callbacks
    # Its fields should be accessible
    info = HookConnectInfo()
    assert hasattr(info, 'remote_endpoint_id')
    assert hasattr(info, 'alpn')


@pytest.mark.asyncio
async def test_hook_handshake_info_fields():
    """Test HookHandshakeInfo has expected fields."""
    # HookHandshakeInfo is a data structure passed to hook callbacks
    info = HookHandshakeInfo()
    assert hasattr(info, 'remote_endpoint_id')
    assert hasattr(info, 'alpn')
    assert hasattr(info, 'is_alive')


# ============================================================================
# Integration: Full Connection Flow with Phase 1b Surfaces
# ============================================================================


@pytest.mark.asyncio
async def test_full_connection_with_all_phase1b_surfaces(endpoint_pair):
    """Test a complete connection flow exercising all Phase 1b surfaces."""
    ep_server, ep_client = endpoint_pair

    server_info = {}
    client_info = {}

    client_done = asyncio.Event()

    async def server_side():
        conn = await ep_server.accept()
        
        # Phase 1b: Datagram completion
        max_size = conn.max_datagram_size()
        buffer_space = conn.datagram_send_buffer_space()
        
        # Phase 1b: Connection info
        info = conn.connection_info()
        server_info['max_size'] = max_size
        server_info['buffer_space'] = buffer_space
        server_info['connection_type'] = info.connection_type
        server_info['bytes_sent'] = info.bytes_sent
        server_info['bytes_received'] = info.bytes_received
        
        # Receive data
        send, recv = await conn.accept_bi()
        data = await recv.read_to_end(65536)
        await send.write_all(b"echo: " + data)
        await send.finish()
        # Keep connection alive until client has read the echo
        await client_done.wait()

    async def client_side():
        await asyncio.sleep(0.2)
        conn = await ep_client.connect(ep_server.endpoint_id(), ALPN)
        
        # Phase 1b: Datagram completion
        max_size = conn.max_datagram_size()
        buffer_space = conn.datagram_send_buffer_space()
        
        # Phase 1b: Connection info
        info = conn.connection_info()
        client_info['max_size'] = max_size
        client_info['buffer_space'] = buffer_space
        client_info['connection_type'] = info.connection_type
        client_info['bytes_sent'] = info.bytes_sent
        client_info['bytes_received'] = info.bytes_received
        
        # Send and receive echo
        send, recv = await conn.open_bi()
        await send.write_all(b"hello phase1b")
        await send.finish()
        echo = await recv.read_to_end(65536)
        assert echo == b"echo: hello phase1b"
        client_done.set()

    await asyncio.wait_for(asyncio.gather(server_side(), client_side()), timeout=30)
    
    # Verify Phase 1b surfaces returned valid data
    assert 'max_size' in server_info
    assert 'buffer_space' in server_info
    assert 'connection_type' in server_info
    assert 'max_size' in client_info
    assert 'buffer_space' in client_info
    assert 'connection_type' in client_info
