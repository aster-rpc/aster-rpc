package com.aster.interceptors;

import java.util.Set;
import java.util.concurrent.ThreadLocalRandom;

/**
 * Retry interceptor providing retry policy hints for client calls.
 *
 * <p>This interceptor does not perform retries itself -- it exposes {@link #shouldRetry} and {@link
 * #backoffSeconds} for the client dispatch loop to consult.
 */
public final class RetryInterceptor implements Interceptor {

  private final Set<StatusCode> retryableCodes;
  private final long initialMs;
  private final long maxMs;
  private final double multiplier;
  private final double jitter;

  /**
   * Creates a retry interceptor with the given backoff parameters.
   *
   * @param retryableCodes status codes eligible for retry (default: UNAVAILABLE only)
   * @param initialMs initial backoff in milliseconds (default 100)
   * @param maxMs maximum backoff in milliseconds (default 5000)
   * @param multiplier exponential backoff multiplier (default 2.0)
   * @param jitter jitter factor (0.0 to 1.0) applied to the delay (default 0.2)
   */
  public RetryInterceptor(
      Set<StatusCode> retryableCodes,
      long initialMs,
      long maxMs,
      double multiplier,
      double jitter) {
    this.retryableCodes = retryableCodes != null ? retryableCodes : Set.of(StatusCode.UNAVAILABLE);
    this.initialMs = initialMs;
    this.maxMs = maxMs;
    this.multiplier = multiplier;
    this.jitter = jitter;
  }

  /** Creates a retry interceptor with default settings. */
  public RetryInterceptor() {
    this(Set.of(StatusCode.UNAVAILABLE), 100, 5000, 2.0, 0.2);
  }

  /**
   * Returns {@code true} if the call should be retried.
   *
   * <p>A call is retryable if it is marked idempotent and the error code is in the set of retryable
   * codes.
   */
  public boolean shouldRetry(CallContext ctx, RpcError error) {
    return ctx.isIdempotent() && retryableCodes.contains(error.code());
  }

  /**
   * Computes the backoff delay in seconds for the given attempt number.
   *
   * <p>Uses exponential backoff with jitter: {@code min(maxMs, initialMs * multiplier^(attempt-1))
   * + jitter}.
   *
   * @param attempt the retry attempt number (1-based)
   * @return delay in seconds before the next retry
   */
  public double backoffSeconds(int attempt) {
    long delayMs =
        Math.min(maxMs, (long) (initialMs * Math.pow(multiplier, Math.max(0, attempt - 1))));
    double jitterMs = delayMs * jitter * ThreadLocalRandom.current().nextDouble();
    return (delayMs + jitterMs) / 1000.0;
  }
}
