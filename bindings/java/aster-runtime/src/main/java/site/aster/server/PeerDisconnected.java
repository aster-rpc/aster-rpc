package site.aster.server;

/**
 * A peer's last connection ended.
 *
 * @param peer the peer's endpoint id
 * @param reason why the last connection ended
 */
public record PeerDisconnected(String peer, DisconnectReason reason) implements PeerEvent {}
