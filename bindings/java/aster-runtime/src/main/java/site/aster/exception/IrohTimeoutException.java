package site.aster.exception;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/** Thrown when an operation times out. */
public class IrohTimeoutException extends IrohException {
  public IrohTimeoutException(String message) {
    super(IrohStatus.TIMEOUT, message);
  }
}
