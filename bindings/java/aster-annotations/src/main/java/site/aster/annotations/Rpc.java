package site.aster.annotations;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

@Retention(RetentionPolicy.CLASS)
@Target(ElementType.METHOD)
public @interface Rpc {
  String name() default "";

  /**
   * Human-readable description of the method. When empty, the APT/KSP processor falls back to the
   * first paragraph of the method's Javadoc. Non-canonical.
   */
  String description() default "";

  /**
   * Semantic tags. See {@link Service#tags()} for the vocabulary. The framework may act on
   * method-level tags (MCP filters, client retry policy). Non-canonical.
   */
  String[] tags() default {};

  /** Whether this method is deprecated. Surfaces in generated clients and MCP schemas. */
  boolean deprecated() default false;
}
