"""Shared pytest fixtures for aster-python tests."""
import pytest
import pytest_asyncio

from aster import Node, create_endpoint
from wait_budget import TIME_SCALE


ALPN = b"test/echo/1"


@pytest.hookimpl(tryfirst=True)
def pytest_configure(config):
    """Scale pytest-timeout's per-test limit (``--timeout``) with the wait
    budget (wait_budget.py), so a test whose waits stretch under
    ``ASTER_TEST_TIME_SCALE`` is not cut off by the unscaled limit.
    ``pytest.mark.timeout(N)`` on a test is not scaled."""
    timeout = getattr(config.option, "timeout", None)
    if timeout and TIME_SCALE != 1.0:
        config.option.timeout = float(timeout) * TIME_SCALE

# Network fixtures used below.  Any test that requests one of these is
# automatically marked ``@pytest.mark.network`` so the CI fast-path
# (``pytest -m "not network"``) can skip them.
_NETWORK_FIXTURES = frozenset({"node", "node_pair", "endpoint_pair"})


def pytest_collection_modifyitems(items):
    """Auto-mark tests that use network fixtures."""
    net_marker = pytest.mark.network
    for item in items:
        if _NETWORK_FIXTURES & set(item.fixturenames):
            item.add_marker(net_marker)


@pytest_asyncio.fixture
async def node():
    """Single in-memory Node, shut down after test."""
    n = await Node.start()
    yield n
    await n.shutdown()


@pytest_asyncio.fixture
async def node_pair():
    """Two Nodes with addresses exchanged, shut down after test."""
    n1 = await Node.start()
    n2 = await Node.start()
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