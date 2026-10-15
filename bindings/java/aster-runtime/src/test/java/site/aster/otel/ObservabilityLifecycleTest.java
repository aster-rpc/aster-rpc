package site.aster.otel;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.Map;
import java.util.Optional;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.health.HealthServer;
import site.aster.node.IrohNode;

/**
 * Pipeline lifecycle and the pure parts of the trace API (no server needed): init / shutdown /
 * re-init, a differing configuration, handles that survive a shutdown, the health listener giving
 * the pipeline a Prometheus pull reader, and the W3C codecs.
 */
final class ObservabilityLifecycleTest {

  @BeforeEach
  void clean() {
    Observability.shutdown();
  }

  @AfterAll
  static void leaveClean() {
    Observability.shutdown();
  }

  private static ObservabilityConfig named(String service) {
    return ObservabilityConfig.builder()
        .fromEnv(false)
        .serviceName(service)
        .logFilter("warn,aster=info")
        .build();
  }

  @Test
  void abiFloorAndAlreadyExistsStatus() {
    assertEquals(6, IrohLibrary.EXPECTED_ABI_MAJOR);
    assertEquals(0, IrohLibrary.REQUIRED_ABI_MINOR);
    assertTrue(IrohLibrary.getInstance().abiVersionMinor() >= IrohLibrary.REQUIRED_ABI_MINOR);
    assertEquals(IrohStatus.ALREADY_EXISTS, IrohStatus.fromCode(17));
    assertEquals(17, IrohStatus.ALREADY_EXISTS.code);
    assertEquals(IrohStatus.OPERATION_LIMIT, IrohStatus.fromCode(18));
  }

  @Test
  void initShutdownAndReinitWithANewConfiguration() {
    assertFalse(Observability.isInitialized());
    Observability.init(named("first"));
    assertTrue(Observability.isInitialized());
    Observability.init(named("first")); // same configuration: joins (a second reference)
    IllegalStateException differing =
        assertThrows(IllegalStateException.class, () -> Observability.init(named("second")));
    assertTrue(differing.getMessage().contains("ALREADY_EXISTS"), differing.getMessage());

    assertTrue(Observability.shutdown(), "something was held");
    assertFalse(Observability.isInitialized());
    assertFalse(Observability.shutdown(), "nothing left to release");

    Observability.init(named("second")); // a new pipeline accepts the new configuration
    assertTrue(Observability.isInitialized());
  }

  @Test
  void logPollThreadStopsWithShutdownAndRestartsWithInit() throws Exception {
    Observability.init(named("poll"));
    assertTrue(pollThreadAlive());
    Observability.shutdown();
    long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
    while (pollThreadAlive() && System.nanoTime() < end) {
      Thread.sleep(20);
    }
    assertFalse(pollThreadAlive());
    assertEquals(Optional.empty(), Observability.syncLevel(), "no bridge is open");
    Observability.init(named("poll"));
    assertTrue(pollThreadAlive());
    assertTrue(Observability.droppedLogRecords() >= 0);
  }

  private static boolean pollThreadAlive() {
    return Thread.getAllStackTraces().keySet().stream()
        .anyMatch(t -> t.getName().equals("aster-log-poll") && t.isAlive());
  }

  @Test
  void instrumentsAndAttributesSurviveAPipelineShutdown() {
    Observability.init(named("handles"));
    Metrics.Counter c = Metrics.counter("obs.lifecycle.count", "", "");
    Metrics.Attributes a = Metrics.Attributes.of("k", "v");
    c.add(1, a);
    Observability.shutdown(); // frees every instrument and attribute set handle
    c.add(1, a); // re-declared and re-interned; with no pipeline it is a no-op
    Observability.init(named("handles"));
    c.add(2, a);
    c.close();
    a.close();
  }

