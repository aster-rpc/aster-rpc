"""Base interceptor primitives and helper utilities for Aster RPC."""

from __future__ import annotations

import asyncio
import time
import uuid
from abc import ABC
from dataclasses import dataclass, field
from typing import Any

from aster.status import RpcError, StatusCode


@dataclass
class CallContext:
    """Context for a single RPC call, available to interceptors and handlers.

    Passed to every interceptor in the chain. Read ``service`` and ``method``
    to know which RPC is being called. Use ``metadata`` to pass headers
    between client and server. Check ``remaining_seconds`` for deadline
    awareness.

    Attributes:
        service: The service name (e.g., ``"MissionControl"``).
        method: The method name (e.g., ``"getStatus"``).
        call_id: Unique ID for this call (auto-generated UUID).
        peer: Remote peer identifier (endpoint ID hex).
        metadata: Key/value headers sent with the call.
        attributes: Enrollment attributes from the consumer's credential.
        deadline: Absolute deadline as epoch timestamp, or ``None``.
        is_streaming: ``True`` for streaming RPC patterns.
        pattern: RPC pattern (``"unary"``, ``"server_stream"``, etc.).
        idempotent: ``True`` if the method is safe to retry.
        attempt: Current retry attempt number (starts at 1).
    """

    service: str
    method: str
    call_id: str = field(default_factory=lambda: str(uuid.uuid4()))
    session_id: str | None = None
    peer: str | None = None
    metadata: dict[str, str] = field(default_factory=dict)
    attributes: dict[str, str] = field(default_factory=dict)
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


def deadline_from_relative_secs(deadline_secs: int) -> float | None:
    if deadline_secs <= 0:
        return None
    return time.time() + deadline_secs


def build_call_context(
    *,
    service: str,
    method: str,
    metadata: dict[str, str] | None = None,
    deadline_secs: int = 0,
    peer: str | None = None,
    is_streaming: bool = False,
    pattern: str | None = None,
    idempotent: bool = False,
    call_id: int = 0,
    session_id: str | None = None,
    attributes: dict[str, str] | None = None,
) -> CallContext:
    return CallContext(
        service=service,
        method=method,
        call_id=str(call_id) if call_id else str(uuid.uuid4()),
        session_id=session_id,
        peer=peer,
        metadata=dict(metadata or {}),
        attributes=dict(attributes or {}),
        deadline=deadline_from_relative_secs(deadline_secs),
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