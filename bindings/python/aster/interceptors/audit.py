"""Audit logging interceptor."""

from __future__ import annotations

import logging
import time
from typing import Any

from aster.interceptors.base import CallContext, Interceptor
from aster.status import RpcError


class AuditLogInterceptor(Interceptor):
    """Captures structured audit events for requests, responses, and errors."""

    def __init__(self, *, sink: list[dict[str, Any]] | None = None, logger: logging.Logger | None = None) -> None:
        self.sink = sink if sink is not None else []
        self.logger = logger or logging.getLogger(__name__)

    def _record(self, event: str, ctx: CallContext, **extra: Any) -> None:
        entry = {
            "event": event,
            "service": ctx.service,
            "method": ctx.method,
            "call_id": ctx.call_id,
            "attempt": ctx.attempt,
            "ts": time.time(),
            **extra,
        }
        self.sink.append(entry)
        self.logger.debug("audit=%s", entry)

    async def on_request(self, ctx: CallContext, request: object) -> object:
        self._record("request", ctx)
        return request

    async def on_response(self, ctx: CallContext, response: object) -> object:
        self._record("response", ctx)
        return response

    async def on_error(self, ctx: CallContext, error: RpcError) -> RpcError | None:
        self._record("error", ctx, code=error.code.name, message=error.message)
        return error