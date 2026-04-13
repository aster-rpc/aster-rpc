package site.aster.examples.missioncontrol;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Tag;
import org.junit.jupiter.api.Test;
import site.aster.client.AsterClient;
import site.aster.codec.ForyCodec;
import site.aster.examples.missioncontrol.types.IngestResult;
import site.aster.examples.missioncontrol.types.LogEntry;
import site.aster.examples.missioncontrol.types.MetricPoint;
import site.aster.examples.missioncontrol.types.StatusRequest;
import site.aster.examples.missioncontrol.types.StatusResponse;
import site.aster.examples.missioncontrol.types.SubmitLogResult;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * In-process Mission Control benchmark — Java server + Java client running in the same JVM.
 *
 * <p>Mirrors the shape of {@code examples/python/mission_control/benchmark.py}: warmup, unary
 * getStatus, unary submitLog, client-streaming ingestMetrics at three batch sizes, and concurrent
 * unary at three concurrency levels.
 *
 * <p>Tagged {@code benchmark} so it's excluded from the default {@code mvn test} run. Invoke
 * explicitly with:
 *
 * <pre>{@code
 * cd bindings/java && mvn -P fast -pl aster-examples-mission-control test \
 *   -Dtest=MissionControlBenchmark -Dgroups=benchmark
 * }</pre>
 */
@Tag("benchmark")
final class MissionControlBenchmark {

  private static final int WARMUP = 50;
  private static final int UNARY_ITERATIONS = 1000;

  @Test
  void runFullBenchmarkSuite() throws Exception {
    ForyCodec serverCodec = new ForyCodec();
    Server.registerWireTypes(serverCodec);
    ForyCodec clientCodec = new ForyCodec();
    Server.registerWireTypes(clientCodec);

    MissionControl missionControl = new MissionControl();

    AsterServer server =
        AsterServer.builder()
            .codec(serverCodec)
            .service(missionControl)
            .sessionService(AgentSession.class, AgentSession::new)
            .build()
            .get(15, TimeUnit.SECONDS);

    long memStartMb = heapUsedMb();

    try (AsterClient client =
        AsterClient.builder().codec(clientCodec).build().get(15, TimeUnit.SECONDS)) {
      NodeAddr addr = server.node().nodeAddr();

      System.out.println();
      System.out.println("════════════════════════════════════════════════════════════════════");
      System.out.println(" Mission Control Java↔Java in-process benchmark");
      System.out.println("════════════════════════════════════════════════════════════════════");
      System.out.println(" jvm heap used at start: " + memStartMb + " MB");
      System.out.println(" iterations per unary stage: " + UNARY_ITERATIONS);
      System.out.println(" warmup: " + WARMUP);
      System.out.println("────────────────────────────────────────────────────────────────────");

      warmup(client, addr);

      benchmarkUnaryGetStatus(client, addr);
      benchmarkUnarySubmitLog(client, addr);
      benchmarkClientStreamIngestMetrics(client, addr);
      benchmarkConcurrentGetStatus(client, addr);

      long memEndMb = heapUsedMb();
      System.out.println("────────────────────────────────────────────────────────────────────");
      System.out.println(
          " jvm heap: start="
              + memStartMb
              + "MB  end="
              + memEndMb
              + "MB  delta="
              + (memEndMb - memStartMb)
              + "MB");
      System.out.println("════════════════════════════════════════════════════════════════════");
    } finally {
      server.close();
    }
  }

  // ─── Stages ───────────────────────────────────────────────────────────────

  private static void warmup(AsterClient client, NodeAddr addr) throws Exception {
    for (int i = 0; i < WARMUP; i++) {
      client
          .<StatusRequest, StatusResponse>call(
              addr,
              MissionControlDispatcher.SERVICE_NAME,
              "getStatus",
              new StatusRequest("warmup-" + i),
              StatusResponse.class)
          .orTimeout(10, TimeUnit.SECONDS)
          .get();
    }
  }

  private static void benchmarkUnaryGetStatus(AsterClient client, NodeAddr addr) throws Exception {
    long[] latsNs = new long[UNARY_ITERATIONS];
    long t0 = System.nanoTime();
    for (int i = 0; i < UNARY_ITERATIONS; i++) {
      long start = System.nanoTime();
      try {
        client
            .<StatusRequest, StatusResponse>call(
                addr,
                MissionControlDispatcher.SERVICE_NAME,
                "getStatus",
                new StatusRequest("bench-" + i),
                StatusResponse.class)
            .orTimeout(10, TimeUnit.SECONDS)
            .get();
      } catch (Exception e) {
        System.out.printf(
            "  bench[%d] FAILED after %.1fms — %s%n",
            i, (System.nanoTime() - start) / 1e6, e.getCause());
        throw e;
      }
      latsNs[i] = System.nanoTime() - start;
      if (i % 100 == 99) {
        System.out.printf("  bench[%d] %.2fms%n", i, latsNs[i] / 1e6);
      }
    }
    long elapsedNs = System.nanoTime() - t0;
    printRow("Unary (getStatus)", UNARY_ITERATIONS, elapsedNs, latsNs);
  }

