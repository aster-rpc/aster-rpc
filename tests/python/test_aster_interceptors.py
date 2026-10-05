from __future__ import annotations

import asyncio
from dataclasses import dataclass

import pytest

from aster.client import create_client, create_local_client
from aster.codec import ForyCodec, wire_type
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
from aster.rpc_types import RetryPolicy, SerializationMode


@wire_type("test.interceptors/EchoRequest")
@dataclass
class EchoRequest:
    message: str = ""


@wire_type("test.interceptors/EchoResponse")
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
        await asyncio.sleep(2.0)
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
            await client.slow(EchoRequest("late"), timeout=1)
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

    @staticmethod
    def _scripted_client(script, interceptors):
        """A client whose unary calls follow `script`: each entry is an
        exception to raise, the string "hang", or a response message."""

        class ScriptedTransport:
            async def unary(self, *args, **kwargs):
                step = script.pop(0)
                if isinstance(step, BaseException):
                    raise step
                if step == "hang":
                    await asyncio.Event().wait()
                return EchoResponse(message=step)

            def server_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            async def client_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            def bidi_stream(self, *args, **kwargs):
                raise AssertionError("unexpected")

            async def close(self):
                return None

        return create_client(
            InterceptorService, transport=ScriptedTransport(), interceptors=interceptors
        )

    @staticmethod
    async def _open_then_half_open(client, breaker):
        with pytest.raises(RpcError):
            await client.echo(EchoRequest("trip"))
        assert breaker.state == breaker.OPEN
        await asyncio.sleep(0.02)

    @pytest.mark.asyncio
    async def test_circuit_breaker_uncounted_error_frees_half_open_probe(self):
        # A NOT_FOUND probe says nothing about the peer's health: the breaker
        # stays half-open but must admit the next probe, which closes it.
        breaker = CircuitBreakerInterceptor(failure_threshold=1, recovery_timeout=0.01)
        script = [
            RpcError(StatusCode.UNAVAILABLE, "down"),
            RpcError(StatusCode.NOT_FOUND, "no such thing"),
            "ok",
        ]
        client = self._scripted_client(script, [breaker])
        await self._open_then_half_open(client, breaker)

        with pytest.raises(RpcError) as exc_info:
            await client.echo(EchoRequest("probe"))
        assert exc_info.value.code == StatusCode.NOT_FOUND
        assert breaker.state == breaker.HALF_OPEN
        assert breaker.half_open_calls == 0

        assert (await client.echo(EchoRequest("probe"))).message == "ok"
        assert breaker.state == breaker.CLOSED

    @pytest.mark.asyncio
    async def test_circuit_breaker_cancelled_probe_frees_half_open_slot(self):
        breaker = CircuitBreakerInterceptor(failure_threshold=1, recovery_timeout=0.01)
        script = [RpcError(StatusCode.UNAVAILABLE, "down"), "hang", "ok"]
        client = self._scripted_client(script, [breaker])
        await self._open_then_half_open(client, breaker)

        probe = asyncio.ensure_future(client.echo(EchoRequest("probe")))
        await asyncio.sleep(0.01)
        assert breaker.half_open_calls == 1
        probe.cancel()
        with pytest.raises(asyncio.CancelledError):
            await probe
        assert breaker.half_open_calls == 0

        assert (await client.echo(EchoRequest("probe"))).message == "ok"
        assert breaker.state == breaker.CLOSED

    @pytest.mark.asyncio
    async def test_circuit_breaker_refused_by_another_breaker_frees_slot(self):
        first = CircuitBreakerInterceptor(failure_threshold=1, recovery_timeout=0.01)
        second = CircuitBreakerInterceptor(failure_threshold=1, recovery_timeout=60.0)
        client = self._scripted_client([RpcError(StatusCode.UNAVAILABLE, "down")], [first, second])
        await self._open_then_half_open(client, first)
        assert second.state == second.OPEN

        # `first` admits a probe, `second` (still open) refuses the call:
        # `first` must get its probe slot back.
        with pytest.raises(RpcError, match="open"):
            await client.echo(EchoRequest("probe"))
        assert first.state == first.HALF_OPEN
        assert first.half_open_calls == 0

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