  @Test
  void healthListenerGivesThePipelineAPrometheusPullReader() throws Exception {
    Observability.init(named("scrape"));
    assertEquals(Optional.empty(), Metrics.prometheus(), "no pull reader yet");
    Metrics.Counter c = Metrics.counter("obs.lifecycle.scrape", "Scrapes", "");
    try (IrohNode node = IrohNode.start(null, java.util.List.of(), null).get(20, TimeUnit.SECONDS);
        HealthServer health = HealthServer.start(node, "127.0.0.1", 0)) {
      c.add(3);
      assertTrue(health.port() > 0);
      String text = Metrics.prometheus().orElseThrow();
      assertTrue(text.contains("obs_lifecycle_scrape_total"), text);
      assertEquals(Observability.metricsText().orElseThrow().contains("target_info"), true);
    }
  }

  @Test
  void healthStartRefusesAnOutOfRangePort() throws Exception {
    try (IrohNode node =
        IrohNode.start(null, java.util.List.of(), null).get(20, TimeUnit.SECONDS)) {
      assertThrows(
          IllegalArgumentException.class, () -> HealthServer.start(node, "127.0.0.1", 70000));
    }
  }

  // ── W3C codecs and the carrier (no pipeline) ─────────────────────────────

  @Test
  void spanContextParsesAndFormatsTraceparent() {
    String tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    SpanContext c = SpanContext.parse(tp);
    assertEquals("4bf92f3577b34da6a3ce929d0e0e4736", c.traceId());
    assertEquals("00f067aa0ba902b7", c.spanId());
    assertTrue(c.sampled());
    assertTrue(c.remote());
    assertEquals(tp, c.traceparent());
    for (String bad :
        new String[] {
          "",
          "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
          "00-" + "0".repeat(32) + "-00f067aa0ba902b7-01",
          "00-4bf92f3577b34da6a3ce929d0e0e4736-" + "0".repeat(16) + "-01",
          "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
          "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7"
        }) {
      assertNull(SpanContext.tryParse(bad), bad);
      assertThrows(IllegalArgumentException.class, () -> SpanContext.parse(bad));
    }
  }

  @Test
  void baggageRoundTripsWithPercentEncodingAndLimits() {
    Map<String, String> b = Map.of("tenant", "acme", "note", "a b,c=d;é");
    Map<String, String> back = TraceContext.parseBaggage(TraceContext.formatBaggage(b));
    assertEquals(b, back);
    assertEquals(Map.of("k", "v"), TraceContext.parseBaggage("k=v;prop=1, bad ,=x"));
    assertTrue(TraceContext.withinLimits("a=b"));
    assertFalse(TraceContext.withinLimits("a=" + "x".repeat(9000)));
    assertFalse(
        TraceContext.withinLimits(String.join(",", java.util.Collections.nCopies(65, "a=b"))));
  }

  @Test
  void carrierBindsForTheBlockAndPlainThreadsDoNotInheritIt() throws Exception {
    String tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    assertNull(Tracing.currentTraceId());
    Tracing.runWithTraceContext(
        tp,
        Map.of("a", "b"),
        () -> {
          assertEquals("4bf92f3577b34da6a3ce929d0e0e4736", Tracing.currentTraceId());
          assertEquals("00f067aa0ba902b7", Tracing.currentSpanId());
          assertEquals(Map.of("a", "b"), Tracing.currentBaggage());
          assertEquals("4bf92f3577b34da6a3ce929d0e0e4736", org.slf4j.MDC.get("trace_id"));
          // A plain pool thread has no binding, the propagating executor carries it.
          java.util.concurrent.ExecutorService pool =
              java.util.concurrent.Executors.newSingleThreadExecutor();
          try {
            assertNull(pool.submit(Tracing::currentTraceId).get());
            String[] seen = new String[1];
            Tracing.propagating(pool).execute(() -> seen[0] = Tracing.currentTraceId());
            pool.shutdown();
            assertTrue(pool.awaitTermination(5, TimeUnit.SECONDS));
            assertEquals("4bf92f3577b34da6a3ce929d0e0e4736", seen[0]);
          } catch (Exception e) {
            throw new AssertionError(e);
          }
        });
    assertNull(Tracing.currentTraceId());
    assertNull(org.slf4j.MDC.get("trace_id"));
    assertThrows(
        IllegalArgumentException.class,
        () -> Tracing.runWithTraceContext("nope", Map.of(), () -> {}));
  }

