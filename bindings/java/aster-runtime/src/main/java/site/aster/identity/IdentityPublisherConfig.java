package site.aster.identity;

import java.util.List;
import java.util.Map;

/**
 * {@code [identity.publisher]} (Rust {@code aster::IdentityPublisherConfig}; env {@code
 * ASTER_IDENTITY_PUBLISHER}): on the node that publishes issuer keys, normally the gateway. It
 * fetches each issuer's JWK set over validated HTTPS and writes it into the fleet namespace. Needs
 * Enrollment, a credential granting {@link GatewayIdentity#IDENTITY_PUBLISHER_ROLE} and the root's
 * sealed write grant ({@code aster enroll identity-publisher}). A server runs it from start, a
 * client from its first enrollment.
 *
 * @param issuers the issuers to publish
 * @param jwksUrl each issuer's https JWK set URL
 * @param refreshSecs the scheduled refresh (60..86400); {@code null} = 3600
 * @param minRefreshSecs bounds unknown-kid refreshes per issuer; {@code null} = 60
 * @param caCerts extra PEM trust anchors for the providers' TLS
 * @param caOnly trust only {@code caCerts}
 */
public record IdentityPublisherConfig(
    List<String> issuers,
    Map<String, String> jwksUrl,
    Long refreshSecs,
    Long minRefreshSecs,
    List<String> caCerts,
    boolean caOnly) {

  public IdentityPublisherConfig {
    issuers = issuers == null ? List.of() : List.copyOf(issuers);
    jwksUrl = jwksUrl == null || jwksUrl.isEmpty() ? null : Map.copyOf(jwksUrl);
    caCerts = caCerts == null || caCerts.isEmpty() ? null : List.copyOf(caCerts);
  }

  /** A publisher for {@code issuers} with the defaults. */
  public static IdentityPublisherConfig of(List<String> issuers, Map<String, String> jwksUrl) {
    return new IdentityPublisherConfig(issuers, jwksUrl, null, null, null, false);
  }
}
