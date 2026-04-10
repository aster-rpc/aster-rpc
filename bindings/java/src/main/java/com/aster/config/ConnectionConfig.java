package com.aster.config;

import com.aster.ffi.IrohLibrary;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;

/**
 * Builder for {@code iroh_connect_config_t}.
 *
 * <p>Call {@link #toNative(SegmentAllocator)} to produce a native struct suitable for {@code
 * iroh_connect}.
 */
public class ConnectionConfig {

  private static final MemoryLayout LAYOUT = IrohLibrary.IROH_CONNECT_CONFIG;

  private int flags = 0;
  private byte[] nodeId = new byte[0];
  private byte[] alpn = new byte[0];
  private long addr = 0L; // 0L = NULL, no direct address

  public ConnectionConfig flags(int flags) {
    this.flags = flags;
    return this;
  }

  /**
   * Set the remote node ID as a hex string.
   *
   * @param nodeIdHex the node ID in hex string form
   * @return this builder
   */
  public ConnectionConfig nodeId(String nodeIdHex) {
    this.nodeId = nodeIdHex.getBytes(StandardCharsets.UTF_8);
    return this;
  }

  /**
   * Set the ALPN protocol string.
   *
   * @param alpn the ALPN protocol name
   * @return this builder
   */
  public ConnectionConfig alpn(String alpn) {
    this.alpn = alpn.getBytes(StandardCharsets.UTF_8);
    return this;
  }

  /**
   * Set a direct node address (not typically needed — relay handles routing).
   *
   * @param addr the raw address value, or 0 for NULL (relay-assisted connect)
   * @return this builder
   */
  public ConnectionConfig addr(long addr) {
    this.addr = addr;
    return this;
  }

  /**
   * Encode this config into a native {@code iroh_connect_config_t} struct allocated from {@code
   * allocator}.
   *
   * <p>Layout: struct_size(4) + flags(4) + node_id IROH_BYTES(16) + alpn IROH_BYTES(16) + addr
   * ADDRESS(8) = 48 bytes.
   */
  public MemorySegment toNative(SegmentAllocator alloc) {
    MemorySegment seg = alloc.allocate(LAYOUT);

    // Use seg.set(...) to avoid VarHandle varargs arity issues
    seg.set(ValueLayout.JAVA_INT, 0, (int) LAYOUT.byteSize());
    seg.set(ValueLayout.JAVA_INT, 4, flags);

    // node_id: IROH_BYTES at offset 8 (ptr at +0, len at +8)
    if (nodeId.length > 0) {
      MemorySegment dataSeg = alloc.allocate(nodeId.length);
      dataSeg.copyFrom(MemorySegment.ofArray(nodeId));
      seg.set(ValueLayout.ADDRESS, 8, dataSeg);
      seg.set(ValueLayout.JAVA_LONG, 16, (long) nodeId.length);
    }

    // alpn: IROH_BYTES at offset 24 (ptr at +24, len at +32)
    if (alpn.length > 0) {
      MemorySegment dataSeg = alloc.allocate(alpn.length);
      dataSeg.copyFrom(MemorySegment.ofArray(alpn));
      seg.set(ValueLayout.ADDRESS, 24, dataSeg);
      seg.set(ValueLayout.JAVA_LONG, 32, (long) alpn.length);
    }

    // addr: NULL (0) by default — stored as raw 64-bit pointer value
    seg.set(ValueLayout.JAVA_LONG, 40, addr);

    return seg;
  }
}
