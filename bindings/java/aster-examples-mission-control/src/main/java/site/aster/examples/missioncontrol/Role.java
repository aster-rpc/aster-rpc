package site.aster.examples.missioncontrol;

/**
 * Capability role strings for Mission Control (Chapter 5). Mirrors {@code
 * examples/python/mission_control/roles.py} — each constant is the exact string that appears in
 * {@code CallContext.attributes()} under the canonical {@code aster.role} key.
 */
public final class Role {

  public static final String STATUS = "ops.status";
  public static final String LOGS = "ops.logs";
  public static final String ADMIN = "ops.admin";
  public static final String INGEST = "ops.ingest";

  private Role() {}
}
