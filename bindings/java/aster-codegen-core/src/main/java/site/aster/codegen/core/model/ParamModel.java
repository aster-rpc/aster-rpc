package site.aster.codegen.core.model;

import com.palantir.javapoet.TypeName;
import java.util.List;

/**
 * One positional parameter on an inline ({@link RequestStyle#INLINE}) method. Captured once by the
 * processor at build time and consumed by emitters.
 *
 * <p>{@code description} and {@code tags} are non-canonical and carry {@code @Description} metadata
 * (or a Javadoc fallback for description). They end up in the generated dispatcher's {@link
 * site.aster.codegen.core.model.FieldModel}-equivalent constants so the {@code ContractManifest}
 * JSON builder can read them off the SPI.
 *
 * @param name parameter name as declared in the user source
 * @param type Java type reference used for synthesizing the {@code {Method}Request} record and for
 *     unpacking in the generated dispatcher
 * @param description free-text description; empty when neither {@code @Description} nor Javadoc
 *     supplied one
 * @param tags open-vocabulary semantic tags; empty when no {@code @Description} was set
 */
public record ParamModel(String name, TypeName type, String description, List<String> tags) {

  public ParamModel {
    description = description == null ? "" : description;
    tags = tags == null ? List.of() : List.copyOf(tags);
  }

  /** Legacy constructor for callers not yet supplying metadata. */
  public ParamModel(String name, TypeName type) {
    this(name, type, "", List.of());
  }
}
