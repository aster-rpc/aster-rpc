package site.aster.lease;

/** Holder-side lease status ({@code aster_lease_status_t}). */
public enum LeaseStatus {
  HELD,
  /** Released by this holder. */
  RELEASED,
  /** A renewal was refused: someone holds a newer epoch, or the lease was revoked. */
  FENCED,
  /** The self-limit passed without a successful renewal. */
  LOST;

  static LeaseStatus fromCode(int code) {
    return switch (code) {
      case 0 -> HELD;
      case 1 -> RELEASED;
      case 2 -> FENCED;
      case 3 -> LOST;
      default -> throw new IllegalArgumentException("unknown aster_lease_status_t " + code);
    };
  }
}
