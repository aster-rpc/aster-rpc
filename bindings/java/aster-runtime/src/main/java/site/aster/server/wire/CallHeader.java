package site.aster.server.wire;

import java.util.List;
import java.util.Objects;

/**
 * Per-call header within a session stream (CALL flag). Used for session-scoped services where
 * multiple RPCs share a single QUIC stream.
 *
 * <p>Encoded in the AEv1 envelope format (Aster-SPEC §6.9) via {@link
 * site.aster.ffi.AsterEnvelope}, not by Fory.
 */
public final class CallHeader {

  public String method = "";

  public int callId;

  public short deadline;

  public List<String> metadataKeys = List.of();

  public List<String> metadataValues = List.of();

  public CallHeader() {}

  public CallHeader(
      String method,
      int callId,
      short deadline,
      List<String> metadataKeys,
      List<String> metadataValues) {
    this.method = method == null ? "" : method;
    this.callId = callId;
    this.deadline = deadline;
    this.metadataKeys = metadataKeys == null ? List.of() : List.copyOf(metadataKeys);
    this.metadataValues = metadataValues == null ? List.of() : List.copyOf(metadataValues);
  }

  public String method() {
    return method;
  }

  public int callId() {
    return callId;
  }

  public short deadline() {
    return deadline;
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
    if (!(o instanceof CallHeader that)) return false;
    return callId == that.callId
        && deadline == that.deadline
        && Objects.equals(method, that.method)
        && Objects.equals(metadataKeys, that.metadataKeys)
        && Objects.equals(metadataValues, that.metadataValues);
  }

  @Override
  public int hashCode() {
    return Objects.hash(method, callId, deadline, metadataKeys, metadataValues);
  }
}
