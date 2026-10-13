package site.aster.enrollment;

import java.util.List;

/**
 * This node's Enrollment state ({@code aster_enrollment_status}).
 *
 * @param side {@code "server"} or {@code "client"}
 * @param credential this node's credential evidence; follows renewals
 * @param renewing whether this node renews its credential
 * @param issuer the configured delegated issuer's key, or {@code null}
 */
public record EnrollmentStatus(
    String side,
    String trustedRoot,
    String fleetNamespace,
    String audience,
    PeerEvidence credential,
    RootState rootState,
    List<String> rootRotations,
    boolean renewing,
    String issuer) {

  /** The pinned root rotation state. */
  public record RootState(String root, String nextCommitment, long seq) {}
}
