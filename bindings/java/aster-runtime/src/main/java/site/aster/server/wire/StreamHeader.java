package site.aster.server.wire;

import java.util.List;

/**
 * First frame on every QUIC stream (HEADER flag). Carries service routing, contract identity, call
 * metadata, and the negotiated serialization mode.
 *
 * <p>Matches the Python reference type {@code _aster/StreamHeader} defined in {@code
 * bindings/python/aster/protocol.py}. Fory xlang serialization with exactly these field names, in
 * declaration order, is required for cross-language interop.
 *
 * <p>{@code metadata} is modelled as parallel key/value lists rather than a map to match the Python
 * layout (dataclass field order determines Fory field ids).
 */
public record StreamHeader(
    String service,
    String method,
    int version,
    int callId,
    short deadline,
    byte serializationMode,
    List<String> metadataKeys,
    List<String> metadataValues) {

  public static final byte SERIALIZATION_XLANG = 0;
  public static final byte SERIALIZATION_NATIVE = 1;
  public static final byte SERIALIZATION_ROW = 2;
  public static final byte SERIALIZATION_JSON = 3;

  public StreamHeader {
    metadataKeys = metadataKeys == null ? List.of() : List.copyOf(metadataKeys);
    metadataValues = metadataValues == null ? List.of() : List.copyOf(metadataValues);
  }
}
