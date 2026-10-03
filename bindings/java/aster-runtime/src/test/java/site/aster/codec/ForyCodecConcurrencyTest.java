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

/**
 * Stresses {@link ForyCodec} from many virtual threads concurrently. This test exists as the
 * regression guard for Fory thread-safety: {@link org.apache.fory.Fory} is not thread-safe on its
 * own, and {@link ForyCodec} is shared across every in-flight call on an {@code AsterServer}
 * dispatched on {@code Executors.newVirtualThreadPerTaskExecutor()}. If this test ever fails again
 * the wrapping in {@link ForyCodec} has regressed back to a plain {@code Fory}.
 *
 * <p>We mirror the real server dispatch shape — one virtual thread per iteration, encode then
 * decode a small payload struct — rather than a synthetic micro-benchmark, so that the test
 * exercises the same concurrency pattern the server does in production.
 */
final class ForyCodecConcurrencyTest {

  /** Payload-shaped test type (the RPC envelope itself is no longer Fory-encoded). */
  public static final class Sample {
    public String service = "";
    public String method = "";
    public int callId;
    public List<String> tags = List.of();

    public Sample() {}

    Sample(String service, String method, int callId, List<String> tags) {
      this.service = service;
      this.method = method;
      this.callId = callId;
      this.tags = tags;
    }

    @Override
    public String toString() {
      return service + "/" + method + "#" + callId + tags;
    }
  }

  private static final int THREAD_COUNT = 32;
  private static final int ITERATIONS_PER_THREAD = 200;

  @Test
  void sharedCodecSurvivesManyConcurrentVirtualThreads() throws Exception {
    ForyCodec codec = new ForyCodec();
    site.aster.codec.ForyTags.register(codec.fory(), Sample.class, "_test/Sample");

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
                Sample header =
                    new Sample(
                        "Service-" + threadIndex,
                        "method-" + i,
                        threadIndex * ITERATIONS_PER_THREAD + i,
                        List.of("t" + i));
                byte[] encoded = codec.encode(header);
                Sample decoded = (Sample) codec.decode(encoded, Sample.class);
                if (!header.service.equals(decoded.service)
                    || !header.method.equals(decoded.method)
                    || header.callId != decoded.callId
                    || !header.tags.equals(decoded.tags)) {
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
