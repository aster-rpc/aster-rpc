package site.aster.audit;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.PropertyNamingStrategies;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.io.IOException;
import java.util.Map;

/** Versioned JSON value conversion; opaque envelopes are never reserialized. */
final class AuditJson {
  private AuditJson() {}

  static final ObjectMapper MAPPER =
      new ObjectMapper().setPropertyNamingStrategy(PropertyNamingStrategies.SNAKE_CASE);

  static void validatePayload(JsonNode value) {
    if (value.isFloatingPointNumber() && !Double.isFinite(value.doubleValue())) {
      throw new AuditOutboxException(
          AuditOutboxException.Kind.INVALID_EVENT,
          "audit payload contains a nonfinite JSON number");
    }
    if (value.isContainerNode()) value.forEach(AuditJson::validatePayload);
  }

  static byte[] request(String key, Object value) {
    try {
      return MAPPER.writeValueAsBytes(Map.of("version", 1, key, value));
    } catch (IOException | IllegalArgumentException error) {
      throw new AuditOutboxException(
          AuditOutboxException.Kind.INVALID_EVENT, "audit arguments must be JSON serializable");
    }
  }

  static byte[] config(AuditOutbox.Config config) {
    ObjectNode value = MAPPER.createObjectNode();
    value.put("directory", config.directory().toString());
    value.set("destination", MAPPER.valueToTree(config.destination()));
    value.put("max_retained_bytes", config.maxRetainedBytes());
    value.put("max_event_bytes", config.maxEventBytes());
    return request("config", value);
  }

  static <T> T response(byte[] data, String key, Class<T> type) {
    try {
      JsonNode root = MAPPER.readTree(data);
      if (root.path("version").asInt() != 1 || !root.has(key))
        throw new IOException("unsupported response");
      return MAPPER.treeToValue(root.get(key), type);
    } catch (IOException | IllegalArgumentException error) {
      throw new AuditOutboxException(AuditOutboxException.Kind.OTHER, "invalid audit response");
    }
  }
}
