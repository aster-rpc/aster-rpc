package site.aster.client;

import java.util.concurrent.Executor;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Shared executor for FFI blocking operations.
 *
 * <p>Each {@code aster_call_*} FFI entry point internally drives tokio's {@code block_on}, which
 * parks the calling thread until the underlying async future completes. On a Java virtual thread
 * that park PINS the carrier (it's a JNI/FFM call, not a recognised park-point), so N concurrent
 * FFI calls from N virtual threads can exhaust all platform-thread carriers and stall the
 * scheduler. The MC benchmark exposed this at 50+ concurrent unary calls.
 *
 * <p>Fix: route every Aster call task onto a cached pool of platform threads. Cached so it grows on
 * demand (matching the on-call concurrency) and idle threads expire after 60 s. Threads are daemons
 * named {@code aster-call-N}.
 */
final class CallExecutor {

  static final Executor INSTANCE = newPlatformPool();

  private CallExecutor() {}

  private static Executor newPlatformPool() {
    AtomicLong seq = new AtomicLong();
    ThreadPoolExecutor pool =
        new ThreadPoolExecutor(
            0,
            Integer.MAX_VALUE,
            60L,
            TimeUnit.SECONDS,
            new SynchronousQueue<>(),
            r -> {
              Thread t = new Thread(r, "aster-call-" + seq.incrementAndGet());
              t.setDaemon(true);
              return t;
            });
    pool.allowCoreThreadTimeOut(true);
    return pool;
  }
}
