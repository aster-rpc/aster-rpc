package site.aster.grants;

/**
 * What a reconcile loop should do with a grant key ({@code aster_grant_outcome_t}). Every failure
 * mode is a decision, so reconcile never throws for a bad record.
 *
 * @param <T> the opened payload: {@code byte[]} or {@link NamespaceCapability}
 */
public sealed interface GrantOutcome<T> {

  /** Active and opened: apply or refresh the capability. */
  record Active<T>(T value) implements GrantOutcome<T> {}

  /** Explicitly revoked by the granter: drop the capability, tear down sessions. */
  record Revoked<T>(String reason, String role) implements GrantOutcome<T> {}

  /**
   * A record is present but cannot be turned into a capability (bad framing, context mismatch,
   * failed decrypt). Fail closed like {@link Revoked}, and alert.
   */
  record Unopenable<T>(String error) implements GrantOutcome<T> {}

  /** No record. Not revocation: keep waiting and keep any capability already held. */
  record Absent<T>() implements GrantOutcome<T> {}

  /** Whether this outcome means "stop using the capability now" (revoked or unopenable). */
  default boolean dropsCapability() {
    return this instanceof Revoked || this instanceof Unopenable;
  }
}
