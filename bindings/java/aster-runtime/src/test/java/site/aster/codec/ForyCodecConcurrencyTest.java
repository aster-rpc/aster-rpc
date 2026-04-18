package site.aster.codec;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import site.aster.server.wire.StreamHeader;

/**
 * Stresses {@link ForyCodec} from many virtual threads concurrently. This test exists as the
 * regression guard for Fory thread-safety: {@link org.apache.fory.Fory} is not thread-safe on its
 * own, and {@link ForyCodec} is shared across every in-flight call on an {@code AsterServer}
 * dispatched on {@code Executors.newVirtualThreadPerTaskExecutor()}. If this test ever fails again
 * the wrapping in {@link ForyCodec} has regressed back to a plain {@code Fory}.
 *
 * <p>We mirror the real server dispatch shape — one virtual thread per iteration, encode then
 * decode a {@link StreamHeader} — rather than a synthetic micro-benchmark, so that the test
 * exercises the same concurrency pattern the server does in production.
 */
final class ForyCodecConcurrencyTest {

  private static final int THREAD_COUNT = 32;
  private static final int ITERATIONS_PER_THREAD = 200;

  @Test
  void sharedCodecSurvivesManyConcurrentVirtualThreads() throws Exception {
    ForyCodec codec = new ForyCodec();
    site.aster.codec.ForyTags.register(codec.fory(), StreamHeader.class, "_aster/StreamHeader");

    ExecutorService executor = Executors.newVirtualThreadPerTaskExecutor();
    CountDownLatch startGate = new CountDownLatch(1);
    CountDownLatch doneGate = new CountDownLatch(THREAD_COUNT);
    AtomicReference<Throwable> firstFailure = new AtomicReference<>();
    AtomicInteger successCount = new AtomicInteger();

    for (int t = 0; t < THREAD_COUNT; t++) {
      final int threadIndex = t;
      executor.execute(
          () -> {
            try {
              startGate.await();
              for (int i = 0; i < ITERATIONS_PER_THREAD; i++) {
                StreamHeader header =
                    new StreamHeader(
                        "Service-" + threadIndex,
                        "method-" + i,
                        1,
                        threadIndex * ITERATIONS_PER_THREAD + i,
                        (short) 0,
                        StreamHeader.SERIALIZATION_XLANG,
                        List.of(),
                        List.of());
                byte[] encoded = codec.encode(header);
                StreamHeader decoded = (StreamHeader) codec.decode(encoded, StreamHeader.class);
                if (!header.service().equals(decoded.service())
                    || !header.method().equals(decoded.method())
                    || header.callId() != decoded.callId()) {
                  throw new AssertionError(
                      "round-trip mismatch on thread "
                          + threadIndex
                          + " iter "
                          + i
                          + ": encoded="
                          + header
                          + " decoded="
                          + decoded);
                }
                successCount.incrementAndGet();
              }
            } catch (Throwable ex) {
              firstFailure.compareAndSet(null, ex);
            } finally {
              doneGate.countDown();
            }
          });
    }

    startGate.countDown();
    boolean finished = doneGate.await(60, TimeUnit.SECONDS);
    executor.shutdown();

    if (firstFailure.get() != null) {
      throw new AssertionError(
          "concurrent round-trip failed after "
              + successCount.get()
              + " successes — likely Fory thread-safety regression: "
              + firstFailure.get(),
          firstFailure.get());
    }
    assertEquals(true, finished, "stress test timed out");
    assertEquals(THREAD_COUNT * ITERATIONS_PER_THREAD, successCount.get());
  }
}
