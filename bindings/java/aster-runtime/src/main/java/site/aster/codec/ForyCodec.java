package site.aster.codec;

import org.apache.fory.Fory;
import org.apache.fory.config.Language;

/**
 * Apache Fory v0.16 backed codec. Exposes the underlying {@link Fory} instance so the host (or
 * eventually a decorator-driven generator) can register the contract types it needs to serialize.
 * Type registration is the caller's responsibility — this class only owns the encode/decode pump
 * and the mode tag the registry advertises.
 */
public final class ForyCodec implements Codec {

  private final Fory fory;

  public ForyCodec() {
    this.fory = Fory.builder().withLanguage(Language.XLANG).withRefTracking(true).build();
  }

  public ForyCodec(Fory fory) {
    this.fory = fory;
  }

  /**
   * The underlying Fory instance. Use this to register types via {@code fory().register(MyClass
   * .class, "fully.qualified.name")} before serializing them.
   */
  public Fory fory() {
    return fory;
  }

  @Override
  public String mode() {
    return "fory-xlang";
  }

  @Override
  public byte[] encode(Object value) {
    if (value == null) {
      return new byte[0];
    }
    return fory.serialize(value);
  }

  @Override
  public Object decode(byte[] payload, Class<?> type) {
    if (payload == null || payload.length == 0) {
      return null;
    }
    return fory.deserialize(payload);
  }
}
