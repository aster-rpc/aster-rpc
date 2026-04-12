package com.aster.interceptors;

/**
 * Base interceptor interface for the Aster RPC pipeline.
 *
 * <p>Interceptors are synchronous — the AsterServer dispatch already runs on virtual threads, so
 * blocking here is safe and simple.
 *
 * <p>Default implementations pass through unchanged. Override only the hooks you need.
 */
public interface Interceptor {

  /**
   * Called before the request is dispatched to the handler.
   *
   * @param ctx the call context (mutable metadata, attributes, etc.)
   * @param request the incoming request payload
   * @return the (possibly transformed) request to pass downstream
   */
  default Object onRequest(CallContext ctx, Object request) {
    return request;
  }

  /**
   * Called after the handler returns a successful response.
   *
   * @param ctx the call context
   * @param response the outgoing response payload
   * @return the (possibly transformed) response to return to the caller
   */
  default Object onResponse(CallContext ctx, Object response) {
    return response;
  }

  /**
   * Called when the handler (or a downstream interceptor) throws an {@link RpcError}.
   *
   * <p>Error interceptors run in reverse order (innermost first).
   *
   * @param ctx the call context
   * @param error the error to handle
   * @return the error to propagate, or {@code null} to suppress it
   */
  default RpcError onError(CallContext ctx, RpcError error) {
    return error;
  }
}
