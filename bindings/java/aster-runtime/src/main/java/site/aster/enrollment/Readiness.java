package site.aster.enrollment;

/**
 * A peer's admission (membership only, never operation permission).
 *
 * @param source {@code "chain"} or {@code "registration"}
 * @param localClock {@code synchronized}, {@code unsynchronized} or {@code not_checked}
 */
public record Readiness(
    String peer, String source, String observedSource, PolicySync policySync, String localClock) {

  /**
   * The last completed policy synchronization.
   *
   * @param source {@code "root"} or the replica's node id
   */
  public record PolicySync(long completedAt, String source, Long observedRevision) {}
}
