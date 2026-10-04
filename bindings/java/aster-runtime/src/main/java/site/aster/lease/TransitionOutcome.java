package site.aster.lease;

/** Result of a transition at a lease serializer. A refusal is information, not a fault. */
public sealed interface TransitionOutcome {

  /** ACQUIRE succeeded: the issued fence. */
  record Granted(Fence fence) implements TransitionOutcome {}

  record Renewed() implements TransitionOutcome {}

  record Released() implements TransitionOutcome {}

  record Revoked() implements TransitionOutcome {}

  /** The predicate refused; the snapshot (post-decision) is a back-off hint. */
  record Denied(DenyReason reason, LeaseSnapshot snapshot) implements TransitionOutcome {}

  TransitionOutcome RENEWED = new Renewed();
  TransitionOutcome RELEASED = new Released();
  TransitionOutcome REVOKED = new Revoked();
}
