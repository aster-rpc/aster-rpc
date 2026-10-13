package site.aster.otel;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.TreeSet;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Predicate;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.MethodOrderer;
import org.junit.jupiter.api.Order;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.TestMethodOrder;
import org.slf4j.LoggerFactory;
import org.slf4j.MDC;
import org.slf4j.event.Level;
import site.aster.annotations.Scope;
import site.aster.client.AsterClient;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.interceptors.CallContext;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * Observability over the C ABI 4.6 (design §8 O8): Rust log records reach SLF4J with trace ids,
 * handler spans parent from the dispatch trace context, inbound baggage is exposed through the
 * trace API, outbound calls inject {@code traceparent}/{@code baggage}, metric downcalls run on
 * virtual threads, and the ops endpoints answer.
 *
 * <p>The pipeline is process-global: this class shuts down whatever an earlier class started and
 * initialises its own (traces on, exporter pointed at a closed port so spans are sampled and
 * "exported" without a collector).
 */
@TestMethodOrder(MethodOrderer.OrderAnnotation.class)
final class ObservabilityE2ETest {

  static final String MSG = "_aster_test/ObsMsg";

  public record Msg(String text, int n) {}

  public static final class Impl {}

  /** What the handler saw, per call (the handler runs on a virtual thread). */
  record Seen(
      String traceId,
      String spanId,
      String traceparent,
      Map<String, String> baggage,
      java.util.Set<String> metadataKeys,
      String mdcTrace,
      String nestedTraceId,
      String nestedSpanId,
      String afterNestedSpanId,
      boolean virtual) {}

  static final AtomicReference<Seen> SEEN = new AtomicReference<>();

  static void register(org.apache.fory.BaseFory fory) {
    try {
      fory.register(Msg.class, MSG);
    } catch (Throwable ignored) {
      // already registered
    }
  }

  static final class Service implements ServiceDispatcher {
    @Override
    public ServiceDescriptor descriptor() {
      return new ServiceDescriptor("test.Obs", 1, Scope.SHARED, Impl.class);
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      UnaryDispatcher echo =
          new UnaryDispatcher() {
            public MethodDescriptor descriptor() {
              return new MethodDescriptor(
                  "echo",
                  StreamingKind.UNARY,
                  RequestStyle.EXPLICIT,
                  MSG,
                  List.of(),
                  MSG,
                  false,
                  false,
                  null);
            }

            public byte[] invoke(Object impl, byte[] req, Codec c, CallContext ctx) {
              Msg m = (Msg) c.decode(req, Msg.class);
              String traceId = Tracing.currentTraceId();
              String spanId = Tracing.currentSpanId();
              String traceparent = Tracing.currentTraceparent();
              Map<String, String> baggage = Tracing.currentBaggage();
              String mdcTrace = MDC.get("trace_id");
              String[] nested = new String[3];
              Tracing.runInSpan(
                  "handler.work",
                  () -> {
                    nested[0] = Tracing.currentTraceId();
                    nested[1] = Tracing.currentSpanId();
                    LoggerFactory.getLogger("app.handler").warn("inside the handler");
                  });
              nested[2] = Tracing.currentSpanId();
              SEEN.set(
                  new Seen(
                      traceId,
                      spanId,
                      traceparent,
                      baggage,
                      new TreeSet<>(ctx.metadata().keySet()),
                      mdcTrace,
                      nested[0],
                      nested[1],
                      nested[2],
                      Thread.currentThread().isVirtual()));
              return c.encode(new Msg("echo:" + m.text(), m.n()));
            }
          };
      return Map.of("echo", echo);
    }

    @Override
    public void registerTypes(org.apache.fory.Fory fory) {
      register(fory);
    }
  }

  static AsterServer server;
  static AsterClient client;
  static NodeAddr addr;
  static HttpClient http;

  @BeforeAll
  static void start() throws Exception {
    CapturingLogs.threshold(Level.TRACE);
    Observability.shutdown(); // whatever an earlier class (or the implicit start) left running
    Observability.init(
        ObservabilityConfig.builder()
            .fromEnv(false)
            .traces(true)
            .otlpEndpoint("http://127.0.0.1:9")
            .logFilter("warn,aster=trace")
            .build());
    ForyCodec serverCodec = new ForyCodec();
    register(serverCodec.fory());
    ForyCodec clientCodec = new ForyCodec();
    register(clientCodec.fory());
    server =
        AsterServer.builder()
            .codec(serverCodec)
            .service(new Impl(), new Service())
            .health("127.0.0.1", 0)
            .build()
            .get(20, TimeUnit.SECONDS);
    client = AsterClient.builder().codec(clientCodec).build().get(20, TimeUnit.SECONDS);
    addr = server.node().nodeAddr();
    http = HttpClient.newHttpClient();
  }

