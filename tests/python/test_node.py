"""
Basic tests for Node functionality.

Tests:
- Node creation (memory)
- Node creation (persistent)
- Node ID retrieval
- Graceful shutdown
"""
import tempfile
import pytest

pytestmark = [pytest.mark.asyncio, pytest.mark.iroh_primitives]


async def test_node_id():
    """Test retrieving node ID."""
    from aster import Node

    node = await Node.start()
    node_id = node.node_id()

    # Node ID should be a non-empty string
    assert isinstance(node_id, str)
    assert len(node_id) > 0

    await node.shutdown()


async def test_multiple_nodes():
    """Test creating multiple nodes simultaneously."""
    from aster import Node

    node1 = await Node.start()
    node2 = await Node.start()

    # Each node should have a unique ID
    id1 = node1.node_id()
    id2 = node2.node_id()
    assert id1 != id2

    await node1.shutdown()
    await node2.shutdown()


async def test_persistent_node_creation():
    """Test creating a persistent node backed by FsStore."""
    from aster import AsterConfig, Node

    with tempfile.TemporaryDirectory() as td:
        node = await Node.start(AsterConfig(storage_path=td))
        assert node is not None
        assert isinstance(node.node_id(), str)
        await node.shutdown()


async def test_node_addr_info_roundtrip():
    """Structured node addr can be serialized and deserialized."""
    from aster import Node, NodeAddr

    node = await Node.start()
    addr = node.node_addr_info()
    assert isinstance(addr.endpoint_id, str)
    assert addr.endpoint_id == node.node_id()

    data = addr.to_bytes()
    restored = NodeAddr.from_bytes(data)
    assert restored.endpoint_id == addr.endpoint_id
    assert restored.relay_url == addr.relay_url
    assert restored.direct_addresses == addr.direct_addresses

    restored2 = NodeAddr.from_dict(addr.to_dict())
    assert restored2.endpoint_id == addr.endpoint_id

    await node.shutdown()

# -- Node over aster::Node + AsterConfig (plan P1) ---------------------------

import os
from dataclasses import dataclass

from aster import (
    AsterClient,
    AsterConfig,
    AsterServer,
    Node,
    NodeId,
    SecretKey,
    rpc,
    service,
    wire_type,
)


def _local(**kw) -> AsterConfig:
    return AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0", **kw)


async def test_node_async_context_manager_and_typed_id():
    async with await Node.start(_local()) as node:
        assert isinstance(node.id, NodeId)
        assert node.id == node.node_id()
        assert str(node.id) == node.node_id()
        assert node.secret_key().node_id() == node.id
        assert node.id.public_key().node_id() == node.id
        assert {node.id: 1}[node.node_id()] == 1


async def test_node_start_with_secret_key_pins_identity():
    key = SecretKey.generate()
    node = await Node.start(_local(secret_key=key))
    try:
        assert node.id == key.node_id()
    finally:
        await node.shutdown()


async def test_persistent_node_restart_keeps_identity(tmp_path):
    cfg = _local(
        storage_path=str(tmp_path / "data"),
        secret_key_file=str(tmp_path / "data" / "node.key"),
    )
    first = await Node.start(cfg)
    first_id = first.id
    await first.shutdown()
    key_file = tmp_path / "data" / "node.key"
    assert key_file.read_text().strip() == SecretKey.read_file(str(key_file)).to_hex()

    second = await Node.start(cfg)
    try:
        assert second.id == first_id
    finally:
        await second.shutdown()


async def test_node_start_malformed_key_file_is_error(tmp_path):
    bad = tmp_path / "node.key"
    bad.write_text('{"secret_key": "00", "node_id": "x"}\n')  # old JSON format
    with pytest.raises(ValueError, match="secret key file"):
        await Node.start(_local(secret_key_file=str(bad)))


async def test_node_start_key_file_conflict_is_error(tmp_path):
    path = tmp_path / "node.key"
    SecretKey.generate().write_file(str(path))
    with pytest.raises(ValueError, match="different keys"):
        await Node.start(_local(secret_key=SecretKey.generate(), secret_key_file=str(path)))


@wire_type("test.p1/Ping")
@dataclass
class _P1Ping:
    value: str = ""


@service("P1NodeService", version=1)
class _P1NodeService:
    @rpc
    async def ping(self, req: _P1Ping) -> _P1Ping:
        return _P1Ping(value=f"pong:{req.value}")


async def test_aster_server_persistent_restart_keeps_identity(tmp_path):
    """A persistent AsterServer keeps its node id across restarts: without a
    configured key it uses ``<storage_path>/node.key``."""
    cfg = _local(storage_path=str(tmp_path / "srv"))
    srv = AsterServer(services=[_P1NodeService()], config=cfg, allow_all_consumers=True)
    await srv.start()
    first_id = srv.node.id
    await srv.close()
    assert (tmp_path / "srv" / "node.key").exists()

    srv2 = AsterServer(services=[_P1NodeService()], config=cfg, allow_all_consumers=True)
    await srv2.start()
    try:
        assert srv2.node.id == first_id
        assert SecretKey.read_file(str(tmp_path / "srv" / "node.key")).node_id() == first_id
    finally:
        await srv2.close()


async def test_aster_server_and_client_over_node(tmp_path):
    """Server and client both start via Node.start(AsterConfig); RPC works."""
    srv_key = tmp_path / "srv.key"
    async with AsterServer(
        services=[_P1NodeService()],
        config=_local(secret_key_file=str(srv_key)),
        allow_all_consumers=True,
    ) as srv:
        assert srv.node.id == SecretKey.read_file(str(srv_key)).node_id()
        client = AsterClient(
            config=_local(storage_path=str(tmp_path / "cli")),
            address=srv.address,
        )
        await client.connect()
        try:
            assert client._node.id == SecretKey.read_file(
                str(tmp_path / "cli" / "node.key")
            ).node_id()
            stub = await client.client(_P1NodeService)
            reply = await stub.ping(_P1Ping(value="x"))
            assert reply.value == "pong:x"
        finally:
            await client.close()


async def test_aster_server_identity_key_file_conflict(tmp_path):
    """An .aster-identity [node] key that differs from secret_key_file is an
    error at start (one node, one identity)."""
    key_file = tmp_path / "node.key"
    SecretKey.generate().write_file(str(key_file))
    ident = tmp_path / ".aster-identity"
    ident.write_text(f'[node]\nsecret_key = "{SecretKey.generate().to_hex()}"\n')
    srv = AsterServer(
        services=[_P1NodeService()],
        config=_local(secret_key_file=str(key_file), identity_file=str(ident)),
        allow_all_consumers=True,
    )
    with pytest.raises(ValueError, match="different keys"):
        await srv.start()
