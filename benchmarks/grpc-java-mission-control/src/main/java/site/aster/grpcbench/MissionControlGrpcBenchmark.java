package site.aster.grpcbench;

import io.grpc.ChannelCredentials;
import io.grpc.Grpc;
import io.grpc.ManagedChannel;
import io.grpc.Server;
import io.grpc.ServerCredentials;
import io.grpc.TlsChannelCredentials;
import io.grpc.TlsServerCredentials;
import io.grpc.netty.NettyChannelBuilder;
import io.netty.handler.ssl.util.SelfSignedCertificate;
import java.util.Arrays;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import mission_control.MissionControlGrpc;
import mission_control.MissionControlOuterClass.LogEntry;
import mission_control.MissionControlOuterClass.StatusRequest;
import mission_control.MissionControlOuterClass.StatusResponse;
import mission_control.MissionControlOuterClass.SubmitLogResult;

/**
 * In-process gRPC + TLS Mission Control benchmark — Java server + Java client running in the
 * same JVM. Mirrors the shape of the Aster Java benchmark
 * ({@code bindings/java/aster-examples-mission-control/.../MissionControlBenchmark}) so the two
 * can be visually compared side-by-side.
 *
 * <p>Wire setup: gRPC over HTTP/2 with TLS using a Netty {@code SelfSignedCertificate}. This
 * matches the encryption posture of the Aster benchmark which uses QUIC's mandatory TLS 1.3.
 *
 * <p>Stages exercised (must match the Aster benchmark's stage list to be comparable):
 *
 * <ul>
 *   <li>Warmup (50 unary getStatus calls)
 *   <li>Sequential unary getStatus (1000 iterations)
 *   <li>Sequential unary submitLog (1000 iterations)
 *   <li>Concurrent unary getStatus at 10 / 50 / 100 parallel calls
 * </ul>
 *
 * <p>NOTE: the {@code .proto} file currently only defines GetStatus + SubmitLog (the original
 * gRPC baseline was scoped to chapters 1-2 of the Mission Control sample). The client-streaming
 * IngestMetrics stage from the Aster benchmark is NOT exercised here — extending the proto with
 * the streaming methods is a follow-up.
 *
 * <p>Run with:
 *
 * <pre>{@code
 * cd benchmarks/grpc-java-mission-control && mvn -q exec:java
 * }</pre>
 */
public final class MissionControlGrpcBenchmark {

  private static final int WARMUP = 50;
  private static final int UNARY_ITERATIONS = 1000;
  private static final String SERVER_HOST = "localhost";

  public static void main(String[] args) throws Exception {
    // Self-signed cert for TLS — generated in-process so there are no fixtures on disk.
    SelfSignedCertificate ssc = new SelfSignedCertificate(SERVER_HOST);
    ServerCredentials serverCreds =
        TlsServerCredentials.create(ssc.certificate(), ssc.privateKey());

    Server server =
        Grpc.newServerBuilderForPort(0, serverCreds)
            .addService(new MissionControlServiceImpl())
            .build()
            .start();
    int port = server.getPort();

    ChannelCredentials clientCreds =
        TlsChannelCredentials.newBuilder().trustManager(ssc.certificate()).build();
    ManagedChannel channel =
        NettyChannelBuilder.forAddress(SERVER_HOST, port, clientCreds)
            .overrideAuthority(SERVER_HOST)
            .build();

    long memStartMb = heapUsedMb();
    MissionControlGrpc.MissionControlBlockingStub blockingStub =
        MissionControlGrpc.newBlockingStub(channel);
    MissionControlGrpc.MissionControlFutureStub futureStub =
        MissionControlGrpc.newFutureStub(channel);

    System.out.println();
    System.out.println("════════════════════════════════════════════════════════════════════");
    System.out.println(" gRPC Java↔Java in-process benchmark (TLS, self-signed cert)");
    System.out.println("════════════════════════════════════════════════════════════════════");
    System.out.println(" jvm heap used at start: " + memStartMb + " MB");
    System.out.println(" iterations per unary stage: " + UNARY_ITERATIONS);
    System.out.println(" warmup: " + WARMUP);
    System.out.println("────────────────────────────────────────────────────────────────────");

    try {
      warmup(blockingStub);

      benchmarkUnaryGetStatus(blockingStub);
      benchmarkUnarySubmitLog(blockingStub);
      benchmarkConcurrentGetStatus(futureStub);

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
      channel.shutdown();
      channel.awaitTermination(5, TimeUnit.SECONDS);
      server.shutdown();
      server.awaitTermination(5, TimeUnit.SECONDS);
    }
  }

