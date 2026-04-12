package com.aster.interceptors;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.logging.Logger;

/**
 * Captures structured audit events for requests, responses, and errors.
 *
 * <p>Events are appended to an in-memory sink (a {@code List<Map<String,Object>>}) and logged via
 * {@link java.util.logging.Logger} at FINE level.
 */
public final class AuditInterceptor implements Interceptor {

  private static final Logger DEFAULT_LOGGER = Logger.getLogger(AuditInterceptor.class.getName());

  private final List<Map<String, Object>> sink;
  private final Logger logger;

  /** Creates an audit interceptor with a new empty sink and the default logger. */
  public AuditInterceptor() {
    this(new ArrayList<>(), DEFAULT_LOGGER);
  }

  /**
   * Creates an audit interceptor with the given sink and logger.
   *
   * @param sink mutable list to which audit entries are appended
   * @param logger logger for debug output
   */
  public AuditInterceptor(List<Map<String, Object>> sink, Logger logger) {
    this.sink = sink;
    this.logger = logger != null ? logger : DEFAULT_LOGGER;
  }

  /** Returns the audit event sink. */
  public List<Map<String, Object>> sink() {
    return sink;
  }

  @Override
  public Object onRequest(CallContext ctx, Object request) {
    record("request", ctx, Map.of());
    return request;
  }

  @Override
  public Object onResponse(CallContext ctx, Object response) {
    record("response", ctx, Map.of());
    return response;
  }

  @Override
  public RpcError onError(CallContext ctx, RpcError error) {
    record("error", ctx, Map.of("code", error.code().name(), "message", error.rpcMessage()));
    return error;
  }

  private void record(String event, CallContext ctx, Map<String, Object> extra) {
    Map<String, Object> entry = new HashMap<>();
    entry.put("event", event);
    entry.put("service", ctx.service());
    entry.put("method", ctx.method());
    entry.put("call_id", ctx.callId());
    entry.put("attempt", ctx.attempt());
    entry.put("ts", System.currentTimeMillis() / 1000.0);
    entry.putAll(extra);

    sink.add(entry);
    logger.fine("audit=" + entry);
  }
}
