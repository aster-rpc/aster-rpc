package site.aster.upcall;

/**
 * The answer a decision upcall takes when the binding gives none ({@code
 * aster_hook_failure_mode_t}): error, timeout, overload, shutdown or an undecodable answer.
 */
public enum FailureMode {
  /** Allow. */
  FAIL_OPEN(0),
  /** Deny (the default). */
  FAIL_CLOSED(1);

  public final int code;

  FailureMode(int code) {
    this.code = code;
  }
}
