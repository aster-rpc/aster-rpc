package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonPropertyOrder;
import java.util.List;

/** Role-based capability requirement attached to a {@link MethodDef} or {@link ServiceContract}. */
@JsonPropertyOrder({"kind", "roles"})
public record CapabilityRequirement(
    @JsonProperty("kind") CapabilityKind kind, @JsonProperty("roles") List<String> roles) {

  public CapabilityRequirement {
    kind = kind == null ? CapabilityKind.ROLE : kind;
    roles = roles == null ? List.of() : List.copyOf(roles);
  }
}
