package site.aster.otel;

import static site.aster.otel.OtelNative.I64;
import static site.aster.otel.OtelNative.PTR;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import site.aster.ffi.IrohStatus;

/**
 * Application metrics over the Rust OpenTelemetry pipeline (design §4.2).
 *
 * <p>Thin wrappers: each instrument is a handle into {@code aster-otel} (meter scope {@code
 * aster.app}), recorded through the same {@code MeterProvider} as Aster's built-in RPC metrics
 * ({@code aster.rpc.server.*}, {@code aster.admission.*}, ...), so one exporter and one {@code
 * /metrics} endpoint serve both. Without a pipeline ({@link Observability}) recording is a no-op.
 *
 * <pre>{@code
 * Metrics.Counter orders = Metrics.counter("orders.placed", "Orders placed", "");
 * Metrics.Histogram latency = Metrics.histogram("orders.latency", "Order latency", "s");
 * Attributes eu = Attributes.of("region", "eu");     // build once, record many times
 * orders.add(1, eu);
 * latency.record(0.042, Map.of("region", "eu"));      // ad-hoc attributes
 * }</pre>
 *
 * <p>Threading: every call is a plain (non-{@code critical}) downcall, so it is safe from virtual
 * threads (the carrier is briefly pinned) and never blocks the JVM's safepoints; a record can take
 * the SDK's write lock when it creates a series. Attribute values are strings. Keep their
 * cardinality bounded: the SDK folds series beyond 2000 per instrument into {@code
 * otel.metric.overflow=true}. Names follow the OpenTelemetry rules (a letter, then letters, digits,
 * {@code _ . - /}); a name can have only one kind.
 *
 * <p>Prometheus text appends the unit to the name (unit {@code s} renders {@code ..._seconds}) and
 * {@code _total} to counters. A histogram in seconds gets the RPC duration buckets (0.005 .. 10 s);
 * pass {@link Options#boundaries} for others (without them, any other unit gets the SDK default,
 * which is millisecond-scaled). {@code autoAttrs} and {@code cardinalityLimit} are accepted and
 * recorded but have no effect yet (ABI 4.6).
 */
public final class Metrics {

  private Metrics() {}

  /** {@code aster_instrument_kind_t}. */
  private static final int KIND_COUNTER = 1;

  private static final int KIND_UP_DOWN = 2;
  private static final int KIND_HISTOGRAM = 3;
  private static final int KIND_GAUGE = 4;

  /**
   * Optional instrument settings.
   *
   * @param autoAttrs attribute keys merged in from the call being handled (no effect yet)
   * @param cardinalityLimit per-instrument series limit, 0 = the default (no effect yet)
   * @param boundaries explicit histogram bucket boundaries (finite, strictly increasing), or empty
   */
  public record Options(List<String> autoAttrs, int cardinalityLimit, double[] boundaries) {
    public static Options defaults() {
      return new Options(List.of(), 0, new double[0]);
    }

    public static Options boundaries(double... boundaries) {
      return new Options(List.of(), 0, boundaries.clone());
    }

    public Options {
      autoAttrs = List.copyOf(autoAttrs);
      boundaries = boundaries == null ? new double[0] : boundaries.clone();
    }
  }

  // ── factories ────────────────────────────────────────────────────────────

  public static Counter counter(String name, String description, String unit) {
    return counter(name, description, unit, Options.defaults());
  }

  public static Counter counter(String name, String description, String unit, Options options) {
    return new Counter(name, description, unit, options);
  }

  public static UpDownCounter upDownCounter(String name, String description, String unit) {
    return upDownCounter(name, description, unit, Options.defaults());
  }

  public static UpDownCounter upDownCounter(
      String name, String description, String unit, Options options) {
    return new UpDownCounter(name, description, unit, options);
  }

  public static Histogram histogram(String name, String description, String unit) {
    return histogram(name, description, unit, Options.defaults());
  }

  public static Histogram histogram(String name, String description, String unit, Options options) {
    return new Histogram(name, description, unit, options);
  }

  public static Gauge gauge(String name, String description, String unit) {
    return gauge(name, description, unit, Options.defaults());
  }

  public static Gauge gauge(String name, String description, String unit, Options options) {
    return new Gauge(name, description, unit, options);
  }

  /**
   * The Prometheus text of the running pipeline (built-ins and custom instruments), what {@code
   * /metrics} serves, without HTTP ({@code aster_metrics_prometheus}). Empty while no pull reader
   * exists: start a {@code site.aster.health.HealthServer} (or set {@code
   * OTEL_METRICS_EXPORTER=prometheus}).
   */
  public static Optional<String> prometheus() {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(16, 8);
      int status = (int) OtelNative.METRICS_PROMETHEUS.invoke(out);
      if (status == IrohStatus.NOT_FOUND.code) {
        return Optional.empty();
      }
      OtelNative.check(status, "aster_metrics_prometheus");
      return Optional.of(OtelNative.takeString(out));
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
  }

