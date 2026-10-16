package site.aster.identity;

import java.util.List;

/**
 * {@code [identity]} fields overridden through {@code aster.ops.NodeConfig} (Rust {@code
 * IdentityOverride}): only the non-null fields apply; {@code trustedIssuers} replaces the whole
 * list.
 */
public record IdentityOverride(
    Boolean acceptOnBehalfOf,
    List<TrustedIssuer> trustedIssuers,
    Long edgeTokenMaxLifetimeSecs,
    Long clockLeewaySecs,
    Long maxKeySetAgeSecs) {

  /** Overrides nothing. */
  public static final IdentityOverride NONE = new IdentityOverride(null, null, null, null, null);

  public IdentityOverride {
    trustedIssuers = trustedIssuers == null ? null : List.copyOf(trustedIssuers);
  }

  /** A copy overriding {@code [identity] accept_on_behalf_of}. */
  public IdentityOverride withAcceptOnBehalfOf(Boolean v) {
    return new IdentityOverride(
        v, trustedIssuers, edgeTokenMaxLifetimeSecs, clockLeewaySecs, maxKeySetAgeSecs);
  }

  /** A copy replacing {@code [[identity.trusted_issuers]]}. */
  public IdentityOverride withTrustedIssuers(List<TrustedIssuer> v) {
    return new IdentityOverride(
        acceptOnBehalfOf, v, edgeTokenMaxLifetimeSecs, clockLeewaySecs, maxKeySetAgeSecs);
  }

  /** A copy overriding the three limits ({@code null} = not overridden). */
  public IdentityOverride withLimits(Long edgeTokenMaxLifetime, Long clockLeeway, Long keySetAge) {
    return new IdentityOverride(
        acceptOnBehalfOf, trustedIssuers, edgeTokenMaxLifetime, clockLeeway, keySetAge);
  }
}
