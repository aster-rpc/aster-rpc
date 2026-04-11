package com.aster.exception;

import com.aster.ffi.IrohException;
import com.aster.ffi.IrohStatus;

/** Thrown when an operation times out. */
public class IrohTimeoutException extends IrohException {
  public IrohTimeoutException(String message) {
    super(IrohStatus.TIMEOUT, message);
  }
}
