package site.aster.otel;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.sun.net.httpserver.HttpServer;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.net.ConnectException;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.zip.GZIPInputStream;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.slf4j.event.Level;
import site.aster.health.HealthServer;
import site.aster.node.IrohNode;

/**
 * Path B: an application record written through {@link LogEmit} reaches the OTLP logs exporter (a
 * local receiver here) with its fields and trace ids; a record the Rust bridge delivered is not
 * re-emitted.
 */
final class LogEmitTest {

  static volatile HttpServer receiver;
  static int receiverPort;
  static final List<byte[]> LOG_BODIES = new CopyOnWriteArrayList<>();
  static final AtomicInteger LOG_REQUESTS = new AtomicInteger();
  static final AtomicInteger REJECTED_LOGS = new AtomicInteger();
  static final AtomicInteger STALLED_LOGS = new AtomicInteger();
  static volatile boolean rejectLogs;
  static volatile boolean stallLogs;
  static volatile CountDownLatch releaseStalledLog = new CountDownLatch(0);

  @BeforeAll
  static void start() throws Exception {
    receiver = startReceiver(0);
    receiverPort = receiver.getAddress().getPort();
    Observability.shutdown();
    initObservability();
  }

  private static HttpServer startReceiver(int port) throws java.io.IOException {
    HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", port), 0);
    server.createContext(
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
          boolean logs = ex.getRequestURI().getPath().endsWith("/v1/logs");
          boolean stalled = logs && stallLogs;
          boolean reject = rejectLogs;
          if (logs) {
            LOG_REQUESTS.incrementAndGet();
            if (stalled) {
              STALLED_LOGS.incrementAndGet();
              try {
                releaseStalledLog.await(10, TimeUnit.SECONDS);
              } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
              }
            }
          }
          if (logs && (stalled ? reject : rejectLogs)) {
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
    server.start();
    return server;
  }

  private static void initObservability() {
    Observability.init(
        ObservabilityConfig.builder()
            .fromEnv(false)
            .serviceName("java-pathb-test")
            .otlpEndpoint("http://127.0.0.1:" + receiverPort)
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

  private static void assertAuditReady(HealthServer health, String stage) throws Exception {
    HttpClient client = HttpClient.newHttpClient();
    HttpResponse<String> live =
        client.send(
            HttpRequest.newBuilder(URI.create("http://" + health.address() + "/healthz"))
                .timeout(Duration.ofSeconds(3))
                .build(),
            HttpResponse.BodyHandlers.ofString());
    assertEquals(200, live.statusCode(), stage + " liveness");
    HttpResponse<String> ready =
        client.send(
            HttpRequest.newBuilder(URI.create("http://" + health.address() + "/readyz"))
                .timeout(Duration.ofSeconds(3))
                .build(),
            HttpResponse.BodyHandlers.ofString());
    assertEquals(200, ready.statusCode(), stage + " readiness: " + ready.body());
    assertTrue(ready.body().contains("\"audit\":true"), stage + " audit check: " + ready.body());
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
  void applicationEmissionStaysBoundedDuringHttp503AndExportsAfterRecovery() throws Exception {
    exerciseOutage("http503");
  }

  @Test
  void applicationEmissionStaysBoundedDuringConnectionRefusalAndExportsAfterRecovery()
      throws Exception {
    exerciseOutage("refusal");
  }

  @Test
  void applicationEmissionStaysBoundedDuringStalledResponseAndExportsAfterRecovery()
      throws Exception {
    exerciseOutage("stall");
  }

  private static void exerciseOutage(String mode) throws Exception {
    Observability.flush(Duration.ofSeconds(5));
    int rejectedBefore = REJECTED_LOGS.get();
    int stalledBefore = STALLED_LOGS.get();
    int requestsBefore = LOG_REQUESTS.get();
    String marker = "java-pathb-" + mode + "-";
    IrohNode node = IrohNode.start(null, List.of(), null).get(20, TimeUnit.SECONDS);
    HealthServer health = HealthServer.start(node, "127.0.0.1", 0);
    try {
      if (mode.equals("refusal")) {
        receiver.stop(0);
        receiver = null;
        try (Socket probe = new Socket()) {
          probe.connect(new InetSocketAddress("127.0.0.1", receiverPort), 500);
          throw new AssertionError(
              "collector port remained bound during connection-refusal outage");
        } catch (ConnectException expected) {
          // The listener is actually closed, rather than returning an HTTP error.
        } catch (java.io.IOException e) {
          throw new AssertionError("could not verify the collector listener was unbound", e);
        }
      } else if (mode.equals("stall")) {
        rejectLogs = true;
        stallLogs = true;
        releaseStalledLog = new CountDownLatch(1);
      } else {
        rejectLogs = true;
      }
      health.setReady(true);
      assertAuditReady(health, "during " + mode + " exporter outage");
      long emitStarted = System.nanoTime();
      for (int i = 0; i < 1_000; i++) {
        LogEmit.emit(Level.WARN, "app.outage", marker + "outage-" + i, Map.of("phase", "outage"));
      }
      assertTrue(Duration.ofNanos(System.nanoTime() - emitStarted).toMillis() < 1_000);

      long flushStarted = System.nanoTime();
      assertThrows(RuntimeException.class, () -> Observability.flush(Duration.ofSeconds(1)));
      assertTrue(Duration.ofNanos(System.nanoTime() - flushStarted).toMillis() < 3_000);
      long rejectedDeadline = System.nanoTime() + Duration.ofSeconds(5).toNanos();
      while (mode.equals("http503")
          && REJECTED_LOGS.get() == rejectedBefore
          && System.nanoTime() < rejectedDeadline) {
        try {
          Observability.flush(Duration.ofMillis(250));
        } catch (RuntimeException expectedDuringOutage) {
          // Keep waiting for the receiver to confirm that it returned HTTP 503.
        }
        Thread.sleep(25);
      }
      if (mode.equals("http503")) {
        assertTrue(REJECTED_LOGS.get() > rejectedBefore, "collector observed an HTTP 503 request");
        assertTrue(LOG_REQUESTS.get() > requestsBefore, "collector received a logs request");
      } else if (mode.equals("stall")) {
        long stalledDeadline = System.nanoTime() + Duration.ofSeconds(5).toNanos();
        while (STALLED_LOGS.get() == stalledBefore && System.nanoTime() < stalledDeadline) {
          Thread.sleep(25);
        }
        assertTrue(
            STALLED_LOGS.get() > stalledBefore,
            "collector read a request and withheld its response");
        assertTrue(LOG_REQUESTS.get() > requestsBefore, "collector received a logs request");
        releaseStalledLog.countDown();
        stallLogs = false;
      } else {
        assertEquals(
            rejectedBefore,
            REJECTED_LOGS.get(),
            "no collector HTTP response exists during refusal");
        assertEquals(
            requestsBefore, LOG_REQUESTS.get(), "no logs request reached the closed collector");
      }
      assertFalse(has(marker + "outage-"));

      rejectLogs = false;
      if (mode.equals("refusal")) {
        try {
          receiver = startReceiver(receiverPort);
        } catch (java.io.IOException e) {
          throw new AssertionError("could not reopen collector listener", e);
        }
      }
      String traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
      LogEmit.emit(
          Level.WARN,
          "app.outage",
          marker + "recovered",
          Map.of("phase", "recovered"),
          0L,
          SpanContext.parse(traceparent));
      long recoveryDeadline = System.nanoTime() + Duration.ofSeconds(10).toNanos();
      while (bodyWith(marker + "recovered") == null && System.nanoTime() < recoveryDeadline) {
        Observability.flush(Duration.ofSeconds(1));
        Thread.sleep(25);
      }
      byte[] recovered = bodyWith(marker + "recovered");
      assertTrue(recovered != null, "recovered application log was exported");
      assertTrue(indexOf(recovered, "app.outage".getBytes(StandardCharsets.UTF_8)) >= 0);
      assertTrue(indexOf(recovered, "phase".getBytes(StandardCharsets.UTF_8)) >= 0);
      assertTrue(indexOf(recovered, "recovered".getBytes(StandardCharsets.UTF_8)) >= 0);
      assertTrue(
          indexOf(recovered, java.util.HexFormat.of().parseHex("0af7651916cd43dd8448eb211c80319c"))
              >= 0);
      assertTrue(indexOf(recovered, java.util.HexFormat.of().parseHex("b7ad6b7169203331")) >= 0);
      assertAuditReady(health, "after " + mode + " exporter recovery");

      int shutdownRequestsBefore = LOG_REQUESTS.get();
      int shutdownRejectedBefore = REJECTED_LOGS.get();
      int shutdownStalledBefore = STALLED_LOGS.get();
      if (mode.equals("refusal")) {
        receiver.stop(0);
        receiver = null;
        try (Socket probe = new Socket()) {
          probe.connect(new InetSocketAddress("127.0.0.1", receiverPort), 500);
          throw new AssertionError("collector port remained bound during refusal shutdown");
        } catch (ConnectException expected) {
          // Confirm the exporter sees an unbound listener during shutdown.
        }
      } else {
        rejectLogs = true;
        if (mode.equals("stall")) {
          stallLogs = true;
          releaseStalledLog = new CountDownLatch(1);
        }
      }
      LogEmit.emit(Level.WARN, "app.outage", marker + "shutdown-outage");
      health.close(); // Release its observability reference before measuring shutdown.
      node.close();
      long shutdownStarted = System.nanoTime();
      assertTrue(Observability.shutdown(Duration.ofSeconds(1)));
      assertTrue(Duration.ofNanos(System.nanoTime() - shutdownStarted).toMillis() < 3_000);
      assertFalse(Observability.shutdown(Duration.ofSeconds(1)));
      if (mode.equals("refusal")) {
        assertEquals(
            shutdownRequestsBefore, LOG_REQUESTS.get(), "shutdown request hit a closed listener");
        assertEquals(shutdownRejectedBefore, REJECTED_LOGS.get());
      } else if (mode.equals("stall")) {
        long deadline = System.nanoTime() + Duration.ofSeconds(2).toNanos();
        while (STALLED_LOGS.get() == shutdownStalledBefore && System.nanoTime() < deadline) {
          Thread.sleep(25);
        }
        assertTrue(
            STALLED_LOGS.get() > shutdownStalledBefore,
            "shutdown reached a request whose response remained stalled");
        assertTrue(LOG_REQUESTS.get() > shutdownRequestsBefore);
      } else {
        assertTrue(
            REJECTED_LOGS.get() > shutdownRejectedBefore, "shutdown exporter request was rejected");
        assertTrue(LOG_REQUESTS.get() > shutdownRequestsBefore);
      }
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
      throw new AssertionError("interrupted while waiting for log export", e);
    } finally {
      health.close();
      node.close();
      releaseStalledLog.countDown();
      rejectLogs = false;
      stallLogs = false;
      if (receiver == null) {
        try {
          receiver = startReceiver(receiverPort);
        } catch (java.io.IOException e) {
          throw new AssertionError("could not restore collector listener", e);
        }
      }
      if (!Observability.isInitialized()) {
        initObservability();
      }
    }
  }
}
