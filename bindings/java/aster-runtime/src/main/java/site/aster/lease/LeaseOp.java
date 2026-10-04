package site.aster.lease;

/** A requested lease transition (the four verbs of the lease state machine). */
public sealed interface LeaseOp {

  /** The identity performing the transition (for authorization). */
  String actor();

  /** ACQUIRE (legal on a free or expired row). */
  record Acquire(String candidate, long ttlMs) implements LeaseOp {
    @Override
    public String actor() {
      return candidate;
    }
  }

  /** RENEW by the current holder at its epoch. */
  record Renew(String holder, long epoch) implements LeaseOp {
    @Override
    public String actor() {
      return holder;
    }
  }

  /** RELEASE by the current holder (advances the fence). */
  record Release(String holder, long epoch) implements LeaseOp {
    @Override
    public String actor() {
      return holder;
    }
  }

  /** REVOKE by an authority (advances the fence). */
  record Revoke(String authority) implements LeaseOp {
    @Override
    public String actor() {
      return authority;
    }
  }
}
