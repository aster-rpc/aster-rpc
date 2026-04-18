package site.aster.annotations;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

@Retention(RetentionPolicy.CLASS)
@Target(ElementType.TYPE)
public @interface Service {
  String name();

  int version() default 1;

  Scope scoped() default Scope.SHARED;

  /**
   * Human-readable description of the service. Flows into the published {@code ContractManifest}
   * and is surfaced by MCP tool definitions and shell displays. Non-canonical: does not affect the
   * contract identity hash. When empty, the APT/KSP processor falls back to the first paragraph of
   * the class Javadoc.
   */
  String description() default "";

  /**
   * Open-vocabulary semantic tags. See {@code docs/_internal/rich_metadata/README.md} for the
   * conventional vocabulary (e.g. {@code readonly}, {@code sensitive}, {@code destructive}). The
   * framework may act on service-level tags (MCP filters, transport policy). Non-canonical.
   */
  String[] tags() default {};
}