  @AfterAll
  static void stop() {
    if (client != null) client.close();
    if (server != null) server.close(); // stops the attached ops listener
    Observability.shutdown();
    CapturingLogs.threshold(Level.INFO);
  }

  static Msg call(String text) throws Exception {
    return client
        .<Msg, Msg>call(addr, "test.Obs", "echo", new Msg(text, 1), Msg.class)
        .orTimeout(20, TimeUnit.SECONDS)
        .get();
  }

  static <T> T eventually(java.util.function.Supplier<T> supplier, Predicate<T> done)
      throws Exception {
    long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
    T value = supplier.get();
    while (!done.test(value) && System.nanoTime() < end) {
      Thread.sleep(25);
      value = supplier.get();
    }
    return value;
  }

  static HttpResponse<String> get(String path) throws Exception {
    return http.send(
        HttpRequest.newBuilder(URI.create("http://" + server.health().address() + path))
            .timeout(Duration.ofSeconds(5))
            .build(),
        HttpResponse.BodyHandlers.ofString());
  }

  // ── logs, spans, propagation ─────────────────────────────────────────────

  @Test
  @Order(1)
  void rustLogRecordsReachSlf4jWithTraceIdsAndHandlerSpansParentFromDispatch() throws Exception {
    CapturingLogs.clear();
    String[] clientTrace = new String[1];
    Msg reply =
        Tracing.span("client.work")
            .call(
                () -> {
                  clientTrace[0] = Tracing.currentTraceId();
                  return call("hi");
                });
    assertEquals("echo:hi", reply.text());
    assertNotNull(clientTrace[0]);
    assertEquals(32, clientTrace[0].length());

    Seen seen = SEEN.get();
    assertTrue(seen.virtual(), "the handler runs on a virtual thread");
    // The client's traceparent crossed the wire: the handler is in the client's trace.
    assertEquals(clientTrace[0], seen.traceId());
    assertEquals(16, seen.spanId().length());
    assertEquals(seen.traceId(), seen.mdcTrace(), "the MDC holds the call's trace id");
    // A handler span nests under the dispatch context, in the same trace, with its own span id.
    assertEquals(seen.traceId(), seen.nestedTraceId());
    assertNotEquals(seen.spanId(), seen.nestedSpanId());
    assertEquals(seen.spanId(), seen.afterNestedSpanId(), "the carrier is restored after the span");
    // Reserved trace keys are hidden from handler metadata (Q7).
    assertFalse(seen.metadataKeys().contains("traceparent"), seen.metadataKeys().toString());

    // Rust records emitted inside the server span arrive in SLF4J with its ids; the span id equals
    // the one the dispatcher handed the handler.
    List<CapturingLogs.Rec> rust =
        eventually(
            () ->
                CapturingLogs.snapshot().stream()
                    .filter(r -> "rust".equals(r.keyValues().get("aster_origin")))
                    .filter(r -> seen.traceId().equals(r.keyValues().get("trace_id")))
                    .toList(),
            l -> l.stream().anyMatch(r -> r.message().contains("rpc completed")));
    CapturingLogs.Rec done =
        rust.stream().filter(r -> r.message().contains("rpc completed")).findFirst().orElseThrow();
    assertTrue(done.logger().startsWith("aster."), done.logger());
    assertEquals(seen.spanId(), done.keyValues().get("span_id"));
    assertEquals("test.Obs", done.keyValues().get("rpc.service"));
    assertEquals("echo", done.keyValues().get("rpc.method"));
    assertEquals(seen.traceId(), done.mdc().get("trace_id"), "MDC set around the logger call");
    assertEquals(seen.spanId(), done.mdc().get("span_id"));
    assertTrue(done.thread().contains("aster-log-poll"), done.thread());

    // The application's own record, written in the handler, carries the ids through the MDC.
    CapturingLogs.Rec app =
        eventually(
            () ->
                CapturingLogs.snapshot().stream()
                    .filter(r -> r.logger().equals("app.handler"))
                    .findFirst()
                    .orElse(null),
            r -> r != null);
    assertNotNull(app);
    assertEquals(seen.nestedTraceId(), app.mdc().get("trace_id"));
    assertEquals(seen.nestedSpanId(), app.mdc().get("span_id"));
  }

