package site.aster.interceptors;

import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Collects RED metrics (Rate, Errors, Duration) for RPC calls.
 *
 * <p>Provides in-memory counters via {@link #snapshot()}. OpenTelemetry integration is left as a
 * future extension point -- this implementation covers the in-memory counters needed for
 * diagnostics and testing.
 */
public final class MetricsInterceptor implements Interceptor {

  private final AtomicLong started = new AtomicLong();
  private final AtomicLong succeeded = new AtomicLong();
  private final AtomicLong failed = new AtomicLong();

  /** Tracks start times per call key for duration calculation. */
  private final ConcurrentHashMap<String, Long> callStarts = new ConcurrentHashMap<>();

  @Override
  public Object onRequest(CallContext ctx, Object request) {
    started.incrementAndGet();

    // Start timing using a composite key
    String callKey = ctx.service() + "." + ctx.method() + "." + System.identityHashCode(request);
    callStarts.put(callKey, System.nanoTime());

    // Store the key on metadata so on_response/on_error can find it
    ctx.metadata().put("_metrics_call_key", callKey);

    return request;
  }

  @Override
  public Object onResponse(CallContext ctx, Object response) {
    succeeded.incrementAndGet();
    finishTiming(ctx);
    return response;
  }

  @Override
  public RpcError onError(CallContext ctx, RpcError error) {
    failed.incrementAndGet();
    finishTiming(ctx);
    return error;
  }

  private void finishTiming(CallContext ctx) {
    String callKey = ctx.metadata().remove("_metrics_call_key");
    if (callKey != null) {
      callStarts.remove(callKey);
    }
  }

  /** Returns a snapshot of the in-memory counters. */
  public Map<String, Long> snapshot() {
    long s = started.get();
    long ok = succeeded.get();
    long err = failed.get();
    return Map.of(
        "started", s,
        "succeeded", ok,
        "failed", err,
        "in_flight", s - ok - err);
  }
}
