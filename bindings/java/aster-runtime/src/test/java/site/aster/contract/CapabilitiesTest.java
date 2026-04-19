package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.Map;
import org.junit.jupiter.api.Test;

/**
 * Mirrors the Python evaluator in {@code bindings/python/aster/trust/rcan.py}: ROLE requires the
 * single role; ANY_OF accepts at least one; ALL_OF demands every role; empty role lists are
 * vacuously satisfied; a null requirement is vacuously satisfied.
 */
final class CapabilitiesTest {

  @Test
  void nullRequirementPasses() {
    assertTrue(Capabilities.evaluate(null, Map.of()));
  }

  @Test
  void roleRequirementMatches() {
    CapabilityRequirement req = Capabilities.role("ops.status");
    assertTrue(Capabilities.evaluate(req, Map.of("aster.role", "ops.status")));
    assertTrue(Capabilities.evaluate(req, Map.of("aster.role", "ops.logs,ops.status")));
    assertFalse(Capabilities.evaluate(req, Map.of("aster.role", "ops.logs")));
    assertFalse(Capabilities.evaluate(req, Map.of()));
  }

  @Test
  void anyOfNeedsAtLeastOne() {
    CapabilityRequirement req = Capabilities.anyOf("ops.logs", "ops.admin");
    assertTrue(Capabilities.evaluate(req, Map.of("aster.role", "ops.admin")));
    assertTrue(Capabilities.evaluate(req, Map.of("aster.role", "ops.status,ops.logs")));
    assertFalse(Capabilities.evaluate(req, Map.of("aster.role", "ops.status")));
  }

  @Test
  void allOfRequiresEvery() {
    CapabilityRequirement req = Capabilities.allOf("ops.logs", "ops.admin");
    assertTrue(Capabilities.evaluate(req, Map.of("aster.role", "ops.logs,ops.admin,ops.status")));
    assertFalse(Capabilities.evaluate(req, Map.of("aster.role", "ops.logs")));
    assertFalse(Capabilities.evaluate(req, Map.of("aster.role", "ops.admin")));
  }

  @Test
  void whitespaceAroundRolesIsTrimmed() {
    CapabilityRequirement req = Capabilities.role("ops.admin");
    assertTrue(Capabilities.evaluate(req, Map.of("aster.role", " ops.admin , ops.logs ")));
  }

  @Test
  void emptyRoleListIsVacuouslySatisfied() {
    CapabilityRequirement req =
        new CapabilityRequirement(CapabilityKind.ANY_OF, java.util.List.of());
    assertTrue(Capabilities.evaluate(req, Map.of()));
  }
}
