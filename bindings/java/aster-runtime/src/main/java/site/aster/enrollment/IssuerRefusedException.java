package site.aster.enrollment;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;

/**
 * An issuer refused to sign ({@code PERMISSION_DENIED}). Codes as {@code aster.enrollment.IssuerV1}
 * reports them: {@code subject_denied}, {@code registered}, {@code policy_unavailable}, {@code
 * foreign_credential}, {@code issuer_revoked}, {@code invalid_credential}, {@code
 * invalid_argument}.
 */
public final class IssuerRefusedException extends IrohException {
  private static final long serialVersionUID = 1L;

  private final String code;
  private final boolean retryable;

  public IssuerRefusedException(String code, String message, boolean retryable) {
    super(IrohStatus.PERMISSION_DENIED, "enrollment issuer refused: " + code + ": " + message);
    this.code = code;
    this.retryable = retryable;
  }

  /** The refusal code. */
  public String code() {
    return code;
  }

  /** Whether retrying later may succeed. */
  public boolean retryable() {
    return retryable;
  }
}
