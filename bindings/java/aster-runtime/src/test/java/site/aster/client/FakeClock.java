package site.aster.client;

import java.time.Duration;
import java.util.concurrent.atomic.AtomicLong;
import site.aster.interceptors.InterceptorClock;

/**
 * A fake {@link InterceptorClock}: time moves only when a test advances it or the code under test
 * sleeps on it, and a sleep returns at once. Thread-safe.
 */
class FakeClock implements InterceptorClock {
  private final AtomicLong now = new AtomicLong(1_000_000_000L);
  private final AtomicLong slept = new AtomicLong();

  @Override
  public long nanoTime() {
    return now.get();
  }

  @Override
  public void sleep(long nanos) throws InterruptedException {
    slept.addAndGet(nanos);
    now.addAndGet(nanos);
  }

  /** Moves time forward by {@code d}. */
  void advance(Duration d) {
    now.addAndGet(d.toNanos());
  }

  /** The total time the code under test has slept on this clock. */
  Duration slept() {
    return Duration.ofNanos(slept.get());
  }
}