  private static void benchmarkUnarySubmitLog(AsterClient client, NodeAddr addr) throws Exception {
    long[] latsNs = new long[UNARY_ITERATIONS];
    long t0 = System.nanoTime();
    for (int i = 0; i < UNARY_ITERATIONS; i++) {
      LogEntry entry = new LogEntry(System.nanoTime() / 1e9, "info", "bench log " + i, "bench");
      long start = System.nanoTime();
      client
          .<LogEntry, SubmitLogResult>call(
              addr,
              MissionControlDispatcher.SERVICE_NAME,
              "submitLog",
              entry,
              SubmitLogResult.class)
          .orTimeout(10, TimeUnit.SECONDS)
          .get();
      latsNs[i] = System.nanoTime() - start;
    }
    long elapsedNs = System.nanoTime() - t0;
    printRow("Unary (submitLog)", UNARY_ITERATIONS, elapsedNs, latsNs);
  }

  private static void benchmarkClientStreamIngestMetrics(AsterClient client, NodeAddr addr)
      throws Exception {
    int[] batchSizes = {100, 1_000, 10_000};
    for (int batch : batchSizes) {
      List<MetricPoint> points = new ArrayList<>(batch);
      for (int i = 0; i < batch; i++) {
        points.add(
            new MetricPoint(
                "cpu.usage",
                42.0d + (i % 100) * 0.1d,
                System.nanoTime() / 1e9,
                java.util.Map.of()));
      }
      long t0 = System.nanoTime();
      IngestResult result =
          client
              .<MetricPoint, IngestResult>callClientStream(
                  addr,
                  MissionControlDispatcher.SERVICE_NAME,
                  "ingestMetrics",
                  points,
                  IngestResult.class)
              .orTimeout(60, TimeUnit.SECONDS)
              .get();
      long elapsedNs = System.nanoTime() - t0;
      double mps = batch * 1e9 / elapsedNs;
      double elapsedMs = elapsedNs / 1e6;
      System.out.printf(
          "  Client stream (%6d)  %,12.0f msg/s   %8.1f ms total   accepted=%d%n",
          batch, mps, elapsedMs, result.accepted());
    }
  }

  private static void benchmarkConcurrentGetStatus(AsterClient client, NodeAddr addr) {
    int[] concurrencies = {10, 50, 100};
    for (int concurrency : concurrencies) {
      try {
        @SuppressWarnings("unchecked")
        CompletableFuture<StatusResponse>[] futures = new CompletableFuture[concurrency];
        long t0 = System.nanoTime();
        for (int i = 0; i < concurrency; i++) {
          futures[i] =
              client.<StatusRequest, StatusResponse>call(
                  addr,
                  MissionControlDispatcher.SERVICE_NAME,
                  "getStatus",
                  new StatusRequest("concurrent-" + i),
                  StatusResponse.class);
        }
        CompletableFuture.allOf(futures).orTimeout(15, TimeUnit.SECONDS).get();
        long elapsedNs = System.nanoTime() - t0;
        double rps = concurrency * 1e9 / elapsedNs;
        double elapsedMs = elapsedNs / 1e6;
        System.out.printf(
            "  Concurrent (%3d)        %,12.0f req/s   %8.1f ms total%n",
            concurrency, rps, elapsedMs);
      } catch (Exception e) {
        // Likely Quinn's initial_max_streams_bidi=100 cap. Report and keep going.
        System.out.printf(
            "  Concurrent (%3d)        TIMED OUT — likely Quinn bi-stream cap (%s)%n",
            concurrency, e.getClass().getSimpleName());
      }
    }
  }

  // ─── Helpers ──────────────────────────────────────────────────────────────

  private static void printRow(String label, int iterations, long elapsedNs, long[] latsNs) {
    double rps = iterations * 1e9 / elapsedNs;
    long[] sorted = latsNs.clone();
    Arrays.sort(sorted);
    double p50 = sorted[(int) (iterations * 0.50)] / 1e6;
    double p90 = sorted[(int) (iterations * 0.90)] / 1e6;
    double p99 = sorted[(int) (iterations * 0.99)] / 1e6;
    System.out.printf(
        "  %-22s  %,12.0f req/s   p50=%6.2fms  p90=%6.2fms  p99=%6.2fms%n",
        label, rps, p50, p90, p99);
  }

  private static long heapUsedMb() {
    Runtime rt = Runtime.getRuntime();
    return (rt.totalMemory() - rt.freeMemory()) / (1024 * 1024);
  }
}
