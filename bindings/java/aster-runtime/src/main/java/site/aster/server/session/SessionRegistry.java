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
   * The factory receives the peer id (for ergonomics — sessions still want to know who they're
   * talking to even though peer is no longer part of the key).
   */
  Object getOrCreate(SessionKey key, String peerId, Function<String, Object> factory);

  /**
   * Dispose every session instance associated with {@code connectionId}. Called by the runtime when
   * the underlying QUIC connection closes (multiplexed-streams spec §7.5: sessions are scoped
   * per-{@code (peer, connection)} and reaped on connection close, not on peer disconnect).
   */
  void onConnectionClosed(long connectionId);

  /** Dispose every session instance. Called by the runtime at shutdown. */
  void clear();
}
