package site.aster.server.session;

/**
 * Identifies a single session-scoped service instance (multiplexed-streams spec §6 / §7.5).
 *
 * <p>Keying is {@code (connectionId, sessionId, implClass)}: one session instance per {@code (QUIC
 * connection, client-allocated sessionId, service class)}. The {@code connectionId} makes
 * concurrent connections from the same peer-identity independent (two browser tabs on one machine
 * each open their own connection and get their own session graveyard); the {@code sessionId}
 * discriminates concurrent sessions on the same connection.
 *
 * <p>{@code peerId} is no longer part of the key — it is implied by {@code connectionId} (one peer
 * per connection) and any caller wanting to surface it to user code carries it separately via the
 * {@code CallContext}.
 */
public record SessionKey(long connectionId, int sessionId, Class<?> implClass) {}
