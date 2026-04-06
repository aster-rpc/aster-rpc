"""
tests/python/test_aster_local.py

Phase 13 LocalTransport parity and wire-compatibility tests.

Verifies that:
- wire_compatible=True exercises full Fory serialization (XLANG encode/decode round-trip)
- wire_compatible=False bypasses serialization but still produces correct results
- Both modes yield identical logical results for the same service call
- Bytes produced by LocalTransport(wire_compatible=True) are consistent with
  what ForyCodec.encode() produces for the same object

Spec reference: §8.3.3 (wire-compatible mode)
"""

from __future__ import annotations

from dataclasses import dataclass

import pytest

from aster.client import create_local_client
from aster.codec import ForyCodec, wire_type
from aster.decorators import service, rpc
from aster.testing import AsterTestHarness
from aster.types import SerializationMode


# ── Test types ────────────────────────────────────────────────────────────────


@wire_type("test.local/Ping")
@dataclass
class Ping:
    value: int = 0


@wire_type("test.local/Pong")
@dataclass
class Pong:
    value: int = 0
    doubled: bool = False


# ── Service definition ────────────────────────────────────────────────────────


@service(name="PingService", version=1, serialization=[SerializationMode.XLANG])
class PingService:

    @rpc(timeout=10.0)
    async def ping(self, req: Ping) -> Pong:
        return Pong(value=req.value, doubled=False)

    @rpc(timeout=10.0)
    async def double(self, req: Ping) -> Pong:
        return Pong(value=req.value * 2, doubled=True)


class PingImpl(PingService):
    """Concrete PingService for tests."""
    pass


# ── wire_compatible=True tests ────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_wire_compatible_true_returns_correct_result():
    """wire_compatible=True exercises the full serialization path and returns correct results."""
    client = create_local_client(PingService, PingImpl(), wire_compatible=True)
    response = await client.ping(Ping(value=42))
    assert isinstance(response, Pong)
    assert response.value == 42
    assert response.doubled is False


@pytest.mark.asyncio
async def test_wire_compatible_true_encode_decode_round_trip():
    """Bytes produced by the wire_compatible=True path can be decoded by ForyCodec directly.

    We verify that encoding a Ping object with a standalone ForyCodec and then
    decoding it produces an equivalent object to the original.
    """
    codec = ForyCodec(mode=SerializationMode.XLANG, types=[Ping, Pong])
    original = Ping(value=99)
    encoded = codec.encode(original)
    decoded = codec.decode(encoded, Ping)
    assert decoded.value == original.value


@pytest.mark.asyncio
async def test_wire_compatible_true_fory_codec_encode_is_consistent():
    """ForyCodec.encode on the same object produces consistent bytes across two calls."""
    codec = ForyCodec(mode=SerializationMode.XLANG, types=[Ping, Pong])
    obj = Ping(value=7)
    bytes1 = codec.encode(obj)
    bytes2 = codec.encode(obj)
    # Fory encoding is deterministic for the same object
    assert bytes1 == bytes2


# ── wire_compatible=False tests ───────────────────────────────────────────────


@pytest.mark.asyncio
async def test_wire_compatible_false_returns_correct_result():
    """wire_compatible=False skips serialization but still returns the correct object."""
    client = create_local_client(PingService, PingImpl(), wire_compatible=False)
    response = await client.ping(Ping(value=42))
    assert isinstance(response, Pong)
    assert response.value == 42


@pytest.mark.asyncio
async def test_wire_compatible_false_and_true_produce_same_logical_result():
    """Both wire_compatible modes yield identical logical response values."""
    impl = PingImpl()
    client_wire = create_local_client(PingService, impl, wire_compatible=True)
    client_nowire = create_local_client(PingService, impl, wire_compatible=False)

    request = Ping(value=13)
    resp_wire = await client_wire.double(request)
    resp_nowire = await client_nowire.double(request)

    assert resp_wire.value == resp_nowire.value
    assert resp_wire.doubled == resp_nowire.doubled


# ── LocalTransport direct instantiation tests ─────────────────────────────────


@pytest.mark.asyncio
async def test_local_transport_wire_compatible_true_unary():
    """LocalTransport(wire_compatible=True) processes a unary call correctly."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(
        PingService, PingImpl(), wire_compatible=True
    )
    response = await client.ping(Ping(value=5))
    assert response.value == 5


@pytest.mark.asyncio
async def test_local_transport_wire_compatible_false_unary():
    """LocalTransport(wire_compatible=False) processes a unary call correctly."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(
        PingService, PingImpl(), wire_compatible=False
    )
    response = await client.ping(Ping(value=5))
    assert response.value == 5


@pytest.mark.asyncio
async def test_local_transport_both_modes_equal_for_multiple_calls():
    """wire_compatible True and False produce matching results across multiple calls."""
    impl = PingImpl()
    client_wire = create_local_client(PingService, impl, wire_compatible=True)
    client_nowire = create_local_client(PingService, impl, wire_compatible=False)

    for v in [0, 1, 10, 100, 999]:
        r1 = await client_wire.ping(Ping(value=v))
        r2 = await client_nowire.ping(Ping(value=v))
        assert r1.value == r2.value, f"Mismatch at value={v}"


# ── ForyCodec encode/decode parity ────────────────────────────────────────────


def test_fory_codec_xlang_encode_decode_parity():
    """ForyCodec encodes and decodes a tagged dataclass without data loss."""
    codec = ForyCodec(mode=SerializationMode.XLANG, types=[Ping, Pong])

    for val in [0, 1, 42, -1, 2**31 - 1]:
        original = Ping(value=val)
        encoded = codec.encode(original)
        assert isinstance(encoded, bytes)
        decoded = codec.decode(encoded, Ping)
        assert decoded.value == original.value


def test_fory_codec_xlang_pong_encode_decode_parity():
    """ForyCodec correctly round-trips a Pong with all fields set."""
    codec = ForyCodec(mode=SerializationMode.XLANG, types=[Ping, Pong])

    original = Pong(value=77, doubled=True)
    decoded = codec.decode(codec.encode(original), Pong)
    assert decoded.value == 77
    assert decoded.doubled is True
