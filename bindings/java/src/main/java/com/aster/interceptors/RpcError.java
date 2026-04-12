package com.aster.interceptors;

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

  @Override
  public String toString() {
    return "RpcError(code=" + code + ", message=" + rpcMessage + ", details=" + details + ")";
  }
}
