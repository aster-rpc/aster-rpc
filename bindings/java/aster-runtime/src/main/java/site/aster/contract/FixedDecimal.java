package site.aster.contract;

import java.lang.annotation.Documented;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Declares a {@link java.math.BigDecimal} field (or the {@code BigDecimal} elements of an {@code
 * Optional}, {@code List}, {@code Set} or {@code Map} value field) as the contract type {@code
 * decimal(p,s)} (Aster-ContractIdentity.md, "Time, UUID, and Decimal"; Rust {@code
 * aster::rpc::FixedDecimal<P, S>}, Python {@code aster.decimal(p, s)}).
 *
 * <p>{@code (precision, scale)} is part of the field's contract identity ({@code decimal(18,4)}
 * hashes differently from {@code decimal(10,2)} and {@code decimal}). On the wire it is Fory
 * DECIMAL at scale exactly {@code scale}: {@link site.aster.codec.ForyCodec} zero-pads a value to
 * that scale on encode and refuses one that would need rounding or has more than {@code precision}
 * digits, and applies the same rule to what it decodes. A plain {@code BigDecimal} field is {@code
 * decimal} (arbitrary precision, the value's own scale, no check).
 */
@Documented
@Retention(RetentionPolicy.RUNTIME)
@Target({ElementType.FIELD, ElementType.RECORD_COMPONENT, ElementType.METHOD})
public @interface FixedDecimal {
  /** Total significant digits of the unscaled value, {@code 1..1000}. */
  int precision();

  /** Digits after the point, {@code 0..precision}. */
  int scale();
}
