package site.aster.server.session;

import java.util.function.Function;

/**
 * Lifecycle manager for session-scoped service instances.
 *
 * <p>A {@link SessionRegistry} holds at most one instance of each session-scoped service class per
 * session key. When a connection closes, the registry disposes every instance associated with that
 * peer; instances that implement {@link AutoCloseable} are closed, other instances are dropped.
 */
public interface SessionRegistry {

  /**
   * Return the session instance for {@code key}, creating one via {@code factory} if none exists.
   * The factory receives the peer id and must return a new service instance.
   */
  Object getOrCreate(SessionKey key, Function<String, Object> factory);

  /**
   * Dispose every session instance associated with {@code peerId}. Called by the runtime when the
   * underlying connection closes.
   */
  void onPeerDisconnected(String peerId);

  /** Dispose every session instance. Called by the runtime at shutdown. */
  void clear();
}
