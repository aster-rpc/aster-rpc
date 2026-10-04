package site.aster.lease;

/**
 * May {@code op} be attempted at all? Served by upcall (kind {@code LEASE_AUTHORIZE}, a decision):
 * {@code false} yields {@code Denied(UNAUTHORIZED)}; an exception or no answer applies the kind's
 * failure mode (fail-closed by default).
 */
@FunctionalInterface
public interface LeaseAuthorizer {
  boolean authorize(LeaseOp op) throws Exception;
}