  @Test
  @Order(2)
  void inboundBaggageIsVisibleInTheHandlerAndHiddenFromMetadata() throws Exception {
    String tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    Tracing.withTraceContext(tp, Map.of("tenant", "acme", "note", "a b,c"), () -> call("bag"));
    Seen seen = SEEN.get();
    assertEquals("4bf92f3577b34da6a3ce929d0e0e4736", seen.traceId());
    assertEquals(Map.of("tenant", "acme", "note", "a b,c"), seen.baggage());
    assertFalse(seen.metadataKeys().contains("baggage"), seen.metadataKeys().toString());
    assertFalse(seen.metadataKeys().contains("traceparent"), seen.metadataKeys().toString());

    // Without a context the handler's carrier holds only the server span, and no baggage.
    call("plain");
    Seen plain = SEEN.get();
    assertTrue(plain.baggage().isEmpty());
    assertNotEquals("4bf92f3577b34da6a3ce929d0e0e4736", plain.traceId());
  }

  // ── metrics ──────────────────────────────────────────────────────────────

  static double sum(String prometheus, String metric) {
    double total = 0;
    for (String line : prometheus.split("\n")) {
      if (line.startsWith(metric + "{") || line.startsWith(metric + " ")) {
        total += Double.parseDouble(line.substring(line.lastIndexOf(' ') + 1));
      }
    }
    return total;
  }

  @Test
  @Order(3)
  void metricDowncallsFromVirtualThreadsAreCountedAndScrapeable() throws Exception {
    Metrics.Counter orders = Metrics.counter("obs.test.orders", "Orders placed", "");
    Metrics.Histogram latency = Metrics.histogram("obs.test.latency", "Order latency", "s");
    Metrics.UpDownCounter inflight = Metrics.upDownCounter("obs.test.inflight", "In flight", "");
    Metrics.Gauge depth = Metrics.gauge("obs.test.depth", "Queue depth", "");
    Metrics.Attributes eu = Metrics.Attributes.of("region", "eu");
    int threads = 8;
    int per = 500;
    CountDownLatch done = new CountDownLatch(threads);
    AtomicReference<Throwable> failure = new AtomicReference<>();
    boolean[] sawNonVirtual = new boolean[1];
    for (int t = 0; t < threads; t++) {
      Thread.ofVirtual()
          .start(
              () -> {
                try {
                  if (!Thread.currentThread().isVirtual()) sawNonVirtual[0] = true;
                  for (int i = 0; i < per; i++) {
                    orders.add(1, eu);
                    orders.add(1, Map.of("region", "us"));
                    latency.record(0.01, eu);
                    inflight.add(1);
                    inflight.add(-1);
                  }
                  depth.record(7);
                } catch (Throwable e) {
                  failure.set(e);
                } finally {
                  done.countDown();
                }
              });
    }
    assertTrue(done.await(30, TimeUnit.SECONDS));
    assertNull(failure.get());
    assertFalse(sawNonVirtual[0]);

    String text = Metrics.prometheus().orElseThrow();
    assertEquals(threads * per * 2.0, sum(text, "obs_test_orders_total"), text);
    assertTrue(text.contains("region=\"eu\""), text);
    assertEquals(threads * per, sum(text, "obs_test_latency_seconds_count"), text);
    assertTrue(text.contains("obs_test_latency_seconds_bucket{"), "seconds histogram buckets");
    assertEquals(0.0, sum(text, "obs_test_inflight"), text);
    assertEquals(7.0, sum(text, "obs_test_depth"), text);
    assertTrue(
        text.contains("aster_rpc_server_requests_completed_total"), "built-ins share the scrape");

    assertThrowsIllegalArgument(() -> orders.add(-1));
  }

