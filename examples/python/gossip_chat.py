"""Example: gossip chat interoperable with Iroh's chat example.

Message format matches https://docs.iroh.computer/examples/chat

    struct Message { body: MessageBody, nonce: [u8; 16] }
    enum MessageBody {
        AboutMe { from: EndpointId, name: String },
        Message { from: EndpointId, text: String },
    }

Serialized as JSON (serde_json).  This Python implementation encodes and
decodes the exact same JSON schema so messages can flow between a Rust
``iroh-gossip-chat`` peer and this Python peer.

Usage (two-node local demo):
    python examples/python/gossip_chat.py
"""
import asyncio
import json
import os
from aster_python import IrohNode, gossip_client


TOPIC = bytes(32)  # 32 zero bytes — must match the Rust peer's TopicId


# ---------------------------------------------------------------------------
# Message helpers (Iroh chat-example compatible)
# ---------------------------------------------------------------------------

def _random_nonce() -> list[int]:
    """16-byte random nonce as a JSON-serialisable list of ints."""
    return list(os.urandom(16))


def encode_about_me(from_id: str, name: str) -> bytes:
    """Encode an ``AboutMe`` message."""
    return json.dumps(
        {"body": {"AboutMe": {"from": from_id, "name": name}},
         "nonce": _random_nonce()},
        separators=(",", ":"),
    ).encode()


def encode_message(from_id: str, text: str) -> bytes:
    """Encode a chat ``Message``."""
    return json.dumps(
        {"body": {"Message": {"from": from_id, "text": text}},
         "nonce": _random_nonce()},
        separators=(",", ":"),
    ).encode()


def decode_message(data: bytes) -> dict:
    """Decode an incoming gossip payload (JSON)."""
    return json.loads(data)


def display(msg: dict) -> str:
    """Pretty-print a decoded message for the terminal."""
    body = msg.get("body", {})
    if "AboutMe" in body:
        info = body["AboutMe"]
        return f"> {info['from'][:8]}… is now known as {info['name']}"
    if "Message" in body:
        info = body["Message"]
        return f"{info['from'][:8]}…: {info['text']}"
    return f"[unknown] {msg}"


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def main():
    # Create two in-memory nodes for a local demo.
    # To interop with a Rust peer, you would instead create a single node
    # and use the Rust peer's EndpointId as a bootstrap peer.
    node1 = await IrohNode.memory()
    node2 = await IrohNode.memory()

    # Exchange addresses for peer discovery (in-process shortcut).
    node1.add_node_addr(node2)
    node2.add_node_addr(node1)

    g1 = gossip_client(node1)
    g2 = gossip_client(node2)

    node1_id = node1.node_id()
    node2_id = node2.node_id()
    print(f"Node 1 (alice): {node1_id[:16]}…")
    print(f"Node 2 (bob):   {node2_id[:16]}…")

    # Both must subscribe concurrently.
    topic1, topic2 = await asyncio.wait_for(
        asyncio.gather(
            g1.subscribe(TOPIC, [node2_id]),
            g2.subscribe(TOPIC, [node1_id]),
        ),
        timeout=30,
    )
    print("Both nodes subscribed to topic.\n")

    # --- Alice introduces herself and sends a message ---
    await topic1.broadcast(encode_about_me(node1_id, "alice"))
    await topic1.broadcast(encode_message(node1_id, "Hey Bob!"))

    # --- Bob introduces himself and sends a message ---
    await topic2.broadcast(encode_about_me(node2_id, "bob"))
    await topic2.broadcast(encode_message(node2_id, "Hey Alice!"))

    # --- Receive messages on both sides ---
    async def recv_loop(name: str, topic, count: int):
        for _ in range(40):
            event_type, data = await asyncio.wait_for(topic.recv(), timeout=10)
            if event_type == "received":
                msg = decode_message(data)
                print(f"  [{name}] {display(msg)}")
                count -= 1
                if count <= 0:
                    break

    await asyncio.gather(
        recv_loop("node2/bob", topic2, 2),   # receives alice's msgs
        recv_loop("node1/alice", topic1, 2),  # receives bob's msgs
    )

    print()
    await node1.shutdown()
    await node2.shutdown()
    print("Done.")


if __name__ == "__main__":
    asyncio.run(main())