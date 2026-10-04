package site.aster.contract;

import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.HexFormat;
import site.aster.annotations.ContractDefault;

/**
 * Lowers a {@link ContractDefault} into the canonical {@code FieldDef.default_value} bytes
 * (Aster-ContractIdentity.md §11.3.2.3 "Default lowering into {@code FieldDef}"). The same table
 * Rust ({@code #[aster(default = ...)]}), Python ({@code contract_default}) and TypeScript
 * ({@code @ContractDefault}) implement; {@code conformance/vectors/contract-defaults.json} pins it.
 */
final class ContractDefaults {

  private ContractDefaults() {}

  /**
   * The lowercase-hex {@code default_value} for a field declared with {@code ann}.
   *
   * @throws IllegalArgumentException for any default the §11.3.2.3 table does not define
   *     (decoration-time fail-fast)
   */
  static String lower(
      String owner,
      String field,
      ContractDefault ann,
      ContainerKind container,
      TypeKind kind,
      String primitive) {
    String where = "field " + owner + "." + field;
    String literal = ann.value();
    boolean unset = ContractDefault.UNSET.equals(literal);

    if (container != ContainerKind.NONE) {
      if (!unset) {
        throw new IllegalArgumentException(
            where
                + ": a list/set/map contract default must be the empty container (bare"
                + " @ContractDefault), got \""
                + literal
                + "\" (§11.3.2.3 rule 2)");
      }
      return "00";
    }
    if (kind != TypeKind.PRIMITIVE) {
      throw new IllegalArgumentException(
          where + ": only scalar fields may carry a contract default (§11.3.2.3 rule 3)");
    }
    if (unset) {
      throw new IllegalArgumentException(
          where
              + ": a scalar contract default needs a value, e.g. @ContractDefault(\"...\")"
              + " (only containers take a bare @ContractDefault)");
    }
    try {
      return HexFormat.of().formatHex(scalar(primitive, literal, where));
    } catch (NumberFormatException e) {
      throw new IllegalArgumentException(
          where + ": \"" + literal + "\" is not a valid " + primitive + " default", e);
    }
  }

  private static byte[] scalar(String primitive, String literal, String where) {
    ByteArrayOutputStream out = new ByteArrayOutputStream();
    switch (primitive) {
      case "string" -> {
        byte[] utf8 = literal.getBytes(StandardCharsets.UTF_8);
        uvarint(out, ((long) utf8.length << 2) | 2);
        out.writeBytes(utf8);
      }
      case "bool" -> {
        if (!literal.equals("true") && !literal.equals("false")) {
          throw new IllegalArgumentException(
              where
                  + ": a bool contract default must be \"true\" or \"false\", got \""
                  + literal
                  + "\"");
        }
        out.write(literal.equals("true") ? 1 : 0);
      }
      case "varint32" -> {
        int v = Integer.parseInt(literal);
        uvarint(out, ((v << 1) ^ (v >> 31)) & 0xFFFFFFFFL);
      }
      case "varint64" -> {
        long v = Long.parseLong(literal);
        uvarint(out, (v << 1) ^ (v >> 63));
      }
      case "float64" -> {
        double d = Double.parseDouble(literal);
        if (!Double.isFinite(d)) {
          throw new IllegalArgumentException(where + ": a float contract default must be finite");
        }
        if (d != 0.0 && Math.abs(d) < Double.MIN_NORMAL) {
          throw new IllegalArgumentException(
              where + ": a float contract default must not be subnormal");
        }
        if (d == 0.0) {
          d = 0.0; // -0.0 is written as +0.0
        }
        out.writeBytes(ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putDouble(d).array());
      }
      default ->
          throw new IllegalArgumentException(
              where
                  + ": contract defaults for wire type \""
                  + primitive
                  + "\" are reserved in v1 (§11.3.2.3); supported: String, boolean, int/Integer"
                  + " (varint32), long/Long (varint64), double/Double (float64) and empty"
                  + " containers");
    }
    return out.toByteArray();
  }

  /** Unsigned LEB128; {@code n} is treated as an unsigned 64-bit value. */
  private static void uvarint(ByteArrayOutputStream out, long n) {
    while (true) {
      int b = (int) (n & 0x7F);
      n >>>= 7;
      if (n != 0) {
        out.write(b | 0x80);
      } else {
        out.write(b);
        return;
      }
    }
  }
}
