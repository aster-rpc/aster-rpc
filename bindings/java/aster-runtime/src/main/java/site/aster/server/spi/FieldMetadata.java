package site.aster.server.spi;

import java.util.List;

/**
 * Non-canonical per-field metadata carried in the {@code ContractManifest} JSON.
 *
 * <p>Sourced from {@code @Description} on a record component, class field, or Mode 2 parameter.
 * Field tags are advisory only — the framework guarantees round-trip through the manifest but does
 * not act on them.
 *
 * @param description free-text description
 * @param tags open-vocabulary semantic tags (e.g. {@code pii}, {@code secret}, {@code redacted})
 */
public record FieldMetadata(String description, List<String> tags) {

  public static final FieldMetadata EMPTY = new FieldMetadata("", List.of());

  public FieldMetadata {
    description = description == null ? "" : description;
    tags = tags == null ? List.of() : List.copyOf(tags);
  }
}
