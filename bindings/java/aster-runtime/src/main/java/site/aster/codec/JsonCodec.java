package site.aster.codec;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonMappingException;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.exc.UnrecognizedPropertyException;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;
import site.aster.interceptors.ContractViolationError;

/**
 * Jackson-backed JSON codec with strict shape validation.
 *
 * <p>The producer owns the contract. Any JSON key that doesn't map to a declared field on the
 * expected class raises {@link ContractViolationError} — at any nesting depth — before the handler
 * runs. This mirrors Python's {@code json_codec.py} and TypeScript's {@code JsonCodec} strict
 * walker.
 *
 * <p>Strictness is on by default because Jackson's {@code FAIL_ON_UNKNOWN_PROPERTIES} feature is
 * default-true. On violation we catch {@link UnrecognizedPropertyException} and repackage it as
 * {@link ContractViolationError} so the server path surfaces a {@link
 * site.aster.interceptors.StatusCode#CONTRACT_VIOLATION} trailer on the wire without further
 * plumbing.
 *
 * <p>When {@link #decode(byte[], Class)} is called with {@code type == null} or {@code
 * Object.class} the codec is permissive and returns a {@code Map<String, Object>} — there's no
 * shape to enforce.
 */
public final class JsonCodec implements Codec {

  private static final int MAX_REPORTED_KEYS = 5;
  private static final int MAX_KEY_LENGTH = 80;

  private final ObjectMapper mapper;

  public JsonCodec() {
    this(new ObjectMapper());
  }

  public JsonCodec(ObjectMapper mapper) {
    this.mapper = mapper;
  }

  @Override
  public String mode() {
    return "json";
  }

  @Override
  public byte[] encode(Object value) {
    if (value == null) {
      return new byte[0];
    }
    try {
      return mapper.writeValueAsBytes(value);
    } catch (JsonProcessingException e) {
      throw new IllegalArgumentException("JsonCodec.encode failed: " + e.getOriginalMessage(), e);
    }
  }

  @Override
  public Object decode(byte[] payload, Class<?> type) {
    if (payload == null || payload.length == 0) {
      return null;
    }
    Class<?> target = (type == null || type == Object.class) ? Map.class : type;
    try {
      return mapper.readValue(payload, target);
    } catch (UnrecognizedPropertyException e) {
      throw toContractViolation(e, target);
    } catch (JsonMappingException e) {
      Throwable root = e.getCause();
      while (root instanceof JsonMappingException && root.getCause() != null) {
        root = root.getCause();
      }
      if (root instanceof UnrecognizedPropertyException upe) {
        throw toContractViolation(upe, target);
      }
      throw new IllegalArgumentException("JsonCodec.decode failed: " + e.getOriginalMessage(), e);
    } catch (JsonProcessingException e) {
      throw new IllegalArgumentException("JsonCodec.decode failed: " + e.getOriginalMessage(), e);
    } catch (java.io.IOException e) {
      throw new IllegalArgumentException("JsonCodec.decode failed: " + e.getMessage(), e);
    }
  }

  private static ContractViolationError toContractViolation(
      UnrecognizedPropertyException e, Class<?> rootType) {
    String location = buildLocation(e, rootType);
    List<String> sanitized = sanitizeKeys(List.of(e.getPropertyName()));
    String expected =
        e.getKnownPropertyIds() == null
            ? "<unknown>"
            : e.getKnownPropertyIds().stream()
                .map(Object::toString)
                .sorted()
                .collect(Collectors.joining(", ", "[", "]"));
    String message =
        "contract violation at "
            + location
            + ": unexpected JSON field(s) "
            + sanitized
            + " (expected: "
            + expected
            + ")";
    Map<String, String> details = new LinkedHashMap<>();
    details.put("unexpected_fields", String.join(",", sanitized));
    details.put("location", location);
    details.put("expected_class", rootType.getSimpleName());
    return new ContractViolationError(message, details);
  }

  private static String buildLocation(UnrecognizedPropertyException e, Class<?> rootType) {
    StringBuilder sb = new StringBuilder(rootType.getSimpleName());
    if (e.getPath() == null || e.getPath().isEmpty()) {
      return sb.toString();
    }
    for (var ref : e.getPath()) {
      if (ref.getFieldName() != null) {
        sb.append('.').append(ref.getFieldName());
      } else if (ref.getIndex() >= 0) {
        sb.append('[').append(ref.getIndex()).append(']');
      }
    }
    return sb.toString();
  }

  /**
   * Repr-quote unexpected key names for safe logging. Key names can contain control chars, ANSI
   * escapes, or newlines that would corrupt error messages or terminals. Caps count and length so a
   * malicious client can't blow up log storage.
   */
  static List<String> sanitizeKeys(List<String> keys) {
    List<String> out = new java.util.ArrayList<>();
    int limit = Math.min(keys.size(), MAX_REPORTED_KEYS);
    for (int i = 0; i < limit; i++) {
      String k = keys.get(i);
      if (k == null) {
        k = "null";
      }
      if (k.length() > MAX_KEY_LENGTH) {
        k = k.substring(0, MAX_KEY_LENGTH) + "...(truncated)";
      }
      out.add(repr(k));
    }
    if (keys.size() > MAX_REPORTED_KEYS) {
      out.add("...(+" + (keys.size() - MAX_REPORTED_KEYS) + " more)");
    }
    return out;
  }

  private static String repr(String s) {
    StringBuilder sb = new StringBuilder(s.length() + 2);
    sb.append('\'');
    for (int i = 0; i < s.length(); i++) {
      char c = s.charAt(i);
      switch (c) {
        case '\\' -> sb.append("\\\\");
        case '\'' -> sb.append("\\'");
        case '\n' -> sb.append("\\n");
        case '\r' -> sb.append("\\r");
        case '\t' -> sb.append("\\t");
        default -> {
          if (c < 0x20 || c == 0x7f) {
            sb.append(String.format("\\x%02x", (int) c));
          } else {
            sb.append(c);
          }
        }
      }
    }
    sb.append('\'');
    return sb.toString();
  }

  /** Backing Jackson mapper, exposed for users who need custom registrations. */
  public ObjectMapper mapper() {
    return mapper;
  }

  @SuppressWarnings("unused")
  private static Map<String, Object> emptyMap() {
    return new HashMap<>();
  }
}
