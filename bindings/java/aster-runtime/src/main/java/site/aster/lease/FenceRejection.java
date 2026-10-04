package site.aster.lease;

/**
 * Why a protected action was rejected at the resource. {@code carried} / {@code current} are the
 * epochs for {@code STALE_EPOCH} and {@code FUTURE_EPOCH}, 0 otherwise.
 */
public record FenceRejection(Reason reason, long carried, long current) {

  /** UCv1 rejection codes. */
  public enum Reason {
    STALE_EPOCH(1),
    NOT_HOLDER(2),
    NOT_HELD(3),
    FUTURE_EPOCH(4);

    public final int code;

    Reason(int code) {
      this.code = code;
    }

    public static Reason fromCode(int code) {
      for (Reason r : values()) {
        if (r.code == code) {
          return r;
        }
      }
      return null;
    }
  }

  public static FenceRejection staleEpoch(long carried, long current) {
    return new FenceRejection(Reason.STALE_EPOCH, carried, current);
  }

  public static FenceRejection futureEpoch(long carried, long current) {
    return new FenceRejection(Reason.FUTURE_EPOCH, carried, current);
  }

  public static FenceRejection notHolder() {
    return new FenceRejection(Reason.NOT_HOLDER, 0, 0);
  }

  public static FenceRejection notHeld() {
    return new FenceRejection(Reason.NOT_HELD, 0, 0);
  }

  /**
   * The fence rule ({@code check_fence}): accept iff the carried epoch equals the row's, the row is
   * held, and {@code writer} is its holder. Evaluate it inside the resource's own serialization
   * point, atomically with the write. Returns null when the fence passes.
   */
  public static FenceRejection check(
      long rowEpoch, boolean rowHeld, String rowHolder, Fence fence, String writer) {
    if (fence.epoch() < rowEpoch) {
      return staleEpoch(fence.epoch(), rowEpoch);
    }
    if (fence.epoch() > rowEpoch) {
      return futureEpoch(fence.epoch(), rowEpoch);
    }
    if (!rowHeld) {
      return notHeld();
    }
    if (rowHolder == null || !rowHolder.equals(writer)) {
      return notHolder();
    }
    return null;
  }
}
