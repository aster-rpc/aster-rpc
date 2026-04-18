package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonValue;

/** The four RPC patterns. Wire form is snake_case to match {@link MethodDef} serde. */
public enum MethodPattern {
  UNARY(0, "unary"),
  SERVER_STREAM(1, "server_stream"),
  CLIENT_STREAM(2, "client_stream"),
  BIDI_STREAM(3, "bidi_stream");

  private final int code;
  private final String wire;

  MethodPattern(int code, String wire) {
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
  public static MethodPattern fromWire(String s) {
    for (MethodPattern v : values()) {
      if (v.wire.equals(s)) {
        return v;
      }
    }
    throw new IllegalArgumentException("unknown MethodPattern: " + s);
  }

  public static MethodPattern fromStreamingKind(site.aster.server.spi.StreamingKind kind) {
    return switch (kind) {
      case UNARY -> UNARY;
      case SERVER_STREAM -> SERVER_STREAM;
      case CLIENT_STREAM -> CLIENT_STREAM;
      case BIDI_STREAM -> BIDI_STREAM;
    };
  }
}
