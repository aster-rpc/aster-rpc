"""
aster.status -- Status codes and RPC error hierarchy.

Spec reference: §6.5 (status codes)
"""

from __future__ import annotations

from enum import IntEnum


class StatusCode(IntEnum):
    """RPC status codes.

    Codes 0-16 mirror gRPC's ``google.rpc.Code`` semantically. Codes
    100+ are Aster-native and have no gRPC equivalent. The 17-99
    range is reserved as a buffer in case gRPC ever extends its enum.

    A common gripe with gRPC's status codes is that there are too
    few of them to express the variety of failures real services
    actually hit -- everything ends up shoehorned into a handful of
    overloaded categories. The 100+ space gives Aster room to mint
    more precise codes over time, signalling clearly that they are
    intentionally separate from the gRPC vocabulary.

    Use these to inspect errors returned by the server::

        try:
            resp = await svc.get_status(req)
        except RpcError as e:
            if e.code == StatusCode.NOT_FOUND:
                print("Service not found")
            elif e.code == StatusCode.CONTRACT_VIOLATION:
                print("Wire shape doesn't match the contract")
    """

    # ── gRPC-mirrored codes (0-16) ───────────────────────────────────
    OK = 0                    #: Success.
    CANCELLED = 1             #: The call was cancelled by the client.
    UNKNOWN = 2               #: Unknown error (server bug or unhandled exception).
    INVALID_ARGUMENT = 3      #: Client sent an invalid request.
    DEADLINE_EXCEEDED = 4     #: The call timed out.
    NOT_FOUND = 5             #: Requested resource does not exist.
    ALREADY_EXISTS = 6        #: Resource already exists (e.g., duplicate create).
    PERMISSION_DENIED = 7     #: Caller lacks required capability.
    RESOURCE_EXHAUSTED = 8    #: Rate limit or quota exceeded.
    FAILED_PRECONDITION = 9   #: Precondition not met (e.g., wrong state).
    ABORTED = 10              #: Operation aborted (e.g., concurrency conflict).
    OUT_OF_RANGE = 11         #: Value outside valid range.
    UNIMPLEMENTED = 12        #: Method not implemented by the server.
    INTERNAL = 13             #: Internal server error.
    UNAVAILABLE = 14          #: Server temporarily unavailable (retry later).
    DATA_LOSS = 15            #: Unrecoverable data loss.
    UNAUTHENTICATED = 16      #: No valid credentials provided.

    # ── Reserved range (17-99) ───────────────────────────────────────
    # Intentionally left empty -- reserved as a buffer in case gRPC
    # ever extends its enum upward. New Aster-native codes start at
    # 101 and grow from there.

    # ── Aster-native codes (100+) ────────────────────────────────────
    #: The wire payload doesn't match the published contract -- e.g.
    #: the JSON dict has fields the server's @wire_type dataclass
    #: doesn't declare, or vice versa. Distinct from INVALID_ARGUMENT
    #: because the violation is about the SHAPE of the data, not its
    #: value, and because shape violations can occur at any nesting
    #: depth (a top-level INVALID_ARGUMENT label doesn't apply when
    #: the bad field is two objects deep). The producer owns the
    #: contract; consumers must use the field names defined by the
    #: producer's manifest.
    CONTRACT_VIOLATION = 101


class RpcError(Exception):
    """Exception raised when an RPC call fails.

    Catch this in client code to handle server-side errors::

        from aster import RpcError, StatusCode

        try:
            resp = await svc.my_method(request)
        except RpcError as e:
            print(f"RPC failed: {e.code.name} -- {e.message}")
            if e.details:
                print(f"Details: {e.details}")

    Attributes:
        code: The :class:`StatusCode` describing the failure category.
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


class ContractViolationError(RpcError):
    """Raised when the wire payload doesn't match the published contract.

    Carries the offending field names in ``details`` under the key
    ``unexpected_fields`` (comma-separated, sanitized via repr).
    """

    def __init__(self, message: str = "", details: dict[str, str] | None = None) -> None:
        super().__init__(StatusCode.CONTRACT_VIOLATION, message, details)


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
    StatusCode.CONTRACT_VIOLATION: ContractViolationError,
}