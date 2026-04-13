package site.aster.server.session;

/**
 * Identifies a single session-scoped service instance.
 *
 * <p>Keying is {@code (peerId, streamId, implClass)} — one session instance per {@code (peer, QUIC
 * bi-stream, service class)}. The {@code streamId} component is what makes concurrent sessions from
 * the same peer independent: two browser tabs on one machine, or two agents in one process, each
 * open their own Aster stream and get their own session state, instead of fighting over a shared
 * instance as they did in Day-0.
 *
 * <p>The reactor assigns {@code streamId} per accepted bi-stream (see {@code aster_reactor_call_t
 * .stream_id} on the FFI side). Stateless (unary / server-stream) calls always get a fresh streamId
 * because they open a fresh stream per call; session-mode calls share one streamId across the calls
 * multiplexed on the same stream.
 */
public record SessionKey(String peerId, long streamId, Class<?> implClass) {}
