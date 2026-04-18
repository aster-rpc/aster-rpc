package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonPropertyOrder;

/** One value in a {@link TypeDef} of kind {@link TypeDefKind#ENUM}. */
@JsonPropertyOrder({"name", "value"})
public record EnumValueDef(@JsonProperty("name") String name, @JsonProperty("value") int value) {

  public EnumValueDef {
    name = name == null ? "" : name;
  }
}
