package site.aster.annotations;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

@Retention(RetentionPolicy.CLASS)
@Target(ElementType.METHOD)
public @interface ServerStream {
  String name() default "";

  /** See {@link Rpc#description()}. */
  String description() default "";

  /** See {@link Rpc#tags()}. */
  String[] tags() default {};

  /** See {@link Rpc#deprecated()}. */
  boolean deprecated() default false;
}
