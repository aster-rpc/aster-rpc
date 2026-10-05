package site.aster.otel;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.util.LinkedHashMap;
import java.util.Map;
import org.slf4j.event.KeyValuePair;
import org.slf4j.event.Level;
import org.slf4j.event.LoggingEvent;
import site.aster.ffi.IrohStatus;

/**
 * Path B of the observability design (§3.4): write the application's own log records into the Rust
 * OpenTelemetry logs SDK ({@code aster_log_emit}), so they are exported over OTLP next to the Rust
 * records. Opt-in: nothing calls this unless you do (directly, or from your logging backend's
 * appender via {@link #emit(LoggingEvent)}).
 *
 * <p>A no-op, returning normally, when no OTLP logs exporter is running (enable it with {@code
 * ObservabilityConfig.otlpLogs(true)} or {@code OTEL_LOGS_EXPORTER}). It never blocks, and the Rust
 * side emits no event for it, so a record emitted here can never come back through the {@link
 * LogBridge} poll. Records that did come from the bridge (key/value {@code aster_origin} = {@code
 * rust}) are skipped by {@link #emit(LoggingEvent)} to keep an appender from re-exporting them.
 */
public final class LogEmit {

  private LogEmit() {}

  /** {@code sizeof(aster_log_emit_t)} (72 bytes, LP64). */
  static final long SIZE = 72;

  // aster_log_emit_t field offsets.
  private static final long OFF_SIZE = 0;
  private static final long OFF_LEVEL = 4;
  private static final long OFF_TIMESTAMP = 8;
  private static final long OFF_LOGGER = 16;
  private static final long OFF_MESSAGE = 32;
  private static final long OFF_FIELDS = 48;
  private static final long OFF_CONTEXT = 64;

  /** Emit with the current trace context (if any), timestamp now and no fields. */
  public static void emit(Level level, String logger, String message) {
    emit(level, logger, message, Map.of(), 0L, span());
  }

  /** Emit with the current trace context (if any) and timestamp now. */
  public static void emit(Level level, String logger, String message, Map<String, ?> fields) {
    emit(level, logger, message, fields, 0L, span());
  }

  private static SpanContext span() {
    TraceContext c = Tracing.current();
    return c == null ? null : c.span();
  }

  /**
   * Emit one record.
   *
   * @param timestampUnixNs wall-clock nanoseconds since the epoch; 0 = now
   * @param context the trace context to attach, or {@code null} for none
   */
  public static void emit(
      Level level,
      String logger,
      String message,
      Map<String, ?> fields,
      long timestampUnixNs,
      SpanContext context) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment rec = arena.allocate(SIZE, 8);
      rec.set(OtelNative.I32, OFF_SIZE, (int) SIZE);
      rec.set(OtelNative.U8, OFF_LEVEL, (byte) code(level));
      rec.set(OtelNative.I64, OFF_TIMESTAMP, timestampUnixNs);
      put(rec, OFF_LOGGER, OtelNative.bytes(arena, logger == null ? "" : logger));
      put(rec, OFF_MESSAGE, OtelNative.bytes(arena, message == null ? "" : message));
      put(rec, OFF_FIELDS, OtelNative.bytes(arena, OtelNative.kv(fields)));
      if (context != null) {
        MemorySegment cx = arena.allocate(OtelNative.SPAN_CONTEXT_SIZE, 8);
        context.toNative(cx);
        rec.set(OtelNative.PTR, OFF_CONTEXT, cx);
      } else {
        rec.set(OtelNative.PTR, OFF_CONTEXT, MemorySegment.NULL);
      }
      int status = (int) OtelNative.LOG_EMIT.invoke(rec);
      if (status != IrohStatus.OK.code) {
        throw OtelNative.error(status, "aster_log_emit");
      }
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
  }

  /**
   * Emit an SLF4J 2 event, for use from a logging backend's appender. The event's key/values become
   * fields, its timestamp is kept and its trace context is the current one. Events marked {@code
   * aster_origin=rust} (delivered by the Rust log bridge) are skipped. Returns whether the event
   * was handed to Rust.
   */
  public static boolean emit(LoggingEvent event) {
    Map<String, Object> fields = new LinkedHashMap<>();
    if (event.getKeyValuePairs() != null) {
      for (KeyValuePair kv : event.getKeyValuePairs()) {
        if ("aster_origin".equals(kv.key) && "rust".equals(String.valueOf(kv.value))) {
          return false;
        }
        fields.put(kv.key, kv.value);
      }
    }
    String message =
        event.getArgumentArray() == null || event.getArgumentArray().length == 0
            ? event.getMessage()
            : org.slf4j.helpers.MessageFormatter.arrayFormat(
                    event.getMessage(), event.getArgumentArray())
                .getMessage();
    emit(
        event.getLevel(),
        event.getLoggerName(),
        message,
        fields,
        event.getTimeStamp() * 1_000_000L,
        span());
    return true;
  }

  private static void put(MemorySegment rec, long off, MemorySegment bytes) {
    rec.set(OtelNative.PTR, off, bytes.get(OtelNative.PTR, 0));
    rec.set(OtelNative.I64, off + 8, bytes.get(OtelNative.I64, 8));
  }

  /** SLF4J level to {@code aster_log_level_t} (1 error .. 5 trace). */
  static int code(Level level) {
    return switch (level) {
      case ERROR -> 1;
      case WARN -> 2;
      case INFO -> 3;
      case DEBUG -> 4;
      case TRACE -> 5;
    };
  }
}
