package site.aster.otel;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.Map;
import site.aster.ffi.DataPlane;
import site.aster.ffi.Dispatch;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;

/**
 * FFM handles for the C ABI 4.6 observability functions ({@code aster_observability_*}, {@code
 * aster_log_*}, {@code aster_instrument_*}, {@code aster_attrset_*}, {@code aster_metric_*}, {@code
 * aster_span_*}, {@code aster_trace_context_*}).
 *
 * <p>No downcall here is {@code critical} (design §4.2): a metric record can take the SDK's write
 * lock when it creates a series, and a critical downcall must never block. Heap segments are then
 * not allowed, so every pointer argument is copied into a confined arena first. Switching the
 * pre-built-attribute-set record path to {@code critical(true)} is gated on a JMH run (see {@code
 * MetricsBenchmark}).
 */
final class OtelNative {

  private OtelNative() {}

  static final ValueLayout.OfInt I32 = ValueLayout.JAVA_INT;
  static final ValueLayout.OfLong I64 = ValueLayout.JAVA_LONG;
  static final ValueLayout.OfDouble F64 = ValueLayout.JAVA_DOUBLE;
  static final ValueLayout.OfByte U8 = ValueLayout.JAVA_BYTE;
  static final java.lang.foreign.AddressLayout PTR = ValueLayout.ADDRESS;

  /** {@code aster_bytes_t}, passed by value. */
  static final MemoryLayout BYTES = IrohLibrary.ASTER_BYTES;

  /** {@code sizeof(aster_observability_config_t)} (ffi/src/layout.rs). */
  static final long CONFIG_SIZE = 128;

  /** {@code sizeof(aster_log_record_t)}, the {@code aster_log_poll} array stride. */
  static final long LOG_RECORD_SIZE = 96;

  /** {@code sizeof(aster_span_context_t)}. */
  static final long SPAN_CONTEXT_SIZE = 32;

  /** {@code sizeof(aster_instrument_desc_t)} (96 since the boundaries follow-up). */
  static final long INSTRUMENT_DESC_SIZE = 96;

  static final int FLAG_FROM_ENV = 1;
  static final int FLAG_TRACES = 2;
  static final int FLAG_METRICS = 4;
  static final int FLAG_OTLP_LOGS = 8;
  static final int FLAG_LOG_BRIDGE = 16;

  /** {@code ASTER_BINDING_JAVA}. */
  static final int BINDING_JAVA = 4;

  private static MethodHandle h(String name, FunctionDescriptor d) {
    return IrohLibrary.getInstance().getHandle(name, d);
  }

  static final MethodHandle OBSERVABILITY_INIT =
      h("aster_observability_init", FunctionDescriptor.of(I32, PTR));
  static final MethodHandle OBSERVABILITY_FLUSH =
      h("aster_observability_flush", FunctionDescriptor.of(I32, I32));
  static final MethodHandle OBSERVABILITY_SHUTDOWN =
      h("aster_observability_shutdown", FunctionDescriptor.of(I32, I32));
  static final MethodHandle LOG_POLL =
      h("aster_log_poll", FunctionDescriptor.of(I32, PTR, I32, I32, I32, PTR));
  static final MethodHandle LOG_CLOSE = h("aster_log_close", FunctionDescriptor.of(I32));
  static final MethodHandle LOG_DROPPED = h("aster_log_dropped", FunctionDescriptor.of(I32, PTR));
  static final MethodHandle LOG_SET_FILTER =
      h("aster_log_set_filter", FunctionDescriptor.of(I32, PTR, I32));

  static final MethodHandle INSTRUMENT_NEW =
      h("aster_instrument_new", FunctionDescriptor.of(I32, PTR, PTR));
  static final MethodHandle INSTRUMENT_FREE =
      h("aster_instrument_free", FunctionDescriptor.of(I32, I64));
  static final MethodHandle ATTRSET_NEW =
      h("aster_attrset_new", FunctionDescriptor.of(I32, PTR, I32, PTR));
  static final MethodHandle ATTRSET_FREE = h("aster_attrset_free", FunctionDescriptor.of(I32, I64));
  static final MethodHandle METRIC_ADD_U64 =
      h("aster_metric_add_u64", FunctionDescriptor.of(I32, I64, I64, I64, PTR, I32, I64));
  static final MethodHandle METRIC_ADD_I64 =
      h("aster_metric_add_i64", FunctionDescriptor.of(I32, I64, I64, I64, PTR, I32, I64));
  static final MethodHandle METRIC_RECORD_F64 =
      h("aster_metric_record_f64", FunctionDescriptor.of(I32, I64, F64, I64, PTR, I32, I64));
  static final MethodHandle METRICS_PROMETHEUS =
      h("aster_metrics_prometheus", FunctionDescriptor.of(I32, PTR));

