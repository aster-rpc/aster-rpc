package site.aster.identity;

import java.time.Instant;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

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
 * @param claims every claim of the verified token, as signed (C ABI 7.9; Rust {@code
 *     CallContext::token_claims}); unmodifiable, empty against an older library
 */
public record ForwardedIdentity(
    String issuer,
    String subject,
    String jti,
    Instant expiresAt,
    List<String> authMethods,
    Map<String, Object> claims) {

  public ForwardedIdentity {
    authMethods = authMethods == null ? List.of() : List.copyOf(authMethods);
    claims = claims == null ? Map.of() : Collections.unmodifiableMap(new LinkedHashMap<>(claims));
  }

  /** An identity without claims. */
  public ForwardedIdentity(
      String issuer, String subject, String jti, Instant expiresAt, List<String> authMethods) {
    this(issuer, subject, jti, expiresAt, authMethods, Map.of());
  }

  /** The principal the call runs as: {@code iss|sub}. */
  public String principal() {
    return issuer + "|" + subject;
  }
}
