package site.aster.otel;

import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.Callable;
import java.util.concurrent.Executor;
import org.slf4j.MDC;
import site.aster.ffi.Dispatch;

/**
 * Spans and trace context over the Rust OpenTelemetry pipeline (design §3.3, §4, §5.3).
 *
 * <p>The current trace context lives in a {@link ScopedValue} (the Java carrier of §5.3): it
 * follows virtual threads and structured-concurrency forks, and ends with the block that bound it.
 * Inside an RPC handler it holds the server span of the call and the inbound W3C baggage, so spans
 * you start nest under the call and outbound Aster calls continue the trace (a {@code traceparent}
 * and {@code baggage} are written into the call metadata for you).
 *
 * <p>Logging: {@link #withSpan} and the dispatcher put {@code trace_id} and {@code span_id} into
 * the SLF4J {@link MDC} for the block, so a log pattern with {@code %X{trace_id}} (or a JSON
 * encoder that dumps the MDC) correlates the application's own records with the Rust ones. Do not
 * put secrets in baggage: baggage values reach log lines.
 *
 * <pre>{@code
 * String user = Tracing.withSpan("db.query", Map.of("db.system", "postgresql"), () -> fetch());
 * }</pre>
 *
 * <p>Without a tracer provider ({@link Observability#init} with traces on, or {@code OTEL_*} asking
 * for it) spans are no-ops.
 */
public final class Tracing {

  private Tracing() {}

  /** The call-context carrier. Bound by the dispatcher and by {@link #withSpan}. */
  private static final ScopedValue<TraceContext> CARRIER = ScopedValue.newInstance();

  static final String MDC_TRACE_ID = "trace_id";
  static final String MDC_SPAN_ID = "span_id";

  /** The bound context, or {@code null} ({@code ScopedValue.orElse} rejects a null default). */
  private static TraceContext carrier() {
    return CARRIER.isBound() ? CARRIER.get() : null;
  }

  // ── reading the carrier ──────────────────────────────────────────────────

  /** The current trace context, or {@code null} outside any span or handler. */
  public static TraceContext current() {
    return carrier();
  }

  /** The current W3C {@code traceparent}, or {@code null}. */
  public static String currentTraceparent() {
    TraceContext c = carrier();
    return c == null || c.span() == null ? null : c.span().traceparent();
  }

  /** The current trace id (32 lowercase hex characters), or {@code null}. */
  public static String currentTraceId() {
    TraceContext c = carrier();
    return c == null || c.span() == null ? null : c.span().traceId();
  }

  /** The current span id (16 lowercase hex characters), or {@code null}. */
  public static String currentSpanId() {
    TraceContext c = carrier();
    return c == null || c.span() == null ? null : c.span().spanId();
  }

  /** The current W3C baggage (immutable; empty when there is none). */
  public static Map<String, String> currentBaggage() {
    TraceContext c = carrier();
    return c == null ? Map.of() : c.baggage();
  }

  // ── binding a context ────────────────────────────────────────────────────

  /**
   * Run {@code op} with {@code traceparent} (and optional {@code baggage}) as the current context.
   * Use it to continue a trace that arrived by other means (an HTTP header, a message queue).
   *
   * @throws IllegalArgumentException when {@code traceparent} is malformed
   */
  public static <T> T withTraceContext(
      String traceparent, Map<String, String> baggage, Callable<T> op) throws Exception {
    return callIn(new TraceContext(SpanContext.parse(traceparent), baggage), op);
  }

  /** As {@link #withTraceContext(String, Map, Callable)} for a {@link Runnable}. */
  public static void runWithTraceContext(
      String traceparent, Map<String, String> baggage, Runnable op) {
    TraceContext c = new TraceContext(SpanContext.parse(traceparent), baggage);
    runIn(c, op);
  }

  /**
   * Wrap {@code op} so it runs with the context current <em>now</em>, wherever it runs later.
   * {@link ScopedValue} bindings do not cross a plain thread pool; the client's call executor uses
   * this.
   */
  public static Runnable bind(Runnable op) {
    TraceContext c = carrier();
    return c == null ? op : () -> runIn(c, op);
  }