  // ─── Stages ───────────────────────────────────────────────────────────────

  private static void warmup(MissionControlGrpc.MissionControlBlockingStub stub) {
    for (int i = 0; i < WARMUP; i++) {
      stub.getStatus(StatusRequest.newBuilder().setAgentId("warmup-" + i).build());
    }
  }

  private static void benchmarkUnaryGetStatus(MissionControlGrpc.MissionControlBlockingStub stub) {
    long[] latsNs = new long[UNARY_ITERATIONS];
    long t0 = System.nanoTime();
    for (int i = 0; i < UNARY_ITERATIONS; i++) {
      long start = System.nanoTime();
      stub.getStatus(StatusRequest.newBuilder().setAgentId("bench-" + i).build());
      latsNs[i] = System.nanoTime() - start;
      if (i % 100 == 99) {
        System.out.printf("  bench[%d] %.2fms%n", i, latsNs[i] / 1e6);
      }
    }
    long elapsedNs = System.nanoTime() - t0;
    printRow("Unary (getStatus)", UNARY_ITERATIONS, elapsedNs, latsNs);
  }

  private static void benchmarkUnarySubmitLog(MissionControlGrpc.MissionControlBlockingStub stub) {
    long[] latsNs = new long[UNARY_ITERATIONS];
    long t0 = System.nanoTime();
    for (int i = 0; i < UNARY_ITERATIONS; i++) {
      LogEntry entry =
          LogEntry.newBuilder()
              .setTimestamp(System.nanoTime() / 1e9)
              .setLevel("info")
              .setMessage("bench log " + i)
              .setAgentId("bench")
              .build();
      long start = System.nanoTime();
      stub.submitLog(entry);
      latsNs[i] = System.nanoTime() - start;
    }
    long elapsedNs = System.nanoTime() - t0;
    printRow("Unary (submitLog)", UNARY_ITERATIONS, elapsedNs, latsNs);
  }

  private static void benchmarkConcurrentGetStatus(
      MissionControlGrpc.MissionControlFutureStub stub) {
    int[] concurrencies = {10, 50, 100};
    for (int concurrency : concurrencies) {
      try {
        @SuppressWarnings("unchecked")
        CompletableFuture<StatusResponse>[] futures = new CompletableFuture[concurrency];
        long t0 = System.nanoTime();
        for (int i = 0; i < concurrency; i++) {
          int idx = i;
          futures[i] = new CompletableFuture<>();
          com.google.common.util.concurrent.ListenableFuture<StatusResponse> f =
              stub.getStatus(StatusRequest.newBuilder().setAgentId("concurrent-" + idx).build());
          int finalI = i;
          f.addListener(
              () -> {
                try {
                  futures[finalI].complete(f.get());
                } catch (Exception e) {
                  futures[finalI].completeExceptionally(e);
                }
              },
              Runnable::run);
        }
        CompletableFuture.allOf(futures).orTimeout(15, TimeUnit.SECONDS).get();
        long elapsedNs = System.nanoTime() - t0;
        double rps = concurrency * 1e9 / elapsedNs;
        double elapsedMs = elapsedNs / 1e6;
        System.out.printf(
            "  Concurrent (%3d)        %,12.0f req/s   %8.1f ms total%n",
            concurrency, rps, elapsedMs);
      } catch (Exception e) {
        System.out.printf(
            "  Concurrent (%3d)        FAILED — %s%n", concurrency, e.getClass().getSimpleName());
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

  // ─── Service implementation ──────────────────────────────────────────────

  private static final class MissionControlServiceImpl
      extends MissionControlGrpc.MissionControlImplBase {

    private final long startNanos = System.nanoTime();

    @Override
    public void getStatus(
        StatusRequest request, io.grpc.stub.StreamObserver<StatusResponse> responseObserver) {
      long uptimeSecs = (System.nanoTime() - startNanos) / 1_000_000_000L;
      StatusResponse response =
          StatusResponse.newBuilder()
              .setAgentId(request.getAgentId())
              .setStatus("running")
              .setUptimeSecs(uptimeSecs)
              .build();
      responseObserver.onNext(response);
      responseObserver.onCompleted();
    }

    @Override
    public void submitLog(
        LogEntry request, io.grpc.stub.StreamObserver<SubmitLogResult> responseObserver) {
      responseObserver.onNext(SubmitLogResult.newBuilder().setAccepted(true).build());
      responseObserver.onCompleted();
    }
  }

  private MissionControlGrpcBenchmark() {}
}
