package site.aster.server;

/**
 * One change in which peers are connected, per peer and not per connection: {@link PeerConnected},
 * {@link PeerDisconnected} or {@link PeerLagged}.
 */
public sealed interface PeerEvent permits PeerConnected, PeerDisconnected, PeerLagged {}
