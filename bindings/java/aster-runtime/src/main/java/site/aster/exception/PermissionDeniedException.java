package site.aster.exception;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/**
 * A policy refused the caller ({@code ASTER_STATUS_PERMISSION_DENIED}): today a streaming or
 * buffered relay to a service whose admission policy said no.
 */
public class PermissionDeniedException extends IrohException {
  public PermissionDeniedException(String message) {
    super(IrohStatus.PERMISSION_DENIED, message);
  }
}
