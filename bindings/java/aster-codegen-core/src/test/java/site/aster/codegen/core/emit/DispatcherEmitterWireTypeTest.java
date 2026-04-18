package site.aster.codegen.core.emit;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.palantir.javapoet.ClassName;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import site.aster.annotations.Scope;
import site.aster.codegen.core.model.MethodModel;
import site.aster.codegen.core.model.ParamModel;
import site.aster.codegen.core.model.RequestStyle;
import site.aster.codegen.core.model.ServiceModel;
import site.aster.codegen.core.model.StreamingKind;

/**
 * Verifies that {@link ServiceModel#wireTypeTags()} overrides the default Java-package/simple-name
 * Fory tag in both {@code fory.register(...)} calls and the {@code MethodDescriptor} tag fields.
 *
 * <p>Tag parity is load-bearing for cross-language contract_id: if a Python service declares
 * {@code @wire_type("billing/Invoice")} and a Java service leaves the map empty, Rust produces
 * different canonical bytes and the services don't match even though they describe the same logical
 * type.
 */
final class DispatcherEmitterWireTypeTest {

  private static final ClassName SERVICE = ClassName.get("com.example.mission", "MissionControl");
  private static final ClassName INVOICE = ClassName.get("com.example.billing", "Invoice");
  private static final ClassName CONFIRMATION =
      ClassName.get("com.example.billing", "PaymentConfirmation");

  @Test
  void explicitRequestUsesOverriddenWireTag() {
    Map<String, String> tags = new LinkedHashMap<>();
    tags.put(INVOICE.toString(), "billing/Invoice");
    tags.put(CONFIRMATION.toString(), "billing/PaymentConfirmation");

    MethodModel m =
        new MethodModel(
            "pay",
            "pay",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            INVOICE,
            CONFIRMATION,
            false,
            true,
            false);

    ServiceModel svc =
        new ServiceModel(
            "MissionControl", 1, Scope.SHARED, SERVICE, List.of(m), "", List.of(), tags);
    String src = DispatcherEmitter.emit(svc).toString();

    assertTrue(src.contains("safeRegister(fory, Invoice.class, \"billing/Invoice\")"), src);
    assertTrue(
        src.contains(
            "safeRegister(fory, PaymentConfirmation.class, \"billing/PaymentConfirmation\")"),
        src);
    // MethodDescriptor carries the wire tag directly — cross-lang contract hash depends on it.
    assertTrue(src.contains("\"billing/Invoice\""), src);
    assertTrue(src.contains("\"billing/PaymentConfirmation\""), src);
    // And the derived fallback must NOT appear.
    assertFalse(src.contains("\"com.example.billing/Invoice\""), src);
  }

  @Test
  void inlineParamTypeUsesOverriddenWireTag() {
    Map<String, String> tags = Map.of(INVOICE.toString(), "billing/Invoice");
    MethodModel m =
        new MethodModel(
            "stamp",
            "stamp",
            StreamingKind.UNARY,
            RequestStyle.INLINE,
            List.of(new ParamModel("invoice", INVOICE)),
            null,
            CONFIRMATION,
            false,
            false,
            false);

    ServiceModel svc =
        new ServiceModel(
            "MissionControl", 1, Scope.SHARED, SERVICE, List.of(m), "", List.of(), tags);
    String src = DispatcherEmitter.emit(svc).toString();

    assertTrue(src.contains("safeRegister(fory, Invoice.class, \"billing/Invoice\")"), src);
    assertFalse(src.contains("\"com.example.billing/Invoice\""), src);
  }

  @Test
  void typesWithoutEntryFallBackToPackageSlashSimpleName() {
    // Sanity: the map is override-only. Types not in the map still get the derived tag.
    ClassName untagged = ClassName.get("com.example.mission", "StatusResponse");
    MethodModel m =
        new MethodModel(
            "ping",
            "ping",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            INVOICE,
            untagged,
            false,
            true,
            false);

    ServiceModel svc =
        new ServiceModel(
            "MissionControl",
            1,
            Scope.SHARED,
            SERVICE,
            List.of(m),
            "",
            List.of(),
            Map.of(INVOICE.toString(), "billing/Invoice"));
    String src = DispatcherEmitter.emit(svc).toString();

    assertTrue(src.contains("safeRegister(fory, Invoice.class, \"billing/Invoice\")"), src);
    assertTrue(
        src.contains(
            "safeRegister(fory, StatusResponse.class, \"com.example.mission/StatusResponse\")"),
        src);
  }

  @Test
  void emptyMapLeavesDerivedTags() {
    MethodModel m =
        new MethodModel(
            "pay",
            "pay",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            List.of(),
            INVOICE,
            CONFIRMATION,
            false,
            true,
            false);
    ServiceModel svc = new ServiceModel("Mission", 1, Scope.SHARED, SERVICE, List.of(m));
    String src = DispatcherEmitter.emit(svc).toString();

    assertTrue(
        src.contains("safeRegister(fory, Invoice.class, \"com.example.billing/Invoice\")"), src);
    assertFalse(src.contains("\"billing/Invoice\""), src);
  }
}
