package site.aster.interceptors;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import site.aster.annotations.Scope;
import site.aster.contract.Capabilities;
import site.aster.contract.CapabilityRequirement;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

final class CapabilityInterceptorTest {

  private static CallContext ctx(String service, String method, Map<String, String> attrs) {
    return CallContext.builder(service, method).peer("peer-1").attributes(attrs).build();
  }

  private static ServiceDispatcher serviceWith(
      String name,
      CapabilityRequirement serviceRequires,
      String methodName,
      CapabilityRequirement methodRequires) {
    MethodDescriptor md =
        new MethodDescriptor(
            methodName,
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            "dummy/req",
            List.of(),
            "dummy/resp",
            false,
            false,
            methodRequires);
    MethodDispatcher dispatcher =
        new UnaryDispatcher() {
          @Override
          public MethodDescriptor descriptor() {
            return md;
          }

          @Override
          public byte[] invoke(
              Object impl, byte[] requestBytes, site.aster.codec.Codec codec, CallContext ctx) {
            return new byte[0];
          }
        };
    ServiceDescriptor sd =
        new ServiceDescriptor(name, 1, Scope.SHARED, Object.class, serviceRequires);
    return new ServiceDispatcher() {
      @Override
      public ServiceDescriptor descriptor() {
        return sd;
      }

      @Override
      public Map<String, MethodDispatcher> methods() {
        return Map.of(methodName, dispatcher);
      }

      @Override
      public Map<String, Class<?>> requestClasses() {
        return Map.of();
      }

      @Override
      public Map<String, Class<?>> responseClasses() {
        return Map.of();
      }

      @Override
      public void registerTypes(org.apache.fory.Fory fory) {}
    };
  }

  @Test
  void methodLevelRequiresPassesWithCorrectRole() {
    ServiceDispatcher svc = serviceWith("Svc", null, "getStatus", Capabilities.role("ops.status"));
    CapabilityInterceptor interceptor = new CapabilityInterceptor(Map.of("Svc", svc));
    Object req = new Object();
    Object out =
        interceptor.onRequest(ctx("Svc", "getStatus", Map.of("aster.role", "ops.status")), req);
    assertSame(req, out);
  }

  @Test
  void methodLevelRequiresRejectsMissingRole() {
    ServiceDispatcher svc = serviceWith("Svc", null, "getStatus", Capabilities.role("ops.status"));
    CapabilityInterceptor interceptor = new CapabilityInterceptor(Map.of("Svc", svc));
    RpcError err =
        assertThrows(
            RpcError.class,
            () -> interceptor.onRequest(ctx("Svc", "getStatus", Map.of()), new Object()));
    assertEquals(StatusCode.PERMISSION_DENIED, err.code());
  }

  @Test
  void serviceLevelRequiresGatesEveryMethod() {
    ServiceDispatcher svc = serviceWith("Svc", Capabilities.role("ops.admin"), "doThing", null);
    CapabilityInterceptor interceptor = new CapabilityInterceptor(Map.of("Svc", svc));
    // caller has no roles -> service baseline fails even though method has no requires.
    assertThrows(
        RpcError.class, () -> interceptor.onRequest(ctx("Svc", "doThing", Map.of()), new Object()));
    // caller carries ops.admin -> passes.
    Object req = new Object();
    assertSame(
        req, interceptor.onRequest(ctx("Svc", "doThing", Map.of("aster.role", "ops.admin")), req));
  }

  @Test
  void anyOfAcceptsEitherRole() {
    ServiceDispatcher svc =
        serviceWith("Svc", null, "tailLogs", Capabilities.anyOf("ops.logs", "ops.admin"));
    CapabilityInterceptor interceptor = new CapabilityInterceptor(Map.of("Svc", svc));
    Object req = new Object();
    assertSame(
        req, interceptor.onRequest(ctx("Svc", "tailLogs", Map.of("aster.role", "ops.logs")), req));
    assertSame(
        req, interceptor.onRequest(ctx("Svc", "tailLogs", Map.of("aster.role", "ops.admin")), req));
    assertThrows(
        RpcError.class,
        () ->
            interceptor.onRequest(
                ctx("Svc", "tailLogs", Map.of("aster.role", "ops.status")), new Object()));
  }

  @Test
  void unknownServicePassesThrough() {
    CapabilityInterceptor interceptor = new CapabilityInterceptor(Map.of());
    Object req = new Object();
    assertSame(req, interceptor.onRequest(ctx("Unknown", "m", Map.of()), req));
  }

  @Test
  void legacyMapModeStillWorks() {
    CapabilityInterceptor interceptor =
        CapabilityInterceptor.fromMap(Map.of("Svc.m", List.of("ops.admin")));
    Object req = new Object();
    assertSame(req, interceptor.onRequest(ctx("Svc", "m", Map.of("aster.role", "ops.admin")), req));
    assertThrows(
        RpcError.class,
        () ->
            interceptor.onRequest(ctx("Svc", "m", Map.of("aster.role", "ops.logs")), new Object()));
  }
}
