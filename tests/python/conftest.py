"""Shared pytest fixtures for aster-python tests."""
import pytest_asyncio

from aster import IrohNode, create_endpoint


ALPN = b"test/echo/1"


@pytest_asyncio.fixture
async def node():
    """Single in-memory IrohNode, shut down after test."""
    n = await IrohNode.memory()
    yield n
    await n.shutdown()


@pytest_asyncio.fixture
async def node_pair():
    """Two IrohNodes with addresses exchanged, shut down after test."""
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