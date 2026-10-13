package site.aster.enrollment;

import java.util.List;
import java.util.Map;

/**
 * The claims a credential carries. A {@code null} set is unknown; an empty set is complete and
 * empty.
 */
public record EnrollmentClaims(
    List<String> scopes, List<String> tags, Map<String, Attribute> attributes) {

  /** Complete and empty claims. */
  public static EnrollmentClaims empty() {
    return new EnrollmentClaims(List.of(), List.of(), Map.of());
  }

  /** Complete claims carrying {@code roles} in {@code aster.role} (comma-separated). */
  public static EnrollmentClaims roles(String roles) {
    return new EnrollmentClaims(
        List.of(), List.of(), Map.of("aster.role", Attribute.string(roles)));
  }
}
