package site.aster.server.wire;

import java.util.List;
import java.util.Objects;
import org.apache.fory.annotation.ForyField;

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

  // Explicit ForyField(id=N) on every field keeps the Fory struct fingerprint tag-ID based, so
  // Java's snake-case field-name conversion doesn't make its hash diverge from Python's (which
  // uses the raw field name). IDs must stay in sync with bindings/python/aster/protocol.py.
  @ForyField(id = 0)
  public String service = "";

  @ForyField(id = 1)
  public String method = "";

  @ForyField(id = 2)
  public int version;

  @ForyField(id = 3)
  public int callId;

  @ForyField(id = 4)
  public short deadline;

  @ForyField(id = 5)
  public byte serializationMode;

  @ForyField(id = 6)
  public List<String> metadataKeys = List.of();

  @ForyField(id = 7)
  public List<String> metadataValues = List.of();

  /**
   * Session identifier (multiplexed-streams spec §6). {@code 0} means this stream is a stateless
   * SHARED-pool stream; a non-zero value means the stream belongs to the session with this id on
   * the sender's {@code (peer, connection)}. Monotonically allocated client-side per connection.
   * Treated as 4-byte little-endian on the wire; a signed {@code int} suffices because the counter
   * never reaches 2^31 in practice.
   */
  @ForyField(id = 8)
  public int sessionId;

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
    this(
        service,
        method,
        version,
        callId,
        deadline,
        serializationMode,
        metadataKeys,
        metadataValues,
        0);
  }

  public StreamHeader(
      String service,
      String method,
      int version,
      int callId,
      short deadline,
      byte serializationMode,
      List<String> metadataKeys,
      List<String> metadataValues,
      int sessionId) {
    this.service = service == null ? "" : service;
    this.method = method == null ? "" : method;
    this.version = version;
    this.callId = callId;
    this.deadline = deadline;
    this.serializationMode = serializationMode;
    this.metadataKeys = metadataKeys == null ? List.of() : List.copyOf(metadataKeys);
    this.metadataValues = metadataValues == null ? List.of() : List.copyOf(metadataValues);
    this.sessionId = sessionId;
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

  public int sessionId() {
    return sessionId;
  }

  @Override
  public boolean equals(Object o) {
    if (this == o) return true;
    if (!(o instanceof StreamHeader that)) return false;
    return version == that.version
        && callId == that.callId
        && deadline == that.deadline
        && serializationMode == that.serializationMode
        && sessionId == that.sessionId
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
        metadataValues,
        sessionId);
  }
}
