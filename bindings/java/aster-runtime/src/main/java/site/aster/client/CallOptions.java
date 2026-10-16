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
 * </ul>
 *
 * <p>Every call is cancellable: cancelling the returned future ({@link
 * java.util.concurrent.CompletableFuture#cancel}) cancels the native call ({@code FLAG_CANCEL} plus
 * a stream reset), which the server's handler sees. Unary calls stay on the pooled
 * single-round-trip fast path while doing so (a cancel token, ABI 4.5).
 */
public final class CallOptions {

  /** No deadline, not idempotent, no metadata. */
  public static final CallOptions DEFAULT = new CallOptions(null, false, Map.of());

  private final Duration timeout;
  private final boolean idempotent;
  private final Map<String, String> metadata;

  private CallOptions(Duration timeout, boolean idempotent, Map<String, String> metadata) {
    this.timeout = timeout;
    this.idempotent = idempotent;
    this.metadata = metadata;
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

  /** A copy with a deadline ({@code null} or non-positive: none). */
  public CallOptions withTimeout(Duration timeout) {
    Duration t = timeout == null || timeout.isZero() || timeout.isNegative() ? null : timeout;
    return new CallOptions(t, idempotent, metadata);
  }

  public CallOptions withIdempotent(boolean idempotent) {
    return new CallOptions(timeout, idempotent, metadata);
  }

  public CallOptions withMetadata(Map<String, String> metadata) {
    return new CallOptions(timeout, idempotent, metadata == null ? Map.of() : Map.copyOf(metadata));
  }

  /**
   * A copy making the call on behalf of a user (Rust {@code CallOptions::on_behalf_of}): {@code
   * jwt} (a provider's token forwarded unchanged, or one from {@code AsterClient.mintUserToken})
   * goes in the {@link site.aster.identity.GatewayIdentity#ON_BEHALF_OF_METADATA} entry, replacing
   * an earlier one. The server must accept it ({@code [identity] accept_on_behalf_of}) and this
   * node must hold {@code aster.gateway}. A client running {@code [identity.publisher]} notices the
   * token's key id, so a provider's key rotation is published at once.
   */
  public CallOptions withOnBehalfOf(String jwt) {
    Map<String, String> m = new HashMap<>();
    for (Map.Entry<String, String> e : metadata.entrySet()) {
      if (!e.getKey().equalsIgnoreCase(site.aster.identity.GatewayIdentity.ON_BEHALF_OF_METADATA)) {
        m.put(e.getKey(), e.getValue());
      }
    }
    m.put(site.aster.identity.GatewayIdentity.ON_BEHALF_OF_METADATA, jwt);
    return new CallOptions(timeout, idempotent, Map.copyOf(m));
  }

  /** The token set by {@link #withOnBehalfOf}, or {@code null}. */
  public String onBehalfOf() {
    for (Map.Entry<String, String> e : metadata.entrySet()) {
      if (e.getKey().equalsIgnoreCase(site.aster.identity.GatewayIdentity.ON_BEHALF_OF_METADATA)) {
        return e.getValue();
      }
    }
    return null;
  }

  /** A copy with one more metadata entry. */
  public CallOptions withMetadata(String key, String value) {
    Map<String, String> m = new HashMap<>(metadata);
    m.put(key, value);
    return new CallOptions(timeout, idempotent, Map.copyOf(m));
  }
}