  @Test
  void spansAreNoOpsWithoutAPipelineAndNestOnceStarted() throws Exception {
    try (Span s = Tracing.span("no.pipeline").start()) {
      assertNull(s.context());
      assertNull(s.traceId());
      s.setAttribute("k", "v")
          .addEvent("e")
          .setStatus(true, "")
          .recordException(new RuntimeException("x"));
    }
    assertEquals("r", Tracing.withSpan("noop", () -> "r"));
    assertNull(Tracing.currentTraceId(), "a no-op span binds nothing");

    Observability.init(
        ObservabilityConfig.builder()
            .fromEnv(false)
            .traces(true)
            .otlpEndpoint("http://127.0.0.1:9")
            .logFilter("warn,aster=info")
            .build());
    Tracing.withSpan(
        "outer",
        Map.of("a", 1, "ok", true),
        () -> {
          String outerTrace = Tracing.currentTraceId();
          String outerSpan = Tracing.currentSpanId();
          assertEquals(32, outerTrace.length());
          Tracing.runInSpan(
              "inner",
              () -> {
                assertEquals(outerTrace, Tracing.currentTraceId());
                assertTrue(!outerSpan.equals(Tracing.currentSpanId()));
              });
          assertEquals(outerSpan, Tracing.currentSpanId());
          return null;
        });
    // An explicit parent wins over the current context.
    String tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    try (Span s = Tracing.span("explicit").parent(tp).kind(SpanKind.CLIENT).start()) {
      assertEquals("4bf92f3577b34da6a3ce929d0e0e4736", s.traceId());
      assertEquals(16, s.spanId().length());
      assertEquals(55, s.traceparent().length());
    }
    // An exception escaping the block is recorded and rethrown.
    assertThrows(
        IllegalStateException.class,
        () ->
            Tracing.runInSpan(
                "boom",
                () -> {
                  throw new IllegalStateException("boom");
                }));
  }

  @Test
  void concurrentPropagatedCarriersRemainIndependent() throws Exception {
    var pool = java.util.concurrent.Executors.newFixedThreadPool(2);
    var entered = new java.util.concurrent.CountDownLatch(2);
    var release = new java.util.concurrent.CountDownLatch(1);
    var tasks = new java.util.ArrayList<java.util.concurrent.CompletableFuture<Void>>();
    try {
      for (String trace : new String[] {"1".repeat(32), "2".repeat(32)}) {
        Tracing.runWithTraceContext(
            "00-" + trace + "-00f067aa0ba902b7-01",
            Map.of("tenant", trace),
            () -> {
              tasks.add(
                  java.util.concurrent.CompletableFuture.runAsync(
                      () -> {
                        entered.countDown();
                        try {
                          assertTrue(release.await(5, TimeUnit.SECONDS));
                        } catch (InterruptedException e) {
                          Thread.currentThread().interrupt();
                          throw new AssertionError(e);
                        }
                        assertEquals(trace, Tracing.currentTraceId());
                        assertEquals(Map.of("tenant", trace), Tracing.currentBaggage());
                      },
                      Tracing.propagating(pool)));
            });
      }
      assertTrue(entered.await(5, TimeUnit.SECONDS));
      assertNull(Tracing.currentTraceId());
      release.countDown();
      for (var task : tasks) task.get(5, TimeUnit.SECONDS);
      assertNull(pool.submit(Tracing::currentTraceId).get(5, TimeUnit.SECONDS));
    } finally {
      release.countDown();
      pool.shutdownNow();
      assertTrue(pool.awaitTermination(5, TimeUnit.SECONDS));
    }
  }
}
