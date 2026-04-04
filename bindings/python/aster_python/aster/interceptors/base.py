"""Base interceptor primitives and helper utilities for Aster RPC."""

from __future__ import annotations

import asyncio
import time
import uuid
from abc import ABC
from dataclasses import dataclass, field
from typing import Any

from aster_python.aster.status import RpcError, StatusCode


@dataclass
class CallContext:
    """Context describing a single RPC invocation."""

    service: str
    method: str
    call_id: str = field(default_factory=lambda: str(uuid.uuid4()))
    session_id: str | None = None
    peer: str | None = None
    metadata: dict[str, str] = field(default_factory=dict)
    attributes: dict[str, str] = field(default_factory=dict)  # Phase 11: enrollment attrs
    deadline: float | None = None
    is_streaming: bool = False
    pattern: str | None = None
    idempotent: bool = False
    attempt: int = 1

    @property
    def remaining_seconds(self) -> float | None:
        if self.deadline is None:
            return None
        return max(0.0, self.deadline - time.time())

    @property
    def expired(self) -> bool:
        remaining = self.remaining_seconds
        return remaining is not None and remaining <= 0.0


class Interceptor(ABC):
    """Base interceptor interface."""

    async def on_request(self, ctx: CallContext, request: object) -> object:
        return request

    async def on_response(self, ctx: CallContext, response: object) -> object:
        return response

    async def on_error(self, ctx: CallContext, error: RpcError) -> RpcError | None:
        return error


def deadline_from_epoch_ms(deadline_epoch_ms: int) -> float | None:
    if deadline_epoch_ms <= 0:
        return None
    return deadline_epoch_ms / 1000.0


def build_call_context(
    *,
    service: str,
    method: str,
    metadata: dict[str, str] | None = None,
    deadline_epoch_ms: int = 0,
    peer: str | None = None,
    is_streaming: bool = False,
    pattern: str | None = None,
    idempotent: bool = False,
    call_id: str | None = None,
    session_id: str | None = None,
) -> CallContext:
    return CallContext(
        service=service,
        method=method,
        call_id=call_id or str(uuid.uuid4()),
        session_id=session_id,
        peer=peer,
        metadata=dict(metadata or {}),
        deadline=deadline_from_epoch_ms(deadline_epoch_ms),
        is_streaming=is_streaming,
        pattern=pattern,
        idempotent=idempotent,
    )


async def apply_request_interceptors(
    interceptors: list[Interceptor],
    ctx: CallContext,
    request: Any,
) -> Any:
    current = request
    for interceptor in interceptors:
        current = await interceptor.on_request(ctx, current)
    return current


async def apply_response_interceptors(
    interceptors: list[Interceptor],
    ctx: CallContext,
    response: Any,
) -> Any:
    current = response
    for interceptor in interceptors:
        current = await interceptor.on_response(ctx, current)
    return current


async def apply_error_interceptors(
    interceptors: list[Interceptor],
    ctx: CallContext,
    error: RpcError,
) -> RpcError | None:
    current: RpcError | None = error
    for interceptor in reversed(interceptors):
        if current is None:
            return None
        current = await interceptor.on_error(ctx, current)
    return current


def normalize_error(error: Exception) -> RpcError:
    if isinstance(error, RpcError):
        return error
    if isinstance(error, asyncio.TimeoutError):
        return RpcError(StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
    if isinstance(error, TimeoutError):
        return RpcError(StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
    return RpcError(StatusCode.UNKNOWN, str(error))