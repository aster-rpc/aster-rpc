package site.aster.lease;

/**
 * A lease serialization point implemented in Java and served to Rust by upcall (kinds {@code
 * LEASE_TRANSITION} / {@code LEASE_SNAPSHOT}). Apply each transition atomically under your own lock
 * or CAS, evaluating expiry on your own clock. Handlers run on virtual threads and may block; a
 * thrown exception is a backend error (the acquire / renew / release fails), never a denial.
 */
public interface LeaseSerializer {

  /** Apply one transition. Predicate refusals are {@link TransitionOutcome.Denied}. */
  TransitionOutcome transition(String resource, LeaseOp op) throws Exception;

  /** Advisory snapshot (hints only). */
  LeaseSnapshot snapshot(String resource) throws Exception;
}
