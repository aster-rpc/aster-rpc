package site.aster.contract;

import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Capability requirement helpers — shorthand builders that mirror Python's {@code role(...) /
 * any_of(...) / all_of(...)} plus a runtime evaluator matching {@code
 * aster.trust.rcan.evaluate_capability}.
 *
 * <p>The canonical attribute key is {@code "aster.role"}. Its value is a comma-separated list of
 * role strings populated at admission time (Python parity — see {@code
 * bindings/python/aster/trust/rcan.py}).
 */
public final class Capabilities {

  /** Canonical admission attribute key carrying the caller's roles (comma-separated). */
  public static final String ROLE_ATTRIBUTE = "aster.role";

  private Capabilities() {}

  /** Caller must carry the single role. */
  public static CapabilityRequirement role(String role) {
    return new CapabilityRequirement(CapabilityKind.ROLE, List.of(role));
  }

  /** Caller must carry at least one of the roles. */
  public static CapabilityRequirement anyOf(String... roles) {
    return new CapabilityRequirement(CapabilityKind.ANY_OF, Arrays.asList(roles));
  }

  /** Caller must carry every role. */
  public static CapabilityRequirement allOf(String... roles) {
    return new CapabilityRequirement(CapabilityKind.ALL_OF, Arrays.asList(roles));
  }

  /**
   * Return {@code true} if the caller {@code attributes} satisfy {@code req}. A {@code null} {@code
   * req} is vacuously satisfied.
   */
  public static boolean evaluate(CapabilityRequirement req, Map<String, String> attributes) {
    if (req == null) {
      return true;
    }
    Set<String> callerRoles = extractRoles(attributes);
    List<String> required = req.roles();
    return switch (req.kind()) {
      case ROLE -> required.isEmpty() || callerRoles.contains(required.get(0));
      case ANY_OF -> {
        if (required.isEmpty()) {
          yield true;
        }
        for (String r : required) {
          if (callerRoles.contains(r)) {
            yield true;
          }
        }
        yield false;
      }
      case ALL_OF -> {
        if (required.isEmpty()) {
          yield true;
        }
        for (String r : required) {
          if (!callerRoles.contains(r)) {
            yield false;
          }
        }
        yield true;
      }
    };
  }

  private static Set<String> extractRoles(Map<String, String> attributes) {
    if (attributes == null) {
      return Collections.emptySet();
    }
    String raw = attributes.getOrDefault(ROLE_ATTRIBUTE, "");
    if (raw.isEmpty()) {
      return Collections.emptySet();
    }
    Set<String> out = new HashSet<>();
    for (String part : raw.split(",")) {
      String trimmed = part.trim();
      if (!trimmed.isEmpty()) {
        out.add(trimmed);
      }
    }
    return out;
  }
}
