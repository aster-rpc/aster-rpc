"""
tests/python/test_aster_unary.py

Phase 13 end-to-end unary RPC tests using AsterTestHarness.

All tests are in-process (no real network); the harness uses LocalTransport.

Spec reference: Aster-SPEC.md §13.2; Plan: §15.3
"""

from __future__ import annotations

from dataclasses import dataclass

import pytest

from aster.codec import wire_type
from aster.decorators import service, rpc
from aster.status import StatusCode, RpcError
from aster.testing import AsterTestHarness
from aster.types import SerializationMode


# ── Test types ────────────────────────────────────────────────────────────────


@wire_type("test.unary/EchoRequest")
@dataclass
class EchoRequest:
    message: str = ""


@wire_type("test.unary/EchoResponse")
@dataclass
class EchoResponse:
    message: str = ""


# ── Service definition ────────────────────────────────────────────────────────


@service(name="EchoService", version=1, serialization=[SerializationMode.XLANG])
class EchoService:

    @rpc(timeout=10.0, idempotent=True)
    async def echo(self, req: EchoRequest) -> EchoResponse:
        return EchoResponse(message=f"echo: {req.message}")

    @rpc(timeout=10.0)
    async def echo_error(self, req: EchoRequest) -> EchoResponse:
        if req.message == "error":
            raise RpcError(StatusCode.INVALID_ARGUMENT, "bad message")
        return EchoResponse(message=f"echo: {req.message}")

    @rpc(timeout=10.0)
    async def echo_with_metadata(self, req: EchoRequest) -> EchoResponse:
        return EchoResponse(message=f"echo: {req.message}")


class EchoImpl(EchoService):
    """Concrete EchoService implementation for tests."""
    pass


# ── Tests ─────────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_unary_basic_round_trip():
    """Basic unary round-trip via AsterTestHarness.create_local_pair."""
    harness = AsterTestHarness()
    client, impl = await harness.create_local_pair(
        EchoService,
        EchoImpl(),
        wire_compatible=True,
    )

    response = await client.echo(EchoRequest(message="hello"))

    assert isinstance(response, EchoResponse)
    assert response.message == "echo: hello"


@pytest.mark.asyncio
async def test_unary_error_propagation():
    """RpcError raised by the handler propagates back to the caller."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(EchoService, EchoImpl())

    with pytest.raises(RpcError) as exc_info:
        await client.echo_error(EchoRequest(message="error"))

    assert exc_info.value.code == StatusCode.INVALID_ARGUMENT
    assert "bad message" in exc_info.value.message


@pytest.mark.asyncio
async def test_unary_non_error_path_after_error_path():
    """A non-error call succeeds even after a prior error call on the same client."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(EchoService, EchoImpl())

    with pytest.raises(RpcError):
        await client.echo_error(EchoRequest(message="error"))

    response = await client.echo_error(EchoRequest(message="ok"))
    assert response.message == "echo: ok"


@pytest.mark.asyncio
async def test_unary_metadata_threading():
    """Metadata dict is accepted without error and call completes successfully."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(EchoService, EchoImpl())

    response = await client.echo(
        EchoRequest(message="with metadata"),
        metadata={"trace_id": "abc123", "user": "test"},
    )

    assert isinstance(response, EchoResponse)
    assert response.message == "echo: with metadata"


@pytest.mark.asyncio
async def test_unary_multiple_sequential_calls():
    """Multiple sequential unary calls on the same client all succeed."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(EchoService, EchoImpl())

    messages = ["first", "second", "third", "fourth", "fifth"]
    for msg in messages:
        response = await client.echo(EchoRequest(message=msg))
        assert response.message == f"echo: {msg}"


@pytest.mark.asyncio
async def test_unary_wire_compatible_false():
    """wire_compatible=False skips full serialization but still returns correct results."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(
        EchoService,
        EchoImpl(),
        wire_compatible=False,
    )

    response = await client.echo(EchoRequest(message="no wire"))
    assert isinstance(response, EchoResponse)
    assert response.message == "echo: no wire"


@pytest.mark.asyncio
async def test_unary_client_returns_correct_type():
    """Unary response is the exact dataclass type, not a dict or proxy."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(EchoService, EchoImpl())

    response = await client.echo(EchoRequest(message="type check"))
    assert type(response) is EchoResponse


@pytest.mark.asyncio
async def test_unary_empty_message():
    """Empty message string is handled without error."""
    harness = AsterTestHarness()
    client, _ = await harness.create_local_pair(EchoService, EchoImpl())

    response = await client.echo(EchoRequest(message=""))
    assert response.message == "echo: "
