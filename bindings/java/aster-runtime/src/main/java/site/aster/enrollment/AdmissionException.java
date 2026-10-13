package site.aster.enrollment;

import site.aster.ffi.IrohException;

/** Why an Enrollment operation did not admit a peer ({@code aster_enrollment_error_t}). */
public final class AdmissionException extends IrohException {
  private static final long serialVersionUID = 1L;

  private final Kind kind;

  AdmissionException(Kind kind, IrohException cause) {
    super(cause.status(), cause.getMessage());
    initCause(cause);
    this.kind = kind;
  }

  /** The kind of admission error. */
  public Kind kind() {
    return kind;
  }

  /** The kind of an admission error ({@code aster_enrollment_error_t}). */
  public enum Kind {
    /** Not an admission error (configuration, transport, timeout). */
    OTHER,
    /** No admission session: the peer must enroll (again). */
    NOT_ADMITTED,
    /** A definite denial; only this kind revokes a session. */
    DENIED,
    /** The peer's (or its issuer's) clock is outside the activation leeway. */
    CLOCK_SKEW,
    /** Policy could not be evaluated now; the session is kept. Retry. */
    UNAVAILABLE;

    static Kind fromFlags(int flags) {
      Kind[] all = values();
      return flags >= 0 && flags < all.length ? all[flags] : OTHER;
    }
  }
}
