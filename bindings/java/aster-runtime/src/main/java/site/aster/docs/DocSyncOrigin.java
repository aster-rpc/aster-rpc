package site.aster.docs;

/** Which side started a document sync round. */
public enum DocSyncOrigin {
  /** The peer dialled this node and it accepted. */
  ACCEPT(0),
  /** This node dialled the peer. */
  CONNECT(1);

  private final int code;

  DocSyncOrigin(int code) {
    this.code = code;
  }

  public int code() {
    return code;
  }

  public static DocSyncOrigin fromCode(int code) {
    for (DocSyncOrigin o : values()) {
      if (o.code == code) return o;
    }
    throw new IllegalArgumentException("unknown sync origin: " + code);
  }
}
