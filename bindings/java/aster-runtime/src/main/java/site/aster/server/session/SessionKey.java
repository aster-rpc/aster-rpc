package site.aster.server.session;

/**
 * Identifies a single session-scoped service instance.
 *
 * <p>Day 0 keying is {@code (peerId, implClass)} — one session per peer per service class. This is
 * correct for the MissionControl agent case (one agent process per peer) but collapses multiple
 * concurrent streams from the same peer onto a single session instance. Widening this to include a
 * stream id is tracked as a post-Commit-G follow-up that also requires a reactor FFI change to
 * surface the QUIC stream id on {@code iroh_call_t}.
 */
public record SessionKey(String peerId, Class<?> implClass) {}
