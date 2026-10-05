package site.aster.otel;

import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * The call-context carrier (design §3.3, §5.3): the current span context and W3C baggage. Bound to
 * a {@link ScopedValue} by {@link Tracing}; inside an RPC handler it holds the call's server span
 * and the inbound baggage.
 *
 * @param span the current span, or {@code null} when the call carries baggage but no exported span
 * @param baggage W3C baggage members; immutable, never {@code null}
 */
public record TraceContext(SpanContext span, Map<String, String> baggage) {

  /** W3C limits (Rust enforces them when it strips the header; this is the pass-through path). */
  static final int BAGGAGE_MAX_BYTES = 8192;

  static final int BAGGAGE_MAX_MEMBERS = 64;

  public TraceContext {
    baggage = baggage == null || baggage.isEmpty() ? Map.of() : Map.copyOf(baggage);
  }

  /** {@code W3C baggage} header text for {@code baggage}: {@code k=v,k2=v2}, percent-encoded. */
  static String formatBaggage(Map<String, String> baggage) {
    StringBuilder sb = new StringBuilder();
    for (Map.Entry<String, String> e : baggage.entrySet()) {
      if (!sb.isEmpty()) {
        sb.append(',');
      }
      sb.append(encode(e.getKey())).append('=').append(encode(e.getValue()));
    }
    return sb.toString();
  }

  /** Parse a {@code baggage} header; members without {@code =} and properties are dropped. */
  static Map<String, String> parseBaggage(String header) {
    if (header == null || header.isEmpty()) {
      return Map.of();
    }
    Map<String, String> out = new LinkedHashMap<>();
    for (String member : header.split(",")) {
      int semi = member.indexOf(';');
      String pair = semi >= 0 ? member.substring(0, semi) : member;
      int eq = pair.indexOf('=');
      if (eq <= 0) {
        continue;
      }
      String k = pair.substring(0, eq).strip();
      if (!k.isEmpty()) {
        out.put(decode(k), decode(pair.substring(eq + 1).strip()));
      }
    }
    return out;
  }

  /** Whether a pass-through {@code baggage} header is within the W3C limits. */
  static boolean withinLimits(String header) {
    if (header == null) {
      return false;
    }
    if (header.getBytes(StandardCharsets.UTF_8).length > BAGGAGE_MAX_BYTES) {
      return false;
    }
    int members = 1;
    for (int i = 0; i < header.length(); i++) {
      if (header.charAt(i) == ',') {
        members++;
      }
    }
    return members <= BAGGAGE_MAX_MEMBERS;
  }

  private static String encode(String s) {
    StringBuilder sb = new StringBuilder();
    for (byte b : s.getBytes(StandardCharsets.UTF_8)) {
      int c = b & 0xff;
      if ((c >= 'A' && c <= 'Z')
          || (c >= 'a' && c <= 'z')
          || (c >= '0' && c <= '9')
          || c == '-'
          || c == '.'
          || c == '_'
          || c == '~') {
        sb.append((char) c);
      } else {
        sb.append('%')
            .append("0123456789ABCDEF".charAt(c >> 4))
            .append("0123456789ABCDEF".charAt(c & 15));
      }
    }
    return sb.toString();
  }

  private static String decode(String s) {
    if (s.indexOf('%') < 0) {
      return s;
    }
    ByteArrayOutputStream out = new ByteArrayOutputStream(s.length());
    byte[] in = s.getBytes(StandardCharsets.UTF_8);
    for (int i = 0; i < in.length; i++) {
      if (in[i] == '%' && i + 2 < in.length && hex(in[i + 1]) >= 0 && hex(in[i + 2]) >= 0) {
        out.write((hex(in[i + 1]) << 4) | hex(in[i + 2]));
        i += 2;
      } else {
        out.write(in[i]);
      }
    }
    return out.toString(StandardCharsets.UTF_8);
  }

  private static int hex(byte b) {
    return Character.digit((char) b, 16);
  }
}
