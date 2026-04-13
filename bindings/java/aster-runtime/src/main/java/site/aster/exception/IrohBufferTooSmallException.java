package site.aster.exception;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/** Thrown when a provided buffer is too small to hold the result. */
public class IrohBufferTooSmallException extends IrohException {
  public IrohBufferTooSmallException(String message) {
    super(IrohStatus.BUFFER_TOO_SMALL, message);
  }
}
