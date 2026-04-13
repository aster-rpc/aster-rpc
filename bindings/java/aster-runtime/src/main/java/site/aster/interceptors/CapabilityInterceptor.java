package site.aster.interceptors;

import java.util.List;
import java.util.Map;
import java.util.logging.Logger;

/**
 * Capability interceptor for method-level access control.
 *
 * <p>Takes a mapping of {@code "service"} or {@code "service.method"} to a list of required roles.
 * Checks the caller's {@link CallContext#attributes()} for a {@code "roles"} entry containing a
 * comma-separated list of the caller's roles. Rejects with {@link StatusCode#PERMISSION_DENIED} if
 * the caller lacks any required role.
 */
public final class CapabilityInterceptor implements Interceptor {

  private static final Logger LOG = Logger.getLogger(CapabilityInterceptor.class.getName());

  private final Map<String, List<String>> requirements;

  /**
   * Creates a capability interceptor.
   *
   * @param requirements mapping of {@code "service"} or {@code "service.method"} to required role
   *     names
   */
  public CapabilityInterceptor(Map<String, List<String>> requirements) {
    this.requirements = Map.copyOf(requirements);
  }

  @Override
  public Object onRequest(CallContext ctx, Object request) {
    // Check method-level requirement first (more specific)
    String methodKey = ctx.service() + "." + ctx.method();
    List<String> methodRoles = requirements.get(methodKey);
    if (methodRoles != null) {
      checkRoles(ctx, methodRoles, methodKey);
    }

    // Check service-level requirement
    List<String> serviceRoles = requirements.get(ctx.service());
    if (serviceRoles != null) {
      checkRoles(ctx, serviceRoles, ctx.service());
    }

    return request;
  }

  private void checkRoles(CallContext ctx, List<String> requiredRoles, String scope) {
    String callerRolesStr = ctx.attributes().getOrDefault("roles", "");
    for (String required : requiredRoles) {
      if (!hasRole(callerRolesStr, required)) {
        LOG.warning(
            "Capability denied: service="
                + ctx.service()
                + " method="
                + ctx.method()
                + " peer="
                + ctx.peer()
                + " (missing role: "
                + required
                + ")");
        throw new RpcError(
            StatusCode.PERMISSION_DENIED, "capability check failed for '" + scope + "'");
      }
    }
  }

  private static boolean hasRole(String callerRolesStr, String requiredRole) {
    if (callerRolesStr.isEmpty()) {
      return false;
    }
    for (String role : callerRolesStr.split(",")) {
      if (role.trim().equals(requiredRole)) {
        return true;
      }
    }
    return false;
  }
}
