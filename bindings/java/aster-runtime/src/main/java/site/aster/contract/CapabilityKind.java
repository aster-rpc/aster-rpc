package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;

/** Role-check capability kind for {@link CapabilityRequirement}. */
public enum CapabilityKind {
  ROLE(0, "role"),
  ANY_OF(1, "any_of"),
  ALL_OF(2, "all_of");

  private final int code;
  private final String wire;

  CapabilityKind(int code, String wire) {
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
  public static CapabilityKind fromWire(String s) {
    for (CapabilityKind v : values()) {
      if (v.wire.equals(s)) {
        return v;
      }
    }
    throw new IllegalArgumentException("unknown CapabilityKind: " + s);
  }
}
