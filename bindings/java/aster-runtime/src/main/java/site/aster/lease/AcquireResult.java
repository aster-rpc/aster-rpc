package site.aster.lease;

/** Outcome of {@link NativeLeaseSerializer#acquire}. */
public sealed interface AcquireResult {

  /** Granted: a held lease that renews in the background until released or closed. */
  record Granted(Lease lease) implements AcquireResult {}

  /**
   * Refused (typically {@code HELD_BY_OTHER}); back off using {@code snapshot.remainingMs()} plus
   * jitter.
   */
  record Denied(DenyReason reason, LeaseSnapshot snapshot) implements AcquireResult {}
}
