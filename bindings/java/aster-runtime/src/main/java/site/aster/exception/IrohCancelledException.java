package site.aster.exception;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/** Thrown when an operation is cancelled. */
public class IrohCancelledException extends IrohException {
  public IrohCancelledException(String message) {
    super(IrohStatus.CANCELLED, message);
  }
}
