package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonPropertyOrder;
import java.util.List;

/**
 * User-defined type descriptor. Java hands these off to the Rust canonicalizer one at a time via
 * {@link ContractIdentity#computeTypeHash(String)} so the resulting 32-byte digest can be embedded
 * as a {@code type_ref} in other TypeDefs / the parent MethodDef.
 *
 * <p>{@code package} and {@code name} derive from {@code @WireType("ns/Name")} when present (split
 * on slash), falling back to the Java class's package + simple name for untagged types.
 */
@JsonPropertyOrder({"kind", "package", "name", "fields", "enum_values", "union_variants"})
public record TypeDef(
    @JsonProperty("kind") TypeDefKind kind,
    @JsonProperty("package") String packageName,
    @JsonProperty("name") String name,
    @JsonProperty("fields") List<FieldDef> fields,
    @JsonProperty("enum_values") List<EnumValueDef> enumValues,
    @JsonProperty("union_variants") List<UnionVariantDef> unionVariants) {

  public TypeDef {
    kind = kind == null ? TypeDefKind.MESSAGE : kind;
    packageName = packageName == null ? "" : packageName;
    name = name == null ? "" : name;
    fields = fields == null ? List.of() : List.copyOf(fields);
    enumValues = enumValues == null ? List.of() : List.copyOf(enumValues);
    unionVariants = unionVariants == null ? List.of() : List.copyOf(unionVariants);
  }

  @JsonCreator
  public static TypeDef deserialize(
      @JsonProperty("kind") TypeDefKind kind,
      @JsonProperty("package") String packageName,
      @JsonProperty("name") String name,
      @JsonProperty("fields") List<FieldDef> fields,
      @JsonProperty("enum_values") List<EnumValueDef> enumValues,
      @JsonProperty("union_variants") List<UnionVariantDef> unionVariants) {
    return new TypeDef(kind, packageName, name, fields, enumValues, unionVariants);
  }
}
