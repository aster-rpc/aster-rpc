package site.aster.enrollment;

import com.fasterxml.jackson.databind.JsonNode;

/**
 * A formerly admitted peer revoked by a definite denial; its connections are already closed and its
 * roles are gone from the next call. {@code lagged > 0} instead means that many revocations were
 * missed (recheck every peer relied on).
 */
public record Revocation(String peer, Reason reason, long lagged) {

  /**
   * Why a peer was denied; definite reasons revoke sessions.
   *
   * @param code e.g. {@code node_revoked}, {@code credential_revoked}, {@code issuer_revoked}
   */
  public record Reason(String code, boolean definite, JsonNode detail) {}
}
