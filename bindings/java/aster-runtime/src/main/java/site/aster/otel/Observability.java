package site.aster.otel;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.Optional;
import org.slf4j.LoggerFactory;
import site.aster.ffi.IrohStatus;

/**
 * Starts and stops the observability pipeline: metrics, traces and logs over the Rust {@code
 * aster-otel} crate (design §2.4, §3). The OpenTelemetry SDK, the exporters, the {@code tracing}
 * subscriber and the Prometheus renderer all live in Rust; this class only configures them and
 * delivers Rust log records to SLF4J.
 *
 * <p>Without any call, the first {@code AsterServer} or {@code AsterClient} start runs {@link
 * #ensureStarted()}: the log bridge opens (Rust logs appear in SLF4J) and exporters start only when
 * {@code OTEL_*} asks for them. To configure the pipeline yourself, call {@link #init} before that.
 * The SDK's globals are per process, so a configuration that differs from the running pipeline's
 * throws {@link IllegalStateException}; {@link #shutdown} first to change it.
 *
 * <p>Logs: a daemon platform thread ({@code aster-log-poll}) blocks in {@code aster_log_poll} (it
 * never occupies a virtual-thread carrier) and writes each Rust record to the SLF4J logger named
 * after its target ({@code aster::rpc::dispatch} to {@code aster.rpc.dispatch}), after an {@code
 * isEnabledForLevel} check. Each record carries the key/values {@code trace_id} and {@code span_id}
 * (when emitted inside an exported span), {@code aster_call_id}, {@code aster_audit}, {@code
 * aster_origin=rust} and the event's own fields ({@code rpc.service}, {@code rpc.method}, {@code
 * aster.peer}, ...), and the MDC holds {@code trace_id} / {@code span_id} during the call to the
 * logger.
 *
 * <p>Level sync: Rust filters by level before allocating anything, so its filter follows the
 * effective level of the SLF4J logger {@code aster}. Call {@link #syncLevel()} after changing
 * logging configuration; the poll thread also re-checks every 5 s. An explicit {@link
 * ObservabilityConfig#logFilter} or {@code ASTER_LOG} pins the Rust filter and turns the re-check
 * off.
 */
public final class Observability {

  private Observability() {}

  private static final Object LOCK = new Object();
  private static int refs;
  private static boolean auto;

  /** Start the pipeline with the default configuration (environment applied, log bridge on). */
  public static void init() {
    init(ObservabilityConfig.builder().build());
  }

  /**
   * Start the observability pipeline, or join the running one. Every successful call is one
   * reference; {@link #shutdown} releases all of them.
   *
   * @throws IllegalArgumentException invalid configuration
   * @throws IllegalStateException a configuration that differs from the running pipeline's
   */
  public static void init(ObservabilityConfig cfg) {
    synchronized (LOCK) {
      String filter = cfg.logFilter();
      boolean pinned = filter != null;
      boolean fromEnv = cfg.fromEnv();
      if (cfg.logBridge() && !pinned) {
        if (LogBridge.envPinsFilter()) {
          pinned = true;
          fromEnv = true; // ASTER_LOG / ASTER_LOG_LEVEL say what Rust should do
        } else {
          filter = LogBridge.directive();
        }
      }
      try (Arena arena = Arena.ofConfined()) {
        int status = (int) OtelNative.OBSERVABILITY_INIT.invoke(lower(arena, cfg, filter, fromEnv));
        if (status == IrohStatus.ALREADY_EXISTS.code) {
          throw new IllegalStateException(
              OtelNative.error(status, "aster_observability_init").getMessage());
        }
        if (status == IrohStatus.INVALID_ARGUMENT.code) {
          throw new IllegalArgumentException(
              OtelNative.error(status, "aster_observability_init").getMessage());
        }
        OtelNative.check(status, "aster_observability_init");
      } catch (Throwable t) {
        throw OtelNative.rethrow(t);
      }
      refs++;
      if (cfg.logBridge()) {
        LogBridge.start(pinned);
      }
    }
  }

  private static MemorySegment lower(
      Arena arena, ObservabilityConfig cfg, String filter, boolean fromEnv) {
    MemorySegment c = arena.allocate(OtelNative.CONFIG_SIZE, 8);
    int flags = 0;
    if (fromEnv) flags |= OtelNative.FLAG_FROM_ENV;
    if (Boolean.TRUE.equals(cfg.traces())) flags |= OtelNative.FLAG_TRACES;
    if (Boolean.TRUE.equals(cfg.metrics())) flags |= OtelNative.FLAG_METRICS;
    if (Boolean.TRUE.equals(cfg.otlpLogs())) flags |= OtelNative.FLAG_OTLP_LOGS;
    if (cfg.logBridge()) flags |= OtelNative.FLAG_LOG_BRIDGE;
    c.set(ValueLayout.JAVA_INT, 0, (int) OtelNative.CONFIG_SIZE);
    c.set(ValueLayout.JAVA_INT, 4, flags);
    put(arena, c, 8, cfg.serviceName() == null ? new byte[0] : utf8(cfg.serviceName()));
    put(arena, c, 24, cfg.otlpEndpoint() == null ? new byte[0] : utf8(cfg.otlpEndpoint()));
    put(arena, c, 40, OtelNative.kv(cfg.otlpHeaders()));
    put(arena, c, 56, OtelNative.kv(cfg.resourceAttributes()));
    put(arena, c, 72, filter == null ? new byte[0] : utf8(filter));
    put(arena, c, 88, cfg.configToml() == null ? new byte[0] : utf8(cfg.configToml()));
    c.set(ValueLayout.JAVA_DOUBLE, 104, cfg.sampleRatio() == null ? -1.0 : cfg.sampleRatio());
    c.set(ValueLayout.JAVA_INT, 112, 0); // protocol: default (http/protobuf)
    c.set(ValueLayout.JAVA_INT, 116, cfg.logQueue());
    c.set(ValueLayout.JAVA_INT, 120, OtelNative.BINDING_JAVA);
    return c;
  }

