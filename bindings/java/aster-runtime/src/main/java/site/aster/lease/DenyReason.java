package site.aster.lease;

/** Why a lease transition was refused (UCv1 codes 1..6). */
public enum DenyReason {
  HELD_BY_OTHER(1),
  EPOCH_MISMATCH(2),
  NOT_HOLDER(3),
  EXPIRED(4),
  NOT_HELD(5),
  UNAUTHORIZED(6);

  public final int code;

  DenyReason(int code) {
    this.code = code;
  }

  /** The reason for a UCv1 code, or {@code null} if unknown. */
  public static DenyReason fromCode(int code) {
    for (DenyReason r : values()) {
      if (r.code == code) {
        return r;
      }
    }
    return null;
  }
}
