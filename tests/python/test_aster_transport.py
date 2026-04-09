"""
Phase 3 tests: Transport Abstraction.

Tests cover:
- IrohTransport unary round-trip over real Iroh connection
- LocalTransport unary round-trip
- BidiChannel for both transports
- wire_compatible=True catches missing type tags
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import AsyncIterator

import pytest

from aster.codec import wire_type, ForyCodec, ForyConfig
from aster.framing import HEADER, TRAILER, COMPRESSED, write_frame, read_frame
from aster.protocol import StreamHeader, RpcStatus
from aster.rpc_types import SerializationMode
from aster.status import StatusCode
from aster.transport.base import (
    Transport,
    BidiChannel,
    TransportError,
    ConnectionLostError,
)
from aster.transport.iroh import IrohTransport
from aster.transport.local import LocalTransport, LocalBidiChannel


# ── Test types ───────────────────────────────────────────────────────────────


@wire_type("test.transport/EchoRequest")
@dataclass
class EchoRequest:
    message: str = ""


@wire_type("test.transport/EchoResponse")
@dataclass
class EchoResponse:
    message: str = ""
    received_at_ms: int = 0


@wire_type("test.transport/CounterRequest")
@dataclass
class CounterRequest:
    start: int = 0
    count: int = 5


@wire_type("test.transport/CounterResponse")
@dataclass
class CounterResponse:
    value: int = 0


@wire_type("test.transport/StreamingResponse")
@dataclass
class StreamingResponse:
    item: int = 0


@wire_type("test.transport/AggregateRequest")
@dataclass
class AggregateRequest:
    value: int = 0


@wire_type("test.transport/AggregateResponse")
@dataclass
class AggregateResponse:
    total: int = 0
    count: int = 0


@dataclass
class UntaggedRequest:
    """A type WITHOUT @wire_type -- should fail XLANG registration."""
    data: str = ""


# ── Handler registry for LocalTransport tests ─────────────────────────────────


def make_handler_registry():
    """Create a handler registry with test service handlers."""
    
    async def echo_unary(request: EchoRequest) -> EchoResponse:
        return EchoResponse(
            message=f"echo: {request.message}",
            received_at_ms=0,
        )

    async def echo_server_stream(request: CounterRequest) -> AsyncIterator[StreamingResponse]:
        for i in range(request.count):
            yield StreamingResponse(item=request.start + i)

    async def echo_client_stream(requests: list[AggregateRequest]) -> AggregateResponse:
        total = sum(r.value for r in requests)
        return AggregateResponse(total=total, count=len(requests))

    async def echo_bidi_stream():
        """Bidi stream handler that echoes values."""
        # This is a simplified handler for testing
        yield StreamingResponse(item=42)

    handlers = {
        ("TestService", "Echo"): (echo_unary, [EchoRequest, EchoResponse], "unary"),
        ("TestService", "ServerStream"): (echo_server_stream, [CounterRequest, StreamingResponse], "server_stream"),
        ("TestService", "ClientStream"): (echo_client_stream, [AggregateRequest, AggregateResponse], "client_stream"),
        ("TestService", "BidiStream"): (echo_bidi_stream, [StreamingResponse], "bidi_stream"),
    }

    def registry(service: str, method: str):
        key = (service, method)
        if key not in handlers:
            raise TransportError(f"no handler for {service}/{method}")
        return handlers[key]

    return registry


# ── Transport protocol tests ─────────────────────────────────────────────────


class TestTransportProtocol:
    """Verify Transport protocol compliance."""

    def test_transport_has_required_methods(self):
        """Transport must have all required abstract methods."""
        required = ["unary", "server_stream", "client_stream", "bidi_stream", "close"]
        for method in required:
            assert hasattr(Transport, method), f"Transport missing {method}"

    def test_bidi_channel_has_required_methods(self):
        """BidiChannel must have all required methods."""
        required = ["send", "recv", "close", "wait_for_trailer"]
        for method in required:
            assert hasattr(BidiChannel, method), f"BidiChannel missing {method}"


# ── LocalTransport tests ────────────────────────────────────────────────────


class TestLocalTransportInit:
    """Test LocalTransport initialization."""

    def test_basic_init(self):
        """LocalTransport can be created with just a registry."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)
        assert transport is not None
        assert transport._wire_compatible is False

    def test_with_codec(self):
        """LocalTransport can be created with a custom codec."""
        registry = make_handler_registry()
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[EchoRequest, EchoResponse])
        transport = LocalTransport(handler_registry=registry, codec=codec)
        assert transport._codec is codec

    def test_wire_compatible_flag(self):
        """wire_compatible flag is stored."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry, wire_compatible=True)
        assert transport._wire_compatible is True

    def test_default_codec_is_xlang(self):
        """Default codec is XLANG mode."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)
        assert transport._codec.mode == SerializationMode.XLANG

    def test_default_codec_enables_xlang(self):
        """Default implicit codec enables pyfory xlang mode."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)
        assert transport._codec.fory_config.resolved_xlang(transport._codec.mode) is True

    def test_default_codec_accepts_fory_config(self):
        """LocalTransport threads fory_config into implicit codec creation."""
        registry = make_handler_registry()
        transport = LocalTransport(
            handler_registry=registry,
            fory_config=ForyConfig(xlang=False),
        )
        assert transport._codec.fory_config.resolved_xlang(transport._codec.mode) is False


class TestLocalTransportUnary:
    """Test LocalTransport unary RPC."""

    @pytest.mark.asyncio
    async def test_unary_round_trip(self):
        """A unary call completes successfully."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)

        request = EchoRequest(message="hello")
        response = await transport.unary(
            "TestService",
            "Echo",
            request,
        )

        assert isinstance(response, EchoResponse)
        assert response.message == "echo: hello"

    @pytest.mark.asyncio
    async def test_unary_with_metadata(self):
        """Unary call with metadata succeeds."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)

        request = EchoRequest(message="with metadata")
        response = await transport.unary(
            "TestService",
            "Echo",
            request,
            metadata={"trace_id": "abc123"},
        )

        assert isinstance(response, EchoResponse)
        assert "echo: with metadata" in response.message

    @pytest.mark.asyncio
    async def test_unary_unknown_service(self):
        """Unrecognized service raises error."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)

        request = EchoRequest(message="test")
        with pytest.raises(TransportError, match="no handler"):
            await transport.unary("UnknownService", "Method", request)

    @pytest.mark.asyncio
    async def test_unary_unknown_method(self):
        """Unrecognized method raises error."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)

        request = EchoRequest(message="test")
        with pytest.raises(TransportError, match="no handler"):
            await transport.unary("TestService", "UnknownMethod", request)


class TestLocalTransportServerStream:
    """Test LocalTransport server-streaming RPC."""

    @pytest.mark.asyncio
    async def test_server_stream_round_trip(self):
        """Server-streaming call returns async iterator."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)

        request = CounterRequest(start=10, count=5)
        responses = []
        async for resp in transport.server_stream("TestService", "ServerStream", request):
            responses.append(resp)

        assert len(responses) == 5
        assert [r.item for r in responses] == [10, 11, 12, 13, 14]

    @pytest.mark.asyncio
    async def test_server_stream_single_item(self):
        """Server-streaming with single item."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)

        request = CounterRequest(start=0, count=1)
        responses = []
        async for resp in transport.server_stream("TestService", "ServerStream", request):
            responses.append(resp)

        assert len(responses) == 1
        assert responses[0].item == 0


class TestLocalTransportClientStream:
    """Test LocalTransport client-streaming RPC."""

    @pytest.mark.asyncio
    async def test_client_stream_round_trip(self):
        """Client-streaming call aggregates requests."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)

        async def request_gen():
            for i in range(5):
                yield AggregateRequest(value=i * 10)

        response = await transport.client_stream(
            "TestService",
            "ClientStream",
            request_gen(),
        )

        assert isinstance(response, AggregateResponse)
        assert response.total == 100  # 0 + 10 + 20 + 30 + 40
        assert response.count == 5

    @pytest.mark.asyncio
    async def test_client_stream_empty(self):
        """Client-streaming with no requests."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)

        async def empty_gen():
            return
            yield  # make it an async generator

        response = await transport.client_stream(
            "TestService",
            "ClientStream",
            empty_gen(),
        )

        assert response.total == 0
        assert response.count == 0


class TestLocalBidiChannel:
    """Test LocalTransport bidirectional streaming."""

    @pytest.mark.asyncio
    async def test_bidi_channel_basic(self):
        """BidiChannel can be created and used."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)

        channel = transport.bidi_stream("TestService", "BidiStream")

        assert isinstance(channel, BidiChannel)
        assert isinstance(channel, LocalBidiChannel)
        await channel.close()

    @pytest.mark.asyncio
    async def test_bidi_channel_send_close(self):
        """BidiChannel send and close work."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)

        channel = transport.bidi_stream("TestService", "BidiStream")
        await channel.close()

        # After close, send should raise
        with pytest.raises(TransportError, match="closed"):
            await channel.send(StreamingResponse(item=1))


# ── wire_compatible mode tests ─────────────────────────────────────────────


class TestWireCompatibleMode:
    """Test wire_compatible mode catches serialization issues."""

    def test_codec_rejects_untagged_in_xlang(self):
        """XLANG codec rejects untagged types when not pre-registered."""
        # When a type is NOT in the codec's registered types list,
        # it will fail serialization. The specific error depends on pyfory version.
        codec = ForyCodec(mode=SerializationMode.XLANG)  # No types registered
        with pytest.raises(Exception):  # pyfory raises TypeUnregisteredError or similar
            codec.encode(UntaggedRequest(data="test"))

    def test_codec_requires_registration(self):
        """Codec requires types to be registered for serialization."""
        # Create codec with EchoRequest registered
        codec = ForyCodec(mode=SerializationMode.XLANG, types=[EchoRequest])
        
        # Tagged type works
        req = EchoRequest(message="hello")
        data = codec.encode(req)
        assert isinstance(data, bytes)
        
        # Can decode
        restored = codec.decode(data, EchoRequest)
        assert restored.message == "hello"

    @pytest.mark.asyncio
    async def test_wire_compatible_round_trip(self):
        """wire_compatible=True doesn't break valid round-trips."""
        registry = make_handler_registry()
        
        # Need to provide the codec with types registered
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=[EchoRequest, EchoResponse],
        )
        transport = LocalTransport(
            handler_registry=registry,
            codec=codec,
            wire_compatible=True,
        )

        request = EchoRequest(message="valid test")
        response = await transport.unary(
            "TestService",
            "Echo",
            request,
        )

        assert isinstance(response, EchoResponse)
        assert response.message == "echo: valid test"

    @pytest.mark.asyncio
    async def test_non_wire_compatible_skips_encoding(self):
        """wire_compatible=False skips the encoding step."""
        registry = make_handler_registry()
        transport = LocalTransport(
            handler_registry=registry,
            wire_compatible=False,
        )

        request = EchoRequest(message="skip encoding")
        response = await transport.unary(
            "TestService",
            "Echo",
            request,
        )

        # Should still work
        assert isinstance(response, EchoResponse)

    def test_untagged_fails_xlang_validation(self):
        """XLANG mode validation catches untagged types."""
        # XLANG mode requires @wire_type - codec initialization should fail
        with pytest.raises(TypeError, match="@wire_type"):
            ForyCodec(mode=SerializationMode.XLANG, types=[UntaggedRequest])


