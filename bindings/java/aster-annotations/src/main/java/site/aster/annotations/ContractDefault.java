package site.aster.annotations;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Declare a wire-type field's <strong>contract default</strong> (Aster-ContractIdentity.md
 * §11.3.2.3, "Default lowering into {@code FieldDef}").
 *
 * <p>A Java record or class always constructs its fields with some value, so a plain initializer is
 * <em>not</em> a contract default: such a field is published as {@code required}. Annotate the
 * fields whose default is part of the contract. The default is then baked into {@code contract_id}
 * ({@code FieldDef.required = false} plus the canonical default bytes), exactly like Rust's {@code
 * #[aster(default = ...)]}, Python's {@code contract_default(...)} and TypeScript's
 * {@code @ContractDefault()}. Changing a contract default changes the contract id (a breaking
 * change); the Fory payload bytes are unaffected.
 *
 * <p>The value is a literal parsed according to the field's wire type:
 *
 * <ul>
 *   <li>{@code String}: the text itself ({@code @ContractDefault("idle")},
 *       {@code @ContractDefault("")})
 *   <li>{@code boolean}: {@code "true"} or {@code "false"}
 *   <li>{@code int} / {@code Integer} (varint32) and {@code long} / {@code Long} (varint64): a
 *       decimal integer in range
 *   <li>{@code double} / {@code Double} (float64): a finite, non-subnormal decimal ({@code "0.5"});
 *       {@code -0.0} is written as {@code 0.0}
 *   <li>{@code Optional<T>} of the above: the non-null default of {@code T} (a {@code null} default
 *       is not expressible in v1; leave the field unannotated to keep it required)
 *   <li>{@code List} / {@code Set} / {@code Map}: <em>no value</em> (bare
 *       {@code @ContractDefault}); only the empty container may be a default
 * </ul>
 *
 * <p>Anything else (nested messages, {@code byte}/{@code short}/{@code float}, {@code byte[]},
 * enums, a scalar without a value, an unparsable literal) is refused with an {@link
 * IllegalArgumentException} when the contract is computed.
 *
 * <pre>{@code
 * @WireType("defaults.v1/Settings")
 * public record Settings(
 *     String name,                          // required
 *     @ContractDefault("idle") String state,
 *     @ContractDefault("-3") int retries,
 *     @ContractDefault List<String> tags) {}
 * }</pre>
 *
 * <p>On a plain class put it on the field. In Kotlin use {@code @field:ContractDefault(...)}.
 */
@Retention(RetentionPolicy.RUNTIME)
@Target({ElementType.FIELD, ElementType.RECORD_COMPONENT})
public @interface ContractDefault {

  /** Sentinel for "no value given" (a bare {@code @ContractDefault}); valid for containers only. */
  String UNSET = "\u0000aster:unset";

  /** The default's literal; see the class documentation for the per-type syntax. */
  String value() default UNSET;
}
