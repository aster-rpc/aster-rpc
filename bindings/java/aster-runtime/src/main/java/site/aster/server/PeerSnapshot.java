package site.aster.server;

/**
 * A peer that was connected when {@link AsterServer#peerEvents()} was called.
 *
 * @param peer the peer's endpoint id
 * @param alpn the ALPN of its first admitted connection ({@code aster/1})
 * @param connections how many admitted connections it has
 * @param identity who it is
 */
public record PeerSnapshot(String peer, String alpn, int connections, PeerIdentity identity) {}
