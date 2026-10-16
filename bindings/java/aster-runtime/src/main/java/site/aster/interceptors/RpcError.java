package site.aster.interceptors;

import java.util.Collections;
import java.util.HashMap;
import java.util.Map;

/**
 * Exception raised when an RPC call fails.
 *
 * <p>Carries a {@link StatusCode} describing the failure category, a human-readable message, and
 * optional string key/value details for extra context.
 */
public class RpcError extends RuntimeException {

  private final StatusCode code;
  private final String rpcMessage;
  private final Map<String, String> details;

  public RpcError(StatusCode code, String message) {
    this(code, message, null);
  }

  public RpcError(StatusCode code, String message, Map<String, String> details) {
    super("[" + code.name() + "] " + message);
    this.code = code;
    this.rpcMessage = message;
    this.details = details != null ? new HashMap<>(details) : new HashMap<>();
  }

  /** Returns the status code describing the failure category. */
  public StatusCode code() {
    return code;
  }

  /** Returns the human-readable error description. */
  public String rpcMessage() {
    return rpcMessage;
  }

  /** Returns arbitrary string key/value pairs carrying extra context. */
  public Map<String, String> details() {
    return Collections.unmodifiableMap(details);
  }

  /**
   * Whether a draining server answered this call ({@code UNAVAILABLE} with the {@code
   * aster.draining} detail, ABI 7.2). Retrying on the same connection cannot succeed; connect to
   * another replica.
   */
  public boolean isDraining() {
    return code == StatusCode.UNAVAILABLE
        && details.containsKey(site.aster.server.DrainReport.DRAINING_DETAIL_KEY);
  }

  /** Whether {@code t} is, or is caused by, a draining server's {@link RpcError}. */
  public static boolean isDraining(Throwable t) {
    for (Throwable c = t; c != null; c = c.getCause()) {
      if (c instanceof RpcError e && e.isDraining()) {
        return true;
      }
      if (c.getCause() == c) {
        break;
      }
    }
    return false;
  }

  @Override
  public String toString() {
    return "RpcError(code=" + code + ", message=" + rpcMessage + ", details=" + details + ")";
  }
}
