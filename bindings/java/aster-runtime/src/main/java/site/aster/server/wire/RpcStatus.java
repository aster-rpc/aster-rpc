package site.aster.server.wire;

import java.util.List;

/**
 * Trailing status frame (TRAILER flag). Sent as the last frame on a stream to communicate the
 * outcome of the RPC to the peer.
 *
 * <p>Matches the Python reference type {@code _aster/RpcStatus} defined in {@code
 * bindings/python/aster/protocol.py}.
 */
public record RpcStatus(
    int code, String message, List<String> detailKeys, List<String> detailValues) {

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

  public RpcStatus {
    message = message == null ? "" : message;
    detailKeys = detailKeys == null ? List.of() : List.copyOf(detailKeys);
    detailValues = detailValues == null ? List.of() : List.copyOf(detailValues);
  }

  public static RpcStatus ok() {
    return new RpcStatus(OK, "", List.of(), List.of());
  }
}
