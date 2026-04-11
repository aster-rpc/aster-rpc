package com.aster.node;

import com.aster.ffi.IrohLibrary;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.List;

/** Structured node address info returned by {@link IrohNode#nodeAddr()}. */
public record NodeAddr(
    /** The node's endpoint ID as a hex string. */
    String endpointId,
    /** The relay URL, if configured. */
    String relayUrl,
    /** Direct addresses for direct IP connectivity. */
    List<String> directAddresses) {

  private static final MemoryLayout LAYOUT = IrohLibrary.IROH_NODE_ADDR;

  /**
   * Encode this node address into a native {@code iroh_node_addr_t} struct allocated from {@code
   * allocator}.
   *
   * <p>The returned segment is only valid for the lifetime of {@code allocator}'s arena. The caller
   * must ensure the segment is not deallocated before the FFI call that uses it completes.
   */
  public MemorySegment toNative(SegmentAllocator alloc) {
    MemorySegment seg = alloc.allocate(LAYOUT);

    // endpoint_id: IROH_BYTES at offset 0 (ptr+len)
    if (endpointId != null && !endpointId.isEmpty()) {
      byte[] bytes = endpointId.getBytes(StandardCharsets.UTF_8);
      MemorySegment dataSeg = alloc.allocate(bytes.length);
      dataSeg.copyFrom(MemorySegment.ofArray(bytes));
      seg.set(ValueLayout.ADDRESS, 0, dataSeg);
      seg.set(ValueLayout.JAVA_LONG, 8, (long) bytes.length);
    }

    // relay_url: IROH_BYTES at offset 16 (ptr+len)
    if (relayUrl != null && !relayUrl.isEmpty()) {
      byte[] bytes = relayUrl.getBytes(StandardCharsets.UTF_8);
      MemorySegment dataSeg = alloc.allocate(bytes.length);
      dataSeg.copyFrom(MemorySegment.ofArray(bytes));
      seg.set(ValueLayout.ADDRESS, 16, dataSeg);
      seg.set(ValueLayout.JAVA_LONG, 24, (long) bytes.length);
    }

    // direct_addresses: IROH_BYTES_LIST at offset 32 (items+len)
    if (directAddresses != null && !directAddresses.isEmpty()) {
      MemorySegment listSeg = alloc.allocate(16L * directAddresses.size());
      for (int i = 0; i < directAddresses.size(); i++) {
        byte[] b = directAddresses.get(i).getBytes(StandardCharsets.UTF_8);
        MemorySegment itemSeg = alloc.allocate(b.length);
        itemSeg.copyFrom(MemorySegment.ofArray(b));
        long itemOffset = 16L * i;
        listSeg.set(ValueLayout.ADDRESS, itemOffset, itemSeg);
        listSeg.set(ValueLayout.JAVA_LONG, itemOffset + 8, (long) b.length);
      }
      seg.set(ValueLayout.ADDRESS, 32, listSeg);
      seg.set(ValueLayout.JAVA_LONG, 40, (long) directAddresses.size());
    }

    return seg;
  }
}
