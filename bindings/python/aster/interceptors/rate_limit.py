"""
aster.interceptors.rate_limit — Token bucket rate limiter for RPC calls.

Limits request rate per service, per method, or per peer. Rejects requests
that exceed the limit with RESOURCE_EXHAUSTED status.

Usage::

    from aster.interceptors import RateLimitInterceptor

    # 100 requests/second globally
    server = AsterServer(
        services=[...],
        interceptors=[RateLimitInterceptor(rate=100)],
    )

    # Per-service limits
    server = AsterServer(
        services=[...],
        interceptors=[RateLimitInterceptor(rate=100, per="service")],
    )

    # Per-peer limits (useful for multi-tenant)
    server = AsterServer(
        services=[...],
        interceptors=[RateLimitInterceptor(rate=50, per="peer")],
    )
"""

from __future__ import annotations

import time
from typing import Any

from aster.interceptors.base import CallContext, Interceptor
from aster.status import RpcError, StatusCode


class _TokenBucket:
    """Simple token bucket rate limiter."""

    __slots__ = ("_rate", "_capacity", "_tokens", "_last_refill")

    def __init__(self, rate: float, burst: float | None = None) -> None:
        self._rate = rate  # tokens per second
        self._capacity = burst or rate  # max burst
        self._tokens = self._capacity
        self._last_refill = time.monotonic()

    def try_acquire(self) -> bool:
        """Try to consume one token. Returns True if allowed."""
        now = time.monotonic()
        elapsed = now - self._last_refill
        self._tokens = min(self._capacity, self._tokens + elapsed * self._rate)
        self._last_refill = now

        if self._tokens >= 1.0:
            self._tokens -= 1.0
            return True
        return False


class RateLimitInterceptor(Interceptor):
    """Token bucket rate limiter interceptor.

    Args:
        rate: Maximum requests per second.
        burst: Maximum burst size (defaults to rate).
        per: Granularity: ``"global"``, ``"service"``, ``"method"``, or ``"peer"``.
    """

    def __init__(
        self,
        rate: float = 100.0,
        burst: float | None = None,
        per: str = "global",
    ) -> None:
        self._rate = rate
        self._burst = burst
        self._per = per
        self._buckets: dict[str, _TokenBucket] = {}
        self._global_bucket = _TokenBucket(rate, burst)

    def _get_bucket(self, ctx: CallContext) -> _TokenBucket:
        if self._per == "global":
            return self._global_bucket

        if self._per == "service":
            key = ctx.service
        elif self._per == "method":
            key = f"{ctx.service}.{ctx.method}"
        elif self._per == "peer":
            key = ctx.peer or "unknown"
        else:
            return self._global_bucket

        if key not in self._buckets:
            self._buckets[key] = _TokenBucket(self._rate, self._burst)
        return self._buckets[key]

    async def on_request(self, ctx: CallContext, request: object) -> object:
        bucket = self._get_bucket(ctx)
        if not bucket.try_acquire():
            raise RpcError(
                StatusCode.RESOURCE_EXHAUSTED,
                f"Rate limit exceeded ({self._rate}/s per {self._per})",
            )
        return request
