package site.aster.docs;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import site.aster.blobs.BlobId;
import site.aster.ffi.IrohException;

/** Query metadata. Values are read separately using {@link Doc#readEntryContentAsync}. */
public record DocEntry(
    byte[] keyBytes, AuthorId author, BlobId contentHash, long contentLen, long timestamp) {
  public DocEntry {
    keyBytes = keyBytes.clone();
  }

  @Override
  public byte[] keyBytes() {
    return keyBytes.clone();
  }

  /** UTF-8 convenience view; use keyBytes for arbitrary byte keys. */
  public String key() {
    return new String(keyBytes, StandardCharsets.UTF_8);
  }

  static List<DocEntry> decodeList(byte[] data, long count) {
    if (count > data.length / 156) throw new IrohException("truncated document query payload");
    var b = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN);
    var entries = new ArrayList<DocEntry>();
    try {
      for (long i = 0; i < count; i++) {
        entries.add(decode(b));
      }
      if (b.hasRemaining()) throw new IllegalArgumentException("trailing query bytes");
      return List.copyOf(entries);
    } catch (RuntimeException e) {
      throw new IrohException("invalid document query payload: " + e.getMessage());
    }
  }

  static DocEntry decode(ByteBuffer b) {
    String author = hexField(b, "author");
    byte[] key = field(b);
    String hash = hexField(b, "content hash");
    return new DocEntry(key, AuthorId.of(author), BlobId.of(hash), b.getLong(), b.getLong());
  }

  static byte[] field(ByteBuffer b) {
    long n = Integer.toUnsignedLong(b.getInt());
    if (n > b.remaining()) throw new IllegalArgumentException("truncated query field");
    byte[] out = new byte[(int) n];
    b.get(out);
    return out;
  }

  private static String hexField(ByteBuffer b, String what) {
    String s = new String(field(b), StandardCharsets.US_ASCII);
    if (!s.matches("[0-9a-fA-F]{64}")) throw new IllegalArgumentException("invalid " + what);
    return s;
  }
}
