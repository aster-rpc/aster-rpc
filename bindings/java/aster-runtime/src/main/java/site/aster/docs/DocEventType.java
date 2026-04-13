package site.aster.docs;

/** Document event types. */
public enum DocEventType {
  INSERT_LOCAL(0),
  INSERT_REMOTE(1),
  CONTENT_READY(2),
  PENDING_CONTENT_READY(3),
  NEIGHBOR_UP(4),
  NEIGHBOR_DOWN(5),
  SYNC_FINISHED(6);

  private final int code;

  DocEventType(int code) {
    this.code = code;
  }

  public int code() {
    return code;
  }

  public static DocEventType fromCode(int code) {
    for (DocEventType t : values()) {
      if (t.code == code) return t;
    }
    return INSERT_LOCAL;
  }
}
