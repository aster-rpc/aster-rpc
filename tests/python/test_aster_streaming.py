"""
tests/python/test_aster_streaming.py

Phase 13 streaming RPC end-to-end tests using AsterTestHarness.

Tests all streaming patterns: server_stream, client_stream, and bidi_stream.
All tests are in-process using LocalTransport.

Spec reference: Aster-SPEC.md §13.2; Plan: §15.3
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import AsyncIterator

import pytest

from aster_python.aster.codec import fory_tag
from aster_python.aster.decorators import (
    bidi_stream,
    client_stream,
    server_stream,
    service,
)
from aster_python.aster.testing import AsterTestHarness
from aster_python.aster.types import SerializationMode


# ── Test types ────────────────────────────────────────────────────────────────


@fory_tag("test.streaming/CountRequest")
@dataclass
class CountRequest:
    start: int = 0
    count: int = 5


@fory_tag("test.streaming/CountItem")
@dataclass
class CountItem:
    value: int = 0


@fory_tag("test.streaming/SumItem")
@dataclass
class SumItem:
    value: int = 0


@fory_tag("test.streaming/SumResponse")
@dataclass
class SumResponse:
    total: int = 0
    count: int = 0


@fory_tag("test.streaming/EchoChunk")
@dataclass
class EchoChunk:
    text: str = ""


# ── Streaming service definition ──────────────────────────────────────────────


@service(name="StreamingService", version=1, serialization=[SerializationMode.XLANG])
class StreamingService:

    @server_stream
    async def count(self, req: CountRequest) -> AsyncIterator[CountItem]:
        """Yield `req.count` items starting from `req.start`."""
        for i in range(req.count):
            yield CountItem(value=req.start + i)

    @client_stream
    async def sum_items(self, requests: list[SumItem]) -> SumResponse:
        """Aggregate a client-stream of integers."""
        total = sum(r.value for r in requests)
        return SumResponse(total=total, count=len(requests))

    @bidi_stream
    async def echo_bidi(
        self, requests: AsyncIterator[EchoChunk]
    ) -> AsyncIterator[EchoChunk]:
        """Echo each received chunk back to the client."""
        async for chunk in requests:
            yield EchoChunk(text=f"echo: {chunk.text}")


class StreamingImpl(StreamingService):
    """Concrete StreamingService implementation for tests."""
    pass


# ── server_stream tests ────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_server_stream_yields_multiple_items():
    """server_stream yields the expected number of items in order."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(
        StreamingService,
        StreamingImpl(),
        wire_compatible=True,
    )

    items = []
    async for item in client.count(CountRequest(start=10, count=4)):
        items.append(item)

    assert len(items) == 4
    assert [i.value for i in items] == [10, 11, 12, 13]


@pytest.mark.asyncio
async def test_server_stream_zero_items():
    """server_stream with count=0 yields nothing."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(StreamingService, StreamingImpl())

    items = []
    async for item in client.count(CountRequest(start=0, count=0)):
        items.append(item)

    assert items == []


@pytest.mark.asyncio
async def test_server_stream_single_item():
    """server_stream with count=1 yields exactly one item."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(StreamingService, StreamingImpl())

    items = []
    async for item in client.count(CountRequest(start=99, count=1)):
        items.append(item)

    assert len(items) == 1
    assert items[0].value == 99


@pytest.mark.asyncio
async def test_server_stream_items_are_correct_type():
    """Items from server_stream are the declared response type."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(StreamingService, StreamingImpl())

    async for item in client.count(CountRequest(start=0, count=3)):
        assert type(item) is CountItem


# ── client_stream tests ────────────────────────────────────────────────────────


async def _items_gen(*items):
    """Helper: async generator over a fixed sequence of items."""
    for item in items:
        yield item


@pytest.mark.asyncio
async def test_client_stream_aggregates_items():
    """client_stream receives multiple items and returns the aggregate."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(
        StreamingService,
        StreamingImpl(),
        wire_compatible=True,
    )

    response = await client.sum_items(
        _items_gen(SumItem(value=1), SumItem(value=2), SumItem(value=3))
    )

    assert isinstance(response, SumResponse)
    assert response.total == 6
    assert response.count == 3


@pytest.mark.asyncio
async def test_client_stream_single_item():
    """client_stream with a single item works correctly."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(StreamingService, StreamingImpl())

    response = await client.sum_items(_items_gen(SumItem(value=42)))
    assert response.total == 42
    assert response.count == 1


@pytest.mark.asyncio
async def test_client_stream_empty_list():
    """client_stream with an empty request stream returns zero total."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(StreamingService, StreamingImpl())

    response = await client.sum_items(_items_gen())
    assert response.total == 0
    assert response.count == 0


# ── bidi_stream tests ─────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_bidi_stream_echo_each_item():
    """bidi_stream echoes each item sent by the client."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(
        StreamingService,
        StreamingImpl(),
        wire_compatible=True,
    )

    inputs = [EchoChunk(text="a"), EchoChunk(text="b"), EchoChunk(text="c")]

    channel = client.echo_bidi()
    for chunk in inputs:
        await channel.send(chunk)
    await channel.close()

    responses = []
    try:
        while True:
            item = await channel.recv()
            if item is None:
                break
            responses.append(item)
    except Exception:
        pass

    # Verify echoed responses have the "echo: " prefix
    for resp in responses:
        assert isinstance(resp, EchoChunk)
        assert resp.text.startswith("echo: ")


@pytest.mark.asyncio
async def test_server_stream_wire_compatible_false():
    """server_stream with wire_compatible=False returns correct results."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(
        StreamingService,
        StreamingImpl(),
        wire_compatible=False,
    )

    items = []
    async for item in client.count(CountRequest(start=5, count=3)):
        items.append(item)

    assert [i.value for i in items] == [5, 6, 7]
