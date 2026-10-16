package site.aster.handle;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotSame;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.config.AsterConfig;
import site.aster.node.IrohNode;

/**
 * ABI 4.0 removed bare endpoints: an {@link IrohEndpoint} is a view of a node started with {@code
 * aster_node_start}. Accept on a node handle pulls from the node's ALPN accept queue.
 */
final class NodeEndpointTest {

  private static final String ALPN = "aster-test/raw";

  /** Loopback-only: no relay (a default-relay start waits for the public n0 relay). */
  private static final AsterConfig LOOPBACK =
      AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build();

  @Test
  void nodeBackedEndpointsConnectAndAccept() throws Exception {
    List<byte[]> alpns = List.of(ALPN.getBytes(StandardCharsets.UTF_8));
    IrohNode a = IrohNode.start(LOOPBACK, alpns, null).get(20, TimeUnit.SECONDS);
    IrohNode b = IrohNode.start(LOOPBACK, alpns, null).get(20, TimeUnit.SECONDS);
    IrohEndpoint epA = IrohEndpoint.ofNode(a.runtime(), a.nodeHandle());
    IrohEndpoint epB = IrohEndpoint.ofNode(b.runtime(), b.nodeHandle());
    try {
      assertEquals(64, epA.nodeId().length());
      assertEquals(32, epA.exportSecretKey().length);
      var accepted = epA.acceptAsync();
      IrohConnection out = epB.connectNodeAddrAsync(epA.addrInfo(), ALPN).get(20, TimeUnit.SECONDS);
      IrohConnection in = accepted.get(20, TimeUnit.SECONDS);
      assertEquals(epB.nodeId(), in.remoteId());
      assertEquals(epA.nodeId(), out.remoteId());
      assertTrue(in.nativeHandle() != 0);
      out.close();
      in.close();
    } finally {
      epB.closeAsync().get(20, TimeUnit.SECONDS);
      epA.closeAsync().get(20, TimeUnit.SECONDS);
    }
  }

  @Test
  void explicitEndpointCloseReportsClosedState() throws Exception {
    IrohNode node =
        IrohNode.start(LOOPBACK, List.of(ALPN.getBytes(StandardCharsets.UTF_8)), null)
            .get(20, TimeUnit.SECONDS);
    IrohEndpoint endpoint = IrohEndpoint.ofNode(node.runtime(), node.nodeHandle());
    var closing = endpoint.closeAsync();
    var joined = endpoint.closeAsync();
    assertNotSame(closing, joined);
    closing.cancel(false);
    joined.get(20, TimeUnit.SECONDS);
    endpoint.close();
    endpoint.closeAsync().get(20, TimeUnit.SECONDS);
    assertTrue(endpoint.isClosed(), "explicit endpoint close must update inherited state");
  }
}
