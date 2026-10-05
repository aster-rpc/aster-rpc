package site.aster.otel;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import org.slf4j.ILoggerFactory;
import org.slf4j.IMarkerFactory;
import org.slf4j.Logger;
import org.slf4j.Marker;
import org.slf4j.event.KeyValuePair;
import org.slf4j.event.Level;
import org.slf4j.event.LoggingEvent;
import org.slf4j.helpers.BasicMDCAdapter;
import org.slf4j.helpers.BasicMarkerFactory;
import org.slf4j.helpers.LegacyAbstractLogger;
import org.slf4j.helpers.MessageFormatter;
import org.slf4j.spi.LoggingEventAware;
import org.slf4j.spi.MDCAdapter;
import org.slf4j.spi.SLF4JServiceProvider;

/**
 * A test-only SLF4J 2 backend that keeps every record (message, level, logger, fluent key/values
 * and a copy of the MDC at the moment of the call), so tests can assert what the Rust log bridge
 * delivered. Registered through {@code META-INF/services/org.slf4j.spi.SLF4JServiceProvider}.
 */
public final class CapturingLogs implements SLF4JServiceProvider {

  /** One delivered record. */
  public record Rec(
      String logger,
      Level level,
      String message,
      Map<String, Object> keyValues,
      Map<String, String> mdc,
      String thread) {}

  private static final int CAPACITY = 50_000;
  private static final ArrayDeque<Rec> RECORDS = new ArrayDeque<>();
  private static final ConcurrentHashMap<String, Logger> LOGGERS = new ConcurrentHashMap<>();
  private static volatile int threshold = Level.INFO.toInt();

  /** The lowest level the test backend enables (what {@code isEnabledForLevel} answers). */
  public static void threshold(Level level) {
    threshold = level.toInt();
  }

  public static List<Rec> snapshot() {
    synchronized (RECORDS) {
      return new ArrayList<>(RECORDS);
    }
  }

  public static void clear() {
    synchronized (RECORDS) {
      RECORDS.clear();
    }
  }

  private static void add(Rec r) {
    synchronized (RECORDS) {
      if (RECORDS.size() >= CAPACITY) {
        RECORDS.pollFirst();
      }
      RECORDS.addLast(r);
    }
  }

  private static Map<String, String> mdcCopy() {
    Map<String, String> m = org.slf4j.MDC.getCopyOfContextMap();
    return m == null ? Map.of() : Map.copyOf(m);
  }

  private static final class CapturingLogger extends LegacyAbstractLogger
      implements LoggingEventAware {
    CapturingLogger(String name) {
      this.name = name;
    }

    private boolean on(Level l) {
      return l.toInt() >= threshold;
    }

    @Override
    public boolean isTraceEnabled() {
      return on(Level.TRACE);
    }

    @Override
    public boolean isDebugEnabled() {
      return on(Level.DEBUG);
    }

    @Override
    public boolean isInfoEnabled() {
      return on(Level.INFO);
    }

    @Override
    public boolean isWarnEnabled() {
      return on(Level.WARN);
    }

    @Override
    public boolean isErrorEnabled() {
      return on(Level.ERROR);
    }

    @Override
    protected String getFullyQualifiedCallerName() {
      return null;
    }

    @Override
    protected void handleNormalizedLoggingCall(
        Level level, Marker marker, String msg, Object[] args, Throwable t) {
      add(
          new Rec(
              name,
              level,
              MessageFormatter.arrayFormat(msg, args).getMessage(),
              Map.of(),
              mdcCopy(),
              Thread.currentThread().getName()));
    }

    @Override
    public void log(LoggingEvent e) {
      Map<String, Object> kv = new java.util.LinkedHashMap<>();
      List<KeyValuePair> pairs = e.getKeyValuePairs();
      if (pairs != null) {
        for (KeyValuePair p : pairs) {
          kv.put(p.key, p.value);
        }
      }
      Object[] args = e.getArgumentArray();
      String message =
          args == null
              ? e.getMessage()
              : MessageFormatter.arrayFormat(e.getMessage(), args).getMessage();
      add(new Rec(name, e.getLevel(), message, kv, mdcCopy(), Thread.currentThread().getName()));
    }
  }

  private final ILoggerFactory loggerFactory =
      n -> LOGGERS.computeIfAbsent(n, CapturingLogger::new);
  private final IMarkerFactory markerFactory = new BasicMarkerFactory();
  private final MDCAdapter mdcAdapter = new BasicMDCAdapter();

  @Override
  public ILoggerFactory getLoggerFactory() {
    return loggerFactory;
  }

  @Override
  public IMarkerFactory getMarkerFactory() {
    return markerFactory;
  }

  @Override
  public MDCAdapter getMDCAdapter() {
    return mdcAdapter;
  }

  @Override
  public String getRequestedApiVersion() {
    return "2.0.99";
  }

  @Override
  public void initialize() {}
}
