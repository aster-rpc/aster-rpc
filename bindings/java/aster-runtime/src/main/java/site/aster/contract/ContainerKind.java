package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;

/** Container shape for {@link FieldDef}. See spec §11.3.3. */
public enum ContainerKind {
  NONE(0, "none"),
  LIST(1, "list"),
  SET(2, "set"),
  MAP(3, "map");

  private final int code;
  private final String wire;

  ContainerKind(int code, String wire) {
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
  public static ContainerKind fromWire(String s) {
    for (ContainerKind v : values()) {
      if (v.wire.equals(s)) {
        return v;
      }
    }
    throw new IllegalArgumentException("unknown ContainerKind: " + s);
  }
}
