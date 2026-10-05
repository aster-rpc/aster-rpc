package site.aster.otel;

import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.HexFormat;

/**
 * A W3C trace context: the ids and flags of one span ({@code aster_span_context_t}).
 *
 * @param traceId 32 lowercase hex characters, never all zero
 * @param spanId 16 lowercase hex characters, never all zero
 * @param traceFlags W3C trace flags; bit 0 is "sampled"
 * @param remote whether the context arrived from another process
 */
public record SpanContext(String traceId, String spanId, byte traceFlags, boolean remote) {

  private static final HexFormat HEX = HexFormat.of();
  private static final String ZERO_TRACE = "0".repeat(32);
  private static final String ZERO_SPAN = "0".repeat(16);

  /** The W3C {@code traceparent} text ({@code 00-<trace>-<span>-<flags>}, 55 characters). */
  public String traceparent() {
    return "00-" + traceId + "-" + spanId + "-" + HEX.toHexDigits(traceFlags);
  }

  /** Whether the sampled flag is set. */
  public boolean sampled() {
    return (traceFlags & 1) != 0;
  }

  /**
   * Parse a {@code traceparent} ({@code 00-<32 hex>-<16 hex>-<2 hex>}, lowercase, non-zero ids).
   *
   * @throws IllegalArgumentException when it is malformed
   */
  public static SpanContext parse(String traceparent) {
    SpanContext c = tryParse(traceparent);
    if (c == null) {
      throw new IllegalArgumentException("invalid traceparent: " + traceparent);
    }
    return c;
  }

  /** As {@link #parse(String)}, or {@code null} when malformed. The context is marked remote. */
  public static SpanContext tryParse(String tp) {
    if (tp == null || tp.length() < 55 || tp.charAt(2) != '-') {
      return null;
    }
    if (tp.charAt(0) != '0'
        || tp.charAt(1) != '0'
        || tp.charAt(35) != '-'
        || tp.charAt(52) != '-'
        || (tp.length() > 55 && tp.charAt(55) != '-')) {
      return null;
    }
    String trace = tp.substring(3, 35);
    String span = tp.substring(36, 52);
    if (!lowerHex(trace) || !lowerHex(span) || trace.equals(ZERO_TRACE) || span.equals(ZERO_SPAN)) {
      return null;
    }
    String flags = tp.substring(53, 55);
    if (!lowerHex(flags)) {
      return null;
    }
    return new SpanContext(trace, span, (byte) HEX.parseHex(flags)[0], true);
  }

  private static boolean lowerHex(String s) {
    for (int i = 0; i < s.length(); i++) {
      char c = s.charAt(i);
      if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) {
        return false;
      }
    }
    return true;
  }

  /** Decode an {@code aster_span_context_t}; {@code null} for an all-zero (no-op) context. */
  public static SpanContext fromNative(MemorySegment s) {
    byte[] trace = new byte[16];
    byte[] span = new byte[8];
    MemorySegment.copy(s, ValueLayout.JAVA_BYTE, 0, trace, 0, 16);
    MemorySegment.copy(s, ValueLayout.JAVA_BYTE, 16, span, 0, 8);
    String traceId = HEX.formatHex(trace);
    if (traceId.equals(ZERO_TRACE)) {
      return null;
    }
    return new SpanContext(
        traceId,
        HEX.formatHex(span),
        s.get(ValueLayout.JAVA_BYTE, 24),
        s.get(ValueLayout.JAVA_BYTE, 25) != 0);
  }

  /** Encode into an {@code aster_span_context_t} (32 bytes). */
  void toNative(MemorySegment s) {
    byte[] trace = HEX.parseHex(traceId);
    byte[] span = HEX.parseHex(spanId);
    MemorySegment.copy(trace, 0, s, ValueLayout.JAVA_BYTE, 0, 16);
    MemorySegment.copy(span, 0, s, ValueLayout.JAVA_BYTE, 16, 8);
    s.set(ValueLayout.JAVA_BYTE, 24, traceFlags);
    s.set(ValueLayout.JAVA_BYTE, 25, (byte) (remote ? 1 : 0));
  }
}
