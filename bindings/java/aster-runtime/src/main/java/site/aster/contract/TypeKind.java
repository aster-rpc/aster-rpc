package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;

/**
 * Canonical type kind used by {@link FieldDef} and its container-key companion. Mirrors the
 * normative numeric values from spec §11.3.3; JSON serialization uses the snake_case string form to
 * match Rust's {@code serde(rename_all = "snake_case")}.
 */
public enum TypeKind {
  PRIMITIVE(0, "primitive"),
  REF(1, "ref"),
  SELF_REF(2, "self_ref"),
  ANY(3, "any");

  private final int code;
  private final String wire;

  TypeKind(int code, String wire) {
    this.code = code;
    this.wire = wire;
  }

  public int code() {
    return code;
  }

  @JsonValue
  public String wire() {
    return wire;
  }

  @JsonCreator
  public static TypeKind fromWire(String s) {
    for (TypeKind v : values()) {
      if (v.wire.equals(s)) {
        return v;
      }
    }
    throw new IllegalArgumentException("unknown TypeKind: " + s);
  }
}
