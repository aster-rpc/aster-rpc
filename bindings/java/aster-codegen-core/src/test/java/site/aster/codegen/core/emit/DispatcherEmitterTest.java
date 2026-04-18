package site.aster.codegen.core.emit;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.palantir.javapoet.ClassName;
import com.palantir.javapoet.JavaFile;
import com.palantir.javapoet.TypeName;
import java.util.List;
import org.junit.jupiter.api.Test;
import site.aster.annotations.Scope;
import site.aster.codegen.core.model.MethodModel;
import site.aster.codegen.core.model.ParamModel;
import site.aster.codegen.core.model.RequestStyle;
import site.aster.codegen.core.model.ServiceModel;
import site.aster.codegen.core.model.StreamingKind;

final class DispatcherEmitterTest {

  private static final ClassName SERVICE = ClassName.get("com.example.mission", "MissionControl");
  private static final ClassName STATUS_REQ = ClassName.get("com.example.mission", "StatusRequest");
  private static final ClassName STATUS_RESP =
      ClassName.get("com.example.mission", "StatusResponse");
  private static final ClassName LOG_ENTRY = ClassName.get("com.example.mission", "LogEntry");
  private static final ClassName TAIL_REQ = ClassName.get("com.example.mission", "TailRequest");

  private static ServiceModel sharedService(MethodModel... methods) {
    return new ServiceModel("MissionControl", 1, Scope.SHARED, SERVICE, List.of(methods));
  }

  @Test
  void emitsDispatcherClassWithExpectedNameAndSuperinterface() {
    MethodModel m =
        new MethodModel(
            "getStatus",
            "getStatus",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            STATUS_REQ,
            STATUS_RESP,
            false,
            true,
            false);

    JavaFile file = DispatcherEmitter.emit(sharedService(m));
    String src = file.toString();

    assertTrue(src.contains("public final class MissionControl$AsterDispatcher"));
    assertTrue(src.contains("implements ServiceDispatcher"));
    assertTrue(src.contains("private static final ServiceDescriptor DESCRIPTOR"));
    assertTrue(src.contains("Scope.SHARED"));
    assertTrue(src.contains("m.put(\"getStatus\", new GetStatus$Dispatcher())"));
  }

  @Test
  void unaryExplicitNoCtxEmitsDecodeCallEncode() {
    MethodModel m =
        new MethodModel(
            "getStatus",
            "getStatus",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            STATUS_REQ,
            STATUS_RESP,
            false,
            true,
            false);
    String src = DispatcherEmitter.emit(sharedService(m)).toString();

    assertTrue(src.contains("StatusRequest request = (StatusRequest) codec.decode"));
    assertTrue(src.contains("StatusResponse response = CallContext.runWith"));
    assertTrue(src.contains("((MissionControl) impl).getStatus(request)"));
    assertTrue(src.contains("return codec.encode(response)"));
    // no ctx → getStatus(request), not getStatus(request, ctx)
    assertFalse(src.contains("getStatus(request, ctx)"));
  }

  @Test
  void unaryExplicitWithCtxThreadsCtxThrough() {
    MethodModel m =
        new MethodModel(
            "getStatus",
            "getStatus",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            STATUS_REQ,
            STATUS_RESP,
            true,
            true,
            false);
    String src = DispatcherEmitter.emit(sharedService(m)).toString();

    assertTrue(src.contains("((MissionControl) impl).getStatus(request, ctx)"));
  }

  @Test
  void unaryInlineUnpacksRecordFields() {
    MethodModel m =
        new MethodModel(
            "getStatus",
            "getStatus",
            StreamingKind.UNARY,
            RequestStyle.INLINE,
            List.of(new ParamModel("agentId", ClassName.get(String.class))),
            null,
            STATUS_RESP,
            false,
            true,
            false);
    String src = DispatcherEmitter.emit(sharedService(m)).toString();

    assertTrue(src.contains("MissionControl_GetStatusRequest inline"));
    assertTrue(src.contains("((MissionControl) impl).getStatus(inline.agentId())"));
  }

  @Test
  void unaryInlineMultipleParamsAndCtx() {
    MethodModel m =
        new MethodModel(
            "createAgent",
            "createAgent",
            StreamingKind.UNARY,
            RequestStyle.INLINE,
            List.of(
                new ParamModel("agentName", ClassName.get(String.class)),
                new ParamModel("config", ClassName.get("com.example.mission", "AgentConfig"))),
            null,
            STATUS_RESP,
            true,
            false,
            false);
    String src = DispatcherEmitter.emit(sharedService(m)).toString();

    assertTrue(
        src.contains(
            "((MissionControl) impl).createAgent(inline.agentName(), inline.config(), ctx)"));
  }

  @Test
  void streamingStubsThrowUnsupported() {
    MethodModel server =
        new MethodModel(
            "tailLogs",
            "tailLogs",
            StreamingKind.SERVER_STREAM,
            RequestStyle.EXPLICIT,
            List.of(),
            TAIL_REQ,
            LOG_ENTRY,
            false,
            false,
            false);
    String src = DispatcherEmitter.emit(sharedService(server)).toString();
    assertTrue(src.contains("implements ServerStreamDispatcher"));
    assertTrue(src.contains("throw new UnsupportedOperationException"));
  }

  @Test
  void registerTypesEmitsOneCallPerDistinctType() {
    // Two methods sharing the same response type → only one register call for it.
    MethodModel a =
        new MethodModel(
            "getStatus",
            "getStatus",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            STATUS_REQ,
            STATUS_RESP,
            false,
            true,
            false);
    MethodModel b =
        new MethodModel(
            "reloadStatus",
            "reloadStatus",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            STATUS_REQ,
            STATUS_RESP,
            false,
            true,
            false);
    String src = DispatcherEmitter.emit(sharedService(a, b)).toString();

    // The emitter now references StatusResponse.class in several places (fory.register +
    // REQUEST/RESPONSE_CLASSES maps); check the register-call specifically.
    int registerCount = src.split("safeRegister\\(fory, StatusResponse\\.class", -1).length - 1;
    assertEquals(
        1,
        registerCount,
        "StatusResponse should only be registered with Fory once even when used by 2 methods");
  }

  @Test
  void dispatcherClassNameAndPackage() {
    MethodModel m =
        new MethodModel(
            "ping",
            "ping",
            StreamingKind.UNARY,
            RequestStyle.INLINE,
            List.of(),
            null,
            STATUS_RESP,
            false,
            true,
            false);
    JavaFile file = DispatcherEmitter.emit(sharedService(m));
    assertEquals("com.example.mission", file.packageName());
    assertEquals(
        "MissionControl$AsterDispatcher",
        NameConventions.dispatcherClassName(sharedService(m)).simpleName());
  }

  @Test
  void sessionScopeEmitsScopeSession() {
    MethodModel m =
        new MethodModel(
            "register",
            "register",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            STATUS_REQ,
            STATUS_RESP,
            false,
            false,
            false);
    ServiceModel session = new ServiceModel("AgentSession", 1, Scope.SESSION, SERVICE, List.of(m));
    String src = DispatcherEmitter.emit(session).toString();
    assertTrue(src.contains("Scope.SESSION"));
  }

  // Unused but prevents "unused import" warnings when the class list contracts.
  @SuppressWarnings("unused")
  private static final TypeName UNUSED = ClassName.get(Object.class);
}