  /** {@code executor} that runs every task with the submitter's trace context. */
  public static Executor propagating(Executor executor) {
    return task -> executor.execute(bind(task));
  }

  /**
   * Make {@code ctx} the current context for {@code op} (the dispatcher publishes the call's server
   * span this way). {@code null} runs {@code op} unchanged.
   */
  public static void runIn(TraceContext ctx, Runnable op) {
    if (ctx == null) {
      op.run();
      return;
    }
    supplyIn(
        ctx,
        () -> {
          op.run();
          return null;
        });
  }

  /** {@code f} that runs with the context current <em>now</em>, wherever it is applied later. */
  public static <A, R> java.util.function.Function<A, R> bind(java.util.function.Function<A, R> f) {
    TraceContext c = carrier();
    return c == null ? f : a -> supplyIn(c, () -> f.apply(a));
  }

  private static <T> T supplyIn(TraceContext ctx, java.util.function.Supplier<T> op) {
    return ScopedValue.where(CARRIER, ctx).call(() -> withMdc(ctx, op));
  }

  private static <T> T withMdc(TraceContext ctx, java.util.function.Supplier<T> op) {
    SpanContext span = ctx.span();
    if (span == null) {
      return op.get();
    }
    String priorTrace = MDC.get(MDC_TRACE_ID);
    String priorSpan = MDC.get(MDC_SPAN_ID);
    MDC.put(MDC_TRACE_ID, span.traceId());
    MDC.put(MDC_SPAN_ID, span.spanId());
    try {
      return op.get();
    } finally {
      restore(MDC_TRACE_ID, priorTrace);
      restore(MDC_SPAN_ID, priorSpan);
    }
  }

  private static <T> T callIn(TraceContext ctx, Callable<T> op) throws Exception {
    if (ctx == null) {
      return op.call();
    }
    return ScopedValue.where(CARRIER, ctx).call(() -> mdcCall(ctx, op));
  }

  private static <T> T mdcCall(TraceContext ctx, Callable<T> op) throws Exception {
    SpanContext span = ctx.span();
    if (span == null) {
      return op.call();
    }
    String priorTrace = MDC.get(MDC_TRACE_ID);
    String priorSpan = MDC.get(MDC_SPAN_ID);
    MDC.put(MDC_TRACE_ID, span.traceId());
    MDC.put(MDC_SPAN_ID, span.spanId());
    try {
      return op.call();
    } finally {
      restore(MDC_TRACE_ID, priorTrace);
      restore(MDC_SPAN_ID, priorSpan);
    }
  }

  private static void restore(String key, String prior) {
    if (prior == null) {
      MDC.remove(key);
    } else {
      MDC.put(key, prior);
    }
  }

  // ── spans ────────────────────────────────────────────────────────────────

  /** Start describing a span. */
  public static SpanBuilder span(String name) {
    return new SpanBuilder(name);
  }

  /** Run {@code op} in a span named {@code name}, current for the block; ends it afterwards. */
  public static <T> T withSpan(String name, Callable<T> op) throws Exception {
    return span(name).call(op);
  }

  /** As {@link #withSpan(String, Callable)} with initial attributes (values are stringified). */
  public static <T> T withSpan(String name, Map<String, ?> attrs, Callable<T> op) throws Exception {
    return span(name).attributes(attrs).call(op);
  }

  /** Run {@code op} in a span named {@code name}. */
  public static void runInSpan(String name, Runnable op) {
    span(name).run(op);
  }

  /** Describes a span before it starts. */
  public static final class SpanBuilder {
    private final String name;
    private SpanKind kind = SpanKind.INTERNAL;
    private final Map<String, Object> attrs = new LinkedHashMap<>();
    private SpanContext parent;

    private SpanBuilder(String name) {
      this.name = name;
    }

    public SpanBuilder kind(SpanKind kind) {
      this.kind = kind;
      return this;
    }

    public SpanBuilder attribute(String key, Object value) {
      attrs.put(key, value);
      return this;
    }

