package site.aster.server;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import site.aster.client.AsterClient;
import site.aster.exception.IrohAlreadyClosedException;
import site.aster.testing.WaitBudget;

/**
 * Server peer events (C ABI 7.8): a thin mapping test over {@code aster_server_peer_events}, {@code
 * aster_peer_events_poll} and {@code aster_peer_events_release}. Semantics are tested in Rust
 * ({@code aster/tests/peer_events.rs}).
 */
final class PeerEventsE2ETest {

  private static PeerEvent next(PeerEvents events) {
    PeerEvent event = events.next(WaitBudget.duration());
    assertNotNull(event, "no peer event within the wait budget");
    return event;
  }

  @Test
  void clientConnectThenCloseReportsConnectedThenDisconnectedClosed() throws Exception {
    try (AsterServer server =
            AsterServer.builder().build().get(WaitBudget.SECONDS, TimeUnit.SECONDS);
        PeerEvents events = server.peerEvents()) {
      assertTrue(events.snapshot().isEmpty());
      AsterClient client = AsterClient.builder().build().get(WaitBudget.SECONDS, TimeUnit.SECONDS);
      String clientId = client.nodeId();
      client.connect(server.node().nodeAddr()).get(WaitBudget.SECONDS, TimeUnit.SECONDS);

      PeerConnected connected = assertInstanceOf(PeerConnected.class, next(events));
      assertEquals(clientId, connected.peer());
      assertEquals("aster/1", connected.alpn());
      assertEquals(clientId, connected.identity().endpointId());
      assertNull(connected.identity().principal());
      assertTrue(connected.identity().roles().isEmpty());

      // A subscription taken now sees the connected peer in its snapshot.
      try (PeerEvents later = server.peerEvents()) {
        assertEquals(1, later.snapshot().size());
        assertEquals(clientId, later.snapshot().get(0).peer());
        assertEquals(1, later.snapshot().get(0).connections());
      }

      client.close();
      PeerDisconnected disconnected = assertInstanceOf(PeerDisconnected.class, next(events));
      assertEquals(clientId, disconnected.peer());
      assertEquals(DisconnectReason.CLOSED, disconnected.reason());
    }
  }

  @Test
  void closeWakesAPendingNextAndFurtherReadsFail() throws Exception {
    try (AsterServer server =
        AsterServer.builder().build().get(WaitBudget.SECONDS, TimeUnit.SECONDS)) {
      PeerEvents events = server.peerEvents();
      CompletableFuture<PeerEvent> pending =
          CompletableFuture.supplyAsync(() -> events.next(WaitBudget.duration()));
      events.close();
      ExecutionException failure =
          assertThrows(
              ExecutionException.class, () -> pending.get(WaitBudget.SECONDS, TimeUnit.SECONDS));
      assertInstanceOf(IrohAlreadyClosedException.class, failure.getCause());
      assertThrows(IrohAlreadyClosedException.class, () -> events.next(java.time.Duration.ZERO));
      events.close(); // idempotent
    }
  }
}
