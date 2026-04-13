package site.aster.interceptors;

/**
 * Validates and enforces call deadlines.
 *
 * <p>A request whose deadline has already passed by more than the configured skew tolerance is
 * rejected immediately with {@link StatusCode#DEADLINE_EXCEEDED}.
 */
public final class DeadlineInterceptor implements Interceptor {

  private final int skewToleranceMs;

  /** Creates a deadline interceptor with the default skew tolerance of 5000 ms. */
  public DeadlineInterceptor() {
    this(5000);
  }

  /**
   * Creates a deadline interceptor with the specified skew tolerance.
   *
   * @param skewToleranceMs milliseconds of clock-skew tolerance added when checking on receipt
   */
  public DeadlineInterceptor(int skewToleranceMs) {
    this.skewToleranceMs = skewToleranceMs;
  }

  @Override
  public Object onRequest(CallContext ctx, Object request) {
    Double deadline = ctx.deadline();
    if (deadline != null) {
      long nowEpochMs = System.currentTimeMillis();
      long deadlineEpochMs = (long) (deadline * 1000);

      // Reject on receipt if expired beyond skew tolerance
      if (nowEpochMs > deadlineEpochMs + skewToleranceMs) {
        throw new RpcError(
            StatusCode.DEADLINE_EXCEEDED,
            "deadline already expired on receipt"
                + " (now="
                + nowEpochMs
                + ", deadline="
                + deadlineEpochMs
                + ", skew_tolerance="
                + skewToleranceMs
                + "ms)");
      }

      // Standard expiry check (no tolerance)
      if (ctx.isExpired()) {
        throw new RpcError(StatusCode.DEADLINE_EXCEEDED, "deadline exceeded");
      }
    }
    return request;
  }

  /**
   * Returns the remaining seconds until the deadline, or {@code null} if no deadline is set. Useful
   * for setting timeouts on downstream calls.
   */
  public Double timeoutSeconds(CallContext ctx) {
    Double remaining = ctx.remainingSeconds();
    if (remaining == null) {
      return null;
    }
    return Math.max(0.0, remaining);
  }
}
