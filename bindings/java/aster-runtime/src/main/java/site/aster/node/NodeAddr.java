package site.aster.node;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import site.aster.ffi.IrohLibrary;

/** Structured node address info returned by {@link IrohNode#nodeAddr()}. */
public record NodeAddr(
    /** The node's endpoint ID as a hex string. */
    String endpointId,
    /** The relay URL, if configured. */
    String relayUrl,
    /** Direct addresses for direct IP connectivity. */
    List<String> directAddresses) {

  private static final ObjectMapper TICKET_MAPPER = new ObjectMapper();
  private static final int TICKET_DECODE_BUF_SIZE = 8192;

  /**
   * Parse an {@code aster1…} ticket string into its structured {@link NodeAddr}.
   *
   * <p>Delegates to the {@code aster_ticket_decode} Rust FFI — the same parser Python and
   * TypeScript use — so cross-language addresses round-trip byte-identically. The ticket's
   * credential payload (if any) is discarded here; {@link NodeAddr} models the transport
   * coordinates only.
   *
   * @throws IllegalArgumentException if the ticket is not a valid {@code aster1…} string
   */
  public static NodeAddr fromTicket(String ticket) {
    if (ticket == null || ticket.isEmpty()) {
      throw new IllegalArgumentException("ticket must not be empty");
    }
    byte[] ticketBytes = ticket.getBytes(StandardCharsets.UTF_8);
    IrohLibrary lib = IrohLibrary.getInstance();
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment ticketSeg = arena.allocate(ticketBytes.length);
      ticketSeg.copyFrom(MemorySegment.ofArray(ticketBytes));

      MemorySegment outBuf = arena.allocate(TICKET_DECODE_BUF_SIZE);
      MemorySegment outLen = arena.allocate(ValueLayout.JAVA_LONG);
      outLen.set(ValueLayout.JAVA_LONG, 0, TICKET_DECODE_BUF_SIZE);

      int status = lib.asterTicketDecode(ticketSeg, ticketBytes.length, outBuf, outLen);
      if (status != 0) {
        throw new IllegalArgumentException(
            "aster_ticket_decode failed with status " + status + " for ticket: " + ticket);
      }
      long written = outLen.get(ValueLayout.JAVA_LONG, 0);
      byte[] jsonBytes = new byte[(int) written];
      MemorySegment.copy(outBuf, ValueLayout.JAVA_BYTE, 0, jsonBytes, 0, (int) written);
      String json = new String(jsonBytes, StandardCharsets.UTF_8);

      JsonNode node;
      try {
        node = TICKET_MAPPER.readTree(json);
      } catch (Exception e) {
        throw new IllegalStateException("aster_ticket_decode returned invalid JSON: " + json, e);
      }
      String endpointId = node.path("endpoint_id").asText();
      String relayAddr = node.path("relay_addr").isNull() ? null : node.path("relay_addr").asText();
      List<String> directAddrs = new ArrayList<>();
      JsonNode arr = node.path("direct_addrs");
      if (arr.isArray()) {
        for (JsonNode entry : arr) {
          directAddrs.add(entry.asText());
        }
      }
      return new NodeAddr(endpointId, relayAddr, List.copyOf(directAddrs));
    }
  }

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
