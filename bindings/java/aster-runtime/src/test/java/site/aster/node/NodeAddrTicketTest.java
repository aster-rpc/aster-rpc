package site.aster.node;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * Round-trips {@link NodeAddr#toTicket()} through {@link NodeAddr#fromTicket(String)} to validate
 * the encode FFI matches the decode FFI — and, crucially, that Python / TypeScript / Java agree on
 * the wire format for {@code aster1…} tickets.
 */
final class NodeAddrTicketTest {

  private static final String ENDPOINT_ID =
      "0b7f200cdb5d6b5ea5807ccc61de064a01699610929f2ad2281a1bcff50050e6";

  @Test
  void endpointOnly() {
    NodeAddr orig = new NodeAddr(ENDPOINT_ID, null, List.of());
    String ticket = orig.toTicket();
    assertTrue(ticket.startsWith("aster1"), "ticket must start with aster1");

    NodeAddr decoded = NodeAddr.fromTicket(ticket);
    assertEquals(ENDPOINT_ID, decoded.endpointId());
    assertNull(decoded.relayUrl());
    assertEquals(List.of(), decoded.directAddresses());
  }

  @Test
  void withRelayAndDirects() {
    NodeAddr orig =
        new NodeAddr(ENDPOINT_ID, "5.223.60.113:443", List.of("192.168.1.2:56089", "10.0.0.5:443"));
    String ticket = orig.toTicket();
    NodeAddr decoded = NodeAddr.fromTicket(ticket);
    // The ticket's `relay_addr` is a STUN-resolved ip:port (not a RelayUrl), so fromTicket
    // surfaces it as a direct-address hint and leaves relayUrl null — matching TypeScript's
    // parseTicket path. core_to_endpoint_addr would otherwise fail RelayUrl::parse on ip:port.
    assertEquals(ENDPOINT_ID, decoded.endpointId());
    assertNull(decoded.relayUrl());
    assertTrue(decoded.directAddresses().contains("192.168.1.2:56089"));
    assertTrue(decoded.directAddresses().contains("10.0.0.5:443"));
    assertTrue(decoded.directAddresses().contains("5.223.60.113:443"));
  }

  @Test
  void urlRelayIsOmittedByDefault() {
    NodeAddr orig =
        new NodeAddr(ENDPOINT_ID, "https://aps1-1.relay.n0.iroh-canary.iroh.link./", List.of());
    String ticket = orig.toTicket();
    NodeAddr decoded = NodeAddr.fromTicket(ticket);
    assertEquals(ENDPOINT_ID, decoded.endpointId());
    assertNull(decoded.relayUrl(), "URL-style relay must be omitted from ticket");
  }

  @Test
  void explicitUrlRelayRejected() {
    NodeAddr orig = new NodeAddr(ENDPOINT_ID, null, List.of());
    assertThrows(
        IllegalArgumentException.class,
        () -> orig.toTicket("https://relay.example.com/", List.of()));
  }

  @Test
  void explicitDirectUrlRejected() {
    NodeAddr orig = new NodeAddr(ENDPOINT_ID, null, List.of());
    assertThrows(
        IllegalArgumentException.class,
        () -> orig.toTicket(null, List.of("https://host.example.com:443")));
  }

  @Test
  void missingEndpointRejected() {
    NodeAddr orig = new NodeAddr("", null, List.of());
    assertThrows(IllegalStateException.class, orig::toTicket);
  }
}