  private static byte[] utf8(String s) {
    return s.getBytes(StandardCharsets.UTF_8);
  }

  private static void put(Arena arena, MemorySegment s, long offset, byte[] data) {
    if (data.length > 0) {
      s.set(OtelNative.PTR, offset, arena.allocateFrom(OtelNative.U8, data));
      s.set(OtelNative.I64, offset + 8, data.length);
    }
  }

  /**
   * The implicit start (called when a server or client starts, design Q2): open the log bridge with
   * an environment-only configuration. Exporters start only when {@code OTEL_*} asks for them. A
   * no-op after any {@link #init}; a failure is logged, never thrown (observability must not stop a
   * server).
   */
  public static void ensureStarted() {
    synchronized (LOCK) {
      if (auto || refs > 0) {
        return;
      }
      auto = true;
    }
    try {
      init(ObservabilityConfig.builder().fromEnv(envAsksForTelemetry()).build());
    } catch (RuntimeException e) {
      LoggerFactory.getLogger(Observability.class)
          .warn("observability could not start: {}", e.getMessage());
    }
  }

  private static boolean envAsksForTelemetry() {
    for (String k :
        new String[] {
          "OTEL_TRACES_EXPORTER",
          "OTEL_METRICS_EXPORTER",
          "OTEL_LOGS_EXPORTER",
          "OTEL_EXPORTER_OTLP_ENDPOINT",
          "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
          "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
          "OTEL_SERVICE_NAME",
          "ASTER_LOG",
          "ASTER_LOG_LEVEL"
        }) {
      String v = System.getenv(k);
      if (v != null && !v.isBlank()) {
        return true;
      }
    }
    return false;
  }

  /** Whether this binding holds a reference to the running pipeline. */
  public static boolean isInitialized() {
    synchronized (LOCK) {
      return refs > 0;
    }
  }

  /** Flush every signal, waiting at most {@code timeout}. */
  public static void flush(Duration timeout) {
    try {
      int status = (int) OtelNative.OBSERVABILITY_FLUSH.invoke((int) timeout.toMillis());
      OtelNative.check(status, "aster_observability_flush");
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
  }

  /**
   * Release every reference this process holds on the pipeline: the log poll thread stops,
   * providers flush and stop (unless an ops listener still holds the pipeline), spans end and
   * metric instruments are freed ({@link Metrics} objects re-declare themselves on next use).
   * Returns whether anything was held. A later {@link #init} starts a new pipeline.
   */
  public static boolean shutdown(Duration timeout) {
    synchronized (LOCK) {
      LogBridge.stop(
          timeout.compareTo(Duration.ofSeconds(1)) < 0 ? timeout : Duration.ofSeconds(1));
      boolean held = false;
      try {
        while (true) {
          int status = (int) OtelNative.OBSERVABILITY_SHUTDOWN.invoke((int) timeout.toMillis());
          if (status != IrohStatus.OK.code) {
            break; // ALREADY_CLOSED: none left; TIMEOUT: finishes in the background
          }
          held = true;
        }
      } catch (Throwable t) {
        throw OtelNative.rethrow(t);
      }
      refs = 0;
      auto = false;
      return held;
    }
  }

  /** As {@link #shutdown(Duration)} with a 2 s timeout. */
  public static boolean shutdown() {
    return shutdown(Duration.ofSeconds(2));
  }

  /**
   * Make Rust filter by the {@code aster} SLF4J logger's effective level now. Returns the
   * directives applied, or empty when the log bridge is not open.
   */
  public static Optional<String> syncLevel() {
    return Optional.ofNullable(LogBridge.syncLevel());
  }

  /** The Prometheus text of the running pipeline; see {@link Metrics#prometheus()}. */
  public static Optional<String> metricsText() {
    return Metrics.prometheus();
  }

  /** General Rust log records dropped because the bridge queue was full, since process start. */
  public static long droppedLogRecords() {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(8, 8);
      OtelNative.check((int) OtelNative.LOG_DROPPED.invoke(out), "aster_log_dropped");
      return out.get(OtelNative.I64, 0);
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
  }
}
