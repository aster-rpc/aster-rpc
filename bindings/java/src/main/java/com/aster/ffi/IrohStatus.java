package com.aster.ffi;

/** Return values for synchronous FFI calls. */
public enum IrohStatus {
  OK(0),
  INVALID_ARGUMENT(1),
  NOT_FOUND(2),
  ALREADY_CLOSED(3),
  QUEUE_FULL(4),
  BUFFER_TOO_SMALL(5),
  UNSUPPORTED(6),
  INTERNAL(7),
  TIMEOUT(8),
  CANCELLED(9),
  CONNECTION_REFUSED(10),
  STREAM_RESET(11);

  public final int code;

  IrohStatus(int code) {
    this.code = code;
  }

  public static IrohStatus fromCode(int code) {
    for (var v : values()) {
      if (v.code == code) return v;
    }
    return INTERNAL;
  }
}
