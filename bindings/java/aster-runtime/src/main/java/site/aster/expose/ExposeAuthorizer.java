package site.aster.expose;

/**
 * Admits or refuses a peer, once per {@code (connection, service)} (upcall kind {@code
 * expose_authorize}). It is only an allow / deny callback: returning {@code true} admits, {@code
 * false} refuses. When it throws, times out or the table is overloaded, the kind's failure mode
 * applies: <b>fail-closed (deny) unless configured otherwise</b> with {@code
 * expose.upcalls().setPolicy(UpcallKind.EXPOSE_AUTHORIZE, ...)}. It runs on a virtual thread, so it
 * may block.
 */
@FunctionalInterface
public interface ExposeAuthorizer {

  /**
   * @param serviceId the exposed service the peer opens
   * @param peerId the transport-authenticated peer (node id, hex)
   * @param metadata the consumer's opaque open-frame bytes (empty if none)
   */
  boolean authorize(String serviceId, String peerId, byte[] metadata) throws Exception;
}
