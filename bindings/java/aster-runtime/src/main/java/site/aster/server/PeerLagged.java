package site.aster.server;

/**
 * The subscription fell behind its bounded queue and {@code missed} events were dropped. The
 * consumer's state is stale: close the {@link PeerEvents}, call {@link AsterServer#peerEvents()}
 * again and rebuild from the new snapshot.
 *
 * @param missed how many events were dropped
 */
public record PeerLagged(long missed) implements PeerEvent {}
