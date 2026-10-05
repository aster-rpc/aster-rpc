package site.aster.otel;

import static site.aster.otel.OtelNative.I64;
import static site.aster.otel.OtelNative.PTR;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.Map;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.slf4j.MDC;
import org.slf4j.event.Level;
import org.slf4j.spi.LoggingEventBuilder;
import site.aster.ffi.Dispatch;
import site.aster.ffi.IrohStatus;

/**
 * Delivers Rust log records to SLF4J (design §3.1, §3.2). One dedicated platform daemon thread
 * blocks in {@code aster_log_poll} (a virtual thread's carrier must not block in native code, and
 * the C ABI refuses the poll on a tokio thread) and writes each batch of up to 256 records to the
 * logger named after the record's target.
 *
 * <p>The poll marks its thread as a log drain on the Rust side: events emitted while this thread
 * runs a logger (a logger that calls back into Aster) never reach the bridge, so there is no
 * feedback loop. The thread calls the logger with no Rust lock held.
 */
final class LogBridge {

  private LogBridge() {}

  private static final int BATCH = 256;
  private static final int POLL_TIMEOUT_MS = 200;
  private static final long RECHECK_NANOS = Duration.ofSeconds(5).toNanos();

  private static Thread thread;
  private static volatile boolean stop;
  private static volatile boolean pinned;
  private static volatile String lastLevel;

  // aster_log_record_t field offsets (96 bytes).
  private static final long OFF_LEVEL = 0;
  private static final long OFF_FLAGS = 1;
  private static final long OFF_TIMESTAMP = 8;
  private static final long OFF_TRACE_ID = 16;
  private static final long OFF_SPAN_ID = 32;
  private static final long OFF_CALL_ID = 40;
  private static final long OFF_TARGET = 48;
  private static final long OFF_MESSAGE = 64;
  private static final long OFF_FIELDS = 80;

  private static final int FLAG_HAS_TRACE = 1;
  private static final int FLAG_IN_CALL = 2;
  private static final int FLAG_SYNTHETIC = 4;
  private static final int FLAG_AUDIT = 8;

  private static final java.util.HexFormat HEX = java.util.HexFormat.of();

  // ── level sync ───────────────────────────────────────────────────────────

  /** Whether {@code ASTER_LOG} / {@code ASTER_LOG_LEVEL} already say what Rust should do. */
  static boolean envPinsFilter() {
    return notBlank(System.getenv("ASTER_LOG")) || notBlank(System.getenv("ASTER_LOG_LEVEL"));
  }

  private static boolean notBlank(String s) {
    return s != null && !s.isBlank();
  }

  /** Name of the effective level of the SLF4J logger {@code aster}. */
  static String effectiveLevel() {
    Logger l = LoggerFactory.getLogger("aster");
    if (l.isTraceEnabled()) return "trace";
    if (l.isDebugEnabled()) return "debug";
    if (l.isInfoEnabled()) return "info";
    if (l.isWarnEnabled()) return "warn";
    if (l.isErrorEnabled()) return "error";
    return "off";
  }

  /** Rust filter directives matching the {@code aster} logger: the rest of Rust stays at warn. */
  static String directive() {
    return "warn,aster=" + effectiveLevel();
  }

  /** Apply the logger's level to Rust now; the directives, or {@code null} if no bridge is open. */
  static String syncLevel() {
    String level = effectiveLevel();
    String directive = "warn,aster=" + level;
    byte[] d = directive.getBytes(StandardCharsets.UTF_8);
    try (Arena arena = Arena.ofConfined()) {
      int status = (int) OtelNative.LOG_SET_FILTER.invoke(OtelNative.copy(arena, d), d.length);
      if (status == site.aster.ffi.IrohStatus.NOT_FOUND.code) {
        return null;
      }
      OtelNative.check(status, "aster_log_set_filter");
    } catch (Throwable t) {
      throw OtelNative.rethrow(t);
    }
    lastLevel = level;
    return directive;
  }

  // ── lifecycle ────────────────────────────────────────────────────────────

  static synchronized void start(boolean pinFilter) {
    pinned = pinFilter;
    if (lastLevel == null) {
      lastLevel = effectiveLevel();
    }
    if (thread != null && thread.isAlive()) {
      return;
    }
    stop = false;
    thread = Thread.ofPlatform().daemon().name("aster-log-poll").start(LogBridge::run);
  }

  /** Stop the poll thread and join it; queued records are dropped. */
  static void stop(Duration timeout) {
    Thread t;
    synchronized (LogBridge.class) {
      t = thread;
      thread = null;
    }
    stop = true;
    if (t == null) {
      return;
    }
    try {
      OtelNative.LOG_CLOSE.invoke(); // wakes the poller
      t.join(timeout.toMillis());
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
    } catch (Throwable e) {
      throw OtelNative.rethrow(e);
    } finally {
      lastLevel = null;
    }
  }

  // ── the poll loop ────────────────────────────────────────────────────────