  // ── attributes ───────────────────────────────────────────────────────────

  /**
   * An interned attribute set ({@code aster_attrset_t}): build once, pass to every record on the
   * hot path so no key/value list is re-encoded. Values are stringified. A pipeline shutdown frees
   * the native handle; the set re-interns itself on next use.
   */
  public static final class Attributes implements AutoCloseable {
    private final byte[] kv;
    private volatile long handle;

    private Attributes(byte[] kv) {
      this.kv = kv;
      this.handle = intern();
    }

    public static Attributes of(Map<String, ?> attrs) {
      return new Attributes(OtelNative.kv(attrs));
    }

    /** Alternating keys and values: {@code Attributes.of("region", "eu", "tier", "gold")}. */
    public static Attributes of(String... keysAndValues) {
      if (keysAndValues.length % 2 != 0) {
        throw new IllegalArgumentException("odd number of key/value arguments");
      }
      Map<String, String> m = new LinkedHashMap<>();
      for (int i = 0; i < keysAndValues.length; i += 2) {
        m.put(keysAndValues[i], keysAndValues[i + 1]);
      }
      return of(m);
    }

    private long intern() {
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment out = arena.allocate(8, 8);
        OtelNative.check(
            (int) OtelNative.ATTRSET_NEW.invoke(OtelNative.copy(arena, kv), kv.length, out),
            "aster_attrset_new");
        return out.get(I64, 0);
      } catch (Throwable t) {
        throw OtelNative.rethrow(t);
      }
    }

    long handle() {
      return handle;
    }

    synchronized void refresh(long stale) {
      if (handle == stale) {
        handle = intern();
      }
    }

