package com.aster.exception;

import com.aster.ffi.IrohException;
import com.aster.ffi.IrohStatus;

/** Thrown when a provided buffer is too small to hold the result. */
public class IrohBufferTooSmallException extends IrohException {
  public IrohBufferTooSmallException(String message) {
    super(IrohStatus.BUFFER_TOO_SMALL, message);
  }
}
