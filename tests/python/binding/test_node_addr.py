"""
Binding-layer tests for NodeAddr.

Covers construction, field access, serialization (to_bytes/from_bytes,
to_dict/from_dict), and edge cases like empty direct_addresses.
"""

import pytest
from aster_python import NodeAddr


FAKE_ID = "a" * 64  # 64 hex chars is a plausible node ID format


# ---------------------------------------------------------------------------
# Construction
# ---------------------------------------------------------------------------

def test_minimal():
    addr = NodeAddr(endpoint_id=FAKE_ID)
    assert addr.endpoint_id == FAKE_ID
    assert addr.relay_url is None
    assert addr.direct_addresses == []


def test_with_relay_url():
    addr = NodeAddr(endpoint_id=FAKE_ID, relay_url="https://relay.example.com")
    assert addr.relay_url == "https://relay.example.com"


def test_with_direct_addresses():
    addrs = ["1.2.3.4:1234", "5.6.7.8:5678"]
    addr = NodeAddr(endpoint_id=FAKE_ID, direct_addresses=addrs)
    assert addr.direct_addresses == addrs


def test_full():
    addr = NodeAddr(
        endpoint_id=FAKE_ID,
        relay_url="https://relay.example.com",
        direct_addresses=["1.2.3.4:9000"],
    )
    assert addr.endpoint_id == FAKE_ID
    assert addr.relay_url == "https://relay.example.com"
    assert addr.direct_addresses == ["1.2.3.4:9000"]


# ---------------------------------------------------------------------------
# to_bytes / from_bytes roundtrip
# ---------------------------------------------------------------------------

def test_bytes_roundtrip_full():
    original = NodeAddr(
        endpoint_id=FAKE_ID,
        relay_url="https://relay.example.com",
        direct_addresses=["1.2.3.4:9000", "5.6.7.8:9001"],
    )
    restored = NodeAddr.from_bytes(original.to_bytes())
    assert restored.endpoint_id == original.endpoint_id
    assert restored.relay_url == original.relay_url
    assert restored.direct_addresses == original.direct_addresses


def test_bytes_roundtrip_no_relay():
    original = NodeAddr(
        endpoint_id=FAKE_ID,
        direct_addresses=["1.2.3.4:9000"],
    )
    restored = NodeAddr.from_bytes(original.to_bytes())
    assert restored.endpoint_id == original.endpoint_id
    assert restored.relay_url is None
    assert restored.direct_addresses == ["1.2.3.4:9000"]


def test_bytes_roundtrip_minimal():
    original = NodeAddr(endpoint_id=FAKE_ID)
    restored = NodeAddr.from_bytes(original.to_bytes())
    assert restored.endpoint_id == original.endpoint_id
    assert restored.relay_url is None
    assert restored.direct_addresses == []


def test_from_bytes_returns_node_addr():
    addr = NodeAddr(endpoint_id=FAKE_ID)
    result = NodeAddr.from_bytes(addr.to_bytes())
    assert isinstance(result, NodeAddr)


# ---------------------------------------------------------------------------
# to_dict / from_dict roundtrip
# ---------------------------------------------------------------------------

def test_dict_roundtrip_full():
    original = NodeAddr(
        endpoint_id=FAKE_ID,
        relay_url="https://relay.example.com",
        direct_addresses=["1.2.3.4:9000"],
    )
    d = original.to_dict()
    restored = NodeAddr.from_dict(d)
    assert restored.endpoint_id == original.endpoint_id
    assert restored.relay_url == original.relay_url
    assert restored.direct_addresses == original.direct_addresses


def test_dict_roundtrip_minimal():
    original = NodeAddr(endpoint_id=FAKE_ID)
    restored = NodeAddr.from_dict(original.to_dict())
    assert restored.endpoint_id == FAKE_ID
    assert restored.relay_url is None
    assert restored.direct_addresses == []


def test_dict_keys():
    addr = NodeAddr(endpoint_id=FAKE_ID, relay_url="https://r.example.com")
    d = addr.to_dict()
    assert "endpoint_id" in d
    assert "relay_url" in d
    assert "direct_addresses" in d


def test_from_dict_missing_relay_url():
    """from_dict should treat a missing relay_url key as None."""
    d = {"endpoint_id": FAKE_ID, "direct_addresses": []}
    addr = NodeAddr.from_dict(d)
    assert addr.relay_url is None


def test_from_dict_missing_direct_addresses():
    """from_dict should treat a missing direct_addresses key as []."""
    d = {"endpoint_id": FAKE_ID}
    addr = NodeAddr.from_dict(d)
    assert addr.direct_addresses == []


def test_from_dict_missing_endpoint_id_raises():
    from aster_python import IrohError
    with pytest.raises((IrohError, Exception)):
        NodeAddr.from_dict({})
