package site.aster.interceptors;

import java.net.InetSocketAddress;
import java.time.Duration;
import java.time.Instant;
import java.util.HashMap;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.function.BooleanSupplier;
import java.util.function.Supplier;

/**
 * Context for a single RPC call, available to interceptors and handlers.
 *
 * <p>Passed to every interceptor in the chain. Read {@code service} and {@code method} to know
 * which RPC is being called. Use {@code metadata} to pass headers between client and server. Check
 * {@link #remainingSeconds()} for deadline awareness.
 */
public final class CallContext {

  // The current-context holder (Java 25 ScopedValue, ffi_spec/handler-context-design.md). The
  // server binds it around each handler invocation (every pattern) so deeply-nested handler code
  // can reach the context without threading it through every call. A ScopedValue is not
  // inherited by threads the handler starts itself; pass the context explicitly there.
  private static final ScopedValue<CallContext> CURRENT = ScopedValue.newInstance();

  /**
   * Return the {@link CallContext} for the currently-executing dispatch.
   *
   * @throws IllegalStateException if called outside of a dispatcher-managed scope
   */
  public static CallContext current() {
    if (!CURRENT.isBound()) {
      throw new IllegalStateException("CallContext.current() called outside of a dispatch scope");
    }
    return CURRENT.get();
  }

  /** The current context, or {@code null} outside a dispatch scope. */
  public static CallContext currentOrNull() {
    return CURRENT.isBound() ? CURRENT.get() : null;
  }

  /**
   * Publish {@code ctx} as the current call context for the duration of {@code action}. The server
   * does this for every handler; generated dispatchers do it too (nesting restores the outer one).
   */
  public static <T> T runWith(CallContext ctx, Callable<T> action) throws Exception {
    return ScopedValue.where(CURRENT, ctx).call(action::call);
  }

  /** {@link #runWith(CallContext, Callable)} for an action without a result. */
  public static void runScoped(CallContext ctx, Runnable action) {
    ScopedValue.where(CURRENT, ctx).run(action);
  }

  private final String service;
  private final String method;
  private final String callId;
  private final String sessionId;
  private final String peer;
  private final String principal;
  private final int wireCallId;
  private final long dispatchCallId;
  private final boolean irohAuthenticated;
  private final Map<String, String> metadata;
  private final Map<String, String> attributes;
  private final Double deadline;
  private final boolean streaming;
  private final String pattern;
  private final boolean idempotent;
  private int attempt;
  private final InetSocketAddress peerAddr;
  private final String relayUrl;
  private final Duration rtt;
  private final BooleanSupplier cancellation;
  private final Supplier<CompletableFuture<Void>> cancelledSignal;
  private final String actor;
  private final site.aster.identity.ForwardedIdentity onBehalfOf;
  private final Supplier<site.aster.identity.NodeProperties> nodeProperties;

  private CallContext(Builder builder) {
    this.service = builder.service;
    this.method = builder.method;
    this.callId = builder.callId != null ? builder.callId : UUID.randomUUID().toString();
    this.sessionId = builder.sessionId;
    this.peer = builder.peer;
    this.principal = builder.principal != null ? builder.principal : builder.peer;
    this.wireCallId = builder.wireCallId;
    this.dispatchCallId = builder.dispatchCallId;
    this.irohAuthenticated = builder.irohAuthenticated;
    this.metadata = builder.metadata;
    this.attributes = builder.attributes;
    this.deadline = builder.deadline;
    this.streaming = builder.streaming;
    this.pattern = builder.pattern;
    this.idempotent = builder.idempotent;
    this.attempt = builder.attempt;
    this.peerAddr = builder.peerAddr;
    this.relayUrl = builder.relayUrl;
    this.rtt = builder.rtt;
    this.cancellation = builder.cancellation;
    this.cancelledSignal = builder.cancelledSignal;
    this.actor = builder.actor;
    this.onBehalfOf = builder.onBehalfOf;
    this.nodeProperties = builder.nodeProperties;
  }

  public String service() {
    return service;
  }

  public String method() {
    return method;
  }

  /**
   * The call id. On the server it is the caller's {@code StreamHeader.call_id} (as {@link
   * #wireCallId()}, {@code "0"} when unset), matching Rust's {@code CallContext::call_id}.
   */
  public String callId() {
    return callId;
  }

  /** The caller's {@code StreamHeader.call_id} ({@code 0} when unset or not a server call). */
  public int wireCallId() {
    return wireCallId;
  }

