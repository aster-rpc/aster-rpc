package site.aster.exception;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/** Thrown when an invalid argument is passed to an FFI function. */
public class IrohInvalidArgumentException extends IrohException {
  public IrohInvalidArgumentException(String message) {
    super(IrohStatus.INVALID_ARGUMENT, message);
  }
}
