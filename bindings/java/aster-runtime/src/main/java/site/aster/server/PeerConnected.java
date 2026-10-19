package site.aster.server;

/**
 * A peer's first admitted connection opened.
 *
 * @param peer the peer's endpoint id
 * @param alpn the ALPN of that connection ({@code aster/1})
 * @param identity who it is
 */
public record PeerConnected(String peer, String alpn, PeerIdentity identity) implements PeerEvent {}
