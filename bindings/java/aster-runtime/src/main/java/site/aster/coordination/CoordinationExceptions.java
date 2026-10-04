package site.aster.coordination;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/** Client-side (never reached native code) coordination failures. */
final class CoordinationExceptions {

  private CoordinationExceptions() {}

  static CoordinationException notFound(String message) {
    return CoordinationException.from(new IrohException(IrohStatus.NOT_FOUND, message));
  }

  static CoordinationException closed(String message) {
    return CoordinationException.from(new IrohException(IrohStatus.ALREADY_CLOSED, message));
  }
}