# ── Transport error tests ───────────────────────────────────────────────────


class TestTransportErrors:
    """Test transport-level error handling."""

    def test_transport_error_is_exception(self):
        err = TransportError("test error")
        assert isinstance(err, Exception)

    def test_connection_lost_error(self):
        err = ConnectionLostError("connection closed")
        assert isinstance(err, TransportError)

    @pytest.mark.asyncio
    async def test_local_transport_close_is_noop(self):
        """LocalTransport.close() is a no-op (no network resources)."""
        registry = make_handler_registry()
        transport = LocalTransport(handler_registry=registry)
        await transport.close()  # Should not raise


# ── In-memory stream tests ──────────────────────────────────────────────────


class TestMemStreams:
    """Test in-memory stream classes for testing."""

    @pytest.mark.asyncio
    async def test_mem_send_stream(self):
        """MemSendStream collects written data."""
        from aster.transport.local import MemSendStream
        
        stream = MemSendStream()
        await stream.write_all(b"hello")
        await stream.write_all(b" world")
        await stream.finish()
        
        assert bytes(stream.buf) == b"hello world"
        assert stream._finished is True

    @pytest.mark.asyncio
    async def test_mem_recv_stream_read_exact(self):
        """MemRecvStream.read_exact returns correct data."""
        from aster.transport.local import MemRecvStream
        
        stream = MemRecvStream(b"hello world")
        data = await stream.read_exact(5)
        assert data == b"hello"
        
        data = await stream.read_exact(6)
        assert data == b" world"

    @pytest.mark.asyncio
    async def test_mem_recv_stream_stop(self):
        """MemRecvStream.stop() prevents further reads."""
        from aster.transport.local import MemRecvStream
        
        stream = MemRecvStream(b"hello")
        stream.stop(0)
        
        with pytest.raises(EOFError, match="stopped"):
            await stream.read_exact(5)


