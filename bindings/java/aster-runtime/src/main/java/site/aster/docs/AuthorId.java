package site.aster.docs;

/** A 32-byte author key for content-addressed documents. */
public record AuthorId(String hex) {

  /** Create from a 64-character hex string. */
  public static AuthorId of(String hex) {
    if (hex == null || hex.length() != 64) {
      throw new IllegalArgumentException("AuthorId must be 64 hex characters");
    }
    return new AuthorId(hex);
  }

  /** Create from 32 bytes. */
  public static AuthorId ofBytes(byte[] bytes) {
    if (bytes == null || bytes.length != 32) {
      throw new IllegalArgumentException("AuthorId must be 32 bytes");
    }
    StringBuilder sb = new StringBuilder();
    for (byte b : bytes) {
      sb.append(String.format("%02x", b));
    }
    return new AuthorId(sb.toString());
  }

  /** The 64-character hex string. */
  public String hex() {
    return hex;
  }
}
