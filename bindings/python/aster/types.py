"""
aster.types -- Shared types used across the Aster RPC framework.

Spec reference: §5.1 (serialization modes)
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import IntEnum


class SerializationMode(IntEnum):
    """Serialization protocol negotiated between client and server."""

    XLANG = 0
    NATIVE = 1
    ROW = 2


@dataclass
class ExponentialBackoff:
    """Exponential backoff parameters for retry policies.

    Attributes:
        initial_ms: Initial delay in milliseconds.
        max_ms: Maximum delay in milliseconds.
        multiplier: Multiplicative factor applied after each attempt.
        jitter: Random jitter factor (0.0--1.0) added to each delay.
    """

    initial_ms: int = 100
    max_ms: int = 30_000
    multiplier: float = 2.0
    jitter: float = 0.1


@dataclass
class RetryPolicy:
    """Retry policy for idempotent RPC calls.

    Attributes:
        max_attempts: Maximum number of attempts (including the first).
        backoff: Exponential backoff configuration.
    """

    max_attempts: int = 3
    backoff: ExponentialBackoff = field(default_factory=ExponentialBackoff)