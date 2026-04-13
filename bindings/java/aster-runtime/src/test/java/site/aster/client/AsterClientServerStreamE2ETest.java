package site.aster.client;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.codec.ForyCodec;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * End-to-end Java-to-Java server-streaming smoke test. Proves the reactor's new multi-frame submit
 * path ({@code aster_reactor_submit_frame} + {@code aster_reactor_submit_trailer}) wired through
 * {@link site.aster.server.ReactorResponseStream} delivers N response frames plus a trailer over a
 * single QUIC bi-stream, and that the client's {@code callServerStream} assembles them in order.
 */
final class AsterClientServerStreamE2ETest {

  @Test
  void serverStreamRoundTrip() throws Exception {
    ForyCodec serverCodec = new ForyCodec();
    serverCodec
        .fory()
        .register(EchoServiceDispatcher.EchoRequest.class, EchoServiceDispatcher.REQ_TYPE_TAG);
    serverCodec
        .fory()
        .register(EchoServiceDispatcher.EchoResponse.class, EchoServiceDispatcher.RESP_TYPE_TAG);
    serverCodec
        .fory()
        .register(
            EchoServiceDispatcher.EchoStreamRequest.class,
            EchoServiceDispatcher.STREAM_REQ_TYPE_TAG);

    ForyCodec clientCodec = new ForyCodec();
    clientCodec
        .fory()
        .register(EchoServiceDispatcher.EchoRequest.class, EchoServiceDispatcher.REQ_TYPE_TAG);
    clientCodec
        .fory()
        .register(EchoServiceDispatcher.EchoResponse.class, EchoServiceDispatcher.RESP_TYPE_TAG);
    clientCodec
        .fory()
        .register(
            EchoServiceDispatcher.EchoStreamRequest.class,
            EchoServiceDispatcher.STREAM_REQ_TYPE_TAG);

    AsterServer server =
        AsterServer.builder()
            .codec(serverCodec)
            .service(new EchoServiceDispatcher.Impl())
            .build()
            .get(15, TimeUnit.SECONDS);

    try (AsterClient client =
        AsterClient.builder().codec(clientCodec).build().get(15, TimeUnit.SECONDS)) {
      NodeAddr serverAddr = server.node().nodeAddr();

      int n = 5;
      List<EchoServiceDispatcher.EchoResponse> responses =
          client
              .<EchoServiceDispatcher.EchoStreamRequest, EchoServiceDispatcher.EchoResponse>
                  callServerStream(
                      serverAddr,
                      EchoServiceDispatcher.SERVICE_NAME,
                      EchoServiceDispatcher.STREAM_METHOD_NAME,
                      new EchoServiceDispatcher.EchoStreamRequest("tick", n),
                      EchoServiceDispatcher.EchoResponse.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();

      assertEquals(n, responses.size());
      for (int i = 0; i < n; i++) {
        assertEquals("stream:tick:" + i, responses.get(i).message());
      }
    } finally {
      server.close();
    }
  }
}
