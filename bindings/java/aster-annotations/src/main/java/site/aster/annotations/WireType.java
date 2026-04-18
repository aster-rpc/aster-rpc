package site.aster.annotations;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Pins the on-wire type identity of a request / response / parameter class independently of its
 * Java package and simple name.
 *
 * <p>Mirrors Python's {@code @wire_type("namespace/Name")}: every binding that implements the same
 * logical type in its native language must advertise the same wire tag for the Rust canonicalizer
 * to hash it identically. Without this annotation, Aster derives the tag from the Java class's
 * package + simple name, which means a Java package like {@code com.example.billing.Invoice} and a
 * Python {@code @wire_type("billing/Invoice")} type produce different {@code contract_id}s even
 * though both describe the same service.
 *
 * <p>The tag uses slash as the namespace separator, matching Fory's XLANG tag format (e.g. {@code
 * "billing/Invoice"}, {@code "_aster/StreamHeader"}). No validation is performed on the tag at
 * annotation time; the Rust canonicalizer is the source of truth for well-formedness.
 *
 * <p>{@link RetentionPolicy#RUNTIME} so hand-written Fory registrations can query the tag at
 * runtime via reflection when the build-time codegen isn't involved.
 *
 * <p>Usage on a record:
 *
 * <pre>{@code
 * @WireType("billing/Invoice")
 * public record Invoice(String customer, long amountCents, boolean paid) {}
 * }</pre>
 */
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface WireType {
  /** The wire tag, typically {@code "namespace/TypeName"}. */
  String value();
}
