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
    SessionKey key = new SessionKey(1L, 1, DummySession.class);

    Object first = reg.getOrCreate(key, "peer-1", DummySession::new);
    Object second =
        reg.getOrCreate(
            key,
            "peer-1",
            p -> {
              throw new AssertionError("factory must not run for existing key");
            });

    assertSame(first, second);
    assertEquals(1, reg.size());
  }

  @Test
  void getOrCreateReturnsDistinctInstancesForDifferentConnections() {
    InMemorySessionRegistry reg = new InMemorySessionRegistry();
    Object a =
        reg.getOrCreate(new SessionKey(1L, 1, DummySession.class), "peer-a", DummySession::new);
    Object b =
        reg.getOrCreate(new SessionKey(2L, 1, DummySession.class), "peer-b", DummySession::new);
    assertNotSame(a, b);
    assertEquals(2, reg.size());
  }

  @Test
  void getOrCreateReturnsDistinctInstancesForDifferentSessionsOnSameConnection() {
    InMemorySessionRegistry reg = new InMemorySessionRegistry();
    Object a =
        reg.getOrCreate(new SessionKey(1L, 1, DummySession.class), "peer-1", DummySession::new);
    Object b =
        reg.getOrCreate(new SessionKey(1L, 2, DummySession.class), "peer-1", DummySession::new);
    assertNotSame(a, b);
    assertEquals(2, reg.size());
  }

  @Test
  void onConnectionClosedRemovesAndClosesAutoCloseableInstances() {
    InMemorySessionRegistry reg = new InMemorySessionRegistry();
    DummySession s1 =
        (DummySession)
            reg.getOrCreate(new SessionKey(1L, 1, DummySession.class), "peer-1", DummySession::new);
    DummySession s2 =
        (DummySession)
            reg.getOrCreate(new SessionKey(2L, 1, DummySession.class), "peer-2", DummySession::new);

    reg.onConnectionClosed(1L);

    assertTrue(s1.closed);
    assertEquals(false, s2.closed);
    assertEquals(1, reg.size());
  }

  @Test
  void clearRemovesAndClosesEverything() {
    InMemorySessionRegistry reg = new InMemorySessionRegistry();
    DummySession s1 =
        (DummySession)
            reg.getOrCreate(new SessionKey(1L, 1, DummySession.class), "peer-1", DummySession::new);
    DummySession s2 =
        (DummySession)
            reg.getOrCreate(new SessionKey(2L, 1, DummySession.class), "peer-2", DummySession::new);

    reg.clear();

    assertTrue(s1.closed);
    assertTrue(s2.closed);
    assertEquals(0, reg.size());
  }

  @Test
  void factoryRunsExactlyOncePerKey() {
    InMemorySessionRegistry reg = new InMemorySessionRegistry();
    AtomicInteger calls = new AtomicInteger();
    SessionKey key = new SessionKey(1L, 1, DummySession.class);

    for (int i = 0; i < 10; i++) {
      reg.getOrCreate(
          key,
          "peer-1",
          p -> {
            calls.incrementAndGet();
            return new DummySession(p);
          });
    }

    assertEquals(1, calls.get());
  }
}
