package site.aster.lease;

/**
 * The resource-side half of a lease, implemented in Java and served by upcall (kind {@code
 * FENCED_MUTATE}). Perform {@code op} iff the fence rule ({@link FenceRejection#check}) passes,
 * atomically with the check. An exception means the outcome is unknown to the Rust caller.
 */
@FunctionalInterface
public interface FencedResource {
  MutateResult mutate(Fence fence, String writer, byte[] op) throws Exception;
}
