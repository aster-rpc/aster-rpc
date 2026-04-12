package com.aster.interceptors;

/**
 * RPC status codes.
 *
 * <p>Codes 0-16 mirror gRPC's {@code google.rpc.Code} semantically. Codes 100+ are Aster-native and
 * have no gRPC equivalent. The 17-99 range is reserved as a buffer in case gRPC ever extends its
 * enum.
 */
public enum StatusCode {
  OK(0),
  CANCELLED(1),
  UNKNOWN(2),
  INVALID_ARGUMENT(3),
  DEADLINE_EXCEEDED(4),
  NOT_FOUND(5),
  ALREADY_EXISTS(6),
  PERMISSION_DENIED(7),
  RESOURCE_EXHAUSTED(8),
  FAILED_PRECONDITION(9),
  ABORTED(10),
  OUT_OF_RANGE(11),
  UNIMPLEMENTED(12),
  INTERNAL(13),
  UNAVAILABLE(14),
  DATA_LOSS(15),
  UNAUTHENTICATED(16),
  CONTRACT_VIOLATION(101);

  private final int value;

  StatusCode(int value) {
    this.value = value;
  }

  /** Returns the integer wire value for this status code. */
  public int value() {
    return value;
  }

  /** Looks up a {@link StatusCode} by its integer wire value. */
  public static StatusCode fromValue(int value) {
    for (StatusCode code : values()) {
      if (code.value == value) {
        return code;
      }
    }
    return UNKNOWN;
  }
}
