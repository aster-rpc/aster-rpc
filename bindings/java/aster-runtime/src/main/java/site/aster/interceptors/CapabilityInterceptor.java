package site.aster.interceptors;

import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.logging.Logger;
import site.aster.contract.Capabilities;
import site.aster.contract.CapabilityKind;
import site.aster.contract.CapabilityRequirement;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.ServiceDispatcher;

/**
 * Gate 3 method-level access control. Mirrors {@code
 * bindings/python/aster/interceptors/capability.py}.
 *
 * <p>For each incoming call the interceptor looks up the {@link ServiceDispatcher} (and its {@link
 * MethodDispatcher}) and enforces both the service-level and method-level {@code @Requires}
 * declarations against {@link CallContext#attributes()}. A missing requirement at either level is
 * vacuously satisfied; if both are satisfied the request passes through. Otherwise the caller
 * receives {@code StatusCode.PERMISSION_DENIED}.
 *
 * <p>Canonical caller-role key is {@code aster.role} (comma-separated), set at admission time. See
 * {@link Capabilities} for the evaluation rules.
 */
public final class CapabilityInterceptor implements Interceptor {

  private static final Logger LOG = Logger.getLogger(CapabilityInterceptor.class.getName());

  private final Map<String, ServiceDispatcher> services;
  private final Map<String, CapabilityRequirement> explicitMap;
  private final boolean mapMode;

  /**
   * Enforce capabilities declared via {@code @Requires} on the registered service dispatchers. The
   * usual Java idiom is {@code new CapabilityInterceptor(server.serviceDispatchers())}, wiring it
   * into the interceptor chain after auth and before the handler.
   */
  public CapabilityInterceptor(Map<String, ServiceDispatcher> services) {
    this.services = Map.copyOf(services);
    this.explicitMap = Map.of();
    this.mapMode = false;
  }

  private CapabilityInterceptor(Map<String, CapabilityRequirement> explicit, boolean asMap) {
    this.services = Map.of();
    this.explicitMap = Map.copyOf(explicit);
    this.mapMode = asMap;
  }

  /**
   * Compatibility factory matching the pre-annotation API. Callers pass an ad-hoc map of {@code
   * "service"} or {@code "service.method"} to required role names; every entry is interpreted as an
   * {@link CapabilityKind#ALL_OF} requirement. Retained so pre-{@code @Requires} tests keep
   * working; new code should use the {@link #CapabilityInterceptor(Map)} form and drive the
   * requirements through {@code @Requires} on the service / method.
   */
  public static CapabilityInterceptor fromMap(Map<String, List<String>> requirements) {
    Map<String, CapabilityRequirement> m = new HashMap<>();
    for (Map.Entry<String, List<String>> e : requirements.entrySet()) {
      m.put(
          e.getKey(), new CapabilityRequirement(CapabilityKind.ALL_OF, List.copyOf(e.getValue())));
    }
    return new CapabilityInterceptor(m, true);
  }

  @Override
  public Object onRequest(CallContext ctx, Object request) {
    if (mapMode) {
      return applyMap(ctx, request);
    }
    return applyAnnotations(ctx, request);
  }

  private Object applyAnnotations(CallContext ctx, Object request) {
    ServiceDispatcher svc = services.get(ctx.service());
    if (svc == null) {
      return request;
    }

    CapabilityRequirement svcReq = svc.descriptor().requires();
    if (svcReq != null && !Capabilities.evaluate(svcReq, ctx.attributes())) {
      deny(ctx, ctx.service(), "service-level requirement not met");
    }

    MethodDispatcher md = svc.methods().get(ctx.method());
    if (md != null) {
      CapabilityRequirement mReq = md.descriptor().requires();
      if (mReq != null && !Capabilities.evaluate(mReq, ctx.attributes())) {
        deny(ctx, ctx.service() + "." + ctx.method(), "method-level requirement not met");
      }
    }
    return request;
  }

  private Object applyMap(CallContext ctx, Object request) {
    String methodKey = ctx.service() + "." + ctx.method();
    CapabilityRequirement mReq = explicitMap.get(methodKey);
    if (mReq != null && !Capabilities.evaluate(mReq, ctx.attributes())) {
      deny(ctx, methodKey, "method-level requirement not met");
    }
    CapabilityRequirement svcReq = explicitMap.get(ctx.service());
    if (svcReq != null && !Capabilities.evaluate(svcReq, ctx.attributes())) {
      deny(ctx, ctx.service(), "service-level requirement not met");
    }
    return request;
  }

  private static void deny(CallContext ctx, String scope, String reason) {
    LOG.warning(
        "Capability denied: service="
            + ctx.service()
            + " method="
            + ctx.method()
            + " peer="
            + ctx.peer()
            + " ("
            + reason
            + ")");
    throw new RpcError(StatusCode.PERMISSION_DENIED, "capability check failed for '" + scope + "'");
  }
}
