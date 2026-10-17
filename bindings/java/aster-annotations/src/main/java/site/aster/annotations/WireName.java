package site.aster.annotations;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * A field's contract name, when the contract is defined in another language (default: the Java
 * field or record component name, verbatim). See {@code ffi_spec/Aster-FieldNaming.md}.
 *
 * <p>Fory Java writes the member's own snake_case form on the wire, so the override's snake_case
 * form must equal the member's: a member {@code writtenBy} may carry
 * {@code @WireName("written_by")} (both {@code written_by}), a member {@code author} may not.
 * Contract definition refuses a mismatch.
 *
 * <pre>{@code
 * @WireType("books/Book")   // a contract defined in Rust
 * public record Book(@WireName("written_by") String writtenBy) {}
 * }</pre>
 */
@Retention(RetentionPolicy.RUNTIME)
@Target({ElementType.FIELD, ElementType.RECORD_COMPONENT})
public @interface WireName {
  /** The field's contract name. */
  String value();
}