    /** Release the native handle (optional; a pipeline shutdown frees them all). */
    @Override
    public void close() {
      long h = handle;
      handle = 0;
      if (h != 0) {
        try {
          OtelNative.ATTRSET_FREE.invoke(h);
        } catch (Throwable t) {
          throw OtelNative.rethrow(t);
        }
      }
    }
  }

  // ── instruments ──────────────────────────────────────────────────────────

  /** Common machinery of the four instrument kinds. */
  public abstract static class Instrument implements AutoCloseable {
    private final int kind;
    private final String name;
    private final String description;
    private final String unit;
    private final Options options;
    private volatile long handle;

    Instrument(int kind, String name, String description, String unit, Options options) {
      this.kind = kind;
      this.name = name;
      this.description = description == null ? "" : description;
      this.unit = unit == null ? "" : unit;
      this.options = options;
      this.handle = declare();
    }

    public String name() {
      return name;
    }

    private long declare() {
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment d = arena.allocate(OtelNative.INSTRUMENT_DESC_SIZE, 8);
        d.set(ValueLayout.JAVA_INT, 0, (int) OtelNative.INSTRUMENT_DESC_SIZE);
        d.set(ValueLayout.JAVA_INT, 4, kind);
        putBytes(arena, d, 8, name.getBytes(java.nio.charset.StandardCharsets.UTF_8));
        putBytes(arena, d, 24, description.getBytes(java.nio.charset.StandardCharsets.UTF_8));
        putBytes(arena, d, 40, unit.getBytes(java.nio.charset.StandardCharsets.UTF_8));
        putBytes(arena, d, 56, autoAttrKeys());
        d.set(ValueLayout.JAVA_INT, 72, options.cardinalityLimit());
        double[] b = options.boundaries();
        if (b.length > 0) {
          d.set(PTR, 80, arena.allocateFrom(ValueLayout.JAVA_DOUBLE, b));
          d.set(ValueLayout.JAVA_INT, 88, b.length);
        }
        MemorySegment out = arena.allocate(8, 8);
        OtelNative.check(
            (int) OtelNative.INSTRUMENT_NEW.invoke(d, out), "aster_instrument_new(" + name + ")");
        return out.get(I64, 0);
      } catch (Throwable t) {
        throw OtelNative.rethrow(t);
      }
    }

    /** {@code auto_attrs}: a key list ({@code u32 count}, then {@code u32 len} + key ...). */
    private byte[] autoAttrKeys() {
      if (options.autoAttrs().isEmpty()) {
        return new byte[0];
      }
      // The key list: u32 count, then u32 len + key (UTF-8), little-endian.
      java.nio.ByteBuffer buf =
          java.nio.ByteBuffer.allocate(
                  4 + options.autoAttrs().stream().mapToInt(k -> 4 + k.length() * 4).sum())
              .order(java.nio.ByteOrder.LITTLE_ENDIAN);
      buf.putInt(options.autoAttrs().size());
      for (String k : options.autoAttrs()) {
        byte[] kb = k.getBytes(java.nio.charset.StandardCharsets.UTF_8);
        buf.putInt(kb.length).put(kb);
      }
      byte[] out = new byte[buf.position()];
      buf.flip().get(out);
      return out;
    }

    private static void putBytes(Arena arena, MemorySegment s, long offset, byte[] data) {
      if (data.length > 0) {
        s.set(PTR, offset, arena.allocateFrom(OtelNative.U8, data));
        s.set(I64, offset + 8, data.length);
      }
    }

    /** One record: {@code value} as raw bits (an integer, or a double's bits for f64 kinds). */
    final void send(long valueBits, Attributes set, Map<String, ?> attrs) {
      byte[] kv = attrs == null ? new byte[0] : OtelNative.kv(attrs);
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment kvSeg = OtelNative.copy(arena, kv);
        long h = handle;
        long s = set == null ? 0L : set.handle();
        int status = invoke(h, valueBits, s, kvSeg, kv.length);
        if (status == IrohStatus.NOT_FOUND.code) {
          // A pipeline shutdown freed the handles: declare / intern again, once.
          redeclare(h);
          if (set != null) {
            set.refresh(s);
          }
          status = invoke(handle, valueBits, set == null ? 0L : set.handle(), kvSeg, kv.length);
        }
        OtelNative.check(status, "aster_metric_*(" + name + ")");
      } catch (Throwable t) {
        throw OtelNative.rethrow(t);
      }
    }

    private synchronized void redeclare(long stale) {
      if (handle == stale) {
        handle = declare();
      }
    }

    private int invoke(long h, long bits, long set, MemorySegment kv, int kvLen) {
      try {
        return switch (kind) {
          case KIND_COUNTER -> (int) OtelNative.METRIC_ADD_U64.invoke(h, bits, set, kv, kvLen, 0L);
          case KIND_UP_DOWN -> (int) OtelNative.METRIC_ADD_I64.invoke(h, bits, set, kv, kvLen, 0L);
          default ->
              (int)
                  OtelNative.METRIC_RECORD_F64.invoke(
                      h, Double.longBitsToDouble(bits), set, kv, kvLen, 0L);
        };
      } catch (Throwable t) {
        throw OtelNative.rethrow(t);
      }
    }

    /** Release the instrument's handle (optional; a pipeline shutdown frees them all). */
    @Override
    public void close() {
      long h = handle;
      if (h == 0) {
        return;
      }
      try {
        int status = (int) OtelNative.INSTRUMENT_FREE.invoke(h);
        if (status != IrohStatus.NOT_FOUND.code) {
          OtelNative.check(status, "aster_instrument_free");
        }
      } catch (Throwable t) {
        throw OtelNative.rethrow(t);
      } finally {
        handle = 0;
      }
    }

    @Override
    public String toString() {
      return getClass().getSimpleName() + "(" + name + ")";
    }
  }

  /** Monotonic sum. */
  public static final class Counter extends Instrument {
    private Counter(String name, String description, String unit, Options options) {
      super(KIND_COUNTER, name, description, unit, options);
    }

    public void add(long value) {
      add(value, null, null);
    }

    public void add(long value, Attributes attributes) {
      add(value, attributes, null);
    }

    public void add(long value, Map<String, ?> attributes) {
      add(value, null, attributes);
    }

    private void add(long value, Attributes set, Map<String, ?> attrs) {
      if (value < 0) {
        throw new IllegalArgumentException("a counter only goes up; use an upDownCounter");
      }
      send(value, set, attrs);
    }
  }

  /** Sum that may go down (in-flight work, queue depth). */
  public static final class UpDownCounter extends Instrument {
    private UpDownCounter(String name, String description, String unit, Options options) {
      super(KIND_UP_DOWN, name, description, unit, options);
    }

    public void add(long value) {
      send(value, null, null);
    }

    public void add(long value, Attributes attributes) {
      send(value, attributes, null);
    }

    public void add(long value, Map<String, ?> attributes) {
      send(value, null, attributes);
    }
  }

  /** Distribution of values (durations, sizes). */
  public static final class Histogram extends Instrument {
    private Histogram(String name, String description, String unit, Options options) {
      super(KIND_HISTOGRAM, name, description, unit, options);
    }

    public void record(double value) {
      send(Double.doubleToRawLongBits(value), null, null);
    }

    public void record(double value, Attributes attributes) {
      send(Double.doubleToRawLongBits(value), attributes, null);
    }

    public void record(double value, Map<String, ?> attributes) {
      send(Double.doubleToRawLongBits(value), null, attributes);
    }
  }

  /** Last recorded value. */
  public static final class Gauge extends Instrument {
    private Gauge(String name, String description, String unit, Options options) {
      super(KIND_GAUGE, name, description, unit, options);
    }

    public void record(double value) {
      send(Double.doubleToRawLongBits(value), null, null);
    }

    public void record(double value, Attributes attributes) {
      send(Double.doubleToRawLongBits(value), attributes, null);
    }

    public void record(double value, Map<String, ?> attributes) {
      send(Double.doubleToRawLongBits(value), null, attributes);
    }
  }
}