# ── Integration tests with real Iroh (skipped if no connection) ───────────


class TestIrohTransportIntegration:
    """Integration tests using real Iroh connections.

    These tests are skipped by default unless an Iroh node is available.
    """

    @pytest.mark.asyncio
    async def test_iroh_transport_unary(self):
        """IrohTransport unary round-trip over a real in-memory Iroh connection."""
        import aster

        alpn = b"aster/1"
        codec = ForyCodec(
            mode=SerializationMode.XLANG,
            types=[EchoRequest, EchoResponse],
        )

        server_endpoint = await aster.create_endpoint(alpn)
        client_endpoint = await aster.create_endpoint(alpn)

        async def server_side() -> None:
            conn = await server_endpoint.accept()
            send, recv = await conn.accept_bi()

            try:
                # Read and validate stream header
                frame = await read_frame(recv)
                assert frame is not None
                payload, flags = frame
                assert flags & HEADER

                header = codec.decode(payload, StreamHeader)
                assert header.service == "TestService"
                assert header.method == "Echo"

                # Read request payload
                frame = await read_frame(recv)
                assert frame is not None
                payload, flags = frame
                request = codec.decode_compressed(
                    payload,
                    bool(flags & COMPRESSED),
                    EchoRequest,
                )

                # Write unary response
                response = EchoResponse(
                    message=f"echo: {request.message}",
                    received_at_ms=0,
                )
                response_payload, response_compressed = codec.encode_compressed(response)
                await write_frame(
                    send,
                    response_payload,
                    COMPRESSED if response_compressed else 0,
                )

                # Write OK trailer
                trailer = RpcStatus(code=StatusCode.OK, message="")
                await write_frame(send, codec.encode(trailer), flags=TRAILER)
                await send.finish()
            finally:
                conn.close(0, b"done")

        async def client_side() -> None:
            await asyncio.sleep(0.2)
            conn = await client_endpoint.connect_node_addr(
                server_endpoint.endpoint_addr_info(),
                alpn,
            )
            transport = IrohTransport(conn, codec=codec)
            try:
                response = await transport.unary(
                    "TestService",
                    "Echo",
                    EchoRequest(message="hello over iroh"),
                    metadata={"trace_id": "integration-test"},
                    serialization_mode=SerializationMode.XLANG.value,
                )
                assert isinstance(response, EchoResponse)
                assert response.message == "echo: hello over iroh"
            finally:
                await transport.close()

        try:
            await asyncio.wait_for(
                asyncio.gather(server_side(), client_side()),
                timeout=30,
            )
        finally:
            await server_endpoint.close()
            await client_endpoint.close()
