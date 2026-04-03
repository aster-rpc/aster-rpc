"""
aster.status — Status codes and RPC error hierarchy.

Spec reference: §6.5 (status codes)
"""

from __future__ import annotations

from enum import IntEnum


class StatusCode(IntEnum):
    """Aster RPC status codes (semantically identical to gRPC codes 0–16)."""

    OK = 0
    CANCELLED = 1
    UNKNOWN = 2
    INVALID_ARGUMENT = 3
    DEADLINE_EXCEEDED = 4
    NOT_FOUND = 5
    ALREADY_EXISTS = 6
    PERMISSION_DENIED = 7
    RESOURCE_EXHAUSTED = 8
    FAILED_PRECONDITION = 9
    ABORTED = 10
    OUT_OF_RANGE = 11
    UNIMPLEMENTED = 12
    INTERNAL = 13
    UNAVAILABLE = 14
    DATA_LOSS = 15
    UNAUTHENTICATED = 16


class RpcError(Exception):
    """Base exception for Aster RPC errors.

    Attributes:
        code: The ``StatusCode`` describing the failure category.
        message: A human-readable error description.
        details: Arbitrary string key/value pairs carrying extra context.
    """

    def __init__(
        self,
        code: StatusCode,
        message: str = "",
        details: dict[str, str] | None = None,
    ) -> None:
        self.code = code
        self.message = message
        self.details: dict[str, str] = details or {}
        super().__init__(f"[{code.name}] {message}")

    def __repr__(self) -> str:
        return (
            f"RpcError(code={self.code!r}, message={self.message!r}, "
            f"details={self.details!r})"
        )