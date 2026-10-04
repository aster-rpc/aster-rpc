package site.aster.expose;

import java.lang.foreign.MemoryLayout;
import java.lang.foreign.ValueLayout;

/**
 * Options of a streaming relay ({@code aster_http_relay_options_t}, 24 bytes); {@code 0} = the
 * default of each.
 *
 * @param maxChunk largest chunk one upload write takes and one response read returns (default 1
 *     MiB)
 * @param uploadCapacity upload chunks queued before a write waits (default 8)
 */
public record RelayOptions(long maxChunk, int uploadCapacity) {

  public static final MemoryLayout LAYOUT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), //     0
          ValueLayout.JAVA_INT.withName("flags"), //          4
          ValueLayout.JAVA_LONG.withName("max_chunk"), //     8
          ValueLayout.JAVA_INT.withName("upload_capacity"), // 16
          ValueLayout.JAVA_INT.withName("reserved")); //      20 -> 24

  static {
    if (LAYOUT.byteSize() != 24
        || LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("max_chunk")) != 8
        || LAYOUT.byteOffset(MemoryLayout.PathElement.groupElement("upload_capacity")) != 16) {
      throw new AssertionError("aster_http_relay_options_t layout drift");
    }
  }

  public RelayOptions {
    if (maxChunk < 0 || uploadCapacity < 0) {
      throw new IllegalArgumentException("options must not be negative");
    }
  }

  public static RelayOptions defaults() {
    return new RelayOptions(0, 0);
  }

  public RelayOptions withMaxChunk(long v) {
    return new RelayOptions(v, uploadCapacity);
  }

  public RelayOptions withUploadCapacity(int v) {
    return new RelayOptions(maxChunk, v);
  }

  long effectiveMaxChunk() {
    return maxChunk == 0 ? ExposeHttpOptions.DEFAULT_MAX_CHUNK : maxChunk;
  }
}
