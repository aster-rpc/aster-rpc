package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;

/**
 * Service lifetime scope. Shared = one singleton per server; session = one instance per client
 * connection. Legacy wire spelling "stream" is accepted on input (Rust serde alias) but Java only
 * emits "session".
 */
public enum ScopeKind {
  SHARED(0, "shared"),
  SESSION(1, "session");

  private final int code;
  private final String wire;

  ScopeKind(int code, String wire) {
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
  public static ScopeKind fromWire(String s) {
    if ("shared".equals(s)) return SHARED;
    if ("session".equals(s) || "stream".equals(s)) return SESSION;
    throw new IllegalArgumentException("unknown ScopeKind: " + s);
  }

  public static ScopeKind fromAnnotation(site.aster.annotations.Scope scope) {
    return switch (scope) {
      case SHARED -> SHARED;
      case SESSION -> SESSION;
    };
  }
}
