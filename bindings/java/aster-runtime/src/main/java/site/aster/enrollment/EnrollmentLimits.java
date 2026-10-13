package site.aster.enrollment;

/**
 * {@code [enrollment.limits]} overrides (Rust {@code EnrollmentLimits}; env {@code
 * ASTER_ENROLLMENT_<FIELD>}, e.g. {@code ASTER_ENROLLMENT_HANDSHAKE_TIMEOUT_MS}). {@code null}
 * keeps Enrollment's default. {@code syncTimeoutMs} bounds enrolling with and synchronizing from a
 * replica or server (default 15 000 ms).
 */
public record EnrollmentLimits(
    Long handshakeTimeoutMs,
    Long stepTimeoutMs,
    Long policyTimeoutMs,
    Long reconciliationIntervalMs,
    Long maxAdmittedPeers,
    Long syncTimeoutMs) {

  /** Every limit at its default. */
  public static final EnrollmentLimits DEFAULTS =
      new EnrollmentLimits(null, null, null, null, null, null);

  /** A copy with {@code handshakeTimeoutMs}. */
  public EnrollmentLimits withHandshakeTimeoutMs(Long v) {
    return new EnrollmentLimits(
        v,
        stepTimeoutMs,
        policyTimeoutMs,
        reconciliationIntervalMs,
        maxAdmittedPeers,
        syncTimeoutMs);
  }

  /** A copy with {@code stepTimeoutMs}. */
  public EnrollmentLimits withStepTimeoutMs(Long v) {
    return new EnrollmentLimits(
        handshakeTimeoutMs,
        v,
        policyTimeoutMs,
        reconciliationIntervalMs,
        maxAdmittedPeers,
        syncTimeoutMs);
  }

  /** A copy with {@code policyTimeoutMs}. */
  public EnrollmentLimits withPolicyTimeoutMs(Long v) {
    return new EnrollmentLimits(
        handshakeTimeoutMs,
        stepTimeoutMs,
        v,
        reconciliationIntervalMs,
        maxAdmittedPeers,
        syncTimeoutMs);
  }

  /** A copy with {@code reconciliationIntervalMs}. */
  public EnrollmentLimits withReconciliationIntervalMs(Long v) {
    return new EnrollmentLimits(
        handshakeTimeoutMs, stepTimeoutMs, policyTimeoutMs, v, maxAdmittedPeers, syncTimeoutMs);
  }

  /** A copy with {@code maxAdmittedPeers}. */
  public EnrollmentLimits withMaxAdmittedPeers(Long v) {
    return new EnrollmentLimits(
        handshakeTimeoutMs,
        stepTimeoutMs,
        policyTimeoutMs,
        reconciliationIntervalMs,
        v,
        syncTimeoutMs);
  }

  /** A copy with {@code syncTimeoutMs}. */
  public EnrollmentLimits withSyncTimeoutMs(Long v) {
    return new EnrollmentLimits(
        handshakeTimeoutMs,
        stepTimeoutMs,
        policyTimeoutMs,
        reconciliationIntervalMs,
        maxAdmittedPeers,
        v);
  }
}
