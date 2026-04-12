package com.aster.interceptors;

import java.util.concurrent.ConcurrentHashMap;

/**
 * Token bucket rate limiter interceptor.
 *
 * <p>Limits request rate per service, per method, or per peer. Rejects requests that exceed the
 * limit with {@link StatusCode#RESOURCE_EXHAUSTED}.
 */
public final class RateLimitInterceptor implements Interceptor {

  private final double rate;
  private final double burst;
  private final String per;
  private final ConcurrentHashMap<String, TokenBucket> buckets = new ConcurrentHashMap<>();
  private final TokenBucket globalBucket;

  /**
   * Creates a rate limiter.
   *
   * @param rate maximum requests per second
   * @param burst maximum burst size (defaults to rate if &lt;= 0)
   * @param per granularity: "global", "service", "method", or "peer"
   */
  public RateLimitInterceptor(double rate, double burst, String per) {
    this.rate = rate;
    this.burst = burst > 0 ? burst : rate;
    this.per = per != null ? per : "global";
    this.globalBucket = new TokenBucket(rate, this.burst);
  }

  /** Creates a global rate limiter with the given rate and burst equal to rate. */
  public RateLimitInterceptor(double rate) {
    this(rate, rate, "global");
  }

  @Override
  public Object onRequest(CallContext ctx, Object request) {
    TokenBucket bucket = getBucket(ctx);
    if (!bucket.tryAcquire()) {
      throw new RpcError(
          StatusCode.RESOURCE_EXHAUSTED, "Rate limit exceeded (" + rate + "/s per " + per + ")");
    }
    return request;
  }

  private TokenBucket getBucket(CallContext ctx) {
    if ("global".equals(per)) {
      return globalBucket;
    }

    String key;
    switch (per) {
      case "service" -> key = ctx.service();
      case "method" -> key = ctx.service() + "." + ctx.method();
      case "peer" -> key = ctx.peer() != null ? ctx.peer() : "unknown";
      default -> {
        return globalBucket;
      }
    }

    return buckets.computeIfAbsent(key, k -> new TokenBucket(rate, burst));
  }

  /** Simple token bucket rate limiter (thread-safe). */
  private static final class TokenBucket {
    private final double rate;
    private final double capacity;
    private double tokens;
    private long lastRefillNanos;

    TokenBucket(double rate, double capacity) {
      this.rate = rate;
      this.capacity = capacity;
      this.tokens = capacity;
      this.lastRefillNanos = System.nanoTime();
    }

    synchronized boolean tryAcquire() {
      long now = System.nanoTime();
      double elapsed = (now - lastRefillNanos) / 1_000_000_000.0;
      tokens = Math.min(capacity, tokens + elapsed * rate);
      lastRefillNanos = now;

      if (tokens >= 1.0) {
        tokens -= 1.0;
        return true;
      }
      return false;
    }
  }
}
