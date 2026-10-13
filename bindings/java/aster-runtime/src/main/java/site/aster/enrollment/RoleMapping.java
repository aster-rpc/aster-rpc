package site.aster.enrollment;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Turns a peer's verified claims into the attributes Gate 3 checks {@code @Requires} against
 * ({@code [enrollment.roles]}; Rust {@code RoleMapping}). Roles = the comma-separated values of the
 * string claim attribute {@code attribute}, plus every verified scope when {@code scopesAsRoles},
 * plus the roles {@code scopes} and {@code tags} map each verified scope or tag to; sorted,
 * deduplicated and joined into {@code aster.role}. Rust evaluates it on every call from the peer's
 * live session, so a revocation, promotion or renewal applies on the next call.
 *
 * @param attribute the claim attribute listing roles ({@code null} = {@code "aster.role"}, {@code
 *     ""} disables it); TOML {@code attribute}, env {@code ASTER_ENROLLMENT_ROLE_ATTRIBUTE}
 * @param scopesAsRoles every verified scope is also a role ({@code
 *     ASTER_ENROLLMENT_SCOPES_AS_ROLES})
 * @param scopes verified scope → roles ({@code [enrollment.roles.scopes]})
 * @param tags verified tag → roles ({@code [enrollment.roles.tags]})
 * @param includeAttributes pass the other claim attributes to handlers as strings ({@code null} =
 *     true; {@code ASTER_ENROLLMENT_INCLUDE_ATTRIBUTES}). A claim named {@code aster.role} is never
 *     passed through.
 */
public record RoleMapping(
    String attribute,
    boolean scopesAsRoles,
    Map<String, List<String>> scopes,
    Map<String, List<String>> tags,
    Boolean includeAttributes) {

  /** The default mapping: roles from the {@code aster.role} claim attribute. */
  public static final RoleMapping DEFAULTS = new RoleMapping(null, false, Map.of(), Map.of(), null);

  public RoleMapping {
    scopes = copy(scopes);
    tags = copy(tags);
  }

  private static Map<String, List<String>> copy(Map<String, List<String>> m) {
    if (m == null || m.isEmpty()) {
      return Map.of();
    }
    Map<String, List<String>> out = new LinkedHashMap<>();
    m.forEach((k, v) -> out.put(k, v == null ? List.of() : List.copyOf(v)));
    return java.util.Collections.unmodifiableMap(out);
  }
}
