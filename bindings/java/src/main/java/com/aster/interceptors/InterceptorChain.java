package com.aster.interceptors;

import java.util.List;

/**
 * Static helpers that drive a list of {@link Interceptor}s over a request/response/error lifecycle.
 *
 * <p>Request and response interceptors run in forward order. Error interceptors run in reverse
 * order (innermost first), mirroring the Python implementation.
 */
public final class InterceptorChain {

  private InterceptorChain() {}

  /** Applies {@code onRequest} in forward order, returning the final request object. */
  public static Object applyRequest(
      List<Interceptor> interceptors, CallContext ctx, Object request) {
    Object current = request;
    for (Interceptor interceptor : interceptors) {
      current = interceptor.onRequest(ctx, current);
    }
    return current;
  }

  /** Applies {@code onResponse} in forward order, returning the final response object. */
  public static Object applyResponse(
      List<Interceptor> interceptors, CallContext ctx, Object response) {
    Object current = response;
    for (Interceptor interceptor : interceptors) {
      current = interceptor.onResponse(ctx, current);
    }
    return current;
  }

  /**
   * Applies {@code onError} in reverse order, returning the final error or {@code null} if
   * suppressed.
   */
  public static RpcError applyError(
      List<Interceptor> interceptors, CallContext ctx, RpcError error) {
    RpcError current = error;
    for (int i = interceptors.size() - 1; i >= 0; i--) {
      if (current == null) {
        return null;
      }
      current = interceptors.get(i).onError(ctx, current);
    }
    return current;
  }
}
