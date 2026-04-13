package site.aster.interceptors;

import java.util.HashMap;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.Callable;

/**
 * Context for a single RPC call, available to interceptors and handlers.
 *
 * <p>Passed to every interceptor in the chain. Read {@code service} and {@code method} to know
 * which RPC is being called. Use {@code metadata} to pass headers between client and server. Check
 * {@link #remainingSeconds()} for deadline awareness.
 */
public final class CallContext {

  // Async-local current-context holder. Published by the dispatcher via runWith(...) for the
  // duration of a handler invocation so deeply-nested handler code can access the context
  // without threading it through every call. Falls back to a ThreadLocal until Java's
  // ScopedValue graduates out of preview.
  private static final ThreadLocal<CallContext> CURRENT = new ThreadLocal<>();

  /**
   * Return the {@link CallContext} for the currently-executing dispatch.
   *
   * @throws IllegalStateException if called outside of a dispatcher-managed scope
   */
  public static CallContext current() {
    CallContext ctx = CURRENT.get();
    if (ctx == null) {
      throw new IllegalStateException("CallContext.current() called outside of a dispatch scope");
    }
    return ctx;
  }

  /**
   * Publish {@code ctx} as the current call context for the duration of {@code action}. Used by
   * generated dispatchers to make {@link #current()} accessible from user handlers.
   */
  public static <T> T runWith(CallContext ctx, Callable<T> action) throws Exception {
    CallContext prior = CURRENT.get();
    CURRENT.set(ctx);
    try {
      return action.call();
    } finally {
      if (prior == null) {
        CURRENT.remove();
      } else {
        CURRENT.set(prior);
      }
    }
  }

  private final String service;
  private final String method;
  private final String callId;
  private final String sessionId;
  private final String peer;
  private final Map<String, String> metadata;
  private final Map<String, String> attributes;
  private final Double deadline;
  private final boolean streaming;
  private final String pattern;
  private final boolean idempotent;
  private int attempt;

  private CallContext(Builder builder) {
    this.service = builder.service;
    this.method = builder.method;
    this.callId = builder.callId != null ? builder.callId : UUID.randomUUID().toString();
    this.sessionId = builder.sessionId;
    this.peer = builder.peer;
    this.metadata = builder.metadata;
    this.attributes = builder.attributes;
    this.deadline = builder.deadline;
    this.streaming = builder.streaming;
    this.pattern = builder.pattern;
    this.idempotent = builder.idempotent;
    this.attempt = builder.attempt;
  }

  public String service() {
    return service;
  }

  public String method() {
    return method;
  }

  public String callId() {
    return callId;
  }

  public String sessionId() {
    return sessionId;
  }

  public String peer() {
    return peer;
  }

  public Map<String, String> metadata() {
    return metadata;
  }

  public Map<String, String> attributes() {
    return attributes;
  }

  /** Returns the absolute deadline as epoch seconds, or {@code null} if no deadline is set. */
  public Double deadline() {
    return deadline;
  }

  public boolean isStreaming() {
    return streaming;
  }

  public String pattern() {
    return pattern;
  }

  public boolean isIdempotent() {
    return idempotent;
  }

  public int attempt() {
    return attempt;
  }

  public void setAttempt(int attempt) {
    this.attempt = attempt;
  }

  /**
   * Returns the number of seconds remaining until the deadline, or {@code null} if no deadline is
   * set.
   */
  public Double remainingSeconds() {
    if (deadline == null) {
      return null;
    }
    double remaining = deadline - (System.currentTimeMillis() / 1000.0);
    return Math.max(0.0, remaining);
  }

  /** Returns {@code true} if the deadline has passed. */
  public boolean isExpired() {
    Double remaining = remainingSeconds();
    return remaining != null && remaining <= 0.0;
  }

  public static Builder builder(String service, String method) {
    return new Builder(service, method);
  }

  public static final class Builder {
    private final String service;
    private final String method;
    private String callId;
    private String sessionId;
    private String peer;
    private Map<String, String> metadata = new HashMap<>();
    private Map<String, String> attributes = new HashMap<>();
    private Double deadline;
    private boolean streaming;
    private String pattern;
    private boolean idempotent;
    private int attempt = 1;

    private Builder(String service, String method) {
      this.service = service;
      this.method = method;
    }

    public Builder callId(String callId) {
      this.callId = callId;
      return this;
    }

    public Builder sessionId(String sessionId) {
      this.sessionId = sessionId;
      return this;
    }

    public Builder peer(String peer) {
      this.peer = peer;
      return this;
    }

    public Builder metadata(Map<String, String> metadata) {
      this.metadata = new HashMap<>(metadata);
      return this;
    }

    public Builder attributes(Map<String, String> attributes) {
      this.attributes = new HashMap<>(attributes);
      return this;
    }

    public Builder deadline(Double deadline) {
      this.deadline = deadline;
      return this;
    }

    public Builder deadlineFromRelativeSecs(int deadlineSecs) {
      if (deadlineSecs <= 0) {
        this.deadline = null;
      } else {
        this.deadline = (System.currentTimeMillis() / 1000.0) + deadlineSecs;
      }
      return this;
    }

    public Builder streaming(boolean streaming) {
      this.streaming = streaming;
      return this;
    }

    public Builder pattern(String pattern) {
      this.pattern = pattern;
      return this;
    }

    public Builder idempotent(boolean idempotent) {
      this.idempotent = idempotent;
      return this;
    }

    public Builder attempt(int attempt) {
      this.attempt = attempt;
      return this;
    }

    public CallContext build() {
      return new CallContext(this);
    }
  }
}
