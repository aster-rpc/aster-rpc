package site.aster.server.wire;

import java.util.List;
import java.util.Objects;

/**
 * First frame on every QUIC stream (HEADER flag). Carries service routing, contract identity, call
 * metadata, and the negotiated serialization mode.
 *
 * <p>Matches the Python reference type {@code _aster/StreamHeader} defined in {@code
 * bindings/python/aster/protocol.py}. Fory xlang serialization with exactly these field names, in
 * declaration order, is required for cross-language interop.
 *
 * <p>This is a plain class rather than a {@code record} because Fory (as of 0.16) fails to
 * round-trip Java records that carry collection fields like {@code List<String>} — the decoder
 * reads the collection but then stops before the terminator byte, yielding {@code
 * IndexOutOfBoundsException}. Plain classes with public mutable fields work reliably. Accessor
 * methods are kept so callers can read fields as if this were still a record.
 */
public final class StreamHeader {

  public static final byte SERIALIZATION_XLANG = 0;
  public static final byte SERIALIZATION_NATIVE = 1;
  public static final byte SERIALIZATION_ROW = 2;
  public static final byte SERIALIZATION_JSON = 3;

  public String service = "";
  public String method = "";
  public int version;
  public int callId;
  public short deadline;
  public byte serializationMode;
  public List<String> metadataKeys = List.of();
  public List<String> metadataValues = List.of();

  public StreamHeader() {}

  public StreamHeader(
      String service,
      String method,
      int version,
      int callId,
      short deadline,
      byte serializationMode,
      List<String> metadataKeys,
      List<String> metadataValues) {
    this.service = service == null ? "" : service;
    this.method = method == null ? "" : method;
    this.version = version;
    this.callId = callId;
    this.deadline = deadline;
    this.serializationMode = serializationMode;
    this.metadataKeys = metadataKeys == null ? List.of() : List.copyOf(metadataKeys);
    this.metadataValues = metadataValues == null ? List.of() : List.copyOf(metadataValues);
  }

  public String service() {
    return service;
  }

  public String method() {
    return method;
  }

  public int version() {
    return version;
  }

  public int callId() {
    return callId;
  }

  public short deadline() {
    return deadline;
  }

  public byte serializationMode() {
    return serializationMode;
  }

  public List<String> metadataKeys() {
    return metadataKeys;
  }

  public List<String> metadataValues() {
    return metadataValues;
  }

  @Override
  public boolean equals(Object o) {
    if (this == o) return true;
    if (!(o instanceof StreamHeader that)) return false;
    return version == that.version
        && callId == that.callId
        && deadline == that.deadline
        && serializationMode == that.serializationMode
        && Objects.equals(service, that.service)
        && Objects.equals(method, that.method)
        && Objects.equals(metadataKeys, that.metadataKeys)
        && Objects.equals(metadataValues, that.metadataValues);
  }

  @Override
  public int hashCode() {
    return Objects.hash(
        service,
        method,
        version,
        callId,
        deadline,
        serializationMode,
        metadataKeys,
        metadataValues);
  }
}
