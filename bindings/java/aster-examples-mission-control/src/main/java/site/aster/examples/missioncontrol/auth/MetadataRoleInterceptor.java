package site.aster.examples.missioncontrol.auth;

import site.aster.contract.Capabilities;
import site.aster.interceptors.CallContext;
import site.aster.interceptors.Interceptor;

/**
 * Dev-mode helper: copies a caller-supplied {@code aster.role} metadata header into {@code
 * CallContext.attributes()} so the {@code CapabilityInterceptor} can evaluate it against
 * {@code @Requires} declarations. Emulates the effect of a validated admission credential without
 * requiring the full credential pipeline (arriving in Phase 3a).
 *
 * <p><b>Not for production.</b> A real deployment populates attributes from validated credentials
 * at admission; trusting a client-supplied header lets the client impersonate any role. This
 * interceptor exists so the {@code ServerAuth} example and its E2E tests can exercise the gating
 * path end-to-end before the auth pipeline lands.
 */
public final class MetadataRoleInterceptor implements Interceptor {

  public static final String METADATA_KEY = Capabilities.ROLE_ATTRIBUTE;

  @Override
  public Object onRequest(CallContext ctx, Object request) {
    String role = ctx.metadata().get(METADATA_KEY);
    if (role != null && !role.isEmpty() && !ctx.attributes().containsKey(METADATA_KEY)) {
      ctx.attributes().put(METADATA_KEY, role);
    }
    return request;
  }
}
