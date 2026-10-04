package site.aster.lease;

/**
 * The fencing token a lease holder stamps on every protected action: {@code epoch} is monotonic per
 * resource and never reused; {@code holder} is the identity the epoch was issued to.
 */
public record Fence(String resource, long epoch, String holder) {}
