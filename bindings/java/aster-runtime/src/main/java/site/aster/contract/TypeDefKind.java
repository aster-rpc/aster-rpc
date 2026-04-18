package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;

/** Shape category for a {@link TypeDef}: struct-like message, enum, or sealed union. */
public enum TypeDefKind {
  MESSAGE(0, "message"),
  ENUM(1, "enum"),
  UNION(2, "union");

  private final int code;
  private final String wire;

  TypeDefKind(int code, String wire) {
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
  public static TypeDefKind fromWire(String s) {
    for (TypeDefKind v : values()) {
      if (v.wire.equals(s)) {
        return v;
      }
    }
    throw new IllegalArgumentException("unknown TypeDefKind: " + s);
  }
}
