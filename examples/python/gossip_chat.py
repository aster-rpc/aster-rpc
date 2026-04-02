"""Example: two-node gossip pub-sub messaging.

Demonstrates creating two nodes, subscribing to a shared topic,
and broadcasting/receiving messages.
"""
import asyncio
from aster_python import IrohNode, gossip_client


TOPIC = bytes(32)  # 32 zero bytes as topic ID


async def main():
    # Create two nodes
    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()

    # Exchange addresses for peer discovery
    node1.add_node_addr(node2)
    node2.add_node_addr(node1)

    g1 = gossip_client(node1)
    g2 = gossip_client(node2)

    node1_id = node1.node_id()
    node2_id = node2.node_id()
    print(f"Node 1: {node1_id[:8]}...")
    print(f"Node 2: {node2_id[:8]}...")

    # Both must subscribe concurrently (subscribe_and_join blocks until peer connects)
    topic1, topic2 = await asyncio.wait_for(
        asyncio.gather(
            g1.subscribe(TOPIC, [node2_id]),
            g2.subscribe(TOPIC, [node1_id]),
        ),
        timeout=30,
    )
    print("Both nodes subscribed to topic.")

    # Node 1 broadcasts
    message = b"Hello from gossip!"
    await topic1.broadcast(message)
    print(f"Node 1 broadcast: {message.decode()}")

    # Node 2 receives (skip neighbor events)
    for _ in range(20):
        event_type, data = await asyncio.wait_for(topic2.recv(), timeout=10)
        if event_type == "received":
            print(f"Node 2 received: {data.decode()}")
            break

    await node1.shutdown()
    await node2.shutdown()
    print("Done.")


if __name__ == "__main__":
    asyncio.run(main())