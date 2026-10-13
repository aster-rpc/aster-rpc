package site.aster.enrollment;

/**
 * The delegated issuer a server runs when {@code [enrollment.issuer] delegation_file} is configured
 * ({@code AsterServer.issuer()}; Rust {@code aster::enrollment::Issuer}). It already serves {@code
 * aster.enrollment.IssuerV1} renewals; {@link #issue} enrolls new nodes.
 */
public final class EnrollmentIssuer {

  private final Enrollment enrollment;

  /** The issuer of a server's {@code enrollment}; binding plumbing. */
  public EnrollmentIssuer(Enrollment enrollment) {
    this.enrollment = enrollment;
  }

  /** The issuer's key and delegation expiry. */
  public IssuerInfo info() {
    return enrollment.issuerInfo();
  }

  /**
   * Sign a credential for {@code subject} (a 64-hex node id the orchestrator has authenticated out
   * of band). A refusal (a subject the root denied or registered, claims beyond the delegation, a
   * revoked issuer) throws {@link IssuerRefusedException}.
   */
  public IssuedCredential issue(
      String subject, EnrollmentClaims claims, Registration registration) {
    return enrollment.issue(subject, claims, registration);
  }
}
