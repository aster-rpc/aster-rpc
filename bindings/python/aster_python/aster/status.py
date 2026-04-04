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

    @classmethod
    def from_status(
        cls,
        code: StatusCode,
        message: str = "",
        details: dict[str, str] | None = None,
    ) -> "RpcError":
        """Create the most specific RpcError subclass for a status code."""
        exc_type = _RPC_ERROR_TYPES.get(code, RpcError)
        return exc_type(message=message, details=details)


class CancelledError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.CANCELLED, message, details)


class UnknownRpcError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.UNKNOWN, message, details)


class InvalidArgumentError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.INVALID_ARGUMENT, message, details)


class DeadlineExceededError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.DEADLINE_EXCEEDED, message, details)


class NotFoundError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.NOT_FOUND, message, details)


class AlreadyExistsError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.ALREADY_EXISTS, message, details)


class PermissionDeniedError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.PERMISSION_DENIED, message, details)


class ResourceExhaustedError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.RESOURCE_EXHAUSTED, message, details)


class FailedPreconditionError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.FAILED_PRECONDITION, message, details)


class AbortedError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.ABORTED, message, details)


class OutOfRangeError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.OUT_OF_RANGE, message, details)


class UnimplementedError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.UNIMPLEMENTED, message, details)


class InternalError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.INTERNAL, message, details)


class UnavailableError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.UNAVAILABLE, message, details)


class DataLossError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.DATA_LOSS, message, details)


class UnauthenticatedError(RpcError):
    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.UNAUTHENTICATED, message, details)


_RPC_ERROR_TYPES: dict[StatusCode, type[RpcError]] = {
    StatusCode.CANCELLED: CancelledError,
    StatusCode.UNKNOWN: UnknownRpcError,
    StatusCode.INVALID_ARGUMENT: InvalidArgumentError,
    StatusCode.DEADLINE_EXCEEDED: DeadlineExceededError,
    StatusCode.NOT_FOUND: NotFoundError,
    StatusCode.ALREADY_EXISTS: AlreadyExistsError,
    StatusCode.PERMISSION_DENIED: PermissionDeniedError,
    StatusCode.RESOURCE_EXHAUSTED: ResourceExhaustedError,
    StatusCode.FAILED_PRECONDITION: FailedPreconditionError,
    StatusCode.ABORTED: AbortedError,
    StatusCode.OUT_OF_RANGE: OutOfRangeError,
    StatusCode.UNIMPLEMENTED: UnimplementedError,
    StatusCode.INTERNAL: InternalError,
    StatusCode.UNAVAILABLE: UnavailableError,
    StatusCode.DATA_LOSS: DataLossError,
    StatusCode.UNAUTHENTICATED: UnauthenticatedError,
}