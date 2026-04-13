package site.aster.client;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.codec.ForyCodec;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * End-to-end Java-to-Java smoke test for {@link AsterClient#call}: spins up an {@link AsterServer}
 * with a hand-written {@link EchoServiceDispatcher}, dials it with an {@link AsterClient} on the
 * same process, and asserts a round-trip over the real Iroh transport.
 *
 * <p>This is the first test that exercises the full wire format — framed {@code StreamHeader},
 * framed request, framed response, framed {@code TRAILER} — on a live QUIC stream. Proving the Java
 * wire path lines up with the reactor's parser is the main point of commit G.1.
 */
final class AsterClientUnaryE2ETest {

  @Test
  void unaryEchoRoundTrip() throws Exception {
    ForyCodec serverCodec = new ForyCodec();
    serverCodec
        .fory()
        .register(EchoServiceDispatcher.EchoRequest.class, EchoServiceDispatcher.REQ_TYPE_TAG);
    serverCodec
        .fory()
        .register(EchoServiceDispatcher.EchoResponse.class, EchoServiceDispatcher.RESP_TYPE_TAG);

    ForyCodec clientCodec = new ForyCodec();
    clientCodec
        .fory()
        .register(EchoServiceDispatcher.EchoRequest.class, EchoServiceDispatcher.REQ_TYPE_TAG);
    clientCodec
        .fory()
        .register(EchoServiceDispatcher.EchoResponse.class, EchoServiceDispatcher.RESP_TYPE_TAG);

    AsterServer server =
        AsterServer.builder()
            .codec(serverCodec)
            .service(new EchoServiceDispatcher.Impl())
            .build()
            .get(15, TimeUnit.SECONDS);

    try (AsterClient client =
        AsterClient.builder().codec(clientCodec).build().get(15, TimeUnit.SECONDS)) {
      NodeAddr serverAddr = server.node().nodeAddr();

      EchoServiceDispatcher.EchoResponse resp =
          client
              .<EchoServiceDispatcher.EchoRequest, EchoServiceDispatcher.EchoResponse>call(
                  serverAddr,
                  EchoServiceDispatcher.SERVICE_NAME,
                  EchoServiceDispatcher.METHOD_NAME,
                  new EchoServiceDispatcher.EchoRequest("ping"),
                  EchoServiceDispatcher.EchoResponse.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();

      assertEquals("echo:ping", resp.message());
    } finally {
      server.close();
    }
  }
}