  /**
   * Rust's dispatch-table id for this call ({@code aster_dispatch_event_t.call_id}, unique per
   * server, from 1); {@code 0} when the context is not backed by a live server call.
   */
  public long dispatchCallId() {
    return dispatchCallId;
  }

  public String sessionId() {
    return sessionId;
  }

  /**
   * The transport's caller id: the peer's endpoint id (64 hex) over Iroh; over HTTP / WebTransport
   * the id the transport resolved (the remote address). See {@link #irohAuthenticated()} for how
   * far to trust it, and {@link #principal()} for the verified caller.
   */
  public String peer() {
    return peer;
  }

  /**
   * The verified caller: the {@code Authenticator}'s principal when it resolved one (HTTP auth
   * callbacks), else {@link #peer()} (ABI 7.5).
   */
  public String principal() {
    return principal;
  }

  /**
   * The gateway that forwarded the call on behalf of a user (its node id; Rust {@code
   * CallContext::actor}), or {@code null} for a call the peer made as itself. With an actor, {@link
   * #principal()} is the user ({@code iss|sub}) and {@link #attributes()} carry the token's roles.
   */
  public String actor() {
    return actor;
  }

  /**
   * The verified user of a call a gateway forwarded ({@code iss}, {@code sub}, {@code jti}, expiry,
   * {@code amr}; Rust {@code CallContext::on_behalf_of}), or {@code null}.
   */
  public site.aster.identity.ForwardedIdentity onBehalfOf() {
    return onBehalfOf;
  }

  /**
   * How the user signed in (the token's {@code amr}, e.g. {@link
   * site.aster.identity.GatewayIdentity#AMR_WEBAUTHN}); empty for a call the peer made as itself
   * (Rust {@code CallContext::auth_methods}).
   */
  public java.util.List<String> authMethods() {
    return onBehalfOf == null ? java.util.List.of() : onBehalfOf.authMethods();
  }

  /**
   * Refuse the call with {@code PERMISSION_DENIED} unless it was made on behalf of a user who
   * signed in with {@code method} (one of {@link #authMethods()}); a call the peer made as itself
   * is refused too (Rust {@code CallContext::require_auth_method}). {@code @Requires} checks roles
   * only: use this in handlers for methods that need a stronger sign-in.
   *
   * @throws RpcError {@code PERMISSION_DENIED}
   */
  public void requireAuthMethod(String method) {
    if (!authMethods().contains(method)) {
      throw new RpcError(
          StatusCode.PERMISSION_DENIED, "this method needs a sign-in with " + method);
    }
  }

  /**
   * The serving node's {@code aster.ops.NodeConfig} properties now (name, tags, typed attributes;
   * Rust {@code CallContext::node_properties}): {@link site.aster.identity.NodeProperties#EMPTY}
   * when NodeConfig is off or the context is not backed by a server. Re-read only when the
   * generation changes. Never use them to authorise.
   */
  public site.aster.identity.NodeProperties nodeProperties() {
    if (nodeProperties == null) {
      return site.aster.identity.NodeProperties.EMPTY;
    }
    site.aster.identity.NodeProperties p = nodeProperties.get();
    return p == null ? site.aster.identity.NodeProperties.EMPTY : p;
  }

  /**
   * One NodeConfig attribute of the serving node, or {@code null} (Rust {@code node_attribute}).
   */
  public site.aster.identity.NodeAttribute nodeAttribute(String key) {
    return nodeProperties().attribute(key);
  }

  /** True only for native Iroh endpoint authentication, never for an HTTP auth callback. */
  public boolean irohAuthenticated() {
    return irohAuthenticated;
  }

  public Map<String, String> metadata() {
    return metadata;
  }

  /** One metadata value, or {@code null}. */
  public String metadataValue(String key) {
    return metadata.get(key);
  }

  /** The caller's attributes: the merged set Gate 3 checked (e.g. {@code aster.role}). */
  public Map<String, String> attributes() {
    return attributes;
  }

  /** One attribute, or {@code null}. */
  public String attribute(String key) {
    return attributes.get(key);
  }

  /** Returns the absolute deadline as epoch seconds, or {@code null} if no deadline is set. */
  public Double deadline() {
    return deadline;
  }

  /**
   * When the server cancels the call with {@code DEADLINE_EXCEEDED} (the minimum of the caller's
   * deadline, the method timeout and the server's handler timeout); {@code null} = none.
   */
  public Instant deadlineInstant() {
    return deadline == null ? null : Instant.ofEpochMilli(Math.round(deadline * 1000.0));
  }