  static final MethodHandle SPAN_START =
      h("aster_span_start", FunctionDescriptor.of(I32, BYTES, I32, PTR, PTR, I32, PTR));
  static final MethodHandle SPAN_SET_ATTRIBUTES =
      h("aster_span_set_attributes", FunctionDescriptor.of(I32, I64, PTR, I32));
  static final MethodHandle SPAN_ADD_EVENT =
      h("aster_span_add_event", FunctionDescriptor.of(I32, I64, BYTES, PTR, I32));
  static final MethodHandle SPAN_SET_STATUS =
      h("aster_span_set_status", FunctionDescriptor.of(I32, I64, I32, BYTES));
  static final MethodHandle SPAN_RECORD_ERROR =
      h("aster_span_record_error", FunctionDescriptor.of(I32, I64, BYTES, BYTES));
  static final MethodHandle SPAN_CONTEXT =
      h("aster_span_context", FunctionDescriptor.of(I32, I64, PTR));
  static final MethodHandle SPAN_END = h("aster_span_end", FunctionDescriptor.of(I32, I64));
  static final MethodHandle TRACE_CONTEXT_FORMAT =
      h("aster_trace_context_format", FunctionDescriptor.of(I32, PTR, PTR, I32, PTR));

  // ── helpers ──────────────────────────────────────────────────────────────

  /** An {@code aster_bytes_t} value (ptr + len) over {@code data}, allocated in {@code arena}. */
  static MemorySegment bytes(Arena arena, byte[] data) {
    MemorySegment s = arena.allocate(16, 8);
    if (data.length == 0) {
      s.set(PTR, 0, MemorySegment.NULL);
      s.set(I64, 8, 0L);
    } else {
      s.set(PTR, 0, arena.allocateFrom(U8, data));
      s.set(I64, 8, data.length);
    }
    return s;
  }

  static MemorySegment bytes(Arena arena, String text) {
    return bytes(arena, text == null ? new byte[0] : text.getBytes(StandardCharsets.UTF_8));
  }

  /** A native copy of {@code data}, or NULL when empty. */
  static MemorySegment copy(Arena arena, byte[] data) {
    return data.length == 0 ? MemorySegment.NULL : arena.allocateFrom(U8, data);
  }

  /** Attribute values are strings in 4.6: a value is rendered with {@link String#valueOf}. */
  static byte[] kv(Map<String, ?> attrs) {
    if (attrs == null || attrs.isEmpty()) {
      return new byte[0];
    }
    LinkedHashMap<String, String> m = new LinkedHashMap<>(attrs.size() * 2);
    for (Map.Entry<String, ?> e : attrs.entrySet()) {
      m.put(e.getKey(), String.valueOf(e.getValue()));
    }
    return Dispatch.encodeKv(m);
  }

  static void check(int status, String op) {
    if (status != IrohStatus.OK.code) {
      throw error(status, op);
    }
  }

  static IrohException error(int status, String op) {
    IrohStatus s = IrohStatus.fromCode(status);
    String detail = DataPlane.lastError();
    return IrohException.forStatus(
        s, op + " failed: " + s.name() + (detail.isEmpty() ? "" : " (" + detail + ")"));
  }

  static RuntimeException rethrow(Throwable t) {
    if (t instanceof RuntimeException re) {
      return re;
    }
    if (t instanceof Error err) {
      throw err;
    }
    return new IllegalStateException(t);
  }

  /** Reads and releases a library-owned {@code aster_bytes_t} a call wrote at {@code out}. */
  static String takeString(MemorySegment out) {
    MemorySegment ptr = out.get(PTR, 0);
    long len = out.get(I64, 8);
    if (ptr.equals(MemorySegment.NULL) || len == 0) {
      return "";
    }
    String text = new String(ptr.reinterpret(len).toArray(U8), StandardCharsets.UTF_8);
    IrohLibrary.getInstance().stringRelease(ptr, len);
    return text;
  }
}
