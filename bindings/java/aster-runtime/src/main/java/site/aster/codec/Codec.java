package site.aster.codec;

/**
 * Wire serialization indirection. Aster supports multiple modes (raw bytes, Fory cross-language,
 * JSON for tests). The active codec advertises its mode string in the registry lease's {@code
 * serialization_modes} list so callers know how to encode requests.
 */
public interface Codec {

  /**
   * Mode tag matching the registry contract (e.g. {@code "raw"}, {@code "fory-xlang"}). AsterServer
   * publishes this in its lease so clients can pick a compatible codec via the standard mandatory
   * filters.
   */
  String mode();

  /** Encode a value to bytes. */
  byte[] encode(Object value);

  /** Decode bytes back to a value of the requested type. */
  Object decode(byte[] payload, Class<?> type);
}
