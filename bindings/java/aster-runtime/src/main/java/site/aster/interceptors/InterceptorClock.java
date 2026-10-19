package site.aster.interceptors;

/**
 * The time source of the client's retry backoff (with its deadline budget) and of the circuit
 * breaker's recovery timeout. {@link #SYSTEM} is the real clock; tests pass a fake one so policy
 * timing is checked without real sleeps.
 */
public interface InterceptorClock {

  /** Monotonic time in nanoseconds, as {@link System#nanoTime()}. */
  long nanoTime();

  /** Waits {@code nanos} nanoseconds of this clock's time. */
  void sleep(long nanos) throws InterruptedException;

  /** The system clock: {@link System#nanoTime()} and {@link Thread#sleep(long, int)}. */
  InterceptorClock SYSTEM =
      new InterceptorClock() {
        @Override
        public long nanoTime() {
          return System.nanoTime();
        }

        @Override
        public void sleep(long nanos) throws InterruptedException {
          Thread.sleep(nanos / 1_000_000L, (int) (nanos % 1_000_000L));
        }
      };
}
