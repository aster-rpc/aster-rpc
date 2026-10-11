package site.aster.docs;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import site.aster.blobs.BlobId;
import site.aster.ffi.IrohException;

/** A live change. Only insertion events carry entry metadata; peer/hash depend on type. */
public record DocEvent(DocEventType type, DocEntry entry, String peer, BlobId contentHash) {
  static DocEvent decode(int subtype, byte[] payload) {
    try {
      DocEventType type = DocEventType.fromCode(subtype);
      var b = ByteBuffer.wrap(payload).order(ByteOrder.LITTLE_ENDIAN);
      DocEntry entry = null;
      String peer = null;
      BlobId hash = null;
      switch (type) {
        case INSERT_LOCAL -> entry = DocEntry.decode(b);
        case INSERT_REMOTE -> {
          entry = DocEntry.decode(b);
          peer = hex(DocEntry.field(b));
        }
        case CONTENT_READY -> {
          hash = BlobId.of(hex(payload));
          b.position(b.limit());
        }
        case PENDING_CONTENT_READY -> {}
        case NEIGHBOR_UP, NEIGHBOR_DOWN, SYNC_FINISHED -> {
          peer = hex(payload);
          b.position(b.limit());
        }
      }
      if (b.hasRemaining()) throw new IllegalArgumentException("trailing document event bytes");
      return new DocEvent(type, entry, peer, hash);
    } catch (RuntimeException e) {
      throw new IrohException("invalid document event payload: " + e.getMessage());
    }
  }

  private static String hex(byte[] data) {
    String s = new String(data, StandardCharsets.US_ASCII);
    if (!s.matches("[0-9a-fA-F]{64}"))
      throw new IllegalArgumentException("invalid event identifier");
    return s;
  }
}
