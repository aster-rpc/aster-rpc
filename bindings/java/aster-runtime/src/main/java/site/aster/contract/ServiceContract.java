package site.aster.contract;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.annotation.JsonPropertyOrder;
import com.fasterxml.jackson.core.JsonProcessingException;
import java.util.List;

/**
 * Top-level contract identity descriptor. The canonical bytes of this record, run through BLAKE3,
 * yield the {@code contract_id}. Java hands the JSON form to the Rust FFI ({@link
 * ContractIdentity#computeContractId(String)}); no Java-side canonicalization.
 */
@JsonPropertyOrder({
  "name",
  "version",
  "methods",
  "serialization_modes",
  "scoped",
  "requires",
  "producer_language"
})
@JsonInclude(JsonInclude.Include.ALWAYS)
public record ServiceContract(
    @JsonProperty("name") String name,
    @JsonProperty("version") int version,
    @JsonProperty("methods") List<MethodDef> methods,
    @JsonProperty("serialization_modes") List<String> serializationModes,
    @JsonProperty("scoped") ScopeKind scoped,
    @JsonProperty("requires") CapabilityRequirement requires,
    @JsonProperty("producer_language") String producerLanguage) {

  public ServiceContract {
    name = name == null ? "" : name;
    methods = methods == null ? List.of() : List.copyOf(methods);
    serializationModes = serializationModes == null ? List.of() : List.copyOf(serializationModes);
    scoped = scoped == null ? ScopeKind.SHARED : scoped;
    producerLanguage = producerLanguage == null ? "" : producerLanguage;
  }

  @JsonCreator
  public static ServiceContract deserialize(
      @JsonProperty("name") String name,
      @JsonProperty("version") Integer version,
      @JsonProperty("methods") List<MethodDef> methods,
      @JsonProperty("serialization_modes") List<String> serializationModes,
      @JsonProperty("scoped") ScopeKind scoped,
      @JsonProperty("requires") CapabilityRequirement requires,
      @JsonProperty("producer_language") String producerLanguage) {
    return new ServiceContract(
        name,
        version == null ? 0 : version,
        methods,
        serializationModes,
        scoped,
        requires,
        producerLanguage);
  }

  /** Serialize this contract to JSON using the canonical shape Rust's serde expects. */
  public String toJson() {
    try {
      return ContractJson.mapper().writeValueAsString(this);
    } catch (JsonProcessingException e) {
      throw new IllegalStateException("serialize ServiceContract failed", e);
    }
  }
}
