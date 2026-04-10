package com.aster.ffi;

/** Top-level exception for all FFI-level errors. */
public class IrohException extends RuntimeException {
  private final IrohStatus status;
  private final int errorCode;

  public IrohException(IrohStatus status, String message) {
    super(message);
    this.status = status;
    this.errorCode = -1;
  }

  public IrohException(IrohStatus status, int errorCode, String message) {
    super(message);
    this.status = status;
    this.errorCode = errorCode;
  }

  public IrohException(String message) {
    super(message);
    this.status = IrohStatus.INTERNAL;
    this.errorCode = -1;
  }

  public IrohStatus status() {
    return status;
  }

  public int errorCode() {
    return errorCode;
  }
}
