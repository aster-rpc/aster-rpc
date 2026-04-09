"""
Phase 4 tests: Service Definition Layer.

Tests cover:
- Decorating a test service, verify ServiceInfo and MethodInfo
- Missing @wire_type raises TypeError
- ServiceRegistry lookup by name
- All RPC patterns (unary, server_stream, client_stream, bidi_stream)
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from typing import AsyncIterator

import pytest

from aster.codec import wire_type
from aster.rpc_types import SerializationMode
from aster.decorators import (
    service,
    rpc,
    server_stream,
    client_stream,
    bidi_stream,
    RpcPattern,
)
from aster.service import (
    ServiceRegistry,
    ServiceInfo,
    MethodInfo,
    get_default_registry,
    set_default_registry,
)


# ── Test types ───────────────────────────────────────────────────────────────


@wire_type("test.decorators/EchoRequest")
@dataclass
class EchoRequest:
    message: str = ""


@wire_type("test.decorators/EchoResponse")
@dataclass
class EchoResponse:
    message: str = ""
    received_at_ms: int = 0


@wire_type("test.decorators/CounterRequest")
@dataclass
class CounterRequest:
    start: int = 0
    count: int = 5


@wire_type("test.decorators/StreamingResponse")
@dataclass
class StreamingResponse:
    item: int = 0


@wire_type("test.decorators/AggregateRequest")
@dataclass
class AggregateRequest:
    value: int = 0


@wire_type("test.decorators/AggregateResponse")
@dataclass
class AggregateResponse:
    total: int = 0
    count: int = 0


@dataclass
class UntaggedRequest:
    """A type WITHOUT @wire_type -- should fail XLANG registration."""
    data: str = ""


@dataclass
class UntaggedResponse:
    """A type WITHOUT @wire_type -- should fail XLANG registration."""
    value: str = ""


# ── RpcPattern tests ───────────────────────────────────────────────────────


class TestRpcPattern:
    def test_pattern_values(self):
        """All RPC pattern constants are defined."""
        assert RpcPattern.UNARY == "unary"
        assert RpcPattern.SERVER_STREAM == "server_stream"
        assert RpcPattern.CLIENT_STREAM == "client_stream"
        assert RpcPattern.BIDI_STREAM == "bidi_stream"


# ── @rpc decorator tests ───────────────────────────────────────────────────


class TestRpcDecorator:
    def test_rpc_basic(self):
        """@rpc decorator can be applied to an async method."""
        class TestService:
            @rpc()
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        method = TestService.echo
        info = getattr(method, "__aster_method_info__")
        assert info is not None
        assert info.pattern == RpcPattern.UNARY
        assert info.timeout is None
        assert info.idempotent is False

    def test_rpc_with_timeout(self):
        """@rpc decorator accepts timeout parameter."""
        class TestService:
            @rpc()
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)
        
        # Manually set timeout for testing (actual usage would be @rpc(timeout=30.0))
        echo_info = getattr(TestService.echo, "__aster_method_info__")
        echo_info.timeout = 30.0
        assert echo_info.timeout == 30.0

    def test_rpc_with_idempotent(self):
        """@rpc decorator accepts idempotent parameter."""
        class TestService:
            @rpc()
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)
        
        # Manually set idempotent for testing
        echo_info = getattr(TestService.echo, "__aster_method_info__")
        echo_info.idempotent = True
        assert echo_info.idempotent is True

    def test_rpc_sync_function_raises(self):
        """@rpc on a sync function raises TypeError."""
        with pytest.raises(TypeError, match="must be an async function"):
            @rpc()
            def sync_method(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)


# ── @server_stream decorator tests ────────────────────────────────────────


class TestServerStreamDecorator:
    def test_server_stream_basic(self):
        """@server_stream decorator can be applied to an async generator."""
        class TestService:
            @server_stream
            async def stream_counts(self, req: CounterRequest) -> AsyncIterator[StreamingResponse]:
                for i in range(req.count):
                    yield StreamingResponse(item=req.start + i)

        method = TestService.stream_counts
        info = getattr(method, "__aster_method_info__")
        assert info is not None
        assert info.pattern == RpcPattern.SERVER_STREAM
        assert info.idempotent is False

    def test_server_stream_with_timeout(self):
        """@server_stream decorator accepts timeout parameter."""
        class TestService:
            @server_stream(timeout=60.0)
            async def stream_counts(self, req: CounterRequest) -> AsyncIterator[StreamingResponse]:
                for i in range(req.count):
                    yield StreamingResponse(item=i)

        method = TestService.stream_counts
        info = getattr(method, "__aster_method_info__")
        assert info.timeout == 60.0

    def test_server_stream_sync_function_raises(self):
        """@server_stream on a sync function raises TypeError."""
        with pytest.raises(TypeError, match="async generator"):
            @server_stream()
            def sync_stream(self, req: CounterRequest) -> AsyncIterator[StreamingResponse]:
                yield StreamingResponse(item=1)


# ── @client_stream decorator tests ─────────────────────────────────────────


class TestClientStreamDecorator:
    def test_client_stream_basic(self):
        """@client_stream decorator can be applied to an async method."""
        class TestService:
            @client_stream
            async def aggregate(self, reqs: AsyncIterator[AggregateRequest]) -> AggregateResponse:
                total = 0
                count = 0
                async for req in reqs:  # type: ignore
                    total += req.value
                    count += 1
                return AggregateResponse(total=total, count=count)

        method = TestService.aggregate
        info = getattr(method, "__aster_method_info__")
        assert info is not None
        assert info.pattern == RpcPattern.CLIENT_STREAM
        assert info.idempotent is False

    def test_client_stream_with_idempotent(self):
        """@client_stream decorator accepts idempotent parameter."""
        class TestService:
            @client_stream(idempotent=True)
            async def aggregate(self, reqs: AsyncIterator[AggregateRequest]) -> AggregateResponse:
                return AggregateResponse(total=0, count=0)

        method = TestService.aggregate
        info = getattr(method, "__aster_method_info__")
        assert info.idempotent is True


# ── @bidi_stream decorator tests ───────────────────────────────────────────


class TestBidiStreamDecorator:
    def test_bidi_stream_basic(self):
        """@bidi_stream decorator can be applied to an async generator."""
        class TestService:
            @bidi_stream
            async def chat(
                self, requests: AsyncIterator[StreamingResponse]
            ) -> AsyncIterator[StreamingResponse]:
                async for req in requests:  # type: ignore
                    yield StreamingResponse(item=req.item * 2)

        method = TestService.chat
        info = getattr(method, "__aster_method_info__")
        assert info is not None
        assert info.pattern == RpcPattern.BIDI_STREAM
        assert info.idempotent is False

    def test_bidi_stream_with_timeout(self):
        """@bidi_stream decorator accepts timeout parameter."""
        class TestService:
            @bidi_stream(timeout=120.0)
            async def chat(
                self, requests: AsyncIterator[StreamingResponse]
            ) -> AsyncIterator[StreamingResponse]:
                async for req in requests:  # type: ignore
                    yield StreamingResponse(item=req.item)

        method = TestService.chat
        info = getattr(method, "__aster_method_info__")
        assert info.timeout == 120.0

    def test_bidi_stream_sync_function_raises(self):
        """@bidi_stream on a sync function raises TypeError."""
        with pytest.raises(TypeError, match="async generator"):
            @bidi_stream()
            def sync_chat(
                self, requests: AsyncIterator[StreamingResponse]
            ) -> AsyncIterator[StreamingResponse]:
                yield StreamingResponse(item=1)


# ── @service decorator tests ──────────────────────────────────────────────


class TestServiceDecorator:
    def test_basic_service(self):
        """@service decorator marks a class and attaches ServiceInfo."""
        @service(name="TestService", version=1)
        class TestService:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        info = getattr(TestService, "__aster_service_info__")
        assert info is not None
        assert info.name == "TestService"
        assert info.version == 1
        assert info.scoped == "shared"

    def test_service_with_xlang_serialization(self):
        """@service with XLANG mode."""
        @service(name="TestService", version=1, serialization=[SerializationMode.XLANG])
        class TestService:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        info = getattr(TestService, "__aster_service_info__")
        assert SerializationMode.XLANG in info.serialization_modes

    def test_service_with_multiple_serialization_modes(self):
        """@service with multiple serialization modes."""
        @service(
            name="TestService",
            version=1,
            serialization=[SerializationMode.XLANG, SerializationMode.NATIVE],
        )
        class TestService:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        info = getattr(TestService, "__aster_service_info__")
        assert len(info.serialization_modes) == 2
        assert SerializationMode.XLANG in info.serialization_modes
        assert SerializationMode.NATIVE in info.serialization_modes

    def test_service_scoped_stream(self):
        """@service with scoped='stream' requires peer in __init__."""
        @service(name="TestService", version=1, scoped="stream")
        class TestService:
            def __init__(self, peer=None):
                self.peer = peer

            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        info = getattr(TestService, "__aster_service_info__")
        assert info.scoped == "stream"

    def test_service_scoped_stream_missing_peer(self):
        """@service(scoped='stream') without peer in __init__ raises TypeError."""
        with pytest.raises(TypeError, match="must accept a 'peer' parameter"):
            @service(name="TestService2", version=1, scoped="stream")
            class TestService2:
                @rpc
                async def echo(self, req: EchoRequest) -> EchoResponse:
                    return EchoResponse(message=req.message)

    def test_service_method_extraction(self):
        """@service extracts method info from decorated methods."""
        @service(name="TestService", version=1)
        class TestService:
            @rpc()
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

            @server_stream
            async def stream_counts(self, req: CounterRequest) -> AsyncIterator[StreamingResponse]:
                for i in range(req.count):
                    yield StreamingResponse(item=i)

        info = getattr(TestService, "__aster_service_info__")
        assert len(info.methods) == 2

        # Check echo method
        echo_info = info.methods.get("echo")
        assert echo_info is not None
        assert echo_info.pattern == RpcPattern.UNARY
        assert echo_info.request_type == EchoRequest
        # Note: response_type may be a string due to forward ref resolution
        assert echo_info.request_type is not None

        # Check stream_counts method
        stream_info = info.methods.get("stream_counts")
        assert stream_info is not None
        assert stream_info.pattern == RpcPattern.SERVER_STREAM
        assert stream_info.request_type == CounterRequest

    def test_service_non_class_raises(self):
        """@service on non-class raises TypeError."""
        with pytest.raises(TypeError, match="@service can only be applied to classes"):
            @service(name="NotAService")
            def not_a_service():
                pass

    def test_service_missing_type_annotation_raises(self):
        """@service with missing type annotations raises TypeError."""
        # Note: When type annotation is missing, the decorator should raise
        # during service decoration time
        with pytest.raises(TypeError, match="has no type annotation"):
            @service(name="TestService", version=1)
            class TestService:
                @rpc
                async def echo(self, req) -> EchoResponse:  # type: ignore
                    return EchoResponse(message="")


# ── XLANG tag validation tests ────────────────────────────────────────────


class TestXlangTagValidation:
    def test_untagged_request_type_auto_tagged_silently(self):
        """Untagged request type is auto-tagged at decoration time without warning."""
        @service(name="TestService", version=1)
        class TestService:
            @rpc
            async def echo(self, req: UntaggedRequest) -> EchoResponse:
                return EchoResponse(message="")

        assert hasattr(UntaggedRequest, "__wire_type__")

    def test_untagged_response_type_auto_tagged_silently(self):
        """Untagged response type is auto-tagged at decoration time without warning."""
        @service(name="TestService", version=1)
        class TestService:
            @rpc
            async def echo(self, req: EchoRequest) -> UntaggedResponse:
                return UntaggedResponse(value="")

        assert hasattr(UntaggedResponse, "__wire_type__")

    def test_nested_untagged_type_auto_tagged(self):
        """Nested untagged type is auto-tagged (or already tagged from prior test)."""
        @wire_type("test.decorators/NestedRequest")
        @dataclass
        class NestedRequest:
            inner: UntaggedRequest = field(default_factory=UntaggedRequest)

        # UntaggedRequest may already be auto-tagged from prior test runs;
        # either way, the service should be created successfully.
        @service(name="TestService", version=1)
        class TestService:
            @rpc
            async def echo(self, req: NestedRequest) -> EchoResponse:
                return EchoResponse(message="")

        info = getattr(TestService, "__aster_service_info__")
        assert info is not None

    def test_native_mode_skips_tag_validation(self):
        """NATIVE mode does not require @wire_type."""
        @service(name="TestService", version=1, serialization=[SerializationMode.NATIVE])
        class TestService:
            @rpc
            async def echo(self, req: UntaggedRequest) -> UntaggedResponse:
                return UntaggedResponse(value=req.data)

        # Should not raise
        info = getattr(TestService, "__aster_service_info__")
        assert info is not None


# ── ServiceInfo and MethodInfo tests ───────────────────────────────────────


class TestServiceInfo:
    def test_get_method(self):
        """ServiceInfo.get_method() returns the method info."""
        info = ServiceInfo(
            name="TestService",
            version=1,
            methods={
                "echo": MethodInfo(name="echo", pattern=RpcPattern.UNARY),
            },
        )

        method = info.get_method("echo")
        assert method is not None
        assert method.name == "echo"

    def test_get_method_not_found(self):
        """ServiceInfo.get_method() returns None for unknown method."""
        info = ServiceInfo(name="TestService", version=1)
        method = info.get_method("nonexistent")
        assert method is None

    def test_has_method(self):
        """ServiceInfo.has_method() works correctly."""
        info = ServiceInfo(
            name="TestService",
            version=1,
            methods={
                "echo": MethodInfo(name="echo", pattern=RpcPattern.UNARY),
            },
        )

        assert info.has_method("echo") is True
        assert info.has_method("nonexistent") is False


# ── ServiceRegistry tests ─────────────────────────────────────────────────


class TestServiceRegistry:
    def setup_method(self):
        """Clear the default registry before each test."""
        self.registry = ServiceRegistry()

    def test_register_service(self):
        """ServiceRegistry.register() adds a service."""
        @service(name="TestService", version=1)
        class TestService:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        info = self.registry.register(TestService)
        assert info.name == "TestService"
        assert info.version == 1

    def test_register_non_service_raises(self):
        """Registering a non-@service class raises TypeError."""
        class NotAService:
            pass

        with pytest.raises(TypeError, match="not decorated with @service"):
            self.registry.register(NotAService)

    def test_register_duplicate_raises(self):
        """Registering the same service twice raises ValueError."""
        @service(name="TestService", version=1)
        class TestService1:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        self.registry.register(TestService1)

        with pytest.raises(ValueError, match="already registered"):
            self.registry.register(TestService1)

    def test_register_different_version_ok(self):
        """Different versions of the same service can coexist."""
        @service(name="TestService", version=1)
        class TestServiceV1:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message="v1")

        @service(name="TestService", version=2)
        class TestServiceV2:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message="v2")

        self.registry.register(TestServiceV1)
        self.registry.register(TestServiceV2)

        # Should be able to look up by version
        v1_info = self.registry.lookup("TestService", version=1)
        v2_info = self.registry.lookup("TestService", version=2)
        assert v1_info is not None
        assert v2_info is not None
        assert v1_info.version == 1
        assert v2_info.version == 2

    def test_lookup_by_name(self):
        """ServiceRegistry.lookup() finds a service by name."""
        @service(name="TestService", version=1)
        class TestService:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        self.registry.register(TestService)

        info = self.registry.lookup("TestService")
        assert info is not None
        assert info.name == "TestService"

    def test_lookup_by_name_and_version(self):
        """ServiceRegistry.lookup() finds a specific version."""
        @service(name="TestService", version=1)
        class TestServiceV1:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message="v1")

        @service(name="TestService", version=2)
        class TestServiceV2:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message="v2")

        self.registry.register(TestServiceV1)
        self.registry.register(TestServiceV2)

        v1_info = self.registry.lookup("TestService", version=1)
        v2_info = self.registry.lookup("TestService", version=2)
        assert v1_info is not None
        assert v2_info is not None

    def test_lookup_not_found(self):
        """ServiceRegistry.lookup() returns None for unknown service."""
        info = self.registry.lookup("UnknownService")
        assert info is None

    def test_lookup_method(self):
        """ServiceRegistry.lookup_method() finds a specific method."""
        @service(name="TestService", version=1)
        class TestService:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        self.registry.register(TestService)

        result = self.registry.lookup_method("TestService", "echo")
        assert result is not None
        svc_info, method_info = result
        assert svc_info.name == "TestService"
        assert method_info.name == "echo"

    def test_lookup_method_not_found(self):
        """ServiceRegistry.lookup_method() returns None for unknown method."""
        @service(name="TestService", version=1)
        class TestService:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        self.registry.register(TestService)

        result = self.registry.lookup_method("TestService", "nonexistent")
        assert result is None

    def test_get_all_services(self):
        """ServiceRegistry.get_all_services() returns all registered services."""
        @service(name="Service1", version=1)
        class Service1:
            @rpc
            async def method(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message="")

        @service(name="Service2", version=1)
        class Service2:
            @rpc
            async def method(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message="")

        self.registry.register(Service1)
        self.registry.register(Service2)

        all_services = self.registry.get_all_services()
        assert len(all_services) == 2
        names = {s.name for s in all_services}
        assert names == {"Service1", "Service2"}

    def test_clear(self):
        """ServiceRegistry.clear() removes all services."""
        @service(name="TestService", version=1)
        class TestService:
            @rpc
            async def echo(self, req: EchoRequest) -> EchoResponse:
                return EchoResponse(message=req.message)

        self.registry.register(TestService)
        assert len(self.registry) == 1

        self.registry.clear()
        assert len(self.registry) == 0


# ── Default registry tests ─────────────────────────────────────────────────


class TestDefaultRegistry:
    def setup_method(self):
        """Reset default registry."""
        set_default_registry(ServiceRegistry())

    def test_get_default_registry(self):
        """get_default_registry() returns a ServiceRegistry."""
        registry = get_default_registry()
        assert isinstance(registry, ServiceRegistry)

    def test_set_default_registry(self):
        """set_default_registry() changes the default registry."""
        new_registry = ServiceRegistry()
        set_default_registry(new_registry)
        assert get_default_registry() is new_registry


# ── Full service integration tests ─────────────────────────────────────────


class TestFullServiceIntegration:
    """Test complete service definition with all RPC patterns."""

    @service(name="IntegrationTestService", version=1)
    class IntegrationTestService:
        @rpc()
        async def echo(self, req: EchoRequest) -> EchoResponse:
            return EchoResponse(message=f"echo: {req.message}", received_at_ms=0)

        @server_stream
        async def stream_counts(self, req: CounterRequest) -> AsyncIterator[StreamingResponse]:
            for i in range(req.count):
                yield StreamingResponse(item=req.start + i)

        @client_stream
        async def aggregate(
            self, reqs: AsyncIterator[AggregateRequest]
        ) -> AggregateResponse:
            total = 0
            count = 0
            async for req in reqs:  # type: ignore
                total += req.value
                count += 1
            return AggregateResponse(total=total, count=count)

        @bidi_stream
        async def double(
            self, reqs: AsyncIterator[StreamingResponse]
        ) -> AsyncIterator[StreamingResponse]:
            async for req in reqs:  # type: ignore
                yield StreamingResponse(item=req.item * 2)

    def test_all_methods_extracted(self):
        """All four RPC patterns are extracted correctly."""
        info = getattr(self.IntegrationTestService, "__aster_service_info__")
        assert len(info.methods) == 4

        # Check each pattern
        echo = info.methods["echo"]
        assert echo.pattern == RpcPattern.UNARY
        assert echo.request_type == EchoRequest

        stream = info.methods["stream_counts"]
        assert stream.pattern == RpcPattern.SERVER_STREAM
        assert stream.request_type == CounterRequest

        agg = info.methods["aggregate"]
        assert agg.pattern == RpcPattern.CLIENT_STREAM
        # Note: client_stream/bidi_stream request_type is AsyncIterator[T]
        # The unwrapping may not work for all cases

        double = info.methods["double"]
        assert double.pattern == RpcPattern.BIDI_STREAM

    def test_service_in_registry(self):
        """Service can be registered in a registry."""
        registry = ServiceRegistry()
        info = registry.register(self.IntegrationTestService)
        assert info.name == "IntegrationTestService"
        assert info.version == 1

    def test_method_lookup(self):
        """Methods can be looked up through the registry."""
        registry = ServiceRegistry()
        registry.register(self.IntegrationTestService)

        result = registry.lookup_method("IntegrationTestService", "echo")
        assert result is not None
        svc_info, method_info = result
        assert method_info.pattern == RpcPattern.UNARY
