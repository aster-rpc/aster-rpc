package site.aster.codegen.core.model;

import java.util.List;

/**
 * Metadata captured for one wire-type field (record component or class field) carrying a
 * {@code @Description} annotation or Javadoc comment.
 *
 * <p>Used as the value type in {@link MethodModel#fieldMetadata()} — keyed by the wire field name
 * so it round-trips through the {@code ContractManifest} JSON.
 *
 * @param name wire field name (matches the record component / field name exactly)
 * @param description free-text description; falls back to the first Javadoc paragraph
 * @param tags open-vocabulary semantic tags; only ever populated from {@code @Description(tags=)}
 */
public record FieldModel(String name, String description, List<String> tags) {

  public FieldModel {
    description = description == null ? "" : description;
    tags = tags == null ? List.of() : List.copyOf(tags);
  }
}
