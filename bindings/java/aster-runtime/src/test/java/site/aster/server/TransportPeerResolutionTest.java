package site.aster.server;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;

import org.junit.jupiter.api.Test;
import site.aster.ffi.Dispatch;

/**
 * A call whose {@code aster_dispatch_identity} read finds it over ({@code NOT_FOUND}) is dropped:
 * its transport peer is not the principal, so neither the transport peer nor a forwarded call's
 * actor reports the user.
 */
final class TransportPeerResolutionTest {

  private static final String USER = "https://idp|alice";

  @Test
  void liveCallUsesTheIdentityTransportPeer() {
    assertEquals(
        "gateway-node",
        AsterServer.resolveTransportPeer(
            Dispatch.FLAG_HAS_PRINCIPAL, USER, new Dispatch.Identity("gateway-node", USER)));
  }

  @Test
  void callAlreadyOverHasNoTransportPeer() {
    assertNull(AsterServer.resolveTransportPeer(Dispatch.FLAG_HAS_PRINCIPAL, USER, null));
  }

  @Test
  void withoutAPrincipalTheEventPeerIsTheTransportPeer() {
    assertEquals("node-a", AsterServer.resolveTransportPeer(0, "node-a", null));
  }
}
