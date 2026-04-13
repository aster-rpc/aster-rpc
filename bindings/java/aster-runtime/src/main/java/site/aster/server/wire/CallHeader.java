package site.aster.server.wire;

import java.util.List;

/**
 * Per-call header within a session stream (CALL flag). Used for session-scoped services where
 * multiple RPCs share a single QUIC stream.
 *
 * <p>Matches the Python reference type {@code _aster/CallHeader} defined in {@code
 * bindings/python/aster/protocol.py}.
 */
public record CallHeader(
    String method,
    int callId,
    short deadline,
    List<String> metadataKeys,
    List<String> metadataValues) {

  public CallHeader {
    metadataKeys = metadataKeys == null ? List.of() : List.copyOf(metadataKeys);
    metadataValues = metadataValues == null ? List.of() : List.copyOf(metadataValues);
  }
}
