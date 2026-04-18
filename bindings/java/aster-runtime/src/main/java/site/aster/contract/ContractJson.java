package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;

/**
 * Shared Jackson {@link ObjectMapper} tuned for contract identity JSON. Single instance per JVM;
 * ObjectMapper is thread-safe once configured.
 *
 * <p>{@link JsonInclude.Include#ALWAYS} is deliberate: the Rust serde shape has default values for
 * optional fields, but JSON-with-missing-keys is fragile during cross-language testing. Emitting
 * every field makes diffs human-readable when parity drifts.
 */
public final class ContractJson {

  private static final ObjectMapper MAPPER =
      new ObjectMapper().setSerializationInclusion(JsonInclude.Include.ALWAYS);

  private ContractJson() {}

  public static ObjectMapper mapper() {
    return MAPPER;
  }

  /** Serialize a contract identity record to JSON. */
  public static String toJson(Object record) {
    try {
      return MAPPER.writeValueAsString(record);
    } catch (JsonProcessingException e) {
      throw new IllegalStateException(
          "serialize " + record.getClass().getSimpleName() + " failed", e);
    }
  }

  /** Deserialize a contract identity record from JSON. */
  public static <T> T fromJson(String json, Class<T> type) {
    try {
      return MAPPER.readValue(json, type);
    } catch (com.fasterxml.jackson.core.JsonProcessingException e) {
      throw new IllegalArgumentException("deserialize " + type.getSimpleName() + " failed", e);
    }
  }
}
