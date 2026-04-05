"""
Basic tests for IrohNode functionality.

Tests:
- Node creation (memory)
- Node creation (persistent)
- Node ID retrieval
- Graceful shutdown
"""
import tempfile
import pytest

pytestmark = pytest.mark.asyncio


async def test_node_id():
    """Test retrieving node ID."""
    from aster import IrohNode

    node = await IrohNode.memory()
    node_id = node.node_id()

    # Node ID should be a non-empty string
    assert isinstance(node_id, str)
    assert len(node_id) > 0

    await node.shutdown()


async def test_multiple_nodes():
    """Test creating multiple nodes simultaneously."""
    from aster import IrohNode

    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()

    # Each node should have a unique ID
    id1 = node1.node_id()
    id2 = node2.node_id()
    assert id1 != id2

    await node1.shutdown()
    await node2.shutdown()


async def test_persistent_node_creation():
    """Test creating a persistent node backed by FsStore."""
    from aster import IrohNode

    with tempfile.TemporaryDirectory() as td:
        node = await IrohNode.persistent(td)
        assert node is not None
        assert isinstance(node.node_id(), str)
        await node.shutdown()


async def test_node_addr_info_roundtrip():
    """Structured node addr can be serialized and deserialized."""
    from aster import IrohNode, NodeAddr

    node = await IrohNode.memory()
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