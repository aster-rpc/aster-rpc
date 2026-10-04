package site.aster.docs;

/** What access a namespace grants this node locally ({@code aster_local_access_t}). */
public enum LocalAccess {
  WRITE(1),
  READ(2);

  public final int code;

  LocalAccess(int code) {
    this.code = code;
  }

  public static LocalAccess fromCode(int code) {
    for (var v : values()) {
      if (v.code == code) return v;
    }
    throw new IllegalArgumentException("unknown aster_local_access_t " + code);
  }
}
