package site.aster.codegen.core.emit;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.palantir.javapoet.ClassName;
import com.palantir.javapoet.JavaFile;
import java.util.List;
import org.junit.jupiter.api.Test;
import site.aster.annotations.Scope;
import site.aster.codegen.core.model.MethodModel;
import site.aster.codegen.core.model.ParamModel;
import site.aster.codegen.core.model.RequestStyle;
import site.aster.codegen.core.model.ServiceModel;
import site.aster.codegen.core.model.StreamingKind;

final class RequestRecordEmitterTest {

  private static final ClassName SERVICE = ClassName.get("com.example.mission", "MissionControl");
  private static final ClassName STATUS_RESP =
      ClassName.get("com.example.mission", "StatusResponse");

  @Test
  void emitsOneRecordPerInlineMethod() {
    MethodModel inlineA =
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
    MethodModel explicit =
        new MethodModel(
            "loadAll",
            "loadAll",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            ClassName.get("com.example.mission", "LoadAllRequest"),
            STATUS_RESP,
            false,
            true,
            false);
    ServiceModel svc =
        new ServiceModel("MissionControl", 1, Scope.SHARED, SERVICE, List.of(inlineA, explicit));

    List<JavaFile> files = RequestRecordEmitter.emit(svc);
    assertEquals(1, files.size());
    String src = files.get(0).toString();
    assertTrue(src.contains("public record MissionControl_GetStatusRequest"));
    assertTrue(src.contains("String agentId"));
  }

  @Test
  void emptyInlineMethodEmitsEmptyRecord() {
    MethodModel m =
        new MethodModel(
            "listAgents",
            "listAgents",
            StreamingKind.UNARY,
            RequestStyle.INLINE,
            List.of(),
            null,
            STATUS_RESP,
            false,
            true,
            false);
    ServiceModel svc = new ServiceModel("MissionControl", 1, Scope.SHARED, SERVICE, List.of(m));
    List<JavaFile> files = RequestRecordEmitter.emit(svc);

    assertEquals(1, files.size());
    String src = files.get(0).toString();
    assertTrue(src.contains("MissionControl_ListAgentsRequest"));
    // Record with no components should still compile — the parenthesized parameter list is empty.
    assertTrue(src.contains("ListAgentsRequest()") || src.contains("ListAgentsRequest( )"));
  }

  @Test
  void inlineForyTagMatchesPackageAndMethodName() {
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
    ServiceModel svc = new ServiceModel("MissionControl", 1, Scope.SHARED, SERVICE, List.of(m));
    assertEquals(
        "com.example.mission/GetStatusRequest", NameConventions.inlineRequestForyTag(svc, m));
  }
}
