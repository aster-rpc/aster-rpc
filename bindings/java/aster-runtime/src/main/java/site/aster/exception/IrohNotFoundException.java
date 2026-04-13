package site.aster.exception;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/** Thrown when a requested resource (endpoint, connection, node, etc.) is not found. */
public class IrohNotFoundException extends IrohException {
  public IrohNotFoundException(String message) {
    super(IrohStatus.NOT_FOUND, message);
  }
}
