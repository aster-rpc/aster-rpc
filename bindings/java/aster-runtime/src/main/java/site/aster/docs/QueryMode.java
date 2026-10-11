package site.aster.docs;

/** Native document query modes. Queries do not filter by author. */
public enum QueryMode {
  KEY_EXACT(0),
  KEY_PREFIX(1),
  LATEST_EXACT(2),
  LATEST_PREFIX(3);

  private final int code;

  QueryMode(int code) {
    this.code = code;
  }

  public int code() {
    return code;
  }

  public static QueryMode fromCode(int code) {
    for (QueryMode mode : values()) if (mode.code == code) return mode;
    throw new IllegalArgumentException("unknown query mode: " + code);
  }
}
