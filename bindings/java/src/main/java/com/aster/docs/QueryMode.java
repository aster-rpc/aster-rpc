package com.aster.docs;

/** Query mode for document queries. */
public enum QueryMode {
  /** Query entries by author only. */
  AUTHOR(0),
  /** Query all entries. */
  ALL(1),
  /** Query entries by key prefix. */
  PREFIX(2);

  private final int code;

  QueryMode(int code) {
    this.code = code;
  }

  /** The numeric code used in FFI. */
  public int code() {
    return code;
  }

  /** Convert from FFI numeric code. */
  public static QueryMode fromCode(int code) {
    for (QueryMode m : values()) {
      if (m.code == code) return m;
    }
    return ALL;
  }
}
