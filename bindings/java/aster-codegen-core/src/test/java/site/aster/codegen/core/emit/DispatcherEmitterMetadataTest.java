package site.aster.codegen.core.emit;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.palantir.javapoet.ClassName;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import site.aster.annotations.Scope;
import site.aster.codegen.core.model.FieldModel;
import site.aster.codegen.core.model.MethodModel;
import site.aster.codegen.core.model.ParamModel;
import site.aster.codegen.core.model.RequestStyle;
import site.aster.codegen.core.model.ServiceModel;
import site.aster.codegen.core.model.StreamingKind;

/**
 * Verifies that rich-metadata (description, tags, deprecated, field metadata) plumbs from {@link
 * ServiceModel}/{@link MethodModel}/{@link FieldModel} into the emitted dispatcher source.
 */
final class DispatcherEmitterMetadataTest {

  private static final ClassName SERVICE = ClassName.get("com.example.mission", "MissionControl");
  private static final ClassName STATUS_REQ = ClassName.get("com.example.mission", "StatusRequest");
  private static final ClassName STATUS_RESP =
      ClassName.get("com.example.mission", "StatusResponse");

  @Test
  void serviceDescriptionAndTagsFlowIntoConstants() {
    ServiceModel svc =
        new ServiceModel(
            "MissionControl",
            1,
            Scope.SHARED,
            SERVICE,
            List.of(),
            "Mission control service.",
            List.of("readonly", "experimental"));
    String src = DispatcherEmitter.emit(svc).toString();

    assertTrue(
        src.contains("private static final String DESCRIPTION = \"Mission control service.\""),
        src);
    assertTrue(src.contains("List.of(\"readonly\", \"experimental\")"), src);
    assertTrue(src.contains("public String description()"), src);
    assertTrue(src.contains("public List<String> tags()"), src);
    assertTrue(src.contains("return DESCRIPTION;"), src);
    assertTrue(src.contains("return TAGS;"), src);
  }

  @Test
  void methodMetadataMapIncludesOnlyNonEmptyEntries() {
    MethodModel rich =
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
            false,
            "Return current agent status.",
            List.of("readonly"),
            false,
            Map.of());

    MethodModel empty =
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

    ServiceModel svc =
        new ServiceModel(
            "MissionControl", 1, Scope.SHARED, SERVICE, List.of(rich, empty), "", List.of());
    String src = DispatcherEmitter.emit(svc).toString();

    assertTrue(src.contains("Map.entry(\"getStatus\""), src);
    assertTrue(src.contains("\"Return current agent status.\""), src);
    assertTrue(src.contains("List.of(\"readonly\")"), src);
    // empty metadata for "ping" should not land in the map
    assertFalse(src.contains("Map.entry(\"ping\""), src);

    assertTrue(src.contains("public MethodMetadata methodMetadata(String methodName)"), src);
    assertTrue(src.contains("METHOD_METADATA.getOrDefault(methodName, MethodMetadata.EMPTY)"), src);
  }

  @Test
  void deprecatedAndFieldMetadataFlowThrough() {
    LinkedHashMap<String, FieldModel> fields = new LinkedHashMap<>();
    fields.put("agentId", new FieldModel("agentId", "Unique agent id.", List.of("pii")));
    fields.put("region", new FieldModel("region", "Deployment region.", List.of()));

    MethodModel method =
        new MethodModel(
            "deleteAgent",
            "deleteAgent",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            STATUS_REQ,
            STATUS_RESP,
            false,
            false,
            false,
            "Removes an agent.",
            List.of("destructive"),
            true,
            fields);

    ServiceModel svc =
        new ServiceModel(
            "MissionControl", 1, Scope.SHARED, SERVICE, List.of(method), "", List.of());
    String src = DispatcherEmitter.emit(svc).toString();

    // deprecated flag emitted positionally in the MethodMetadata constructor
    assertTrue(src.contains("new MethodMetadata(\"Removes an agent.\""), src);
    assertTrue(src.contains(", true,"), "deprecated=true should appear in MethodMetadata ctor");

    // field metadata present with both entries
    assertTrue(src.contains("Map.entry(\"agentId\", new FieldMetadata"), src);
    assertTrue(src.contains("\"Unique agent id.\""), src);
    assertTrue(src.contains("List.of(\"pii\")"), src);
    assertTrue(src.contains("Map.entry(\"region\", new FieldMetadata"), src);
    assertTrue(src.contains("\"Deployment region.\""), src);
  }

  @Test
  void inlineParamsPropagateToFieldMetadataViaModelBuilder() {
    // This test confirms the emitter happily serializes an inline-style MethodModel that carries
    // field metadata sourced from ParamModel descriptions. The APT path lives in
    // aster-codegen-apt — here we just pin the emitter contract.
    ParamModel p =
        new ParamModel("name", ClassName.get(String.class), "Agent name.", List.of("pii"));
    MethodModel inline =
        new MethodModel(
            "register",
            "register",
            StreamingKind.UNARY,
            RequestStyle.INLINE,
            List.of(p),
            null,
            STATUS_RESP,
            false,
            false,
            false,
            "Registers an agent.",
            List.of(),
            false,
            Map.of("name", new FieldModel("name", "Agent name.", List.of("pii"))));

    ServiceModel svc =
        new ServiceModel(
            "MissionControl", 1, Scope.SHARED, SERVICE, List.of(inline), "", List.of());
    String src = DispatcherEmitter.emit(svc).toString();

    assertTrue(src.contains("Map.entry(\"register\""), src);
    assertTrue(src.contains("Map.entry(\"name\", new FieldMetadata"), src);
    assertTrue(src.contains("\"Agent name.\""), src);
  }

  @Test
  void emptyMetadataEmitsEmptyMaps() {
    MethodModel noMeta =
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
    ServiceModel svc = new ServiceModel("Foo", 1, Scope.SHARED, SERVICE, List.of(noMeta));

    String src = DispatcherEmitter.emit(svc).toString();
    assertTrue(src.contains("DESCRIPTION = \"\""), src);
    // Either Map.ofEntries() with no args or an empty map literal is acceptable — assert the
    // method_metadata accessor falls back to EMPTY for unknown methods.
    assertTrue(src.contains("MethodMetadata.EMPTY"), src);
    assertTrue(src.contains("List.of()"), src);
  }
}
