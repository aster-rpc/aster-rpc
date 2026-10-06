package site.aster.otel;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.sun.net.httpserver.HttpServer;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.zip.GZIPInputStream;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.slf4j.event.Level;

/**
 * Path B: an application record written through {@link LogEmit} reaches the OTLP logs exporter (a
 * local receiver here) with its fields and trace ids; a record the Rust bridge delivered is not
 * re-emitted.
 */
final class LogEmitTest {

  static HttpServer receiver;
  static final List<byte[]> LOG_BODIES = new CopyOnWriteArrayList<>();
  static final AtomicInteger REJECTED_LOGS = new AtomicInteger();
  static volatile boolean rejectLogs;

  @BeforeAll
  static void start() throws Exception {
    receiver = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
    receiver.createContext(
        "/",
        ex -> {
          byte[] body;
          try (InputStream in = ex.getRequestBody()) {
            body = in.readAllBytes();
          }
          if ("gzip".equals(ex.getRequestHeaders().getFirst("Content-Encoding"))) {
            try (GZIPInputStream z = new GZIPInputStream(new java.io.ByteArrayInputStream(body))) {
              ByteArrayOutputStream o = new ByteArrayOutputStream();
              z.transferTo(o);
              body = o.toByteArray();
            }
          }
          if (ex.getRequestURI().getPath().endsWith("/v1/logs") && rejectLogs) {
            REJECTED_LOGS.incrementAndGet();
            ex.sendResponseHeaders(503, -1);
            ex.close();
            return;
          }
          if (ex.getRequestURI().getPath().endsWith("/v1/logs")) {
            LOG_BODIES.add(body);
          }
          ex.sendResponseHeaders(200, -1);
          ex.close();
        });
    receiver.start();
    Observability.shutdown();
    Observability.init(
        ObservabilityConfig.builder()
            .fromEnv(false)
            .serviceName("java-pathb-test")
            .otlpEndpoint("http://127.0.0.1:" + receiver.getAddress().getPort())
            .otlpLogs(true)
            .logFilter("warn,aster=info")
            .build());
  }

  @AfterAll
  static void stop() {
    Observability.shutdown();
    if (receiver != null) receiver.stop(0);
  }

  private static boolean has(byte[] needle) {
    for (byte[] b : LOG_BODIES) {
      if (indexOf(b, needle) >= 0) return true;
    }
    return false;
  }

  private static boolean has(String s) {
    return has(s.getBytes(StandardCharsets.UTF_8));
  }

  private static byte[] bodyWith(String needle) {
    byte[] bytes = needle.getBytes(StandardCharsets.UTF_8);
    for (byte[] body : LOG_BODIES) {
      if (indexOf(body, bytes) >= 0) return body;
    }
    return null;
  }

  private static int indexOf(byte[] h, byte[] n) {
    outer:
    for (int i = 0; i + n.length <= h.length; i++) {
      for (int j = 0; j < n.length; j++) {
        if (h[i + j] != n[j]) continue outer;
      }
      return i;
    }
    return -1;
  }

  @Test
  void layoutIsSeventyTwoBytes() {
    assertEquals(72, LogEmit.SIZE);
    assertEquals(5, LogEmit.code(Level.TRACE));
    assertEquals(1, LogEmit.code(Level.ERROR));
  }

  @Test
  void appRecordIsExportedWithFieldsAndTraceIds() throws Exception {
    SpanContext sc = SpanContext.parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
    LogEmit.emit(
        Level.WARN,
        "app.pathb",
        "pathb-marker-app",
        Map.of("order", "o-1"),
        System.currentTimeMillis() * 1_000_000L,
        sc);
    // Without an explicit context the current one is attached.
    Tracing.runInSpan(
        "pathb.span", () -> LogEmit.emit(Level.INFO, "app.pathb", "pathb-marker-ctx"));
    // A record the bridge delivered is never re-emitted.
    org.slf4j.event.DefaultLoggingEvent bridged =
        new org.slf4j.event.DefaultLoggingEvent(Level.INFO, org.slf4j.helpers.NOPLogger.NOP_LOGGER);
    bridged.setMessage("pathb-marker-rust");
    bridged.addKeyValue("aster_origin", "rust");
    assertFalse(LogEmit.emit(bridged));

    Observability.flush(Duration.ofSeconds(5));
    long end = System.nanoTime() + Duration.ofSeconds(10).toNanos();
    while (!(has("pathb-marker-app") && has("pathb-marker-ctx")) && System.nanoTime() < end) {
      Thread.sleep(25);
      Observability.flush(Duration.ofSeconds(5));
    }
    assertTrue(has("pathb-marker-app"));
    assertTrue(has("app.pathb"));
    assertTrue(has("order") && has("o-1"));
    assertTrue(has(java.util.HexFormat.of().parseHex("4bf92f3577b34da6a3ce929d0e0e4736")));
    assertTrue(has(java.util.HexFormat.of().parseHex("00f067aa0ba902b7")));
    assertTrue(has("pathb-marker-ctx"));
    assertFalse(has("pathb-marker-rust"));
  }