  @Test
  @Order(4)
  void explicitHistogramBoundariesAndKindClashes() throws Exception {
    Metrics.Histogram sized =
        Metrics.histogram(
            "obs.test.sized", "Payload sizes", "By", Metrics.Options.boundaries(10, 100, 1000));
    sized.record(50);
    sized.record(5000);
    String text = Metrics.prometheus().orElseThrow();
    assertTrue(text.contains("obs_test_sized_bytes_bucket{") && text.contains("le=\"100\""), text);
    assertEquals(2.0, sum(text, "obs_test_sized_bytes_count"), text);
    // One kind per name.
    org.junit.jupiter.api.Assertions.assertThrows(
        RuntimeException.class, () -> Metrics.counter("obs.test.sized", "", ""));
  }

  private static void assertThrowsIllegalArgument(Runnable r) {
    org.junit.jupiter.api.Assertions.assertThrows(IllegalArgumentException.class, r::run);
  }

  // ── health ───────────────────────────────────────────────────────────────

  @Test
  @Order(5)
  void healthEndpointsReportLivenessReadinessChecksAndMetrics() throws Exception {
    call("count one"); // make the built-ins non-empty
    HealthServerChecks.run(server, ObservabilityE2ETest::get);
  }

  /** Split out so the listener assertions read top to bottom. */
  static final class HealthServerChecks {
    interface Get {
      HttpResponse<String> get(String path) throws Exception;
    }

    static void run(AsterServer server, Get http) throws Exception {
      var health = server.health();
      assertNotNull(health);
      assertTrue(health.address().startsWith("127.0.0.1:"), health.address());

      HttpResponse<String> live = http.get("/healthz");
      assertEquals(200, live.statusCode());
      assertTrue(live.body().contains("\"status\":\"ok\""), live.body());

      // A successful server start with the listener attached sets ready.
      HttpResponse<String> ready = http.get("/readyz");
      assertEquals(200, ready.statusCode(), ready.body());

      // A named check is created failing: not ready until it says so.
      var db = health.check("db");
      db.set(false);
      HttpResponse<String> notReady = http.get("/readyz");
      assertEquals(503, notReady.statusCode(), notReady.body());
      assertTrue(notReady.body().contains("\"db\":false"), notReady.body());
      db.set(true);
      assertEquals(200, http.get("/readyz").statusCode());

      HttpResponse<String> metrics = http.get("/metrics");
      assertEquals(200, metrics.statusCode());
      assertTrue(metrics.headers().firstValue("content-type").orElse("").startsWith("text/plain"));
      assertTrue(
          metrics.body().contains("aster_rpc_server_requests_completed_total"), metrics.body());
      assertTrue(metrics.body().contains("aster_rpc_server_duration_seconds_bucket"));

      health.setHealthy(false);
      assertEquals(503, http.get("/healthz").statusCode());
      assertEquals(503, http.get("/readyz").statusCode());
      health.setHealthy(true);
      assertEquals(200, http.get("/healthz").statusCode());

      health.setReady(false);
      assertEquals(503, http.get("/readyz").statusCode());
      health.setReady(true);
      assertEquals(200, http.get("/readyz").statusCode());
    }
  }

  @Test
  @Order(6)
  void syncLevelFollowsTheAsterLogger() {
    CapturingLogs.threshold(Level.WARN);
    try {
      assertEquals("warn,aster=warn", Observability.syncLevel().orElseThrow());
      CapturingLogs.threshold(Level.DEBUG);
      assertEquals("warn,aster=debug", Observability.syncLevel().orElseThrow());
    } finally {
      CapturingLogs.threshold(Level.TRACE);
      Observability.syncLevel();
    }
  }

  @Test
  @Order(7)
  void outboundInjectionAddsTraceparentAndBaggageFromTheCarrier() throws Exception {
    assertNull(Tracing.inject(null));
    Map<String, String> own =
        Map.of("traceparent", "00-" + "a".repeat(32) + "-" + "b".repeat(16) + "-01");
    String tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    Tracing.withTraceContext(
        tp,
        Map.of("k", "v w"),
        () -> {
          Map<String, String> out = Tracing.inject(Map.of("x", "1"));
          assertEquals(tp, out.get("traceparent"));
          assertEquals("k=v%20w", out.get("baggage"));
          assertEquals("1", out.get("x"));
          assertTrue(Tracing.inject(own) == own, "a caller-set traceparent wins");
          return null;
        });
    assertNull(Tracing.currentTraceId(), "the binding ended with the block");
  }
}
