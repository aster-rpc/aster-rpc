package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonPropertyOrder;

/**
 * One field inside a {@link TypeDef} of kind {@link TypeDefKind#MESSAGE}. Serialized form matches
 * the Rust {@code FieldDef} serde struct (snake_case keys, hex-encoded byte fields).
 *
 * <p>{@code id} is carried for source compatibility but is <strong>ignored</strong> by the Rust
 * canonicalizer — per spec §11.3.2.3 field IDs are re-derived as 1-based NFC-name-sorted positions
 * at canonicalization time, so Java's field emission order (reflection order vs. declaration order)
 * can never affect {@code contract_id}.
 *
 * <p>{@code typeRef} and {@code containerKeyRef} are hex-encoded 32-byte BLAKE3 digests (empty
 * string when unused). {@code defaultValue} is the hex-encoded canonical XLANG bytes of the
 * declared scalar default, or the single-byte sentinel {@code "00"} for empty containers.
 */
@JsonPropertyOrder({
  "id",
  "name",
  "type_kind",
  "type_primitive",
  "type_ref",
  "self_ref_name",
  "optional",
  "ref_tracked",
  "container",
  "container_key_kind",
  "container_key_primitive",
  "container_key_ref",
  "required",
  "default_value"
})
@JsonInclude(JsonInclude.Include.ALWAYS)
public record FieldDef(
    @JsonProperty("id") int id,
    @JsonProperty("name") String name,
    @JsonProperty("type_kind") TypeKind typeKind,
    @JsonProperty("type_primitive") String typePrimitive,
    @JsonProperty("type_ref") String typeRef,
    @JsonProperty("self_ref_name") String selfRefName,
    @JsonProperty("optional") boolean optional,
    @JsonProperty("ref_tracked") boolean refTracked,
    @JsonProperty("container") ContainerKind container,
    @JsonProperty("container_key_kind") TypeKind containerKeyKind,
    @JsonProperty("container_key_primitive") String containerKeyPrimitive,
    @JsonProperty("container_key_ref") String containerKeyRef,
    @JsonProperty("required") boolean required,
    @JsonProperty("default_value") String defaultValue) {

  public FieldDef {
    name = name == null ? "" : name;
    typeKind = typeKind == null ? TypeKind.PRIMITIVE : typeKind;
    typePrimitive = typePrimitive == null ? "" : typePrimitive;
    typeRef = typeRef == null ? "" : typeRef;
    selfRefName = selfRefName == null ? "" : selfRefName;
    container = container == null ? ContainerKind.NONE : container;
    containerKeyKind = containerKeyKind == null ? TypeKind.PRIMITIVE : containerKeyKind;
    containerKeyPrimitive = containerKeyPrimitive == null ? "" : containerKeyPrimitive;
    containerKeyRef = containerKeyRef == null ? "" : containerKeyRef;
    defaultValue = defaultValue == null ? "" : defaultValue;
  }

  /**
   * Jackson constructor for deserialization, applying the same defaults the Rust side does. Used
   * when tests round-trip JSON through the model.
   */
  @JsonCreator
  public static FieldDef deserialize(
      @JsonProperty("id") Integer id,
      @JsonProperty("name") String name,
      @JsonProperty("type_kind") TypeKind typeKind,
      @JsonProperty("type_primitive") String typePrimitive,
      @JsonProperty("type_ref") String typeRef,
      @JsonProperty("self_ref_name") String selfRefName,
      @JsonProperty("optional") Boolean optional,
      @JsonProperty("ref_tracked") Boolean refTracked,
      @JsonProperty("container") ContainerKind container,
      @JsonProperty("container_key_kind") TypeKind containerKeyKind,
      @JsonProperty("container_key_primitive") String containerKeyPrimitive,
      @JsonProperty("container_key_ref") String containerKeyRef,
      @JsonProperty("required") Boolean required,
      @JsonProperty("default_value") String defaultValue) {
    return new FieldDef(
        id == null ? 0 : id,
        name,
        typeKind,
        typePrimitive,
        typeRef,
        selfRefName,
        optional != null && optional,
        refTracked != null && refTracked,
        container,
        containerKeyKind,
        containerKeyPrimitive,
        containerKeyRef,
        // Rust defaults required=true on missing to canonicalize legacy JSON as "all required".
        required == null || required,
        defaultValue);
  }
}
