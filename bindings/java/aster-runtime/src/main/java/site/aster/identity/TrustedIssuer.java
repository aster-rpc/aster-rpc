package site.aster.identity;

import java.util.List;
import java.util.Map;

/**
 * An identity provider whose tokens a server accepts in {@link
 * GatewayIdentity#ON_BEHALF_OF_METADATA} ({@code [[identity.trusted_issuers]]}; Rust {@code
 * aster::TrustedIssuer}). Its keys come from the fleet namespace, published by a node holding
 * {@link GatewayIdentity#IDENTITY_PUBLISHER_ROLE}; this says whom to trust and how to map roles.
 * Needs Enrollment.
 *
 * @param iss the exact {@code iss} claim
 * @param audiences the accepted {@code aud} values (required)
 * @param rolesClaim the claim roles are read from (an array of strings or one space-separated
 *     string); {@code null} = the tokens grant no roles
 * @param roleMap claim values to Aster roles; unmapped values are dropped
 * @param maxTokenLifetimeSecs the longest {@code exp - iat} accepted; {@code null} = 900
 */
public record TrustedIssuer(
    String iss,
    List<String> audiences,
    String rolesClaim,
    Map<String, String> roleMap,
    Long maxTokenLifetimeSecs) {

  public TrustedIssuer {
    audiences = audiences == null ? List.of() : List.copyOf(audiences);
    roleMap = roleMap == null || roleMap.isEmpty() ? null : Map.copyOf(roleMap);
    if (rolesClaim != null && rolesClaim.isEmpty()) {
      rolesClaim = null;
    }
  }

  /** An issuer for {@code audiences} granting no roles. */
  public static TrustedIssuer of(String iss, List<String> audiences) {
    return new TrustedIssuer(iss, audiences, null, null, null);
  }

  /** A copy reading roles from {@code claim}, mapped by {@code map}. */
  public TrustedIssuer withRoles(String claim, Map<String, String> map) {
    return new TrustedIssuer(iss, audiences, claim, map, maxTokenLifetimeSecs);
  }

  /** A copy accepting tokens living up to {@code secs}. */
  public TrustedIssuer withMaxTokenLifetimeSecs(Long secs) {
    return new TrustedIssuer(iss, audiences, rolesClaim, roleMap, secs);
  }
}
