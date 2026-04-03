"""Deadline enforcement interceptor."""

from __future__ import annotations

from aster_python.aster.interceptors.base import CallContext, Interceptor
from aster_python.aster.status import RpcError, StatusCode


class DeadlineInterceptor(Interceptor):
    """Validates and enforces call deadlines."""

    async def on_request(self, ctx: CallContext, request: object) -> object:
        if ctx.expired:
            raise RpcError(StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
        return request

    def timeout_seconds(self, ctx: CallContext) -> float | None:
        remaining = ctx.remaining_seconds
        if remaining is None:
            return None
        return max(0.0, remaining)