  private static void run() {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment buf = arena.allocate(OtelNative.LOG_RECORD_SIZE * BATCH, 8);
      MemorySegment count = arena.allocate(ValueLayout.JAVA_INT);
      long nextCheck = System.nanoTime() + RECHECK_NANOS;
      while (!stop) {
        long t0 = System.nanoTime();
        int status =
            (int)
                OtelNative.LOG_POLL.invoke(
                    buf, (int) OtelNative.LOG_RECORD_SIZE, BATCH, POLL_TIMEOUT_MS, count);
        if (status == IrohStatus.NOT_FOUND.code || status == IrohStatus.ALREADY_EXISTS.code) {
          // The bridge is closed (or another poller exists). Stop instead of spinning.
          return;
        }
        int n = status == IrohStatus.OK.code ? count.get(ValueLayout.JAVA_INT, 0) : 0;
        if (n > 0) {
          deliver(buf, n);
        } else if (System.nanoTime() - t0 < Duration.ofMillis(POLL_TIMEOUT_MS / 2).toNanos()) {
          // Any other early return is a refusal: back off instead of spinning.
          Thread.sleep(25);
        }
        if (!pinned && System.nanoTime() >= nextCheck) {
          nextCheck = System.nanoTime() + RECHECK_NANOS;
          if (!effectiveLevel().equals(lastLevel)) {
            syncLevel();
          }
        }
      }
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
    } catch (Throwable t) {
      System.err.println("aster-log-poll: stopped: " + t);
    }
  }

  private static void deliver(MemorySegment buf, int n) {
    for (int i = 0; i < n; i++) {
      try {
        emit(buf.asSlice((long) i * OtelNative.LOG_RECORD_SIZE, OtelNative.LOG_RECORD_SIZE));
      } catch (RuntimeException e) {
        // One bad record (or a logger that throws) must not stop the batch.
      }
    }
  }

  private static String text(MemorySegment rec, long off) {
    MemorySegment p = rec.get(PTR, off);
    int len = rec.get(ValueLayout.JAVA_INT, off + 8);
    if (p.equals(MemorySegment.NULL) || len == 0) {
      return "";
    }
    return new String(p.reinterpret(len).toArray(OtelNative.U8), StandardCharsets.UTF_8);
  }

  private static Level level(int code) {
    return switch (code) {
      case 1 -> Level.ERROR;
      case 2 -> Level.WARN;
      case 3 -> Level.INFO;
      case 4 -> Level.DEBUG;
      default -> Level.TRACE;
    };
  }

  private static void emit(MemorySegment rec) {
    Level level = level(rec.get(ValueLayout.JAVA_BYTE, OFF_LEVEL) & 0xff);
    String name = text(rec, OFF_TARGET).replace("::", ".");
    Logger logger = LoggerFactory.getLogger(name.isEmpty() ? "aster" : name);
    if (!logger.isEnabledForLevel(level)) {
      return;
    }
    int flags = rec.get(ValueLayout.JAVA_BYTE, OFF_FLAGS) & 0xff;
    LoggingEventBuilder b = logger.atLevel(level).setMessage(text(rec, OFF_MESSAGE));
    b = b.addKeyValue("aster_origin", "rust");
    String traceId = "";
    String spanId = "";
    if ((flags & FLAG_HAS_TRACE) != 0) {
      byte[] t = new byte[16];
      byte[] s = new byte[8];
      MemorySegment.copy(rec, ValueLayout.JAVA_BYTE, OFF_TRACE_ID, t, 0, 16);
      MemorySegment.copy(rec, ValueLayout.JAVA_BYTE, OFF_SPAN_ID, s, 0, 8);
      traceId = HEX.formatHex(t);
      spanId = HEX.formatHex(s);
      b = b.addKeyValue("trace_id", traceId).addKeyValue("span_id", spanId);
    }
    if ((flags & FLAG_IN_CALL) != 0) {
      b = b.addKeyValue("aster_call_id", rec.get(I64, OFF_CALL_ID));
    }
    if ((flags & FLAG_AUDIT) != 0) {
      b = b.addKeyValue("aster_audit", true);
    }
    if ((flags & FLAG_SYNTHETIC) != 0) {
      b = b.addKeyValue("aster_synthetic", true);
    }
    b = b.addKeyValue("aster_timestamp_ns", rec.get(I64, OFF_TIMESTAMP));
    MemorySegment fp = rec.get(PTR, OFF_FIELDS);
    int fl = rec.get(ValueLayout.JAVA_INT, OFF_FIELDS + 8);
    if (!fp.equals(MemorySegment.NULL) && fl > 0) {
      Map<String, String> fields = Dispatch.decodeKv(fp.reinterpret(fl).toArray(OtelNative.U8));
      for (Map.Entry<String, String> e : fields.entrySet()) {
        b = b.addKeyValue(e.getKey(), e.getValue());
      }
    }
    if (traceId.isEmpty()) {
      b.log();
      return;
    }
    // The MDC is the poll thread's own: set around the call so a pattern with %X{trace_id} works.
    MDC.put(Tracing.MDC_TRACE_ID, traceId);
    MDC.put(Tracing.MDC_SPAN_ID, spanId);
    try {
      b.log();
    } finally {
      MDC.remove(Tracing.MDC_TRACE_ID);
      MDC.remove(Tracing.MDC_SPAN_ID);
    }
  }
}
