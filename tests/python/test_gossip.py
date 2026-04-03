"""Phase 4: Gossip protocol tests.

These tests verify that two Python nodes can exchange raw bytes over
iroh-gossip and handle the basic event lifecycle (neighbor up/down,
broadcast, receive, lag).

Note: iroh-gossip's upstream chat example uses signed, postcard-encoded
messages (a Rust-specific binary serializer). Python clients are not
expected to be wire-compatible with that format. Instead, we test raw
byte exchange and the JSON-based application-layer protocol that this
repository's own gossip_chat.py example uses.
"""
import asyncio
import json
import os
import pytest
import pytest_asyncio

from aster_python import IrohNode, gossip_client, GossipTopicHandle


TOPIC = bytes(32)  # 32 zero bytes as topic ID


def _random_nonce() -> list[int]:
    """Generate a 16-byte random nonce as a list of ints (matches Rust [u8; 16])."""
    return list(os.urandom(16))


def encode_about_me(from_id: str, name: str) -> bytes:
    """Encode an AboutMe message in the Iroh chat-example JSON format."""
    msg = {
        "body": {"AboutMe": {"from": from_id, "name": name}},
        "nonce": _random_nonce(),
    }
    return json.dumps(msg, separators=(",", ":")).encode("utf-8")


def encode_chat_message(from_id: str, text: str) -> bytes:
    """Encode a chat Message in the Iroh chat-example JSON format."""
    msg = {
        "body": {"Message": {"from": from_id, "text": text}},
        "nonce": _random_nonce(),
    }
    return json.dumps(msg, separators=(",", ":")).encode("utf-8")


def decode_chat_message(data: bytes) -> dict:
    """Decode a message from the Iroh chat-example JSON format.

    Returns the parsed JSON dict.  Callers can inspect
    msg["body"]["Message"]["text"] or msg["body"]["AboutMe"]["name"].
    """
    return json.loads(data)


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
    """Two-node gossip: node1 broadcasts, node2 receives (raw bytes, backwards compat)."""
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


@pytest.mark.asyncio
async def test_gossip_chat_interop(node_pair):
    """Two-node gossip using a JSON application-layer protocol.

    This test verifies that messages encoded in the same JSON schema used
    by this repository's gossip_chat.py example can be sent and received
    correctly over iroh-gossip. The upstream Rust chat example uses a
    different (postcard) binary format and is not wire-compatible.
    """
    node1, node2 = node_pair
    g1 = gossip_client(node1)
    g2 = gossip_client(node2)

    node1_id = node1.node_id()
    node2_id = node2.node_id()

    node1.add_node_addr(node2)
    node2.add_node_addr(node1)

    topic1, topic2 = await asyncio.wait_for(
        asyncio.gather(
            g1.subscribe(TOPIC, [node2_id]),
            g2.subscribe(TOPIC, [node1_id]),
        ),
        timeout=30,
    )

    # --- Node 1 sends an AboutMe, then a chat Message ---
    about_me = encode_about_me(node1_id, "alice")
    await topic1.broadcast(about_me)

    chat_text = "Hello from Python!"
    chat_msg = encode_chat_message(node1_id, chat_text)
    await topic1.broadcast(chat_msg)

    # --- Node 2 receives and decodes both messages ---
    received_messages: list[dict] = []
    for _ in range(30):
        event_type, data = await asyncio.wait_for(topic2.recv(), timeout=10)
        if event_type == "received":
            parsed = decode_chat_message(data)
            received_messages.append(parsed)
            if len(received_messages) >= 2:
                break

    assert len(received_messages) >= 2, (
        f"Expected at least 2 messages, got {len(received_messages)}"
    )

    # Verify AboutMe
    about = received_messages[0]
    assert "AboutMe" in about["body"], f"Expected AboutMe, got {about}"
    assert about["body"]["AboutMe"]["name"] == "alice"
    assert about["body"]["AboutMe"]["from"] == node1_id

    # Verify Message
    msg = received_messages[1]
    assert "Message" in msg["body"], f"Expected Message, got {msg}"
    assert msg["body"]["Message"]["text"] == chat_text
    assert msg["body"]["Message"]["from"] == node1_id


@pytest.mark.asyncio
async def test_gossip_bidirectional_chat(node_pair):
    """Both nodes exchange chat messages using the JSON application-layer format."""
    node1, node2 = node_pair
    g1 = gossip_client(node1)
    g2 = gossip_client(node2)

    node1_id = node1.node_id()
    node2_id = node2.node_id()

    node1.add_node_addr(node2)
    node2.add_node_addr(node1)

    topic1, topic2 = await asyncio.wait_for(
        asyncio.gather(
            g1.subscribe(TOPIC, [node2_id]),
            g2.subscribe(TOPIC, [node1_id]),
        ),
        timeout=30,
    )

    # Node 1 sends
    await topic1.broadcast(encode_about_me(node1_id, "alice"))
    await topic1.broadcast(encode_chat_message(node1_id, "hi bob"))

    # Node 2 sends
    await topic2.broadcast(encode_about_me(node2_id, "bob"))
    await topic2.broadcast(encode_chat_message(node2_id, "hi alice"))

    # Collect messages on both sides
    async def collect_messages(topic, count):
        msgs = []
        for _ in range(40):
            event_type, data = await asyncio.wait_for(topic.recv(), timeout=10)
            if event_type == "received":
                msgs.append(decode_chat_message(data))
                if len(msgs) >= count:
                    break
        return msgs

    msgs_at_node2, msgs_at_node1 = await asyncio.gather(
        collect_messages(topic2, 2),  # expects alice's AboutMe + Message
        collect_messages(topic1, 2),  # expects bob's AboutMe + Message
    )

    # Node 2 should have received alice's messages
    texts_at_2 = [
        m["body"]["Message"]["text"]
        for m in msgs_at_node2
        if "Message" in m["body"]
    ]
    assert "hi bob" in texts_at_2

    # Node 1 should have received bob's messages
    texts_at_1 = [
        m["body"]["Message"]["text"]
        for m in msgs_at_node1
        if "Message" in m["body"]
    ]
    assert "hi alice" in texts_at_1