    public SpanBuilder attributes(Map<String, ?> more) {
      attrs.putAll(more);
      return this;
    }

    /** Parent under {@code traceparent} instead of the current context. */
    public SpanBuilder parent(String traceparent) {
      this.parent = SpanContext.parse(traceparent);
      return this;
    }

    /** Parent under {@code context} instead of the current context. */
    public SpanBuilder parent(SpanContext context) {
      this.parent = context;
      return this;
    }

    /**
     * Start the span without making it current; the caller ends it ({@link Span#end}). Its parent
     * is the one given, else the current context's span.
     */
    public Span start() {
      SpanContext p = parent;
      if (p == null) {
        TraceContext c = carrier();
        p = c == null ? null : c.span();
      }
      return Span.start(name, kind, attrs, p);
    }

    /**
     * Run {@code op} inside the span, current for the block. An exception escaping {@code op} is
     * recorded on the span (status error). The span ends afterwards.
     */
    public <T> T call(Callable<T> op) throws Exception {
      Span s = start();
      TraceContext next = nextContext(s);
      try {
        return callIn(next, op);
      } catch (Throwable t) {
        s.recordException(t);
        throw t;
      } finally {
        s.end();
      }
    }

    /** As {@link #call} for a {@link Runnable}. */
    public void run(Runnable op) {
      Span s = start();
      TraceContext next = nextContext(s);
      try {
        runIn(next, op);
      } catch (Throwable t) {
        s.recordException(t);
        throw t;
      } finally {
        s.end();
      }
    }

    private static TraceContext nextContext(Span s) {
      TraceContext prior = carrier();
      if (s.context() == null) {
        return prior;
      }
      return new TraceContext(s.context(), prior == null ? Map.of() : prior.baggage());
    }
  }

  // ── propagation ──────────────────────────────────────────────────────────

  /**
   * {@code metadata} plus the current {@code traceparent} and {@code baggage}: the same object when
   * there is no current context or the caller set its own {@code traceparent}. Aster's client calls
   * this for every outbound call; Rust parents its client span on the {@code traceparent}.
   */
  public static Map<String, String> inject(Map<String, String> metadata) {
    TraceContext c = carrier();
    if (c == null || (metadata != null && metadata.containsKey("traceparent"))) {
      return metadata;
    }
    boolean hasBaggage =
        !c.baggage().isEmpty() && (metadata == null || !metadata.containsKey("baggage"));
    if (c.span() == null && !hasBaggage) {
      return metadata;
    }
    Map<String, String> out = new LinkedHashMap<>();
    if (metadata != null) {
      out.putAll(metadata);
    }
    if (c.span() != null) {
      out.put("traceparent", c.span().traceparent());
    }
    if (hasBaggage) {
      out.put("baggage", TraceContext.formatBaggage(c.baggage()));
    }
    return out;
  }

  // ── dispatch ─────────────────────────────────────────────────────────────

  /**
   * The carrier for a dispatched call: the server span (when exported) and the inbound baggage,
   * read from the dispatch trace getters; {@code null} when the call has neither. With no tracer
   * provider the {@code baggage} key passes through in {@code metadata} (design §5.1) and is parsed
   * from there.
   */
  public static TraceContext fromDispatch(
      Dispatch dispatch, long callId, int flags, Map<String, String> metadata) {
    SpanContext span = null;
    if ((flags & Dispatch.FLAG_HAS_TRACE) != 0) {
      span = dispatch.traceContext(callId);
    }
    String raw = null;
    if ((flags & Dispatch.FLAG_HAS_INBOUND_TRACE) != 0) {
      raw = dispatch.inboundTrace(callId).get("baggage");
    } else if (span == null && metadata != null) {
      raw = metadata.get("baggage");
      if (raw != null && !TraceContext.withinLimits(raw)) {
        raw = null;
      }
    }
    Map<String, String> baggage = TraceContext.parseBaggage(raw);
    if (span == null && baggage.isEmpty()) {
      return null;
    }
    return new TraceContext(span, baggage);
  }
}
