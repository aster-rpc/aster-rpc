package site.aster.identity;

import java.time.Instant;
import java.util.List;

/**
 * The verified user of a call a gateway made on its behalf (Rust {@code
 * aster::rpc::ForwardedIdentity}; {@code aster_dispatch_on_behalf_of}).
 *
 * @param issuer the token's {@code iss}: a provider's URL, or the gateway's node id for a token it
 *     minted
 * @param subject the token's {@code sub}
 * @param jti the token id ({@code ""} when it has none)
 * @param expiresAt the token's {@code exp}
 * @param authMethods the token's {@code amr} (how the user signed in)
 */
public record ForwardedIdentity(
    String issuer, String subject, String jti, Instant expiresAt, List<String> authMethods) {

  public ForwardedIdentity {
    authMethods = authMethods == null ? List.of() : List.copyOf(authMethods);
  }

  /** The principal the call runs as: {@code iss|sub}. */
  public String principal() {
    return issuer + "|" + subject;
  }
}
