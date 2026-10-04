"""Deadline interceptor (client side).

Spec reference: S6.8.1

On the client, ``timeout_seconds`` bounds each attempt and ``on_request``
refuses to send a call whose deadline has already passed (with a
configurable clock-skew tolerance). Servers no longer need it: the Rust
dispatcher enforces the caller's deadline, the method ``timeout`` and the
server's handler cap, sends ``DEADLINE_EXCEEDED`` and cancels the handler
(plan P4). Wiring it on a server still works and only adds the receipt check.
"""

from __future__ import annotations

import time

from aster.interceptors.base import CallContext, Interceptor
from aster.status import RpcError, StatusCode


class DeadlineInterceptor(Interceptor):
    """Validates and enforces call deadlines.

    Args:
        skew_tolerance_ms: Milliseconds of clock-skew tolerance added to the
            deadline when checking on receipt.  A request whose deadline has
            already passed by more than this tolerance is rejected immediately
            with ``DEADLINE_EXCEEDED``.  Defaults to 5000 ms (5 seconds).
    """

    def __init__(self, skew_tolerance_ms: int = 5000) -> None:
        self._skew_tolerance_ms = skew_tolerance_ms

    async def on_request(self, ctx: CallContext, request: object) -> object:
        if ctx.deadline is not None:
            now_epoch_ms = int(time.time() * 1000)
            deadline_epoch_ms = int(ctx.deadline * 1000)
            # Reject on receipt if expired beyond skew tolerance (S6.8.1)
            if now_epoch_ms > deadline_epoch_ms + self._skew_tolerance_ms:
                raise RpcError(
                    StatusCode.DEADLINE_EXCEEDED,
                    "deadline already expired on receipt "
                    f"(now={now_epoch_ms}, deadline={deadline_epoch_ms}, "
                    f"skew_tolerance={self._skew_tolerance_ms}ms)",
                )
            # Standard expiry check (no tolerance)
            if ctx.expired:
                raise RpcError(StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
        return request

    def timeout_seconds(self, ctx: CallContext) -> float | None:
        """Return remaining seconds until deadline, or None if no deadline set."""
        remaining = ctx.remaining_seconds
        if remaining is None:
            return None
        return max(0.0, remaining)
