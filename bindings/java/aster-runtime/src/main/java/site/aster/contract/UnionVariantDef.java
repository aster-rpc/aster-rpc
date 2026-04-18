package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonPropertyOrder;

/** One variant inside a {@link TypeDef} of kind {@link TypeDefKind#UNION}. */
@JsonPropertyOrder({"name", "id", "type_ref"})
public record UnionVariantDef(
    @JsonProperty("name") String name,
    @JsonProperty("id") int id,
    /** Hex-encoded 32-byte BLAKE3 hash of the variant's referenced TypeDef. */
    @JsonProperty("type_ref") String typeRef) {

  public UnionVariantDef {
    name = name == null ? "" : name;
    typeRef = typeRef == null ? "" : typeRef;
  }
}
