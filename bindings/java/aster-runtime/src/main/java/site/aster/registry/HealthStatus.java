package site.aster.registry;

import java.util.Set;

/**
 * Health state of a registered endpoint (Aster-SPEC.md §11.6).
 *
 * <p>Modeled as a constants class (not enum) because the wire format uses lowercase string values
 * that pass through the registry doc and FFI layer untouched.
 */
public final class HealthStatus {

  public static final String STARTING = "starting";
  public static final String READY = "ready";
  public static final String DEGRADED = "degraded";
  public static final String DRAINING = "draining";

  private static final Set<String> VALID = Set.of(STARTING, READY, DEGRADED, DRAINING);

  private HealthStatus() {}

  public static String validate(String value) {
    if (!VALID.contains(value)) {
      throw new IllegalArgumentException("Invalid HealthStatus: " + value);
    }
    return value;
  }

  public static boolean isRoutable(String status) {
    return READY.equals(status) || DEGRADED.equals(status);
  }
}
