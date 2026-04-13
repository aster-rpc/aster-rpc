package site.aster.exception;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/** Thrown when attempting an operation on a handle that is already closed. */
public class IrohAlreadyClosedException extends IrohException {
  public IrohAlreadyClosedException(String message) {
    super(IrohStatus.ALREADY_CLOSED, message);
  }
}
