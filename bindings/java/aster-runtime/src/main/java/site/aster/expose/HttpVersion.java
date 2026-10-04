package site.aster.expose;

/** The HTTP version of a relayed request (the UCv1 {@code version} byte). */
public enum HttpVersion {
  HTTP_0_9(0),
  HTTP_1_0(1),
  HTTP_1_1(2),
  HTTP_2(3),
  HTTP_3(4);

  public final int code;

  HttpVersion(int code) {
    this.code = code;
  }

  public static HttpVersion fromCode(int code) {
    for (HttpVersion v : values()) {
      if (v.code == code) {
        return v;
      }
    }
    throw new IllegalArgumentException("unknown HTTP version code " + code);
  }
}
