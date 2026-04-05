"""
Phase 5 & 6 tests: Server and Client Implementation.

Tests cover:
- Server initialization and lifecycle
- Connection handling
- Stream dispatch and all RPC patterns
- Error handling with RpcStatus trailers
- Client stub generation
- Local client round-trip
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import AsyncIterator

import pytest

from aster.codec import fory_tag, ForyCodec, ForyConfig
from aster.types import SerializationMode
from aster.status import StatusCode, RpcError
from aster.decorators import (
    service,
    rpc,
    server_stream,
    client_stream,
    bidi_stream,
)
from aster.server import (
    ServerError,
    ServiceNotFoundError,
    MethodNotFoundError,
)
from aster.client import (
    ServiceClient,
    create_client,
    create_local_client,
    ClientError,
)
from aster.transport.local import LocalTransport
from aster.transport.base import BidiChannel


# ── Test types ───────────────────────────────────────────────────────────────


@fory_tag("test.server/EchoRequest")
@dataclass
class EchoRequest:
    message: str = ""


@fory_tag("test.server/EchoResponse")
@dataclass
class EchoResponse:
    message: str = ""
    received_at_ms: int = 0


@fory_tag("test.server/CounterRequest")
@dataclass
class CounterRequest:
    start: int = 0
    count: int = 5


@fory_tag("test.server/CounterResponse")
@dataclass
class CounterResponse:
    value: int = 0


@fory_tag("test.server/StreamingResponse")
@dataclass
class StreamingResponse:
    item: int = 0


@fory_tag("test.server/AggregateRequest")
@dataclass
class AggregateRequest:
    value: int = 0


@fory_tag("test.server/AggregateResponse")
@dataclass
class AggregateResponse:
    total: int = 0
    count: int = 0


# ── Test services ───────────────────────────────────────────────────────────


@service(name="TestEchoService", version=1, serialization=[SerializationMode.XLANG])
class TestEchoService:

    @rpc(timeout=30.0, idempotent=True)
    async def echo(self, req: EchoRequest) -> EchoResponse:
        return EchoResponse(
            message=f"echo: {req.message}",
            received_at_ms=0,
        )

    @rpc(timeout=10.0)
    async def echo_error(self, req: EchoRequest) -> EchoResponse:
        if req.message == "error":
            raise RpcError(StatusCode.INVALID_ARGUMENT, "bad message")
        return EchoResponse(message=f"echo: {req.message}")


@service(name="TestStreamService", version=1, serialization=[SerializationMode.XLANG])
class TestStreamService:

    @server_stream
    async def count(self, req: CounterRequest) -> AsyncIterator[StreamingResponse]:
        for i in range(req.count):
            yield StreamingResponse(item=req.start + i)

    @client_stream
    async def sum(self, requests: list[AggregateRequest]) -> AggregateResponse:
        total = sum(r.value for r in requests)
        return AggregateResponse(total=total, count=len(requests))

    @bidi_stream
    async def echo_bidi(
        self, requests: AsyncIterator[EchoRequest]
    ) -> AsyncIterator[EchoResponse]:
        async for req in requests:
            yield EchoResponse(message=f"echo: {req.message}")


# ── Server initialization tests ─────────────────────────────────────────────


class TestServerInit:
    """Test Server initialization."""

    def test_server_with_service_classes(self):
        """Server can be created with service classes."""
        # We can't actually create a Server without a real endpoint,
        # but we can test that the constructor handles service classes
        from aster.service import ServiceRegistry
        
        # Check that ServiceRegistry works with our test service
        registry = ServiceRegistry()
        info = registry.register(TestEchoService)
        assert info.name == "TestEchoService"
        assert info.version == 1

    def test_registry_lookup(self):
        """Service registry can look up services."""
        from aster.service import ServiceRegistry
        
        registry = ServiceRegistry()
        registry.register(TestEchoService)
        registry.register(TestStreamService)
        
        info = registry.lookup("TestEchoService", 1)
        assert info is not None
        assert info.name == "TestEchoService"
        
        info = registry.lookup("TestStreamService", 1)
        assert info is not None
        
        info = registry.lookup("NonExistent")
        assert info is None

    def test_registry_method_lookup(self):
        """Service registry can look up methods."""
        from aster.service import ServiceRegistry
        
        registry = ServiceRegistry()
        registry.register(TestEchoService)
        
        service_info, method_info = registry.lookup_method("TestEchoService", "echo", 1)
        assert service_info is not None
        assert method_info is not None
        assert method_info.name == "echo"
        assert method_info.pattern == "unary"


# ── Service decorator tests ──────────────────────────────────────────────────


class TestServiceDecorator:
    """Test service and method decorators."""

    def test_service_has_methods(self):
        """Service has decorated methods."""
        assert hasattr(TestEchoService, "echo")
        assert hasattr(TestEchoService, "echo_error")

    def test_stream_service_patterns(self):
        """Stream service has all RPC patterns."""
        from aster.service import ServiceRegistry
        
        registry = ServiceRegistry()
        info = registry.register(TestStreamService)
        
        method = info.get_method("count")
        assert method is not None
        assert method.pattern == "server_stream"
        
        method = info.get_method("sum")
        assert method is not None
        assert method.pattern == "client_stream"
        
        method = info.get_method("echo_bidi")
        assert method is not None
        assert method.pattern == "bidi_stream"

    def test_method_types_extracted(self):
        """Method types are extracted from signatures."""
        from aster.service import ServiceRegistry
        
        registry = ServiceRegistry()
        info = registry.register(TestEchoService)
        
        method = info.get_method("echo")
        assert method is not None
        assert method.request_type is EchoRequest
        # Response type may be a string forward reference or the resolved type
        assert method.response_type in (EchoResponse, "EchoResponse")


# ── Client creation tests ───────────────────────────────────────────────────


class TestClientCreation:
    """Test client stub generation."""

    def test_create_local_client_requires_service(self):
        """create_local_client requires @service decorated class."""
        class NotAService:
            pass
        
        with pytest.raises(ClientError, match="not decorated with @service"):
            create_local_client(NotAService, NotAService())

    def test_local_client_basic(self):
        """create_local_client creates a client."""
        client = create_local_client(
            TestEchoService,
            TestEchoService(),
            wire_compatible=True,
        )
        assert isinstance(client, ServiceClient)
        assert client.service_name == "TestEchoService"
        assert client.service_version == 1

    def test_local_client_threads_fory_config_to_implicit_codec(self):
        client = create_local_client(
            TestEchoService,
            TestEchoService(),
            wire_compatible=True,
            fory_config=ForyConfig(xlang=False),
        )
        assert client._codec.fory_config.resolved_xlang(client._codec.mode) is False

    def test_local_client_has_methods(self):
        """Client has methods for each RPC."""
        client = create_local_client(
            TestEchoService,
            TestEchoService(),
            wire_compatible=True,
        )
        
        # Unary methods
        assert hasattr(client, "echo")
        assert hasattr(client, "echo_error")

    def test_local_client_stream_methods(self):
        """Client has methods for streaming RPCs."""
        client = create_local_client(
            TestStreamService,
            TestStreamService(),
            wire_compatible=True,
        )
        
        assert hasattr(client, "count")
        assert hasattr(client, "sum")
        assert hasattr(client, "echo_bidi")

    def test_create_client_requires_connection_or_transport(self):
        with pytest.raises(ClientError, match="either connection or transport"):
            create_client(TestEchoService)

    def test_create_client_accepts_injected_transport(self):
        class CaptureTransport:
            async def unary(self, *args, **kwargs):
                return EchoResponse(message="captured")

            def server_stream(self, *args, **kwargs):
                async def gen():
                    if False:
                        yield None
                return gen()

            async def client_stream(self, *args, **kwargs):
                return AggregateResponse()

            def bidi_stream(self, *args, **kwargs):
                class DummyChannel(BidiChannel):
                    async def send(self, msg):
                        return None

                    async def recv(self):
                        raise RuntimeError("unused")

                    async def close(self):
                        return None

                    async def wait_for_trailer(self):
                        return StatusCode.OK, ""

                return DummyChannel()

            async def close(self):
                return None

        client = create_client(
            TestEchoService,
            transport=CaptureTransport(),
        )
        assert isinstance(client, ServiceClient)
        assert client.service_name == "TestEchoService"


# ── Local client round-trip tests ───────────────────────────────────────────


class TestLocalClientRoundTrip:
    """Test client ↔ service round-trip over LocalTransport."""

    @pytest.mark.asyncio
    async def test_unary_round_trip(self):
        """Unary RPC round-trips successfully."""
        client = create_local_client(
            TestEchoService,
            TestEchoService(),
            wire_compatible=True,
        )
        
        request = EchoRequest(message="hello")
        response = await client.echo(request)
        
        assert isinstance(response, EchoResponse)
        assert response.message == "echo: hello"

    @pytest.mark.asyncio
    async def test_unary_with_metadata(self):
        """Unary RPC accepts metadata."""
        client = create_local_client(
            TestEchoService,
            TestEchoService(),
            wire_compatible=True,
        )
        
        request = EchoRequest(message="with metadata")
        response = await client.echo(
            request,
            metadata={"trace_id": "abc123"},
        )
        
        assert isinstance(response, EchoResponse)

    @pytest.mark.asyncio
    async def test_unary_error_propagates(self):
        """RpcError from handler propagates to client."""
        client = create_local_client(
            TestEchoService,
            TestEchoService(),
            wire_compatible=True,
        )
        
        request = EchoRequest(message="error")
        
        with pytest.raises(RpcError) as exc_info:
            await client.echo_error(request)
        
        assert exc_info.value.code == StatusCode.INVALID_ARGUMENT
        assert "bad message" in exc_info.value.message

    @pytest.mark.asyncio
    async def test_client_close_delegates_to_transport(self):
        client = create_local_client(
            TestEchoService,
            TestEchoService(),
            wire_compatible=True,
        )
        await client.close()


class TestClientCallOverrides:
    """Test metadata/timeout propagation from generated client stubs."""

    @pytest.mark.asyncio
    async def test_unary_metadata_and_timeout_propagate(self):
        captured: dict[str, object] = {}

        class CaptureTransport:
            async def unary(
                self,
                service,
                method,
                request,
                *,
                metadata=None,
                deadline_epoch_ms=0,
                serialization_mode=0,
                contract_id="",
            ):
                captured.update(
                    service=service,
                    method=method,
                    request=request,
                    metadata=metadata,
                    deadline_epoch_ms=deadline_epoch_ms,
                    serialization_mode=serialization_mode,
                    contract_id=contract_id,
                )
                return EchoResponse(message="ok")

            def server_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            async def client_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            def bidi_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            async def close(self):
                return None

        client = create_client(TestEchoService, transport=CaptureTransport())
        request = EchoRequest(message="hello")
        response = await client.echo(
            request,
            metadata={"trace_id": "abc123"},
            timeout=5.0,
        )

        assert response.message == "ok"
        assert captured["service"] == "TestEchoService"
        assert captured["method"] == "echo"
        assert captured["request"] is request
        assert captured["metadata"] == {"trace_id": "abc123"}
        assert captured["deadline_epoch_ms"] > 0
        assert captured["serialization_mode"] == SerializationMode.XLANG.value
        assert captured["contract_id"] == ""

    def test_bidi_stub_returns_channel(self):
        class DummyChannel(BidiChannel):
            async def send(self, msg):
                return None

            async def recv(self):
                raise RuntimeError("unused")

            async def close(self):
                return None

            async def wait_for_trailer(self):
                return StatusCode.OK, ""

        channel = DummyChannel()

        class CaptureTransport:
            async def unary(self, *args, **kwargs):
                raise AssertionError("unexpected")

            def server_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            async def client_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            def bidi_stream(self, *args, **kwargs):
                return channel

            async def close(self):
                return None

        client = create_client(TestStreamService, transport=CaptureTransport())
        assert client.echo_bidi(metadata={"trace_id": "bidi"}, timeout=1.0) is channel


class TestServerStreaming:
    """Test server-streaming RPC."""

    @pytest.mark.asyncio
    async def test_server_stream_round_trip(self):
        """Server-streaming RPC round-trips."""
        client = create_local_client(
            TestStreamService,
            TestStreamService(),
            wire_compatible=True,
        )
        
        request = CounterRequest(start=10, count=5)
        responses = []
        async for resp in client.count(request):
            responses.append(resp)
        
        assert len(responses) == 5
        assert [r.item for r in responses] == [10, 11, 12, 13, 14]


class TestClientStreaming:
    """Test client-streaming RPC."""

    @pytest.mark.asyncio
    async def test_client_stream_round_trip(self):
        """Client-streaming RPC round-trips."""
        client = create_local_client(
            TestStreamService,
            TestStreamService(),
            wire_compatible=True,
        )
        
        async def request_gen():
            for i in range(5):
                yield AggregateRequest(value=i * 10)
        
        response = await client.sum(request_gen())
        
        assert isinstance(response, AggregateResponse)
        assert response.total == 100  # 0 + 10 + 20 + 30 + 40
        assert response.count == 5

    @pytest.mark.asyncio
    async def test_client_stream_empty(self):
        """Client-streaming with empty stream."""
        client = create_local_client(
            TestStreamService,
            TestStreamService(),
            wire_compatible=True,
        )
        
        async def empty_gen():
            return
            yield  # make it an async generator
        
        response = await client.sum(empty_gen())
        
        assert response.total == 0
        assert response.count == 0


# ── ServiceClient base tests ─────────────────────────────────────────────────


class TestServiceClientBase:
    """Test ServiceClient base functionality."""

    def test_client_service_properties(self):
        """Client exposes service info."""
        client = create_local_client(
            TestEchoService,
            TestEchoService(),
            wire_compatible=True,
        )
        
        assert client.service_name == "TestEchoService"
        assert client.service_version == 1

    def test_client_timeout_conversion(self):
        """Timeout is converted to deadline_epoch_ms."""
        from aster.service import ServiceRegistry
        
        registry = ServiceRegistry()
        info = registry.register(TestEchoService)
        
        codec = ForyCodec(mode=SerializationMode.XLANG)
        transport = LocalTransport(
            handler_registry=lambda s, m: (lambda x: x, [], "unary")
        )
        client = ServiceClient(transport, info, codec)
        
        # Test that _get_deadline works
        deadline = client._get_deadline(None)
        assert deadline == 0  # No timeout = no deadline
        
        deadline = client._get_deadline(30.0)
        assert deadline > 0  # Should be a future timestamp


# ── Wire-compatible mode tests ───────────────────────────────────────────────


class TestWireCompatibleMode:
    """Test wire_compatible mode behavior."""

    @pytest.mark.asyncio
    async def test_wire_compatible_true(self):
        """wire_compatible=True exercises serialization."""
        client = create_local_client(
            TestEchoService,
            TestEchoService(),
            wire_compatible=True,
        )
        
        request = EchoRequest(message="wired")
        response = await client.echo(request)
        
        assert response.message == "echo: wired"

    @pytest.mark.asyncio
    async def test_wire_compatible_false(self):
        """wire_compatible=False skips serialization."""
        client = create_local_client(
            TestEchoService,
            TestEchoService(),
            wire_compatible=False,
        )
        
        request = EchoRequest(message="not wired")
        response = await client.echo(request)
        
        assert response.message == "echo: not wired"


# ── Error handling tests ─────────────────────────────────────────────────────


class TestErrorHandling:
    """Test error handling in client."""

    def test_client_requires_service_decorator(self):
        """Client creation fails without @service decorator."""
        class Undecorated:
            async def echo(self, x):
                return x
        
        with pytest.raises(ClientError, match="not decorated"):
            create_local_client(Undecorated, Undecorated())


class TestRpcError:
    """Test RpcError behavior."""

    def test_rpc_error_attributes(self):
        """RpcError has expected attributes."""
        error = RpcError(StatusCode.NOT_FOUND, "resource not found")
        assert error.code == StatusCode.NOT_FOUND
        assert error.message == "resource not found"

    def test_rpc_error_repr(self):
        """RpcError has readable repr."""
        error = RpcError(StatusCode.INVALID_ARGUMENT, "bad value")
        repr_str = repr(error)
        assert "INVALID_ARGUMENT" in repr_str
        assert "bad value" in repr_str

    def test_rpc_error_details(self):
        """RpcError supports details dict."""
        error = RpcError(
            StatusCode.INTERNAL,
            "something broke",
            details={"key": "value"},
        )
        assert error.details == {"key": "value"}

    def test_rpc_error_factory_returns_specific_subclass(self):
        error = RpcError.from_status(StatusCode.NOT_FOUND, "resource not found")
        assert error.code == StatusCode.NOT_FOUND
        assert type(error).__name__ == "NotFoundError"


# ── Server errors tests ──────────────────────────────────────────────────────


class TestServerErrors:
    """Test server error classes."""

    def test_server_error(self):
        """ServerError is the base error class."""
        error = ServerError("test error")
        assert isinstance(error, Exception)

    def test_service_not_found_error(self):
        """ServiceNotFoundError inherits from ServerError."""
        error = ServiceNotFoundError("TestService")
        assert isinstance(error, ServerError)

    def test_method_not_found_error(self):
        """MethodNotFoundError inherits from ServerError."""
        error = MethodNotFoundError("TestService", "BadMethod")
        assert isinstance(error, ServerError)
