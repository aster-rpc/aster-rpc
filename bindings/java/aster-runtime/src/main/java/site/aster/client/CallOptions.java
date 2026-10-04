package site.aster.client;

import java.time.Duration;
import java.util.HashMap;
import java.util.Map;

/**
 * Per-call options for {@link AsterClient} / {@link ClientSession} calls. Immutable; each {@code
 * with*} returns a copy.
 *
 * <ul>
 *   <li>{@code timeout}: the call's deadline, relative to when it is made. It becomes {@code
 *       CallContext.deadline()} for the interceptor chain and is propagated to the server (the
 *       header's deadline, enforced natively by Rust) as the <em>remaining</em> time of each
 *       attempt. {@code DeadlineInterceptor} refuses to send an expired call.
 *   <li>{@code idempotent}: the call is safe to repeat; {@code RetryInterceptor} only retries
 *       idempotent calls (the client has no method descriptor to read this from).
 *   <li>{@code metadata}: request headers (interceptors may add more).
 *   <li>{@code cancellable}: a unary call runs on a dedicated native call ({@code aster_rpc_open})
 *       instead of the pooled single-round-trip fast path ({@code aster_rpc_unary}), so cancelling
 *       the returned future ({@link java.util.concurrent.CompletableFuture#cancel}) cancels the
 *       native call: {@code FLAG_CANCEL} plus a stream reset, which the server's handler sees
 *       ({@code CallContext.isCancelled()}). Streaming calls are always cancellable.
 * </ul>
 */
public final class CallOptions {

  /** No deadline, not idempotent, no metadata, pooled unary fast path. */
  public static final CallOptions DEFAULT = new CallOptions(null, false, Map.of(), false);

  private final Duration timeout;
  private final boolean idempotent;
  private final Map<String, String> metadata;
  private final boolean cancellable;

  private CallOptions(
      Duration timeout, boolean idempotent, Map<String, String> metadata, boolean cancellable) {
    this.timeout = timeout;
    this.idempotent = idempotent;
    this.metadata = metadata;
    this.cancellable = cancellable;
  }

  /** The call's deadline relative to when it is made, or {@code null} for none. */
  public Duration timeout() {
    return timeout;
  }

  public boolean idempotent() {
    return idempotent;
  }

  public Map<String, String> metadata() {
    return metadata;
  }

  public boolean cancellable() {
    return cancellable;
  }

  /** A copy with a deadline ({@code null} or non-positive: none). */
  public CallOptions withTimeout(Duration timeout) {
    Duration t = timeout == null || timeout.isZero() || timeout.isNegative() ? null : timeout;
    return new CallOptions(t, idempotent, metadata, cancellable);
  }

  public CallOptions withIdempotent(boolean idempotent) {
    return new CallOptions(timeout, idempotent, metadata, cancellable);
  }

  public CallOptions withMetadata(Map<String, String> metadata) {
    return new CallOptions(
        timeout, idempotent, metadata == null ? Map.of() : Map.copyOf(metadata), cancellable);
  }

  /** A copy with one more metadata entry. */
  public CallOptions withMetadata(String key, String value) {
    Map<String, String> m = new HashMap<>(metadata);
    m.put(key, value);
    return new CallOptions(timeout, idempotent, Map.copyOf(m), cancellable);
  }

  public CallOptions withCancellable(boolean cancellable) {
    return new CallOptions(timeout, idempotent, metadata, cancellable);
  }
}
