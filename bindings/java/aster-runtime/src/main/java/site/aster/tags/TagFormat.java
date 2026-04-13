package site.aster.tags;

/** Blob format for tags. */
public enum TagFormat {
  /** Raw blob format (single file). */
  RAW(0),
  /** Hash sequence format (collection). */
  HASH_SEQ(1);

  private final int code;

  TagFormat(int code) {
    this.code = code;
  }

  /** The numeric format code used in FFI. */
  public int code() {
    return code;
  }

  /** Convert from FFI numeric code. */
  public static TagFormat fromCode(int code) {
    for (TagFormat f : values()) {
      if (f.code == code) return f;
    }
    return RAW;
  }
}
