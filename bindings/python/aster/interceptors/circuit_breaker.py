"""Circuit breaker interceptor."""

from __future__ import annotations

import time

from aster.interceptors.base import CallContext, Interceptor
from aster.status import RpcError, StatusCode


class CircuitBreakerInterceptor(Interceptor):
    """Simple CLOSED → OPEN → HALF_OPEN circuit breaker."""

    CLOSED = "closed"
    OPEN = "open"
    HALF_OPEN = "half_open"

    def __init__(
        self,
        *,
        failure_threshold: int = 3,
        recovery_timeout: float = 5.0,
        half_open_max_calls: int = 1,
    ) -> None:
        self.failure_threshold = failure_threshold
        self.recovery_timeout = recovery_timeout
        self.half_open_max_calls = half_open_max_calls
        self.state = self.CLOSED
        self.failure_count = 0
        self.opened_at = 0.0
        self.half_open_calls = 0

    def before_call(self, ctx: CallContext) -> None:
        now = time.monotonic()
        if self.state == self.OPEN:
            if now - self.opened_at >= self.recovery_timeout:
                self.state = self.HALF_OPEN
                self.half_open_calls = 0
            else:
                raise RpcError(StatusCode.UNAVAILABLE, "circuit breaker is open")

        if self.state == self.HALF_OPEN:
            if self.half_open_calls >= self.half_open_max_calls:
                raise RpcError(StatusCode.UNAVAILABLE, "circuit breaker is half-open")
            self.half_open_calls += 1

    def record_success(self) -> None:
        self.failure_count = 0
        self.half_open_calls = 0
        self.state = self.CLOSED

    def record_failure(self, error: RpcError) -> None:
        if error.code not in {StatusCode.UNAVAILABLE, StatusCode.INTERNAL, StatusCode.UNKNOWN}:
            return
        if self.state == self.HALF_OPEN:
            self.state = self.OPEN
            self.opened_at = time.monotonic()
            self.half_open_calls = 0
            return
        self.failure_count += 1
        if self.failure_count >= self.failure_threshold:
            self.state = self.OPEN
            self.opened_at = time.monotonic()
