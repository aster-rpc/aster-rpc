package com.aster.ffi;

/** Relay mode for endpoint configuration. */
public enum IrohRelayMode {
  DEFAULT(0),
  CUSTOM(1),
  DISABLED(2);

  public final int code;

  IrohRelayMode(int code) {
    this.code = code;
  }

  public static IrohRelayMode fromCode(int code) {
    for (var v : values()) {
      if (v.code == code) return v;
    }
    return DEFAULT;
  }
}
