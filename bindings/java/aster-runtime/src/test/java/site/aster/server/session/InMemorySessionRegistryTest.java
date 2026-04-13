package site.aster.server.session;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotSame;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;

final class InMemorySessionRegistryTest {

  static final class DummySession implements AutoCloseable {
    final String peer;
    volatile boolean closed;

    DummySession(String peer) {
      this.peer = peer;
    }

    @Override
    public void close() {
      closed = true;
    }
  }

  @Test
  void getOrCreateReturnsSameInstanceForSameKey() {
    InMemorySessionRegistry reg = new InMemorySessionRegistry();
    SessionKey key = new SessionKey("peer-1", DummySession.class);

    Object first = reg.getOrCreate(key, DummySession::new);
    Object second =
        reg.getOrCreate(
            key,
            p -> {
              throw new AssertionError("factory must not run for existing key");
            });

    assertSame(first, second);
    assertEquals(1, reg.size());
  }

  @Test
  void getOrCreateReturnsDistinctInstancesForDifferentPeers() {
    InMemorySessionRegistry reg = new InMemorySessionRegistry();
    Object a = reg.getOrCreate(new SessionKey("peer-a", DummySession.class), DummySession::new);
    Object b = reg.getOrCreate(new SessionKey("peer-b", DummySession.class), DummySession::new);
    assertNotSame(a, b);
    assertEquals(2, reg.size());
  }

  @Test
  void onPeerDisconnectedRemovesAndClosesAutoCloseableInstances() {
    InMemorySessionRegistry reg = new InMemorySessionRegistry();
    DummySession s1 =
        (DummySession)
            reg.getOrCreate(new SessionKey("peer-1", DummySession.class), DummySession::new);
    DummySession s2 =
        (DummySession)
            reg.getOrCreate(new SessionKey("peer-2", DummySession.class), DummySession::new);

    reg.onPeerDisconnected("peer-1");

    assertTrue(s1.closed);
    assertEquals(false, s2.closed);
    assertEquals(1, reg.size());
  }

  @Test
  void clearRemovesAndClosesEverything() {
    InMemorySessionRegistry reg = new InMemorySessionRegistry();
    DummySession s1 =
        (DummySession)
            reg.getOrCreate(new SessionKey("peer-1", DummySession.class), DummySession::new);
    DummySession s2 =
        (DummySession)
            reg.getOrCreate(new SessionKey("peer-2", DummySession.class), DummySession::new);

    reg.clear();

    assertTrue(s1.closed);
    assertTrue(s2.closed);
    assertEquals(0, reg.size());
  }

  @Test
  void factoryRunsExactlyOncePerKey() {
    InMemorySessionRegistry reg = new InMemorySessionRegistry();
    AtomicInteger calls = new AtomicInteger();
    SessionKey key = new SessionKey("peer-1", DummySession.class);

    for (int i = 0; i < 10; i++) {
      reg.getOrCreate(
          key,
          p -> {
            calls.incrementAndGet();
            return new DummySession(p);
          });
    }

    assertEquals(1, calls.get());
  }
}