  /** Time left until {@link #deadlineInstant()} (zero once passed); {@code null} = no deadline. */
  public Duration remaining() {
    Instant at = deadlineInstant();
    if (at == null) {
      return null;
    }
    Duration left = Duration.between(Instant.now(), at);
    return left.isNegative() ? Duration.ZERO : left;
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
   * Peer's UDP socket address when the selected QUIC path is direct. Returns {@code null} when the
   * call is being relayed; in that case the peer's IP is not visible (use {@link #relayUrl()}
   * instead).
   */
  public InetSocketAddress peerAddr() {
    return peerAddr;
  }

  /**
   * Relay server URL when the selected QUIC path goes through a relay. Returns {@code null} for
   * direct paths.
   */
  public String relayUrl() {
    return relayUrl;
  }

  /** Round-trip-time for the selected QUIC path, or {@code null} if not yet measured. */
  public Duration rtt() {
    return rtt;
  }

  /**
   * Whether the call is over for the caller: the peer cancelled it, its deadline passed, or the
   * server is stopping. A long-running handler (a unary one has no stream to ask) polls this, or
   * checks it between work units, to stop producing a result nobody will read. Always {@code false}
   * for contexts that are not backed by a live call (client-side, tests).
   */
  public boolean isCancelled() {
    return cancellation != null && cancellation.getAsBoolean();
  }

  /**
   * Completes once {@link #isCancelled()} becomes true (peer cancel, deadline, connection gone).
   * Never completes for a call that finishes normally, or for a context not backed by a live call.
   * Each call returns a fresh view: cancelling it does not cancel the RPC.
   */
  public CompletableFuture<Void> cancelled() {
    if (cancelledSignal == null) {
      return new CompletableFuture<>();
    }
    return cancelledSignal.get().copy();
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
    private String principal;
    private int wireCallId;
    private long dispatchCallId;
    private boolean irohAuthenticated;
    private Map<String, String> metadata = new HashMap<>();
    private Map<String, String> attributes = new HashMap<>();
    private Double deadline;
    private boolean streaming;
    private String pattern;
    private boolean idempotent;
    private int attempt = 1;
    private InetSocketAddress peerAddr;
    private String relayUrl;
    private Duration rtt;
    private BooleanSupplier cancellation;
    private Supplier<CompletableFuture<Void>> cancelledSignal;
    private String actor;
    private site.aster.identity.ForwardedIdentity onBehalfOf;
    private Supplier<site.aster.identity.NodeProperties> nodeProperties;

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

    /** The verified principal; defaults to {@link #peer(String)}. */
    public Builder principal(String principal) {
      this.principal = principal;
      return this;
    }

    public Builder wireCallId(int wireCallId) {
      this.wireCallId = wireCallId;
      return this;
    }

    public Builder dispatchCallId(long dispatchCallId) {
      this.dispatchCallId = dispatchCallId;
      return this;
    }

    public Builder irohAuthenticated(boolean value) {
      this.irohAuthenticated = value;
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

    public Builder peerAddr(InetSocketAddress peerAddr) {
      this.peerAddr = peerAddr;
      return this;
    }

    public Builder relayUrl(String relayUrl) {
      this.relayUrl = relayUrl;
      return this;
    }

    public Builder rtt(Duration rtt) {
      this.rtt = rtt;
      return this;
    }

    /** Liveness probe behind {@link CallContext#isCancelled()}. */
    public Builder cancellation(BooleanSupplier cancellation) {
      this.cancellation = cancellation;
      return this;
    }

    /** The signal behind {@link CallContext#cancelled()}; called lazily, once per access. */
    public Builder cancelledSignal(Supplier<CompletableFuture<Void>> signal) {
      this.cancelledSignal = signal;
      return this;
    }

    /** The forwarding gateway ({@link CallContext#actor()}). */
    public Builder actor(String actor) {
      this.actor = actor;
      return this;
    }

    /** The forwarded user ({@link CallContext#onBehalfOf()}). */
    public Builder onBehalfOf(site.aster.identity.ForwardedIdentity identity) {
      this.onBehalfOf = identity;
      return this;
    }

    /** The source of {@link CallContext#nodeProperties()}; called on each access. */
    public Builder nodeProperties(Supplier<site.aster.identity.NodeProperties> properties) {
      this.nodeProperties = properties;
      return this;
    }

    public CallContext build() {
      return new CallContext(this);
    }
  }
}
