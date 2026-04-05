from __future__ import annotations

import asyncio
from dataclasses import dataclass

import pytest

from aster.client import create_client, create_local_client
from aster.codec import ForyCodec, fory_tag
from aster.decorators import rpc, service
from aster.interceptors import (
    AuditLogInterceptor,
    AuthInterceptor,
    CircuitBreakerInterceptor,
    DeadlineInterceptor,
    MetricsInterceptor,
    RetryInterceptor,
)
from aster.status import RpcError, StatusCode
from aster.transport.local import LocalTransport
from aster.types import RetryPolicy, SerializationMode


@fory_tag("test.interceptors/EchoRequest")
@dataclass
class EchoRequest:
    message: str = ""


@fory_tag("test.interceptors/EchoResponse")
@dataclass
class EchoResponse:
    message: str = ""


@service(name="InterceptorService", version=1, serialization=[SerializationMode.XLANG])
class InterceptorService:
    @rpc(timeout=5.0, idempotent=True)
    async def echo(self, req: EchoRequest) -> EchoResponse:
        return EchoResponse(message=req.message)

    @rpc(timeout=5.0, idempotent=True)
    async def slow(self, req: EchoRequest) -> EchoResponse:
        await asyncio.sleep(0.05)
        return EchoResponse(message=req.message)


class TestPhase7Interceptors:
    @pytest.mark.asyncio
    async def test_local_transport_interceptors_run(self):
        audit = AuditLogInterceptor()
        metrics = MetricsInterceptor()
        client = create_local_client(
            InterceptorService,
            InterceptorService(),
            wire_compatible=True,
            interceptors=[audit, metrics],
        )

        response = await client.echo(EchoRequest("hello"))
        assert response.message == "hello"
        assert [e["event"] for e in audit.sink] == ["request", "response"]
        assert metrics.started == 1
        assert metrics.succeeded == 1

    @pytest.mark.asyncio
    async def test_deadline_enforcement(self):
        client = create_local_client(
            InterceptorService,
            InterceptorService(),
            wire_compatible=True,
            interceptors=[DeadlineInterceptor()],
        )

        with pytest.raises(RpcError) as exc_info:
            await client.slow(EchoRequest("late"), timeout=0.001)
        assert exc_info.value.code == StatusCode.DEADLINE_EXCEEDED

    @pytest.mark.asyncio
    async def test_retry_behavior_on_unavailable(self):
        attempts = {"count": 0}

        class FlakyTransport:
            async def unary(self, *args, **kwargs):
                attempts["count"] += 1
                if attempts["count"] < 3:
                    raise RpcError(StatusCode.UNAVAILABLE, "try again")
                return EchoResponse(message="ok")

            def server_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            async def client_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            def bidi_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            async def close(self):
                return None

        client = create_client(
            InterceptorService,
            transport=FlakyTransport(),
            interceptors=[RetryInterceptor(RetryPolicy(max_attempts=3))],
        )
        response = await client.echo(EchoRequest("retry"))
        assert response.message == "ok"
        assert attempts["count"] == 3

    @pytest.mark.asyncio
    async def test_circuit_breaker_state_transitions(self):
        breaker = CircuitBreakerInterceptor(failure_threshold=2, recovery_timeout=0.01)

        class BrokenTransport:
            async def unary(self, *args, **kwargs):
                raise RpcError(StatusCode.UNAVAILABLE, "down")

            def server_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            async def client_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            def bidi_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            async def close(self):
                return None

        client = create_client(
            InterceptorService,
            transport=BrokenTransport(),
            interceptors=[breaker],
        )

        with pytest.raises(RpcError):
            await client.echo(EchoRequest("one"))
        assert breaker.state == breaker.CLOSED

        with pytest.raises(RpcError):
            await client.echo(EchoRequest("two"))
        assert breaker.state == breaker.OPEN

        with pytest.raises(RpcError, match="open"):
            await client.echo(EchoRequest("three"))

        await asyncio.sleep(0.02)
        with pytest.raises(RpcError):
            await client.echo(EchoRequest("half-open"))
        assert breaker.state == breaker.OPEN

    @pytest.mark.asyncio
    async def test_auth_interceptor_injects_metadata_for_local_transport(self):
        captured: dict[str, str] = {}

        async def handler(req: EchoRequest) -> EchoResponse:
            return EchoResponse(message=req.message)

        def registry(service: str, method: str):
            return handler, [EchoRequest, EchoResponse], "unary"

        class CaptureAuth(AuthInterceptor):
            async def on_request(self, ctx, request):
                await super().on_request(ctx, request)
                captured.update(ctx.metadata)
                return request

        transport = LocalTransport(
            handler_registry=registry,
            codec=ForyCodec(mode=SerializationMode.XLANG, types=[EchoRequest, EchoResponse]),
            wire_compatible=True,
            interceptors=[CaptureAuth(token_provider="secret")],
        )
        response = await transport.unary("Svc", "Echo", EchoRequest("x"))
        assert response.message == "x"
        assert captured["authorization"] == "Bearer secret"