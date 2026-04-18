package site.aster.server.spi;

import java.util.List;
import java.util.Map;

/**
 * Non-canonical per-method metadata carried in the {@code ContractManifest} JSON.
 *
 * <p>Framework may act on method-level tags (MCP visibility, client retry policy). Field-level
 * metadata (nested in {@link #fields()}) is advisory.
 *
 * @param description free-text description (first Javadoc paragraph when not explicitly set)
 * @param tags open-vocabulary semantic tags
 * @param deprecated whether the method is deprecated
 * @param fields map from wire field name → field metadata; keyed by the same name used in the
 *     manifest's {@code fields[]} list
 */
public record MethodMetadata(
    String description, List<String> tags, boolean deprecated, Map<String, FieldMetadata> fields) {

  public static final MethodMetadata EMPTY = new MethodMetadata("", List.of(), false, Map.of());

  public MethodMetadata {
    description = description == null ? "" : description;
    tags = tags == null ? List.of() : List.copyOf(tags);
    fields = fields == null ? Map.of() : Map.copyOf(fields);
  }
}
