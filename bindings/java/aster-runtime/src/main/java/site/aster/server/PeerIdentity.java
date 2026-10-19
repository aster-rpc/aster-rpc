package site.aster.server;

import java.util.List;

/**
 * Who a connected peer is, as verified when its first connection was admitted.
 *
 * @param endpointId the peer's verified endpoint id
 * @param principal the principal its credential names; null unless the peer has a live enrollment
 *     session
 * @param roles the roles its credential grants (sorted); empty without enrollment
 */
public record PeerIdentity(String endpointId, String principal, List<String> roles) {
  public PeerIdentity {
    roles = roles == null ? List.of() : List.copyOf(roles);
  }
}
