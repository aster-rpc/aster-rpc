package site.aster.otel;

import static site.aster.otel.OtelNative.I64;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.util.Map;
import site.aster.ffi.IrohStatus;

/**
 * A span in the Rust tracer (scope {@code aster.app}), exported with Aster's own spans under the
 * same resource and sampling decision. Without a tracer provider (see {@link Observability}) a span
 * is a cheap no-op: {@link #context()} is {@code null} and every call does nothing.
 *
 * <p>Start one with {@link Tracing#span(String)}. {@link Tracing.SpanBuilder#call} and {@link
 * Tracing.SpanBuilder#run} make the span current for the block (a {@link ScopedValue} binding, so
 * it follows virtual threads and structured-concurrency forks) and end it afterwards; {@link
 * Tracing.SpanBuilder#start} gives a span you end yourself and that is <em>not</em> current.
 */
public final class Span implements AutoCloseable {

  private volatile long handle;
  private final SpanContext context;

  private Span(long handle, SpanContext context) {
    this.handle = handle;
    this.context = context;
  }

  static Span start(String name, SpanKind kind, Map<String, ?> attrs, SpanContext parent) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment parentSeg = MemorySegment.NULL;
      if (parent != null) {
        parentSeg = arena.allocate(OtelNative.SPAN_CONTEXT_SIZE, 8);
        parent.toNative(parentSeg);
      }
      byte[] kv = OtelNative.kv(attrs);
      MemorySegment out = arena.allocate(8, 8);
      int status =
          (int)
              OtelNative.SPAN_START.invoke(
                  OtelNative.bytes(arena, name),
                  kind.code,
                  parentSeg,
                  OtelNative.copy(arena, kv),
                  kv.length,
                  out);
      if (status == IrohStatus.QUEUE_FULL.code) {
        return new Span(0, null); // live-span table full: degrade to a no-op
      }
      OtelNative.check(status, "aster_span_start");
      long h = out.get(I64, 0);
      MemorySegment ctx = arena.allocate(OtelNative.SPAN_CONTEXT_SIZE, 8);
      OtelNative.check((int) OtelNative.SPAN_CONTEXT.invoke(h, ctx), "aster_span_context");
      return new Span(h, SpanContext.fromNative(ctx));
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
  }

  /** The span's context, or {@code null} for a no-op span (no pipeline, or table full). */
  public SpanContext context() {
    return context;
  }

  /** The trace id (32 hex characters), or {@code null} for a no-op span. */
  public String traceId() {
    return context == null ? null : context.traceId();
  }

  /** The span id (16 hex characters), or {@code null} for a no-op span. */
  public String spanId() {
    return context == null ? null : context.spanId();
  }

  /** The span's {@code traceparent}, or {@code null} for a no-op span. */
  public String traceparent() {
    return context == null ? null : context.traceparent();
  }

  /** Add one attribute (values are stringified; keep cardinality bounded). */
  public Span setAttribute(String key, Object value) {
    return setAttributes(Map.of(key, value));
  }

  /** Add attributes. */
  public Span setAttributes(Map<String, ?> attrs) {
    long h = handle;
    if (h == 0 || attrs.isEmpty()) {
      return this;
    }
    try (Arena arena = Arena.ofConfined()) {
      byte[] kv = OtelNative.kv(attrs);
      done(
          (int) OtelNative.SPAN_SET_ATTRIBUTES.invoke(h, OtelNative.copy(arena, kv), kv.length),
          "aster_span_set_attributes");
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
    return this;
  }

  /** Add an event, timestamped now. */
  public Span addEvent(String name) {
    return addEvent(name, Map.of());
  }

  /** Add an event with attributes, timestamped now. */
  public Span addEvent(String name, Map<String, ?> attrs) {
    long h = handle;
    if (h == 0) {
      return this;
    }
    try (Arena arena = Arena.ofConfined()) {
      byte[] kv = OtelNative.kv(attrs);
      done(
          (int)
              OtelNative.SPAN_ADD_EVENT.invoke(
                  h, OtelNative.bytes(arena, name), OtelNative.copy(arena, kv), kv.length),
          "aster_span_add_event");
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
    return this;
  }

  /** Set the status: OK, or error with {@code message}. */
  public Span setStatus(boolean ok, String message) {
    long h = handle;
    if (h == 0) {
      return this;
    }
    try (Arena arena = Arena.ofConfined()) {
      done(
          (int)
              OtelNative.SPAN_SET_STATUS.invoke(
                  h, ok ? 1 : 2, OtelNative.bytes(arena, message == null ? "" : message)),
          "aster_span_set_status");
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
    return this;
  }

  /** Record {@code error} as an {@code exception} event and set the status to error. */
  public Span recordException(Throwable error) {
    long h = handle;
    if (h == 0) {
      return this;
    }
    try (Arena arena = Arena.ofConfined()) {
      String message = error.getMessage() == null ? "" : error.getMessage();
      done(
          (int)
              OtelNative.SPAN_RECORD_ERROR.invoke(
                  h,
                  OtelNative.bytes(arena, error.getClass().getName()),
                  OtelNative.bytes(arena, message)),
          "aster_span_record_error");
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
    return this;
  }

  /** End the span (idempotent). */
  public void end() {
    long h = handle;
    handle = 0;
    if (h == 0) {
      return;
    }
    try {
      int status = (int) OtelNative.SPAN_END.invoke(h);
      if (status != IrohStatus.NOT_FOUND.code) {
        OtelNative.check(status, "aster_span_end");
      }
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
  }

  @Override
  public void close() {
    end();
  }

  /** A shutdown frees every span handle: {@code NOT_FOUND} means "already ended". */
  private void done(int status, String op) {
    if (status == IrohStatus.NOT_FOUND.code) {
      handle = 0;
      return;
    }
    OtelNative.check(status, op);
  }
}
