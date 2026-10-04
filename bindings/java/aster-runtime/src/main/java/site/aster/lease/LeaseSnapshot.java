package site.aster.lease;

/**
 * Observer view of a serializer row. Advisory: stale the moment it is returned; never gate a write
 * on it. {@code holder} and {@code remainingMs} are null when absent.
 */
public record LeaseSnapshot(
    String resource, long epoch, String holder, boolean held, Long remainingMs) {}
