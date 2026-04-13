package site.aster.blobs;

/** Blob storage status. */
public enum BlobStatus {
  NOT_FOUND(0),
  PARTIAL(1),
  COMPLETE(2);

  public final int code;

  BlobStatus(int code) {
    this.code = code;
  }

  public static BlobStatus fromCode(int code) {
    for (var v : values()) {
      if (v.code == code) return v;
    }
    return NOT_FOUND;
  }
}
