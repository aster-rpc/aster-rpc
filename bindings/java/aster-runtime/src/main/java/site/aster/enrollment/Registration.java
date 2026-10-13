package site.aster.enrollment;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;

/** What an issued credential asks for. */
public enum Registration {
  /** The credential is the node's whole authority (default). */
  EPHEMERAL("ephemeral"),
  /** Admits the node until the root promotes it. */
  PERSISTENT("persistent");

  private final String wire;

  Registration(String wire) {
    this.wire = wire;
  }

  /** The JSON name. */
  @JsonValue
  public String wire() {
    return wire;
  }

  /** The value of a JSON name; {@code null} for an unknown one. */
  @JsonCreator
  public static Registration fromWire(String wire) {
    for (Registration r : values()) {
      if (r.wire.equals(wire)) {
        return r;
      }
    }
    return null;
  }
}
