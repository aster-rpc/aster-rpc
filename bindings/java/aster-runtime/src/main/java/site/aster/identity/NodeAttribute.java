package site.aster.identity;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.JsonNodeFactory;
import java.util.Optional;

/**
 * A typed {@code aster.ops.NodeConfig} attribute (Rust {@code aster::rpc::AttrValue}): {@code type}
 * is {@code "bool"}, {@code "int"}, {@code "float"} or {@code "string"}. Attributes describe the
 * node and never authorise anything.
 */
public record NodeAttribute(String type, JsonNode value) {

  private static final JsonNodeFactory F = JsonNodeFactory.instance;

  public static NodeAttribute ofBool(boolean v) {
    return new NodeAttribute("bool", F.booleanNode(v));
  }

  public static NodeAttribute ofInt(long v) {
    return new NodeAttribute("int", F.numberNode(v));
  }

  public static NodeAttribute ofFloat(double v) {
    return new NodeAttribute("float", F.numberNode(v));
  }

  public static NodeAttribute ofString(String v) {
    return new NodeAttribute("string", F.textNode(v));
  }

  /** The value of a {@code "bool"} attribute. */
  public Optional<Boolean> asBool() {
    return "bool".equals(type) && value != null && value.isBoolean()
        ? Optional.of(value.booleanValue())
        : Optional.empty();
  }

  /** The value of an {@code "int"} attribute. */
  public Optional<Long> asInt() {
    return "int".equals(type) && value != null && value.canConvertToLong()
        ? Optional.of(value.longValue())
        : Optional.empty();
  }

  /** The value of a {@code "float"} attribute. */
  public Optional<Double> asFloat() {
    return "float".equals(type) && value != null && value.isNumber()
        ? Optional.of(value.doubleValue())
        : Optional.empty();
  }

  /** The value of a {@code "string"} attribute. */
  public Optional<String> asString() {
    return "string".equals(type) && value != null && value.isTextual()
        ? Optional.of(value.textValue())
        : Optional.empty();
  }
}
