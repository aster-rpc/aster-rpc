package site.aster.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.codec.ForyCodec;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * §12.4 merge gate: a bounded response channel must still deliver a long server stream, and the
 * Java side must survive backpressure without pinning a carrier thread.
 *
 * <p>The reactor's per-call response channel is bounded, so a producer faster than the peer's
 * reader eventually fills it. Native code never blocks on that — it reserves capacity first and
 * returns {@code QUEUE_FULL} without copying the frame — and {@code Reactor} retries outside the
 * downcall after an asynchronous native capacity reservation wakes it through the reactor poll
 * ring.
 *
 * <p>The deterministic retry state-machine test lives beside {@code Reactor}; the warmed JMH gate
 * records real native capacity handoffs. This integration test owns the complementary property: the
 * bounded end-to-end stream still completes, in order, with nothing dropped. It intentionally does
 * not infer that the native queue filled from scheduler timing in one cold call.
 */
final class ServerStreamBackpressureTest {

  /** Comfortably more frames than one ordinary response burst. */
  private static final int FRAMES = 2_000;

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

  @Test
  void aLongServerStreamCompletesUnderBackpressure() throws Exception {
    AsterServer server =
        AsterServer.builder()
            .codec(codec())
            .service(new EchoServiceDispatcher.Impl())
            .build()
            .get(15, TimeUnit.SECONDS);

    try (AsterClient client =
        AsterClient.builder().codec(codec()).build().get(15, TimeUnit.SECONDS)) {
      NodeAddr serverAddr = server.node().nodeAddr();

      long startNanos = System.nanoTime();
      List<EchoServiceDispatcher.EchoResponse> responses =
          client
              .<EchoServiceDispatcher.EchoStreamRequest, EchoServiceDispatcher.EchoResponse>
                  callServerStream(
                      serverAddr,
                      EchoServiceDispatcher.SERVICE_NAME,
                      EchoServiceDispatcher.STREAM_METHOD_NAME,
                      new EchoServiceDispatcher.EchoStreamRequest("bp", FRAMES),
                      EchoServiceDispatcher.EchoResponse.class)
              .orTimeout(120, TimeUnit.SECONDS)
              .get();
      long elapsedMillis = (System.nanoTime() - startNanos) / 1_000_000;
      // Nothing dropped under a bounded response path.
      assertEquals(FRAMES, responses.size(), "every frame must arrive");

      // And in order: retrying must not reorder or duplicate.
      for (int i = 0; i < FRAMES; i++) {
        assertEquals("stream:bp:" + i, responses.get(i).message(), "frame " + i + " out of order");
      }

      // Eventual progress, not a livelock. Generous because CI machines are
      // slow and this is a correctness gate, not the throughput measurement —
      // that lives in ServerStreamThroughputBenchmark.
      assertTrue(
          elapsedMillis < 60_000,
          "stream took "
              + elapsedMillis
              + "ms; capacity notification may be failing to make progress");
    } finally {
      server.close();
    }
  }

  /**
   * A client that abandons the call must not leave the server retrying forever.
   *
   * <p>When the reader goes away the channel closes, and native code reports that as {@code
   * NOT_FOUND} rather than {@code QUEUE_FULL} — a distinction the retry loop depends on. Conflating
   * the two would spin on a dead call.
   */
  @Test
  void abandoningTheReaderEndsTheStreamRatherThanSpinning() throws Exception {
    AsterServer server =
        AsterServer.builder()
            .codec(codec())
            .service(new EchoServiceDispatcher.Impl())
            .build()
            .get(15, TimeUnit.SECONDS);

    long startNanos;
    try (AsterClient client =
        AsterClient.builder().codec(codec()).build().get(15, TimeUnit.SECONDS)) {
      NodeAddr serverAddr = server.node().nodeAddr();
      startNanos = System.nanoTime();
      // Start a long stream and drop it immediately by closing the client.
      client.callServerStream(
          serverAddr,
          EchoServiceDispatcher.SERVICE_NAME,
          EchoServiceDispatcher.STREAM_METHOD_NAME,
          new EchoServiceDispatcher.EchoStreamRequest("abandon", FRAMES),
          EchoServiceDispatcher.EchoResponse.class);
    }

    // Closing the server must not hang behind a producer stuck in a retry loop.
    server.close();
    long elapsedMillis = (System.nanoTime() - startNanos) / 1_000_000;
    assertTrue(
        elapsedMillis < 30_000,
        "shutdown took " + elapsedMillis + "ms; a closed channel is likely being retried as full");
  }
}
