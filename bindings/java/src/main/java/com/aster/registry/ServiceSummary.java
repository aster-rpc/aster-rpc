package com.aster.registry;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.core.JsonProcessingException;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Compact service descriptor returned in ConsumerAdmissionResponse (Aster-SPEC.md §3.2.2).
 *
 * <p>Provides enough information for a consumer to select a service and fetch its contract without
 * joining the registry doc.
 */
@JsonInclude(JsonInclude.Include.ALWAYS)
public final class ServiceSummary {

  @JsonProperty("name")
  public String name = "";

  @JsonProperty("version")
  public int version;

  @JsonProperty("contract_id")
  public String contractId = "";

  @JsonProperty("channels")
  public Map<String, String> channels = new LinkedHashMap<>();

  @JsonProperty("pattern")
  public String pattern = "shared";

  @JsonProperty("serialization_modes")
  public List<String> serializationModes = new ArrayList<>();

  public String toJson() {
    try {
      return RegistryMapper.MAPPER.writeValueAsString(this);
    } catch (JsonProcessingException e) {
      throw new IllegalStateException("ServiceSummary serialization failed", e);
    }
  }

  public static ServiceSummary fromJson(String json) {
    try {
      return RegistryMapper.MAPPER.readValue(json, ServiceSummary.class);
    } catch (JsonProcessingException e) {
      throw new IllegalArgumentException("ServiceSummary deserialization failed", e);
    }
  }
}
