package site.aster.identity;

import java.util.Locale;
import java.util.Optional;

/**
 * Gateway identity (C ABI 7.7; design {@code docs/_internal/web_gateway/gateway-identity.md}): an
 * edge holding {@link #GATEWAY_ROLE} calls a service on behalf of a user with one JWT per call,
 * either the identity provider's own token (forwarded unchanged) or one the edge mints ({@code
 * AsterClient.mintUserToken}). The server verifies it before Gate 3; the handler sees the user
 * ({@code CallContext.principal()} = {@code iss|sub}), the edge ({@code CallContext.actor()}) and
 * how the user signed in ({@code CallContext.authMethods()}). Everything is configured through
 * {@link site.aster.config.AsterConfig}, as the Rust {@code AsterServer} / {@code AsterClient} are.
 *
 * <p>Roles, metadata and sign-in methods (Rust {@code aster::rpc::identity}).
 */
public final class GatewayIdentity {

  /** The call-metadata entry that carries the user's JWT ({@code CallOptions.withOnBehalfOf}). */
  public static final String ON_BEHALF_OF_METADATA = "aster-on-behalf-of";

  /** Lets an edge forward users' identities. */
  public static final String GATEWAY_ROLE = "aster.gateway";

  /** Required on every {@code aster.ops.NodeConfig} method. */
  public static final String CONFIG_ADMIN_ROLE = "aster.config-admin";

  /** Lets a node write issuer keys into the fleet namespace. */
  public static final String IDENTITY_PUBLISHER_ROLE = "aster.identity-publisher";

  /**
   * The credential attribute naming the roles an edge may put in tokens it mints (comma-separated).
   */
  public static final String ISSUABLE_ROLES_ATTRIBUTE = "aster.identity.issuable_roles";

  /** {@code amr}: a passkey sign-in. */
  public static final String AMR_WEBAUTHN = "webauthn";

  /** {@code amr}: a password sign-in. */
  public static final String AMR_PASSWORD = "pwd";

  /** {@code amr}: an API key. */
  public static final String AMR_API_KEY = "apikey";

  private GatewayIdentity() {}

  /**
   * The token of an {@code Authorization: Bearer <token>} header value, for forwarding a provider's
   * token unchanged (Rust {@code aster::rpc::bearer_token}); empty for any other scheme.
   */
  public static Optional<String> bearerToken(String authorization) {
    if (authorization == null) {
      return Optional.empty();
    }
    String v = authorization.strip();
    int sp = v.indexOf(' ');
    if (sp < 0 || !v.substring(0, sp).toLowerCase(Locale.ROOT).equals("bearer")) {
      return Optional.empty();
    }
    String token = v.substring(sp + 1).strip();
    return token.isEmpty() ? Optional.empty() : Optional.of(token);
  }
}
