package site.aster.server.wire;

import java.util.List;
import java.util.Objects;

/**
 * Trailing status frame (TRAILER flag). Sent as the last frame on a stream to communicate the
 * outcome of the RPC to the peer.
 *
 * <p>Matches the Python reference type {@code _aster/RpcStatus} defined in {@code
 * bindings/python/aster/protocol.py}. Plain class (not a record) for the same Fory-collection
 * reason called out on {@link StreamHeader}.
 */
public final class RpcStatus {

  public static final int OK = 0;
  public static final int CANCELLED = 1;
  public static final int UNKNOWN = 2;
  public static final int INVALID_ARGUMENT = 3;
  public static final int DEADLINE_EXCEEDED = 4;
  public static final int NOT_FOUND = 5;
  public static final int PERMISSION_DENIED = 7;
  public static final int UNAUTHENTICATED = 16;
  public static final int RESOURCE_EXHAUSTED = 8;
  public static final int FAILED_PRECONDITION = 9;
  public static final int INTERNAL = 13;
  public static final int UNAVAILABLE = 14;

  public int code;
  public String message = "";
  public List<String> detailKeys = List.of();
  public List<String> detailValues = List.of();

  public RpcStatus() {}

  public RpcStatus(int code, String message, List<String> detailKeys, List<String> detailValues) {
    this.code = code;
    this.message = message == null ? "" : message;
    this.detailKeys = detailKeys == null ? List.of() : List.copyOf(detailKeys);
    this.detailValues = detailValues == null ? List.of() : List.copyOf(detailValues);
  }

  public static RpcStatus ok() {
    return new RpcStatus(OK, "", List.of(), List.of());
  }

  public int code() {
    return code;
  }

  public String message() {
    return message;
  }

  public List<String> detailKeys() {
    return detailKeys;
  }

  public List<String> detailValues() {
    return detailValues;
  }

  @Override
  public boolean equals(Object o) {
    if (this == o) return true;
    if (!(o instanceof RpcStatus that)) return false;
    return code == that.code
        && Objects.equals(message, that.message)
        && Objects.equals(detailKeys, that.detailKeys)
        && Objects.equals(detailValues, that.detailValues);
  }

  @Override
  public int hashCode() {
    return Objects.hash(code, message, detailKeys, detailValues);
  }
}
