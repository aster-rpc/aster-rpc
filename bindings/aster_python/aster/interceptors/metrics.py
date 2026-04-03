"""Metrics interceptor with optional OpenTelemetry integration."""

from __future__ import annotations

from aster_python.aster.interceptors.base import CallContext, Interceptor
from aster_python.aster.status import RpcError


class MetricsInterceptor(Interceptor):
    """Collects simple in-memory counters and optionally creates OTel spans."""

    def __init__(self) -> None:
        self.started = 0
        self.succeeded = 0
        self.failed = 0
        try:
            from opentelemetry import trace  # type: ignore

            self._tracer = trace.get_tracer(__name__)
        except Exception:
            self._tracer = None

    async def on_request(self, ctx: CallContext, request: object) -> object:
        self.started += 1
        return request

    async def on_response(self, ctx: CallContext, response: object) -> object:
        self.succeeded += 1
        return response

    async def on_error(self, ctx: CallContext, error: RpcError) -> RpcError | None:
        self.failed += 1
        return error