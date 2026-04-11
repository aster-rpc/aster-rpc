/**
 * Aster RPC status codes and error hierarchy.
 *
 * Spec reference: S6.5 (status codes).
 *
 * Codes 0-16 mirror gRPC's google.rpc.Code semantically. Codes 100+
 * are Aster-native and have no gRPC equivalent. The 17-99 range is
 * reserved as a buffer in case gRPC ever extends its enum.
 *
 * A common gripe with gRPC's status codes is that there are too few
 * to express the variety of failures real services actually hit.
 * The 100+ space gives Aster room to mint more precise codes over
 * time, signalling clearly that they are intentionally separate
 * from the gRPC vocabulary.
 */
export const StatusCode = {
  // gRPC-mirrored codes (0-16)
  OK: 0,
  CANCELLED: 1,
  UNKNOWN: 2,
  INVALID_ARGUMENT: 3,
  DEADLINE_EXCEEDED: 4,
  NOT_FOUND: 5,
  ALREADY_EXISTS: 6,
  PERMISSION_DENIED: 7,
  RESOURCE_EXHAUSTED: 8,
  FAILED_PRECONDITION: 9,
  ABORTED: 10,
  OUT_OF_RANGE: 11,
  UNIMPLEMENTED: 12,
  INTERNAL: 13,
  UNAVAILABLE: 14,
  DATA_LOSS: 15,
  UNAUTHENTICATED: 16,

  // 17-99 reserved (gRPC extension buffer)

  // Aster-native codes (100+)
  /**
   * The wire payload doesn't match the published contract: e.g. the
   * JSON dict has fields the receiver's @WireType class doesn't
   * declare, or vice versa. Distinct from INVALID_ARGUMENT because
   * the violation is about data SHAPE, not value, and shape
   * violations can occur at any nesting depth (a top-level
   * INVALID_ARGUMENT label doesn't apply when the bad field is two
   * objects deep). The producer owns the contract; consumers must
   * use the field names defined by the producer's manifest.
   */
  CONTRACT_VIOLATION: 101,
} as const;

export type StatusCode = (typeof StatusCode)[keyof typeof StatusCode];

/** Human-readable name for a status code. */
const STATUS_NAMES: Record<number, string> = {
  0: 'OK', 1: 'CANCELLED', 2: 'UNKNOWN', 3: 'INVALID_ARGUMENT',
  4: 'DEADLINE_EXCEEDED', 5: 'NOT_FOUND', 6: 'ALREADY_EXISTS',
  7: 'PERMISSION_DENIED', 8: 'RESOURCE_EXHAUSTED', 9: 'FAILED_PRECONDITION',
  10: 'ABORTED', 11: 'OUT_OF_RANGE', 12: 'UNIMPLEMENTED', 13: 'INTERNAL',
  14: 'UNAVAILABLE', 15: 'DATA_LOSS', 16: 'UNAUTHENTICATED',
  101: 'CONTRACT_VIOLATION',
};

export function statusName(code: StatusCode): string {
  return STATUS_NAMES[code] ?? `UNKNOWN(${code})`;
}

/** Base error class for Aster RPC errors. */
export class RpcError extends Error {
  readonly code: StatusCode;
  readonly details: Record<string, string>;

  constructor(
    code: StatusCode,
    message = '',
    details?: Record<string, string>,
  ) {
    super(`[${statusName(code)}] ${message}`);
    this.name = 'RpcError';
    this.code = code;
    this.details = details ?? {};
  }

  /** Create the most specific RpcError subclass for a status code. */
  static fromStatus(
    code: StatusCode,
    message = '',
    details?: Record<string, string>,
  ): RpcError {
    const ErrorClass = RPC_ERROR_TYPES.get(code);
    if (ErrorClass) return new ErrorClass(message, details);
    return new RpcError(code, message, details);
  }
}

export class CancelledError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.CANCELLED, message, details);
    this.name = 'CancelledError';
  }
}

export class UnknownRpcError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.UNKNOWN, message, details);
    this.name = 'UnknownRpcError';
  }
}

export class InvalidArgumentError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.INVALID_ARGUMENT, message, details);
    this.name = 'InvalidArgumentError';
  }
}

export class DeadlineExceededError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.DEADLINE_EXCEEDED, message, details);
    this.name = 'DeadlineExceededError';
  }
}

export class NotFoundError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.NOT_FOUND, message, details);
    this.name = 'NotFoundError';
  }
}

export class AlreadyExistsError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.ALREADY_EXISTS, message, details);
    this.name = 'AlreadyExistsError';
  }
}

export class PermissionDeniedError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.PERMISSION_DENIED, message, details);
    this.name = 'PermissionDeniedError';
  }
}

export class ResourceExhaustedError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.RESOURCE_EXHAUSTED, message, details);
    this.name = 'ResourceExhaustedError';
  }
}

export class FailedPreconditionError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.FAILED_PRECONDITION, message, details);
    this.name = 'FailedPreconditionError';
  }
}

export class AbortedError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.ABORTED, message, details);
    this.name = 'AbortedError';
  }
}

export class OutOfRangeError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.OUT_OF_RANGE, message, details);
    this.name = 'OutOfRangeError';
  }
}

export class UnimplementedError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.UNIMPLEMENTED, message, details);
    this.name = 'UnimplementedError';
  }
}

export class InternalError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.INTERNAL, message, details);
    this.name = 'InternalError';
  }
}

export class UnavailableError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.UNAVAILABLE, message, details);
    this.name = 'UnavailableError';
  }
}

export class DataLossError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.DATA_LOSS, message, details);
    this.name = 'DataLossError';
  }
}

export class UnauthenticatedError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.UNAUTHENTICATED, message, details);
    this.name = 'UnauthenticatedError';
  }
}

/**
 * Raised when the wire payload doesn't match the published contract.
 *
 * Carries the offending field names in `details.unexpected_fields`
 * (comma-separated, repr-sanitized).
 */
export class ContractViolationError extends RpcError {
  constructor(message = '', details?: Record<string, string>) {
    super(StatusCode.CONTRACT_VIOLATION, message, details);
    this.name = 'ContractViolationError';
  }
}

const RPC_ERROR_TYPES = new Map<StatusCode, new (message?: string, details?: Record<string, string>) => RpcError>([
  [StatusCode.CANCELLED, CancelledError],
  [StatusCode.UNKNOWN, UnknownRpcError],
  [StatusCode.INVALID_ARGUMENT, InvalidArgumentError],
  [StatusCode.DEADLINE_EXCEEDED, DeadlineExceededError],
  [StatusCode.NOT_FOUND, NotFoundError],
  [StatusCode.ALREADY_EXISTS, AlreadyExistsError],
  [StatusCode.PERMISSION_DENIED, PermissionDeniedError],
  [StatusCode.RESOURCE_EXHAUSTED, ResourceExhaustedError],
  [StatusCode.FAILED_PRECONDITION, FailedPreconditionError],
  [StatusCode.ABORTED, AbortedError],
  [StatusCode.OUT_OF_RANGE, OutOfRangeError],
  [StatusCode.UNIMPLEMENTED, UnimplementedError],
  [StatusCode.INTERNAL, InternalError],
  [StatusCode.UNAVAILABLE, UnavailableError],
  [StatusCode.DATA_LOSS, DataLossError],
  [StatusCode.UNAUTHENTICATED, UnauthenticatedError],
  [StatusCode.CONTRACT_VIOLATION, ContractViolationError],
]);
