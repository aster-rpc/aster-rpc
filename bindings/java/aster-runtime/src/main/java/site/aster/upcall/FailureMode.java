package site.aster.upcall;

/**
 * The answer a decision upcall takes when the binding gives none ({@code
 * aster_hook_failure_mode_t}): error, timeout, overload, shutdown or an undecodable answer.
 */
public enum FailureMode {
  /** Deny (the default; 0 since C ABI 7.0). */
  FAIL_CLOSED(0),
  /** Allow. */
  FAIL_OPEN(1);

  public final int code;

  FailureMode(int code) {
    this.code = code;
  }
}