  @Test
  void applicationEmissionStaysBoundedDuringOutageAndExportsAfterRecovery() {
    int rejectedBefore = REJECTED_LOGS.get();
    rejectLogs = true;
    try {
      long emitStarted = System.nanoTime();
      for (int i = 0; i < 1_000; i++) {
        LogEmit.emit(Level.WARN, "app.outage", "java-pathb-outage-" + i, Map.of("phase", "outage"));
      }
      assertTrue(Duration.ofNanos(System.nanoTime() - emitStarted).toMillis() < 1_000);

      long flushStarted = System.nanoTime();
      assertThrows(RuntimeException.class, () -> Observability.flush(Duration.ofSeconds(1)));
      assertTrue(Duration.ofNanos(System.nanoTime() - flushStarted).toMillis() < 3_000);
      long rejectedDeadline = System.nanoTime() + Duration.ofSeconds(5).toNanos();
      while (REJECTED_LOGS.get() == rejectedBefore && System.nanoTime() < rejectedDeadline) {
        try {
          Observability.flush(Duration.ofMillis(250));
        } catch (RuntimeException expectedDuringOutage) {
          // Keep waiting for the receiver to confirm that it returned HTTP 503.
        }
        Thread.sleep(25);
      }
      assertTrue(REJECTED_LOGS.get() > rejectedBefore, "collector observed an HTTP 503 request");
      assertFalse(has("java-pathb-outage-"));

      rejectLogs = false;
      String traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
      LogEmit.emit(
          Level.WARN,
          "app.outage",
          "java-pathb-recovered",
          Map.of("phase", "recovered"),
          0L,
          SpanContext.parse(traceparent));
      long recoveryDeadline = System.nanoTime() + Duration.ofSeconds(10).toNanos();
      while (bodyWith("java-pathb-recovered") == null && System.nanoTime() < recoveryDeadline) {
        Observability.flush(Duration.ofSeconds(1));
        Thread.sleep(25);
      }
      byte[] recovered = bodyWith("java-pathb-recovered");
      assertTrue(recovered != null, "recovered application log was exported");
      assertTrue(indexOf(recovered, "app.outage".getBytes(StandardCharsets.UTF_8)) >= 0);
      assertTrue(indexOf(recovered, "phase".getBytes(StandardCharsets.UTF_8)) >= 0);
      assertTrue(indexOf(recovered, "recovered".getBytes(StandardCharsets.UTF_8)) >= 0);
      assertTrue(
          indexOf(recovered, java.util.HexFormat.of().parseHex("0af7651916cd43dd8448eb211c80319c"))
              >= 0);
      assertTrue(indexOf(recovered, java.util.HexFormat.of().parseHex("b7ad6b7169203331")) >= 0);

      rejectLogs = true;
      LogEmit.emit(Level.WARN, "app.outage", "java-pathb-shutdown-outage");
      long shutdownStarted = System.nanoTime();
      assertTrue(Observability.shutdown(Duration.ofSeconds(1)));
      assertTrue(Duration.ofNanos(System.nanoTime() - shutdownStarted).toMillis() < 3_000);
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
      throw new AssertionError("interrupted while waiting for log export", e);
    } finally {
      rejectLogs = false;
      if (!Observability.isInitialized()) {
        Observability.init(
            ObservabilityConfig.builder()
                .fromEnv(false)
                .serviceName("java-pathb-test")
                .otlpEndpoint("http://127.0.0.1:" + receiver.getAddress().getPort())
                .otlpLogs(true)
                .logFilter("warn,aster=info")
                .build());
      }
    }
  }
}
