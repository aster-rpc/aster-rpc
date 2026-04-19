package site.aster.server.wire;

import java.util.List;
import java.util.Objects;
import org.apache.fory.annotation.ForyField;

/**
 * Per-call header within a session stream (CALL flag). Used for session-scoped services where
 * multiple RPCs share a single QUIC stream.
 *
 * <p>Matches the Python reference type {@code _aster/CallHeader} defined in {@code
 * bindings/python/aster/protocol.py}. Plain class (not a record) for the same Fory-collection
 * reason called out on {@link StreamHeader}. Every field carries an explicit {@code ForyField#id}
 * for the same cross-binding hash-stability reason as {@link StreamHeader} / {@link RpcStatus}.
 */
public final class CallHeader {

  @ForyField(id = 0)
  public String method = "";

  @ForyField(id = 1)
  public int callId;

  @ForyField(id = 2)
  public short deadline;

  @ForyField(id = 3)
  public List<String> metadataKeys = List.of();

  @ForyField(id = 4)
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
