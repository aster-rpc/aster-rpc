package site.aster.identity;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.PropertyNamingStrategies;
import java.io.IOException;
import site.aster.ffi.IrohException;

/**
 * The JSON forms of the gateway identity settings and results (C ABI 7.7): snake_case, absent
 * fields omitted, unknown fields ignored. Binding plumbing.
 */
public final class IdentityJson {

  /** The shared mapper. */
  public static final ObjectMapper MAPPER =
      new ObjectMapper()
          .setPropertyNamingStrategy(PropertyNamingStrategies.SNAKE_CASE)
          .setSerializationInclusion(JsonInclude.Include.NON_NULL)
          .configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, false);

  private IdentityJson() {}

  /** {@code value} as JSON bytes. */
  public static byte[] encode(Object value) {
    try {
      return MAPPER.writeValueAsBytes(value);
    } catch (com.fasterxml.jackson.core.JsonProcessingException e) {
      throw new IllegalArgumentException(e.getOriginalMessage(), e);
    }
  }

  /** Decode {@code json} as {@code type}; a malformed native result is an {@link IrohException}. */
  public static <T> T decode(byte[] json, Class<T> type) {
    try {
      return MAPPER.readValue(json, type);
    } catch (IOException e) {
      throw new IrohException("malformed " + type.getSimpleName() + " JSON: " + e.getMessage());
    }
  }

  /** Decode {@code json} as {@code type}; a malformed native result is an {@link IrohException}. */
  public static <T> T decode(
      byte[] json, com.fasterxml.jackson.core.type.TypeReference<T> type, String what) {
    try {
      return MAPPER.readValue(json, type);
    } catch (IOException e) {
      throw new IrohException("malformed " + what + " JSON: " + e.getMessage());
    }
  }
}
