package site.aster.exception;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/** Thrown when a stream is reset by the remote peer. */
public class IrohStreamResetException extends IrohException {
  public IrohStreamResetException(String message) {
    super(IrohStatus.STREAM_RESET, message);
  }
}
