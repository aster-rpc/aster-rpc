package site.aster.ffi;

/** Return values for synchronous FFI calls. */
public enum IrohStatus {
  OK(0),
  INVALID_ARGUMENT(1),
  NOT_FOUND(2),
  ALREADY_CLOSED(3),
  QUEUE_FULL(4),
  BUFFER_TOO_SMALL(5),
  UNSUPPORTED(6),
  INTERNAL(7),
  TIMEOUT(8),
  CANCELLED(9),
  CONNECTION_REFUSED(10),
  STREAM_RESET(11),
  /** {@code aster_runtime_new} was called with a different ABI major than the library. */
  ABI_MISMATCH(12),
  /**
   * A well-formed input failed a security check: an envelope did not open, an attestation chain did
   * not verify, or a grant role did not match its capability (ABI 3.4).
   */
  VERIFICATION_FAILED(13),
  /** Reserved for the auth design (ABI 4.3). */
  UNAUTHENTICATED(14),
  /** A policy refused the caller, e.g. a denied relay request (ABI 4.3). */
  PERMISSION_DENIED(15),
  /** Reserved for the auth design (ABI 4.3). */
  CREDENTIAL_EXPIRED(16),
  /**
   * The resource already exists: {@code aster_observability_init} with a configuration that differs
   * from the running pipeline's, or a second concurrent {@code aster_log_poll} (ABI 4.6).
   */
  ALREADY_EXISTS(17);

  public final int code;

  IrohStatus(int code) {
    this.code = code;
  }

  public static IrohStatus fromCode(int code) {
    for (var v : values()) {
      if (v.code == code) return v;
    }
    return INTERNAL;
  }
}
