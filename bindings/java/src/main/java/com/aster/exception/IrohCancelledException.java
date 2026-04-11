package com.aster.exception;

import com.aster.ffi.IrohException;
import com.aster.ffi.IrohStatus;

/** Thrown when an operation is cancelled. */
public class IrohCancelledException extends IrohException {
  public IrohCancelledException(String message) {
    super(IrohStatus.CANCELLED, message);
  }
}
