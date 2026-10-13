package site.aster.exception;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/**
 * A well-formed input failed a security check ({@code ASTER_STATUS_VERIFICATION_FAILED}): a sealed
 * envelope did not open under the given key and context, or a grant role did not match its
 * capability. The message names the failed check.
 */
public class VerificationFailedException extends IrohException {
  public VerificationFailedException(String message) {
    super(IrohStatus.VERIFICATION_FAILED, message);
  }
}
