"""Phase 4: Gossip protocol tests."""
import asyncio
import pytest
import pytest_asyncio

from iroh_python import IrohNode, gossip_client, GossipTopicHandle


TOPIC = bytes(32)  # 32 zero bytes as topic ID


@pytest_asyncio.fixture
async def node_pair():
    """Create two IrohNodes for gossip testing."""
    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()
    yield node1, node2
    await node1.shutdown()
    await node2.shutdown()


@pytest.mark.asyncio
async def test_gossip_broadcast_recv(node_pair):
    """Two-node gossip: node1 broadcasts, node2 receives."""
    node1, node2 = node_pair
    g1 = gossip_client(node1)
    g2 = gossip_client(node2)

    node1_id = node1.node_id()
    node2_id = node2.node_id()

    payload = b"hello gossip!"

    # Exchange address info so nodes can find each other
    node1.add_node_addr(node2)
    node2.add_node_addr(node1)

    # Both subscribe concurrently — subscribe_and_join blocks until a peer connects,
    # so we need them to run in parallel.
    topic1, topic2 = await asyncio.wait_for(
        asyncio.gather(
            g1.subscribe(TOPIC, [node2_id]),
            g2.subscribe(TOPIC, [node1_id]),
        ),
        timeout=30,
    )

    # Broadcast from node1
    await topic1.broadcast(payload)

    # Receive on node2 — skip neighbor events until we get a "received"
    received_data = None
    for _ in range(20):
        event_type, data = await asyncio.wait_for(topic2.recv(), timeout=10)
        if event_type == "received":
            received_data = data
            break

    assert received_data == payload, f"Expected {payload!r}, got {received_data!r}"