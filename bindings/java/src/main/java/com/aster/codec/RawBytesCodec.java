package com.aster.codec;

/**
 * Pass-through codec: only accepts {@code byte[]} values, returns them as-is. Useful for
 * opaque-payload services and tests where the host owns the wire format end-to-end.
 */
public final class RawBytesCodec implements Codec {

  @Override
  public String mode() {
    return "raw";
  }

  @Override
  public byte[] encode(Object value) {
    if (value == null) {
      return new byte[0];
    }
    if (value instanceof byte[] bytes) {
      return bytes;
    }
    throw new IllegalArgumentException(
        "RawBytesCodec only accepts byte[]; got " + value.getClass().getName());
  }

  @Override
  public Object decode(byte[] payload, Class<?> type) {
    if (type == byte[].class || type == Object.class) {
      return payload;
    }
    throw new IllegalArgumentException(
        "RawBytesCodec only decodes to byte[]; got " + type.getName());
  }
}
