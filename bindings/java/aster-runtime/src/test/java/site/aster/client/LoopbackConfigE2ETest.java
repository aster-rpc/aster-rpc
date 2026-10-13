package site.aster.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.codec.ForyCodec;
import site.aster.config.AsterConfig;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;

/**
 * ABI 3.2: server and client start through {@code aster_node_start}, so the config's relay mode and
 * bind address apply. Loopback only, relays off: runs offline.
 */
final class LoopbackConfigE2ETest {

  static AsterConfig loopback() {
    return AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build();
  }

  static ForyCodec codec() {
    ForyCodec c = new ForyCodec();
    c.fory().register(EchoServiceDispatcher.EchoRequest.class, EchoServiceDispatcher.REQ_TYPE_TAG);
    c.fory()
        .register(EchoServiceDispatcher.EchoResponse.class, EchoServiceDispatcher.RESP_TYPE_TAG);
    return c;
  }

  @Test
  void loopbackServerAndClientWithRelayDisabled() throws Exception {
    AsterServer server =
        AsterServer.builder()
            .config(loopback())
            .codec(codec())
            .service(new EchoServiceDispatcher.Impl())
            .build()
            .get(30, TimeUnit.SECONDS);
    try (AsterClient client =
        AsterClient.builder().config(loopback()).codec(codec()).build().get(30, TimeUnit.SECONDS)) {
      NodeAddr addr = server.node().nodeAddr();
      for (int i = 0; i < 40 && addr.directAddresses().isEmpty(); i++) {
        Thread.sleep(50);
        addr = server.node().nodeAddr();
      }
      assertFalse(addr.directAddresses().isEmpty(), "server has direct addresses");
      for (String a : addr.directAddresses()) {
        assertTrue(a.startsWith("127.0.0.1:"), "bound to loopback: " + a);
      }
      assertEquals(null, addr.relayUrl(), "relay disabled");
      assertEquals(64, client.nodeId().length());

      EchoServiceDispatcher.EchoResponse resp =
          client
              .<EchoServiceDispatcher.EchoRequest, EchoServiceDispatcher.EchoResponse>call(
                  addr,
                  EchoServiceDispatcher.SERVICE_NAME,
                  EchoServiceDispatcher.METHOD_NAME,
                  new EchoServiceDispatcher.EchoRequest("loop"),
                  EchoServiceDispatcher.EchoResponse.class)
              .orTimeout(15, TimeUnit.SECONDS)
              .get();
      assertEquals("echo:loop", resp.message());
    } finally {
      server.close();
    }
  }
}
