package site.aster.server.session;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.function.Function;

/**
 * Default in-memory {@link SessionRegistry}. Thread-safe, unbounded. Suitable for Day 0 usage; a
 * production implementation can swap in eviction, metrics, or persistent storage by implementing
 * {@link SessionRegistry} directly.
 */
public final class InMemorySessionRegistry implements SessionRegistry {

  private final Map<SessionKey, Object> instances = new ConcurrentHashMap<>();

  @Override
  public Object getOrCreate(SessionKey key, String peerId, Function<String, Object> factory) {
    return instances.computeIfAbsent(key, k -> factory.apply(peerId));
  }

  @Override
  public void onConnectionClosed(long connectionId) {
    List<SessionKey> toRemove = new ArrayList<>();
    for (SessionKey k : instances.keySet()) {
      if (k.connectionId() == connectionId) {
        toRemove.add(k);
      }
    }
    for (SessionKey k : toRemove) {
      Object instance = instances.remove(k);
      closeIfAutoCloseable(instance);
    }
  }

  @Override
  public void clear() {
    for (Object instance : instances.values()) {
      closeIfAutoCloseable(instance);
    }
    instances.clear();
  }

  /** Exposed for tests. */
  public int size() {
    return instances.size();
  }

  private static void closeIfAutoCloseable(Object instance) {
    if (instance instanceof AutoCloseable closeable) {
      try {
        closeable.close();
      } catch (Exception ignored) {
        // Best-effort dispose; a failing close must not strand the registry.
      }
    }
  }
}
