package site.aster.codegen.core.model;

import java.util.List;
import site.aster.annotations.RequiresKind;

/**
 * Language-neutral carrier for a {@code @Requires} annotation. Populated by {@code
 * aster-codegen-apt} (from {@code AnnotationMirror}) or {@code aster-codegen-ksp} (from {@code
 * KSAnnotation}) and consumed by {@code DispatcherEmitter} to emit a {@code
 * site.aster.contract.CapabilityRequirement} literal.
 *
 * <p>Kept out of {@code aster-runtime}'s {@code CapabilityRequirement} so codegen-core stays
 * decoupled from the runtime; the emitter bridges the two.
 */
public record RequiresSpec(RequiresKind kind, List<String> roles) {

  public RequiresSpec {
    kind = kind == null ? RequiresKind.ROLE : kind;
    roles = roles == null ? List.of() : List.copyOf(roles);
  }
}
