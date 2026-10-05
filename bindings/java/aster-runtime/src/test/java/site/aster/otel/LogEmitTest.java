package site.aster.otel;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
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
}
