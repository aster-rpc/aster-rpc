package site.aster.enrollment;

import java.util.List;
import java.util.Map;

/**
 * The verified evidence a peer's admission session rests on ({@code aster_enrollment_peer_claims};
 * Rust {@code VerifiedAdmissionEvidence}). {@code null} scopes / tags / attributes are unknown, not
 * empty.
 *
 * @param subject the peer's node id (64 hex)
 * @param root the trusted root the evidence is anchored at
 * @param issuer the leaf signer (a delegated issuer, or the root; the root for a registration)
 * @param issuers the chain's issuer path, leaf signer first (empty for a registration)
 * @param source {@code "chain"} or {@code "registration"}
 * @param registration what a presented credential asks for; {@code null} for a registration record
 */
public record PeerEvidence(
    String subject,
    String root,
    String issuer,
    List<String> issuers,
    String source,
    String chainDigest,
    String documentId,
    Long revision,
    String credentialId,
    List<String> audience,
    List<String> scopes,
    List<String> tags,
    Map<String, Attribute> attributes,
    long issuedAt,
    Long notBefore,
    Long expiresAt,
    Registration registration) {

  /** Whether a root-authored registration admits the peer. */
  public boolean registered() {
    return "registration".equals(source);
  }

  /** Whether {@code scope} is among the (known) scopes. */
  public boolean hasScope(String scope) {
    return scopes != null && scopes.contains(scope);
  }
}
