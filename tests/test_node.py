"""
Basic tests for IrohNode functionality.

Tests:
- Node creation (memory)
- Node ID retrieval
- Graceful shutdown
"""
import asyncio
import pytest

pytestmark = pytest.mark.asyncio


async def test_memory_node_creation():
    """Test creating an in-memory node."""
    from iroh_python import IrohNode

    node = await IrohNode.memory()
    assert node is not None
    await node.shutdown()


async def test_node_id():
    """Test retrieving node ID."""
    from iroh_python import IrohNode

    node = await IrohNode.memory()
    node_id = node.node_id()

    # Node ID should be a non-empty string
    assert isinstance(node_id, str)
    assert len(node_id) > 0

    await node.shutdown()


async def test_node_id_is_consistent():
    """Test that node_id returns the same value on multiple calls."""
    from iroh_python import IrohNode

    node = await IrohNode.memory()
    id1 = node.node_id()
    id2 = node.node_id()
    assert id1 == id2

    await node.shutdown()


async def test_multiple_nodes():
    """Test creating multiple nodes simultaneously."""
    from iroh_python import IrohNode

    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()

    # Each node should have a unique ID
    id1 = node1.node_id()
    id2 = node2.node_id()
    assert id1 != id2

    await node1.shutdown()
    await node2.shutdown()