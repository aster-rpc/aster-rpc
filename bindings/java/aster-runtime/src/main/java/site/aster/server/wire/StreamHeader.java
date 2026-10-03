package site.aster.server.wire;

import java.util.List;
import java.util.Objects;

/**
 * First frame on every QUIC stream (HEADER flag). Carries service routing, call metadata, the
 * serialization mode of the payload frames, and the session id.
 *
 * <p>Encoded in the AEv1 envelope format (Aster-SPEC §6.9) by Rust core via {@link
 * site.aster.ffi.AsterEnvelope} — not by Fory — so every binding writes identical bytes. Field
 * order and widths are fixed by the spec; see {@code conformance/vectors/envelope.json}.
 */
public final class StreamHeader {

  public static final byte SERIALIZATION_XLANG = 0;
  public static final byte SERIALIZATION_NATIVE = 1;
  public static final byte SERIALIZATION_ROW = 2;
  // 3 (JSON) is retired (Aster-SPEC §5.1): servers answer it with INVALID_ARGUMENT.

  public String service = "";

  public String method = "";

  public int version;

  public int callId;

  public short deadline;

  public byte serializationMode;

  public List<String> metadataKeys = List.of();

  public List<String> metadataValues = List.of();

  /**
   * Session identifier (multiplexed-streams spec §6). {@code 0} means this stream is a stateless
   * SHARED-pool stream; a non-zero value means the stream belongs to the session with this id on
   * the sender's {@code (peer, connection)}. Monotonically allocated client-side per connection. An
   * {@code int32} on the wire (zigzag varint); the counter never reaches 2^31 in practice.
   */
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
