package site.aster.interceptors;

import java.util.Set;

/**
 * Simple CLOSED -> OPEN -> HALF_OPEN circuit breaker.
 *
 * <p>Tracks consecutive failures and opens the circuit when the threshold is reached. After a
 * recovery timeout, the circuit moves to half-open and allows a limited number of probe calls.
 * Success resets to closed; failure reopens immediately.
 */
public final class CircuitBreakerInterceptor implements Interceptor {

  /** Circuit breaker states. */
  public enum State {
    CLOSED,
    OPEN,
    HALF_OPEN
  }

  private static final Set<StatusCode> TRIP_CODES =
      Set.of(StatusCode.UNAVAILABLE, StatusCode.INTERNAL, StatusCode.UNKNOWN);

  private final int failureThreshold;
  private final double recoveryTimeoutSecs;
  private final int halfOpenMaxCalls;

  private volatile State state = State.CLOSED;
  private int failureCount;
  private long openedAtNanos;
  private int halfOpenCalls;

  /**
   * Creates a circuit breaker interceptor.
   *
   * @param failureThreshold consecutive failures before opening (default 3)
   * @param recoveryTimeoutSecs seconds to wait before transitioning to half-open (default 5.0)
   * @param halfOpenMaxCalls max probe calls in half-open state (default 1)
   */
  public CircuitBreakerInterceptor(
      int failureThreshold, double recoveryTimeoutSecs, int halfOpenMaxCalls) {
    this.failureThreshold = failureThreshold;
    this.recoveryTimeoutSecs = recoveryTimeoutSecs;
    this.halfOpenMaxCalls = halfOpenMaxCalls;
  }

  /** Creates a circuit breaker with default settings (threshold=3, recovery=5s, halfOpen=1). */
  public CircuitBreakerInterceptor() {
    this(3, 5.0, 1);
  }

  /** Returns the current circuit breaker state. */
  public State state() {
    return state;
  }

  @Override
  public synchronized Object onRequest(CallContext ctx, Object request) {
    long now = System.nanoTime();

    if (state == State.OPEN) {
      double elapsedSecs = (now - openedAtNanos) / 1_000_000_000.0;
      if (elapsedSecs >= recoveryTimeoutSecs) {
        state = State.HALF_OPEN;
        halfOpenCalls = 0;
      } else {
        throw new RpcError(StatusCode.UNAVAILABLE, "circuit breaker is open");
      }
    }

    if (state == State.HALF_OPEN) {
      if (halfOpenCalls >= halfOpenMaxCalls) {
        throw new RpcError(StatusCode.UNAVAILABLE, "circuit breaker is half-open");
      }
      halfOpenCalls++;
    }

    return request;
  }

  @Override
  public synchronized Object onResponse(CallContext ctx, Object response) {
    // Success -- reset to closed
    failureCount = 0;
    halfOpenCalls = 0;
    state = State.CLOSED;
    return response;
  }

  @Override
  public synchronized RpcError onError(CallContext ctx, RpcError error) {
    if (!TRIP_CODES.contains(error.code())) {
      return error;
    }

    if (state == State.HALF_OPEN) {
      state = State.OPEN;
      openedAtNanos = System.nanoTime();
      halfOpenCalls = 0;
      return error;
    }

    failureCount++;
    if (failureCount >= failureThreshold) {
      state = State.OPEN;
      openedAtNanos = System.nanoTime();
    }

    return error;
  }
}
