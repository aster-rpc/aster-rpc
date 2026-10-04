package site.aster.benchmarks;

import java.util.List;
import java.util.concurrent.TimeUnit;
import org.openjdk.jmh.annotations.Benchmark;
import org.openjdk.jmh.annotations.BenchmarkMode;
import org.openjdk.jmh.annotations.Fork;
import org.openjdk.jmh.annotations.Measurement;
import org.openjdk.jmh.annotations.Mode;
import org.openjdk.jmh.annotations.OutputTimeUnit;
import org.openjdk.jmh.annotations.Param;
import org.openjdk.jmh.annotations.Scope;
import org.openjdk.jmh.annotations.Setup;
import org.openjdk.jmh.annotations.State;
import org.openjdk.jmh.annotations.TearDown;
import org.openjdk.jmh.annotations.Warmup;
import site.aster.client.AsterClient;
import site.aster.client.EchoServiceDispatcher;
import site.aster.codec.ForyCodec;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * Server-stream response throughput — the merge gate for the §12.4 bounded response channel.
 *
 * <p>The existing {@link IrohBenchmark} measures per-call FFI overhead, which says nothing about
 * this change: bounding the response channel costs a {@code try_reserve} per frame on the fast path
 * and, when the peer is behind, a retry. Neither shows up in a unary microbenchmark. This benchmark
 * exists because the property that matters is end-to-end streaming throughput with a reader keeping
 * up. The response path preserves the former eight-maximum-frame byte envelope while allowing up to
 * 256 tiny items, so a producer can still outrun that reader in bursts and exercise backpressure
 * even though the reader is making progress.
 *
 * <p>Interpretation: {@code frames = 1} is close to unary and isolates the no-backpressure fixed
 * overhead; its {@code QUEUE_FULL} count should be zero. The larger sizes measure the bounded
 * streaming path where per-capacity-notification scheduler/wakeup costs compound. A material
 * end-to-end regression here is what should block the merge.
 *
 * <p>This source is compiled with the cached JMH annotation processor and run with {@code
 * org.openjdk.jmh.Main}; it is not a JUnit test. Record the reported {@code QUEUE_FULL} count for
 * every fork alongside the timing.
 */
@State(Scope.Benchmark)
@BenchmarkMode(Mode.AverageTime)
@OutputTimeUnit(TimeUnit.MILLISECONDS)
@Warmup(iterations = 3, time = 2, timeUnit = TimeUnit.SECONDS)
@Measurement(iterations = 5, time = 2, timeUnit = TimeUnit.SECONDS)
@Fork(1)
public class ServerStreamThroughputBenchmark {

  /** 1 isolates fixed cost; 1000 is where a per-frame regression compounds. */
  @Param({"1", "100", "1000"})
  public int frames;

  private static final java.util.concurrent.atomic.LongAdder OPS =
      new java.util.concurrent.atomic.LongAdder();

  private AsterServer server;
  private AsterClient client;
  private NodeAddr serverAddr;

  private static ForyCodec codec() {
    ForyCodec c = new ForyCodec();
    c.fory().register(EchoServiceDispatcher.EchoRequest.class, EchoServiceDispatcher.REQ_TYPE_TAG);
    c.fory()
        .register(EchoServiceDispatcher.EchoResponse.class, EchoServiceDispatcher.RESP_TYPE_TAG);
    c.fory()
        .register(
            EchoServiceDispatcher.EchoStreamRequest.class,
            EchoServiceDispatcher.STREAM_REQ_TYPE_TAG);
    return c;
  }

  @Setup
  public void setUp() throws Exception {
    server =
        AsterServer.builder()
            .codec(codec())
            .service(new EchoServiceDispatcher.Impl())
            .build()
            .get(30, TimeUnit.SECONDS);
    client = AsterClient.builder().codec(codec()).build().get(30, TimeUnit.SECONDS);
    serverAddr = server.node().nodeAddr();
    site.aster.ffi.Dispatch.resetQueueFullStats();
    OPS.reset();
  }

  /// Printed once per fork so the gate can distinguish the no-backpressure
  /// fast path from sustained bounded-response backpressure.
  @TearDown
  public void reportQueueFull() {
    System.out.println(
        "[gate] frames="
            + frames
            + " QUEUE_FULL="
            + site.aster.ffi.Dispatch.queueFullHits()
            + " ops="
            + OPS.sum()
            + " eventsPerOp="
            + (OPS.sum() == 0
                ? 0.0
                : (double) site.aster.ffi.Dispatch.queueFullHits() / OPS.sum()));
  }

  @TearDown
  public void tearDown() throws Exception {
    if (client != null) {
      client.close();
    }
    if (server != null) {
      server.close();
    }
  }

  /**
   * One full server-streaming call with a reader making continuous progress. The one-frame case is
   * the no-backpressure fast path; larger cases intentionally measure the capacity-notification
   * path as producer and reader scheduling diverge.
   */
  @Benchmark
  public int serverStreamWithReaderKeepingUp() throws Exception {
    OPS.increment();
    List<EchoServiceDispatcher.EchoResponse> responses =
        client
            .<EchoServiceDispatcher.EchoStreamRequest, EchoServiceDispatcher.EchoResponse>
                callServerStream(
                    serverAddr,
                    EchoServiceDispatcher.SERVICE_NAME,
                    EchoServiceDispatcher.STREAM_METHOD_NAME,
                    // The resulting framed response averaged 64 bytes at
                    // frames=1000 in the profiling run that selected this fixture.
                    new EchoServiceDispatcher.EchoStreamRequest("bb", frames),
                    EchoServiceDispatcher.EchoResponse.class)
            .orTimeout(120, TimeUnit.SECONDS)
            .get();
    if (responses.size() != frames) {
      throw new AssertionError(
          "response backpressure dropped frames: expected " + frames + ", got " + responses.size());
    }
    return responses.size();
  }
}
