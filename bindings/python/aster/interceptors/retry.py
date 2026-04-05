"""Retry interceptor for transient client-side failures."""

from __future__ import annotations

import random

from aster.interceptors.base import CallContext, Interceptor
from aster.status import RpcError, StatusCode
from aster.types import RetryPolicy


class RetryInterceptor(Interceptor):
    """Provides retry policy hints for client calls."""

    def __init__(
        self,
        policy: RetryPolicy | None = None,
        retryable_codes: set[StatusCode] | None = None,
    ) -> None:
        self.policy = policy or RetryPolicy()
        self.retryable_codes = retryable_codes or {StatusCode.UNAVAILABLE}

    def should_retry(self, ctx: CallContext, error: RpcError) -> bool:
        return ctx.idempotent and error.code in self.retryable_codes

    def backoff_seconds(self, attempt: int) -> float:
        backoff = self.policy.backoff
        delay_ms = min(
            backoff.max_ms,
            int(backoff.initial_ms * (backoff.multiplier ** max(0, attempt - 1))),
        )
        jitter = delay_ms * backoff.jitter * random.random()
        return (delay_ms + jitter) / 1000.0